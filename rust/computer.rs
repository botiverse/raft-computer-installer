//! Bounded native product calls. Output is never copied into public errors:
//! Computer may print authentication material during diagnostics.
use crate::{Error, Result, config::Config, process, version};
use k_carrier::{
    error::invalid,
    state::Evidence,
    storage::{ensure_dir, sync_dir, write_json},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ffi::OsString, fs, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{Instant, sleep, timeout},
};

const OUTPUT_LIMIT: usize = 1024 * 1024;
const DIAGNOSTIC_STDERR_LIMIT: usize = 2048;

pub struct CommandResult {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleFailure {
    pub kind: String,
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
    pub stderr_truncated: bool,
}

#[derive(Clone, Debug)]
pub struct LifecycleResult {
    pub success: bool,
    pub failure: Option<LifecycleFailure>,
}

fn credential_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "authorization",
        "bearer",
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "api key",
        "api-key",
        "apikey",
        "access_key",
        "access key",
        "access-key",
        "client_secret",
        "client secret",
        "client-secret",
        "cookie",
        "database_url",
        "database-url",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn contains_url_userinfo(value: &str) -> bool {
    let mut remainder = value;
    while let Some(index) = remainder.find("://") {
        let after_scheme = &remainder[index + 3..];
        let authority = after_scheme
            .split(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#'))
            .next()
            .unwrap_or_default();
        // A URL authority may carry either `user:password@host` or a token as
        // the username (`token@host`). Git commonly echoes both forms when a
        // remote operation fails, so neither is safe diagnostic text.
        if authority.rfind('@').is_some_and(|at| at > 0) {
            return true;
        }
        remainder = after_scheme;
    }
    false
}

fn redact_opaque_word(word: &str) -> String {
    let opaque = word.len() >= 24
        && !word.contains('/')
        && !word.contains('\\')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c));
    if opaque {
        "<redacted>".into()
    } else {
        word.into()
    }
}

pub(crate) fn diagnostic_stderr(bytes: &[u8]) -> (String, bool) {
    let original_truncated = bytes.len() > DIAGNOSTIC_STDERR_LIMIT;
    let mut redacted = Vec::new();
    let mut redact_next_nonempty_line = false;
    for line in String::from_utf8_lossy(bytes).lines() {
        let normalized = line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>();
        let trimmed = normalized.trim();
        let redact_from_previous_line = redact_next_nonempty_line && !trimmed.is_empty();
        if redact_from_previous_line {
            redact_next_nonempty_line = false;
        }
        let credential_line = credential_marker(&normalized);
        if credential_line && matches!(trimmed.chars().last(), Some(':' | '=')) {
            // Pretty-printed configuration and JSON may put the value on the
            // next line. Carry the redaction across blank lines to the next
            // non-empty line, but no farther.
            redact_next_nonempty_line = true;
        }
        // A diagnostic does not need partial fidelity on a credential-bearing
        // line. Redacting the entire line avoids delimiter, quoting, multi-word
        // header, cookie-list, and URL-userinfo variants leaking short values.
        if redact_from_previous_line || credential_line || contains_url_userinfo(&normalized) {
            redacted.push("<redacted>".into());
            continue;
        }
        for word in normalized.split_whitespace() {
            redacted.push(redact_opaque_word(word));
        }
    }
    let redacted = redacted.join(" ");
    if redacted.len() <= DIAGNOSTIC_STDERR_LIMIT {
        return (redacted, original_truncated);
    }
    let mut start = redacted.len() - DIAGNOSTIC_STDERR_LIMIT;
    while !redacted.is_char_boundary(start) {
        start += 1;
    }
    (redacted[start..].into(), true)
}

