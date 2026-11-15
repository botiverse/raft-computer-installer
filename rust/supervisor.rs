//! Supervise the complete product operation, including fresh install and
//! repair, rather than only the K upgrade portion. Retry only after worker exit.
use crate::{
    Error, Result,
    config::Config,
    report::FailureCode,
    request::{Reply, Request},
};
use k_carrier::{
    artifact::sha256,
    error::invalid,
    storage::{ensure_dir, write_durable, write_json},
};
use std::{fs, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

/// The first worker's deadline: release resolution, downloads, the
/// candidate's first check and K's transaction (runner::UPGRADE_HOST_BUDGET_MS
/// bounds the preparation inside it).
pub const FIRST_ATTEMPT_BUDGET: Duration = Duration::from_secs(1200);
/// Each recovery worker's deadline.
pub const RECOVERY_ATTEMPT_BUDGET: Duration = Duration::from_secs(360);

fn attempt_budget(attempt: usize) -> Duration {
    if attempt == 0 {
        FIRST_ATTEMPT_BUDGET
    } else {
        RECOVERY_ATTEMPT_BUDGET
    }
}

pub async fn run(cfg: &Config, request: &Request) -> Result<Reply> {
    request.validate()?;
    let root = cfg.scratch().join("supervisors");
    ensure_dir(&root)?;
    let directory = tempfile::Builder::new()
        .prefix("operation-")
        .tempdir_in(root)?;
    let executable = directory.path().join(if cfg!(windows) {
        "installer.exe"
    } else {
        "installer"
    });
    let bytes = fs::read(std::env::current_exe()?)?;
    write_durable(&executable, &bytes, true)?;
    persist_durable_installer(cfg, &bytes)?;
    let digest = sha256(&bytes);
    let owner = crate::process::observe(std::process::id())?
        .ok_or_else(|| invalid("supervisor identity is unavailable"))?;
    write_json(
        &directory.path().join("recovery.json"),
        &serde_json::json!({
            "formatVersion":1,"sha256":digest,"size":bytes.len(),"request":request,"owner":owner
        }),
    )?;
    let mut last = Reply::failure(
        &request.id,
        3,
        String::from(
            "The installation could not be completed or recovered. Run the same install command again.",
        ),
        FailureCode::RecoveryUnresolved,
        "The installation could not be completed or recovered.",
    );
    for attempt in 0..3 {
        // Verify every execution, including retries of the retained copy.
        if sha256(&fs::read(&executable)?) != digest {
            return Err(invalid("supervisor executable identity changed"));
        }
        let mut next = request.clone();
        next.recovery_only |= attempt != 0;
        let mut child = Command::new(&executable)
            .arg("--operation-worker")
            .envs(cfg.environment())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| invalid("worker stdin missing"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| invalid("worker stdout missing"))?;
        // The first operation includes preparation and artifact transfer.
        // Recovery receives a shorter, independent deadline.
        let budget = attempt_budget(attempt);
        let result = timeout(budget, async {
            input.write_all(&serde_json::to_vec(&next)?).await?;
            input.shutdown().await?;
            drop(input);
            let mut bytes = Vec::new();
            output.take(65537).read_to_end(&mut bytes).await?;
            let status = child.wait().await?;
            if bytes.len() > 65536 {
                return Err(invalid("worker response too large"));
            }
            let reply: Reply = serde_json::from_slice(&bytes)?;
            reply.validate(&request.id)?;
            if status.code() != Some(i32::from(reply.exit_code)) {
                return Err(invalid("worker exit does not match its response"));
            }
            Ok::<_, Error>(reply)
        })
        .await;
        match result {
            Ok(Ok(reply)) => {
                if reply.exit_code == 2 && next.recovery_only {
                    // A busy recovery has not settled the original operation.
                    // Preserve its executable/request and its unresolved status.
                    last = Reply::failure(
                        &request.id,
                        3,
                        String::from(
                            "Recovery is blocked by another installation still running. Run the same install command again after it finishes.",
                        ),
                        FailureCode::RecoveryUnresolved,
                        "Recovery is blocked by another installation still running.",
                    );
                } else if reply.exit_code != 3 {
                    if reply.exit_code <= 1 && reply.receipt.is_some() {
                        // Housekeeping does not change the durable result or
                        // need intervention; retry on the next invocation.
                        let _ = remove_finished_supervisors(cfg, directory.path());
                    }
                    return Ok(reply);
                } else {
                    last = reply;
                }
            }
            _ => {
                let _ = child.start_kill();
                if !matches!(
                    timeout(Duration::from_secs(5), child.wait()).await,
                    Ok(Ok(_))
                ) {
                    let _retained = directory.keep();
                    // Killed: it writes nothing more.
                    return Ok(given_up(
                        cfg,
                        request,
                        Reply::failure(
                            &request.id,
                            3,
                            "The previous installation step has not exited. Recovery must wait for it.",
                            FailureCode::RecoveryUnresolved,
                            "The previous installation step has not exited.",
                        ),
                    ));
                }
            }
        }
    }
    // Preserve the exact executable and request when automatic recovery stops.
    let _retained = directory.keep();
    Ok(given_up(cfg, request, last))
}

/// Automatic recovery stopped, and every worker has exited. A worker that
/// timed out wrote no receipt, so persist this unresolved answer as one (the
/// same request's later run may still replace it: `Receipt::finish`).
/// Create-only: a receipt a worker did write stays.
fn given_up(cfg: &Config, request: &Request, reply: Reply) -> Reply {
    if reply.receipt.is_some() {
        return reply;
    }
    match crate::settle::settle(cfg, request, &reply, crate::settle::Claim::Own) {
        Some(receipt) => Reply::receipt(receipt),
        None => reply,
    }
}

fn remove_finished_supervisors(cfg: &Config, current: &std::path::Path) -> Result<()> {
    for name in ["active.json", "cleanup.json", "metadata-damage.json"] {
        if k_carrier::storage::exists(&cfg.installer_dir.join(name))? {
            return Ok(());
        }
    }
    for entry in fs::read_dir(cfg.scratch().join("supervisors"))? {
        let entry = entry?;
        if entry.path() == current
            || !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with("operation-")
        {
            continue;
        }
        let record = entry.path().join("recovery.json");
        if !fs::symlink_metadata(&record)
            .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink() && m.len() <= 65536)
        {
            continue;
        }
        let saved: serde_json::Value = match serde_json::from_slice(&fs::read(record)?) {
            Ok(saved) => saved,
            Err(_) => continue,
        };
        let Some(owner) = saved.get("owner") else {
            continue;
        };
        let owner: crate::process::Identity = match serde_json::from_value(owner.clone()) {
            Ok(owner) => owner,
            Err(_) => continue,
        };
        if !matches!(crate::process::matches(&owner), Ok(false)) {
            continue;
        }
        let executable = entry.path().join(if cfg!(windows) {
            "installer.exe"
        } else {
            "installer"
        });
        if !crate::process::installed(&executable)?.is_empty() {
            continue;
        }
        k_carrier::storage::remove_dir(&entry.path())?;
    }
    Ok(())
}

