//! Supervise the complete product operation, including fresh install and
//! repair, rather than only the K upgrade portion. Retry only after worker exit.
use crate::{
    Error, Result,
    config::Config,
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
    let digest = sha256(&bytes);
    let owner = crate::process::observe(std::process::id())?
        .ok_or_else(|| invalid("supervisor identity is unavailable"))?;
    write_json(
        &directory.path().join("recovery.json"),
        &serde_json::json!({
            "formatVersion":1,"sha256":digest,"size":bytes.len(),"request":request,"owner":owner
        }),
    )?;
    let mut last = Reply::plain(
        &request.id,
        3,
        format!(
            "Installation could not be settled. Run {} recover.",
            crate::report::installer_invocation()
        ),
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
        // The first operation may include interactive login. Recovery never
        // repeats login and receives a shorter, independent deadline.
        let budget = Duration::from_secs(if attempt == 0 { 1200 } else { 360 });
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
                    last = Reply::plain(
                        &request.id,
                        3,
                        format!(
                            "Recovery is blocked by another installer. Run {} recover after it exits.",
                            crate::report::installer_invocation()
                        ),
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
                    return Ok(Reply::plain(
                        &request.id,
                        3,
                        "The installer worker has not exited. Recovery must wait for it.",
                    ));
                }
            }
        }
    }
    // Preserve the exact executable and request when automatic recovery stops.
    let _retained = directory.keep();
    Ok(last)
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