async fn bounded_read(mut stream: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(n) > OUTPUT_LIMIT {
            return Err(std::io::Error::other(
                "product command output exceeded limit",
            ));
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

pub async fn run(
    binary: &Path,
    args: &[&str],
    environment: &BTreeMap<OsString, OsString>,
    budget: Duration,
) -> Result<CommandResult> {
    let mut command = Command::new(binary);
    command
        .args(args)
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW; setup uses the retained console instead.
    let mut child = command
        .spawn()
        .map_err(|_| invalid("product command could not start"))?;
    let pid = child
        .id()
        .ok_or_else(|| invalid("product command has no process identity"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("product stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| invalid("product stderr unavailable"))?;
    let result = timeout(budget, async {
        tokio::try_join!(child.wait(), bounded_read(stdout), bounded_read(stderr))
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok(CommandResult {
            success: status.success(),
            stdout,
            stderr,
            exit_code: status.code(),
            pid,
        }),
        failure => {
            // Wait for the bounded controller process before returning. The K
            // host fence also accounts for controllers after a worker crash.
            let _ = child.start_kill();
            if !matches!(
                timeout(Duration::from_secs(5), child.wait()).await,
                Ok(Ok(_))
            ) {
                return Err(crate::Error::Uncertain(
                    "product command could not be reaped".into(),
                ));
            }
            Err(invalid(if failure.is_err() {
                "product command timed out"
            } else {
                "product command output could not be read"
            }))
        }
    }
}

pub async fn self_report(binary: &Path, cfg: &Config) -> Result<Evidence> {
    let home = tempfile::Builder::new()
        .prefix("raft-installer-probe-")
        .tempdir()?;
    let mut environment = cfg.environment();
    environment.insert("RAFT_HOME".into(), home.path().as_os_str().to_owned());
    environment.insert("SLOCK_HOME".into(), home.path().as_os_str().to_owned());
    let result = run(
        binary,
        &["--version"],
        &environment,
        Duration::from_secs(20),
    )
    .await?;
    if !result.success {
        return Err(invalid("product self-report failed"));
    }
    let output =
        std::str::from_utf8(&result.stdout).map_err(|_| invalid("invalid product self-report"))?;
    let reported = output
        .split_whitespace()
        .next()
        .ok_or_else(|| invalid("empty product self-report"))?;
    Ok(Evidence {
        version: version::normalize(reported)?,
        pid: result.pid,
        start_id: format!("cold-{}-{}", result.pid, uuid::Uuid::new_v4()),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAttestation {
    service_pid: u32,
    computer_version: String,
    service_generation: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStatus {
    attestation: Option<WireAttestation>,
    next_step: Option<String>,
}

pub struct Status {
    /// Product evidence; the host must additionally verify OS process identity.
    pub evidence: Option<Evidence>,
    pub next_step: Option<String>,
}

pub async fn status(cfg: &Config) -> Result<Status> {
    let result = run(
        &cfg.binary,
        &["status", "--json"],
        &cfg.environment(),
        Duration::from_secs(30),
    )
    .await?;
    if !result.success {
        return Err(invalid("product status unavailable"));
    }
    let status: WireStatus =
        serde_json::from_slice(&result.stdout).map_err(|_| invalid("product status invalid"))?;
    let evidence = match status.attestation {
        Some(a) => {
            if a.service_pid <= 1
                || a.service_pid == std::process::id()
                || a.computer_version.is_empty()
                || a.computer_version.len() > 256
                || a.service_generation.trim().is_empty()
                || a.service_generation.len() > 256
            {
                return Err(invalid("product attestation invalid"));
            }
            Some(Evidence {
                version: version::normalize(&a.computer_version)?,
                pid: a.service_pid,
                start_id: a.service_generation,
            })
        }
        None => None,
    };
    // Keep the product's actionable hint, but never pass terminal control bytes
    // or unbounded output into the installer's one-line result.
    let next_step = status
        .next_step
        .filter(|v| !v.trim().is_empty() && v.len() <= 512 && !v.chars().any(char::is_control));
    Ok(Status {
        evidence,
        next_step,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandRecord {
    format_version: u32,
    id: String,
    action: String,
    helper: process::Identity,
    completed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LifecycleRequest {
    protocol_version: u32,
    id: String,
    action: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LifecycleResponse {
    protocol_version: u32,
    completed: bool,
    success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<LifecycleFailure>,
}

fn command_path(cfg: &Config, id: &str) -> Result<std::path::PathBuf> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(invalid("invalid lifecycle command identity"));
    }
    Ok(cfg
        .installer_dir
        .join("commands")
        .join(format!("{id}.json")))
}

/// A helper waits for stdin authorization. Its OS identity is durable before
/// it may invoke Computer, and it survives the calling host controller. That
/// closes the late-effect window when K kills a timed-out controller while a
/// product `start` or `stop` is still in flight.
pub async fn lifecycle(cfg: &Config, action: &str) -> Result<LifecycleResult> {
    if !["start", "stop"].contains(&action) {
        return Err(invalid("invalid product lifecycle action"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let path = command_path(cfg, &id)?;
    ensure_dir(
        path.parent()
            .ok_or_else(|| invalid("command directory missing"))?,
    )?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--product-command")
        .envs(cfg.environment())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| invalid("product helper has no pid"))?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| invalid("product helper input missing"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| invalid("product helper output missing"))?;
    let helper =
        process::observe(pid)?.ok_or_else(|| invalid("product helper identity missing"))?;
    write_json(
        &path,
        &CommandRecord {
            format_version: 1,
            id: id.clone(),
            action: action.into(),
            helper,
            completed: false,
        },
    )?;
    let request = LifecycleRequest {
        protocol_version: 1,
        id,
        action: action.into(),
    };
    input.write_all(&serde_json::to_vec(&request)?).await?;
    input.shutdown().await?;
    drop(input);
    let result = timeout(Duration::from_secs(55), async {
        tokio::try_join!(child.wait(), bounded_read(output))
    })
    .await;
    let (status, bytes) = match result {
        Ok(Ok(value)) => value,
        _ => {
            return Err(Error::Uncertain(
                "product lifecycle command has not settled".into(),
            ));
        }
    };
    let response: LifecycleResponse = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Uncertain("product lifecycle response is unreadable".into()))?;
    if !status.success() || response.protocol_version != 1 || !response.completed {
        return Err(Error::Uncertain(
            "product lifecycle command has not settled".into(),
        ));
    }
    let valid_failure = response.failure.as_ref().is_some_and(|failure| {
        !response.success
            && ["nonzero-exit", "command-error"].contains(&failure.kind.as_str())
            && failure.argv == [action]
            && failure.stderr_tail.len() <= DIAGNOSTIC_STDERR_LIMIT
            && !failure.stderr_tail.chars().any(char::is_control)
    });
    if response.success != response.failure.is_none()
        || (response.failure.is_some() && !valid_failure)
    {
        return Err(Error::Uncertain(
            "product lifecycle diagnostic is invalid".into(),
        ));
    }
    let record: CommandRecord = serde_json::from_slice(&fs::read(&path)?)?;
    if !record.completed {
        return Err(Error::Uncertain(
            "product lifecycle completion was not saved".into(),
        ));
    }
    fs::remove_file(&path)?;
    sync_dir(
        path.parent()
            .ok_or_else(|| invalid("command directory missing"))?,
    )?;
    Ok(LifecycleResult {
        success: response.success,
        failure: response.failure,
    })
}

pub async fn serve_lifecycle(cfg: &Config) -> Result<u8> {
    k_carrier::host::isolate_standard_handles()?;
    let mut bytes = Vec::new();
    timeout(
        Duration::from_secs(30),
        tokio::io::stdin().take(16385).read_to_end(&mut bytes),
    )
    .await
    .map_err(|_| invalid("product helper input timeout"))??;
    if bytes.len() > 16384 {
        return Err(invalid("product helper input too large"));
    }
    let request: LifecycleRequest = serde_json::from_slice(&bytes)?;
    if request.protocol_version != 1 || !["start", "stop"].contains(&request.action.as_str()) {
        return Err(invalid("invalid product helper request"));
    }
    let path = command_path(cfg, &request.id)?;
    let mut record: CommandRecord = serde_json::from_slice(&fs::read(&path)?)?;
    if record.format_version != 1
        || record.id != request.id
        || record.action != request.action
        || record.helper.pid != std::process::id()
        || record.completed
        || !process::matches(&record.helper)?
    {
        return Err(invalid("product helper authorization mismatch"));
    }
    let result = run(
        &cfg.binary,
        &[&request.action],
        &cfg.environment(),
        Duration::from_secs(45),
    )
    .await;
    let completed = !result.as_ref().is_err_and(|e| e.is_uncertain());
    let (success, failure) = match result {
        Ok(result) if result.success => (true, None),
        Ok(result) => {
            let (stderr_tail, stderr_truncated) = diagnostic_stderr(&result.stderr);
            (
                false,
                Some(LifecycleFailure {
                    kind: "nonzero-exit".into(),
                    argv: vec![request.action.clone()],
                    exit_code: result.exit_code,
                    stderr_tail,
                    stderr_truncated,
                }),
            )
        }
        Err(error) if error.is_uncertain() => (false, None),
        Err(_) => (
            false,
            Some(LifecycleFailure {
                kind: "command-error".into(),
                argv: vec![request.action.clone()],
                exit_code: None,
                stderr_tail: String::new(),
                stderr_truncated: false,
            }),
        ),
    };
    if completed {
        record.completed = true;
        write_json(&path, &record)?;
    }
    let mut bytes = serde_json::to_vec(&LifecycleResponse {
        protocol_version: 1,
        completed,
        success,
        failure,
    })?;
    bytes.push(b'\n');
    let mut output = tokio::io::stdout();
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(if completed { 0 } else { 3 })
}

pub async fn fence(cfg: &Config) -> Result<()> {
    let directory = cfg.installer_dir.join("commands");
    ensure_dir(&directory)?;
    let deadline = Instant::now() + Duration::from_secs(110);
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            return Err(Error::Uncertain("invalid product command record".into()));
        }
        let record: CommandRecord = serde_json::from_slice(&fs::read(&path)?)?;
        if record.format_version != 1
            || command_path(cfg, &record.id)? != path
            || !["start", "stop"].contains(&record.action.as_str())
        {
            return Err(Error::Uncertain("invalid product command record".into()));
        }
        while process::matches(&record.helper)? {
            if Instant::now() >= deadline {
                return Err(Error::Uncertain("product lifecycle fence timed out".into()));
            }
            sleep(Duration::from_millis(100)).await;
        }
        let latest: CommandRecord = serde_json::from_slice(&fs::read(&path)?)?;
        if latest.id != record.id || latest.helper != record.helper || !latest.completed {
            // A crashed helper cannot prove that its child finished. Retain the
            // evidence for explicit repair; never declare a late effect fenced.
            return Err(Error::Uncertain(
                "product lifecycle helper exited without durable completion".into(),
            ));
        }
        fs::remove_file(path)?;
    }
    sync_dir(&directory)
}

/// Explicit repair first waits for every still-live helper. A helper that died
/// without completion is not accepted by ordinary recovery; repair may stop all
/// identified product processes and preserve those records in quarantine.
pub async fn wait_for_repair(cfg: &Config) -> Result<bool> {
    let directory = cfg.installer_dir.join("commands");
    ensure_dir(&directory)?;
    let deadline = Instant::now() + Duration::from_secs(110);
    let mut abandoned = false;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|name| name.strip_prefix(".k-write-"))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        {
            continue; // Uncommitted durable-write staging is preserved below.
        }
        let record: CommandRecord = serde_json::from_slice(&fs::read(entry.path())?)?;
        if record.format_version != 1
            || command_path(cfg, &record.id)? != entry.path()
            || !["start", "stop"].contains(&record.action.as_str())
        {
            return Err(Error::Uncertain(
                "unreadable product command lifetime".into(),
            ));
        }
        while process::matches(&record.helper)? {
            if Instant::now() >= deadline {
                return Err(Error::Uncertain(
                    "product helper still active during repair".into(),
                ));
            }
            sleep(Duration::from_millis(100)).await;
        }
        let latest: CommandRecord = serde_json::from_slice(&fs::read(entry.path())?)?;
        if latest.helper != record.helper || latest.id != record.id {
            return Err(Error::Uncertain(
                "product command identity changed during repair".into(),
            ));
        }
        abandoned |= !latest.completed;
    }
    Ok(abandoned)
}

pub async fn preserve_commands_after_stop(
    cfg: &Config,
    id: &str,
) -> Result<Option<std::path::PathBuf>> {
    wait_for_repair(cfg).await?;
    if !crate::host::installed_product_processes(cfg)?.is_empty() {
        return Err(Error::Uncertain(
            "product remains active while preserving command records".into(),
        ));
    }
    let directory = cfg.installer_dir.join("commands");
    if fs::read_dir(&directory)?.next().is_none() {
        return Ok(None);
    }
    let parent = cfg.installer_dir.join("quarantine");
    ensure_dir(&parent)?;
    let destination = parent.join(format!(
        "commands-{}-{}",
        k_carrier::artifact::sha256(id.as_bytes()),
        uuid::Uuid::new_v4()
    ));
    fs::rename(&directory, &destination)?;
    sync_dir(&cfg.installer_dir)?;
    sync_dir(&parent)?;
    Ok(Some(destination))
}