/// Refresh the stable installer copy that failure hints print. It lives
/// outside the scratch supervisors (which are cleaned once operations settle)
/// and is overwritten atomically only when the bytes change, so a hint stays
/// runnable after this process exits.
fn persist_durable_installer(cfg: &Config, bytes: &[u8]) -> Result<()> {
    let durable = cfg.durable_binary();
    if let Ok(existing) = fs::read(&durable)
        && sha256(&existing) == sha256(bytes)
    {
        return Ok(());
    }
    let parent = durable
        .parent()
        .ok_or_else(|| invalid("invalid durable binary path"))?;
    ensure_dir(parent)?;
    write_durable(&durable, bytes, true)
}

#[cfg(test)]
mod give_up_tests {
    use super::*;
    use crate::{
        presence::Presence,
        report::{self, Outcome},
    };

    /// Every worker ends without a valid reply (this test binary stands in
    /// for the worker: it rejects `--operation-worker` and exits, as a worker
    /// killed at its deadline leaves no reply), so the supervisor gives up.
    /// The request is a terminal run's, not a web upgrade's: the CLI writes
    /// nothing for it, so the receipt can only be the supervisor's own.
    #[tokio::test]
    async fn a_supervisor_that_gives_up_persists_its_answer_as_the_receipt() {
        let home = tempfile::tempdir().unwrap();
        let state_home = home.path().join(".raft");
        let cfg = Config {
            user_home: home.path().to_path_buf(),
            k_state: state_home.join("computer/k"),
            installer_dir: state_home.join("computer/installer"),
            state_home,
            install_dir: home.path().join(".local/bin"),
            binary: home.path().join(".local/bin/raft-computer"),
            sidecar: home.path().join(".local/bin/photon_rs_bg.wasm"),
            hands_origin: "https://hands.build".into(),
            hands_app: "raft-computer-cli".into(),
            waiting_caller: None,
        };
        let request = Request {
            protocol_version: 1,
            id: "local-given-up".into(),
            command: "upgrade".into(),
            version: Some("1.25.3".into()),
            channel: None,
            yes: true,
            allow_downgrade: false,
            presence: Presence::Unattended,
            approved_by: "terminal operator".into(),
            recovery_only: false,
            waiting_caller: None,
            escape_failure: None,
        };
        assert!(!crate::settle::from_web(&request));
        let reply = run(&cfg, &request).await.unwrap();
        assert_eq!(reply.exit_code, 3);
        let receipt = report::read(&cfg, "local-given-up").unwrap().unwrap();
        assert_eq!(reply.receipt.as_ref(), Some(&receipt));
        assert_eq!(receipt.outcome, Outcome::Unresolved);
        assert_eq!(receipt.code, Some(FailureCode::RecoveryUnresolved));
        assert_eq!(
            receipt.reason.as_deref(),
            Some(crate::settle::UNRESOLVED_REASON)
        );
        assert_eq!(
            receipt.detail.get("requestReason").map(String::as_str),
            Some("The installation could not be completed or recovered.")
        );
        assert_eq!(receipt.target_version.as_deref(), Some("1.25.3"));
    }
}
