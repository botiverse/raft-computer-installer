//! Product lifecycle controller, launched separately by K's CommandHost.
//! The durable mode belongs to one operation and is never inferred anew during
//! recovery. Running images live at the installed path, outside K's slots.
use crate::{Error, Result, artifact, computer, config::Config, process, version};
use k_carrier::{
    error::invalid,
    state::{Evidence, OperationRead, Slot},
    storage::{FileStore, ensure_dir, now_ms, sync_dir, write_durable, write_json},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{Instant, sleep, timeout},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductState {
    pub format_version: u32,
    pub operation_id: String,
    pub target_version: String,
    pub from_version: String,
    pub running: bool,
    pub setup_known: bool,
    pub next_step: Option<String>,
    pub initial_processes: Vec<process::Identity>,
    pub forced_stops: Vec<u32>,
    pub last_start: Option<Start>,
    #[serde(default)]
    pub waiting_caller: Option<process::Identity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Start {
    pub slot: Slot,
    pub version: String,
    pub succeeded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_diagnostic: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartDiagnostic {
    format_version: u32,
    operation_id: String,
    slot: Slot,
    version: String,
    action: String,
    argv: Vec<String>,
    failure: String,
    exit_code: Option<i32>,
    stderr_tail: String,
    stderr_truncated: bool,
    recorded_at_ms: u64,
}

struct ProductStartError {
    error: Error,
    failure: computer::LifecycleFailure,
}

const START_DIAGNOSTIC_LIMIT: usize = 16;

pub struct Answer {
    pub running: bool,
    pub setup_known: bool,
    pub next_step: Option<String>,
    pub processes: Vec<process::Identity>,
}

pub async fn bind_recovery_caller(cfg: &Config, operation_id: &str) -> Result<()> {
    // The normal recovery path diagnoses missing/corrupt transaction state.
    let Ok(mut record) = read(cfg) else {
        return Ok(());
    };
    if record.operation_id != operation_id {
        return Ok(());
    }
    // execute() already holds the installer gate: a live CLI is not another
    // active installer. A predecessor's orphaned waiter may be terminated;
    // only this invocation's waiting caller must survive. Reuse OS-attested
    // termination, never a persisted PID alone, including after image rename.
    if let Some(previous) = &record.waiting_caller
        && !cfg
            .waiting_caller
            .as_ref()
            .is_some_and(|caller| process::same_instance(previous, caller))
        && let Some(live) = process::observe(previous.pid)?
        && process::same_instance(previous, &live)
    {
        process::terminate(&[live]).await?;
    }
    // Preserve the original running/stopped intent. The old waiter is not a
    // service to restart. A standalone recovery has no replacement caller.
    record.waiting_caller = cfg.waiting_caller.clone();
    save(cfg, &record)
}

/// Executables an existing product process may run from. Besides the installed
/// binary, a legacy two-layer installation keeps a launcher at the binary path
/// that execs into K's slot artifact, so its live processes run from the slot.
/// Post-install proof (`live_evidence`) still requires the installed binary.
fn product_executables(cfg: &Config) -> [PathBuf; 3] {
    let store = FileStore::new(&cfg.k_state);
    [
        cfg.binary.clone(),
        store.artifact(Slot::Stable),
        store.artifact(Slot::Experiment),
    ]
}

pub fn installed_product_processes(cfg: &Config) -> Result<Vec<process::Identity>> {
    let caller = cfg.waiting_caller.as_ref();
    let mut processes = Vec::new();
    for executable in product_executables(cfg) {
        for identity in process::installed(&executable)? {
            if !caller.is_some_and(|c| process::same_instance(c, &identity))
                && !processes.contains(&identity)
            {
                processes.push(identity);
            }
        }
    }
    processes.sort_by_key(|p| p.pid);
    Ok(processes)
}

/// Attest a status-reported product pid against any executable a product
/// process may run from, preferring the installed binary's diagnostic.
fn attest_product(cfg: &Config, pid: u32) -> Result<process::Identity> {
    let mut first_error = None;
    for executable in product_executables(cfg) {
        match process::attest(pid, &executable) {
            Ok(identity) => return Ok(identity),
            Err(error) if error.is_uncertain() => return Err(error),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    Err(first_error.expect("product_executables is non-empty"))
}

pub async fn answer(cfg: &Config) -> Result<Answer> {
    let status = match computer::status(cfg).await {
        Ok(status) => Some(status),
        Err(error) if error.is_uncertain() => return Err(error),
        Err(_) => None,
    };
    let mut processes = installed_product_processes(cfg)?;
    if let Some(evidence) = status.as_ref().and_then(|s| s.evidence.as_ref()) {
        let identity = attest_product(cfg, evidence.pid)?;
        if !processes.contains(&identity) {
            processes.push(identity);
        }
    }
    Ok(Answer {
        running: !processes.is_empty(),
        setup_known: status.is_some(),
        next_step: status.and_then(|s| s.next_step),
        processes,
    })
}

fn state_path(cfg: &Config) -> PathBuf {
    cfg.installer_dir.join("product-state.json")
}

fn operation(cfg: &Config) -> Result<k_carrier::state::Operation> {
    match FileStore::new(&cfg.k_state).read_operation() {
        OperationRead::Observed { operation } => Ok(operation),
        _ => Err(Error::Uncertain(
            "product state has no readable transaction identity".into(),
        )),
    }
}

pub async fn prepare(
    cfg: &Config,
    path: &std::path::Path,
    release: &k_carrier::artifact::Release,
) -> Result<()> {
    artifact::check_candidate(cfg, path, release).await?;
    let operation = operation(cfg)?;
    if operation.target_version != release.version || operation.outcome.is_some() {
        return Err(invalid("candidate preparation transaction mismatch"));
    }
    // Both sides of a rollback must have locally available, verified sidecars.
    artifact::saved_sidecar(cfg, &operation.from_version)?;
    let answer = answer(cfg).await?;
    save(
        cfg,
        &ProductState {
            format_version: 1,
            operation_id: operation.id,
            target_version: operation.target_version,
            from_version: operation.from_version,
            running: answer.running,
            setup_known: answer.setup_known,
            next_step: answer.next_step,
            initial_processes: answer.processes,
            forced_stops: vec![],
            last_start: None,
            waiting_caller: cfg.waiting_caller.clone(),
        },
    )
}

pub fn read(cfg: &Config) -> Result<ProductState> {
    let operation = operation(cfg)?;
    let bytes = fs::read(state_path(cfg))
        .map_err(|_| Error::Uncertain("product state is missing or unreadable".into()))?;
    if bytes.len() > 65536 {
        return Err(Error::Uncertain("product state is too large".into()));
    }
    let record: ProductState = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Uncertain("product state is invalid".into()))?;
    if record.format_version != 1
        || record.operation_id != operation.id
        || record.target_version != operation.target_version
        || record.from_version != operation.from_version
    {
        return Err(Error::Uncertain(
            "product state belongs to a different transaction".into(),
        ));
    }
    Ok(record)
}

fn save(cfg: &Config, record: &ProductState) -> Result<()> {
    write_json(&state_path(cfg), record)
}

pub async fn live_evidence(cfg: &Config) -> Result<Evidence> {
    let status = computer::status(cfg).await?;
    let evidence = status
        .evidence
        .ok_or_else(|| invalid("product returned no live attestation"))?;
    process::attest(evidence.pid, &cfg.binary)?;
    Ok(evidence)
}

pub async fn stop_product(cfg: &Config) -> Result<Vec<u32>> {
    // The product gets the first opportunity to unregister or stop its service.
    // The bounded fallback acts only on OS-identified instances of this binary.
    let _ = computer::lifecycle(cfg, "stop").await?;
    let processes = installed_product_processes(cfg)?;
    let remaining = process::wait_gone(&processes, Duration::from_secs(3)).await?;
    let forced = process::terminate(&remaining).await?;
    if !installed_product_processes(cfg)?.is_empty() {
        return Err(Error::Uncertain(
            "product service restarted while stopping".into(),
        ));
    }
    Ok(forced)
}

/// Repair uses the same controller lifetime fence without asking a new
/// controller to accept an abandoned product-command record as completed.
pub async fn fence_controllers_for_repair(cfg: &Config) -> Result<()> {
    let directory = cfg.k_state.join("controllers");
    if !k_carrier::storage::exists(&directory)? {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(110);
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let record: Value = serde_json::from_slice(&fs::read(entry.path())?)?;
        let pid = record
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 1)
            .ok_or_else(|| Error::Uncertain("controller lifetime record is unreadable".into()))?;
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|name| name.starts_with(&format!("{pid}-")))
        {
            return Err(Error::Uncertain(
                "controller lifetime identity mismatch".into(),
            ));
        }
        while k_carrier::lock::process_alive(pid) {
            if Instant::now() >= deadline {
                return Err(Error::Uncertain(
                    "controller is still active during repair".into(),
                ));
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

fn regular_destination(path: &std::path::Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            Err(invalid("installed destination is not a regular file"))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn publish(cfg: &Config, slot: Slot) -> Result<String> {
    let store = FileStore::new(&cfg.k_state);
    let version = store
        .version(slot)?
        .ok_or_else(|| invalid("slot is missing"))?;
    version::exact(&version)?;
    let binary = fs::read(store.artifact(slot))?;
    artifact::check_platform(&binary)?;
    let sidecar = artifact::saved_sidecar(cfg, &version)?
        .map(fs::read)
        .transpose()?;
    regular_destination(&cfg.binary)?;
    regular_destination(&cfg.sidecar)?;
    let directory = cfg
        .binary
        .parent()
        .ok_or_else(|| invalid("installed directory is missing"))?;
    ensure_dir(directory)?;
    // With service stopped and intent durable, interruption between these two
    // writes is recovered by republishing a complete slot and its sidecar.
    match sidecar {
        Some(bytes) => write_durable(&cfg.sidecar, &bytes, false)?,
        None => match fs::remove_file(&cfg.sidecar) {
            Ok(()) => sync_dir(directory)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        },
    }
    write_durable(&cfg.binary, &binary, true)?;
    Ok(version)
}

async fn start_product_attempt(cfg: &Config) -> std::result::Result<Evidence, ProductStartError> {
    let lifecycle = computer::lifecycle(cfg, "start")
        .await
        .map_err(|error| ProductStartError {
            error,
            failure: computer::LifecycleFailure {
                kind: "lifecycle-unsettled".into(),
                argv: vec!["start".into()],
                exit_code: None,
                stderr_tail: String::new(),
                stderr_truncated: false,
            },
        })?;
    if !lifecycle.success {
        return Err(ProductStartError {
            error: invalid("product start failed"),
            failure: lifecycle.failure.unwrap_or(computer::LifecycleFailure {
                kind: "command-error".into(),
                argv: vec!["start".into()],
                exit_code: None,
                stderr_tail: String::new(),
                stderr_truncated: false,
            }),
        });
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match live_evidence(cfg).await {
            Ok(evidence) => return Ok(evidence),
            Err(error) if error.is_uncertain() => {
                return Err(ProductStartError {
                    error,
                    failure: computer::LifecycleFailure {
                        kind: "readiness-uncertain".into(),
                        argv: vec!["start".into()],
                        exit_code: Some(0),
                        stderr_tail: String::new(),
                        stderr_truncated: false,
                    },
                });
            }
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(ProductStartError {
                error: invalid("product did not become ready"),
                failure: computer::LifecycleFailure {
                    kind: "readiness-timeout".into(),
                    argv: vec!["start".into()],
                    exit_code: Some(0),
                    stderr_tail: String::new(),
                    stderr_truncated: false,
                },
            });
        }
        sleep(Duration::from_millis(200)).await;
    }
}

pub async fn start_product(cfg: &Config) -> Result<Evidence> {
    start_product_attempt(cfg)
        .await
        .map_err(|failure| failure.error)
}

fn preserve_start_failure(
    cfg: &Config,
    record: &ProductState,
    slot: Slot,
    version: &str,
    failure: &computer::LifecycleFailure,
) -> Result<String> {
    let directory = cfg.installer_dir.join("diagnostics");
    ensure_dir(&directory)?;
    let name = format!("{}.json", uuid::Uuid::new_v4());
    write_json(
        &directory.join(&name),
        &StartDiagnostic {
            format_version: 1,
            operation_id: record.operation_id.clone(),
            slot,
            version: version.into(),
            action: "start".into(),
            argv: failure.argv.clone(),
            failure: failure.kind.clone(),
            exit_code: failure.exit_code,
            stderr_tail: failure.stderr_tail.clone(),
            stderr_truncated: failure.stderr_truncated,
            recorded_at_ms: now_ms(),
        },
    )?;
    // Keep the private evidence useful without turning repeated failures into
    // unbounded state. Cleanup is best-effort and never changes rollback.
    let _ = prune_start_diagnostics(&directory);
    Ok(format!("diagnostics/{name}"))
}

fn prune_start_diagnostics(directory: &std::path::Path) -> Result<()> {
    let mut diagnostics = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !entry.file_type()?.is_file()
            || !name.ends_with(".json")
            || uuid::Uuid::parse_str(name.trim_end_matches(".json")).is_err()
        {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(diagnostic) = serde_json::from_slice::<StartDiagnostic>(&bytes) else {
            continue;
        };
        diagnostics.push((diagnostic.recorded_at_ms, entry.path()));
    }
    diagnostics.sort_by_key(|(recorded_at_ms, _)| *recorded_at_ms);
    let excess = diagnostics.len().saturating_sub(START_DIAGNOSTIC_LIMIT);
    for (_, path) in diagnostics.into_iter().take(excess) {
        fs::remove_file(path)?;
    }
    if excess > 0 {
        sync_dir(directory)?;
    }
    Ok(())
}

pub fn latest_start_failure_diagnostic(cfg: &Config, operation_id: &str) -> Result<Option<String>> {
    let directory = cfg.installer_dir.join("diagnostics");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut latest: Option<(u64, String)> = None;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !entry.file_type()?.is_file()
            || !name.ends_with(".json")
            || uuid::Uuid::parse_str(name.trim_end_matches(".json")).is_err()
        {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        if bytes.len() > 65536 {
            return Err(Error::Uncertain("start diagnostic is too large".into()));
        }
        let diagnostic: StartDiagnostic = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Uncertain("start diagnostic is invalid".into()))?;
        if diagnostic.format_version != 1
            || diagnostic.action != "start"
            || diagnostic.argv != ["start"]
            || diagnostic.stderr_tail.len() > 2048
            || diagnostic.stderr_tail.chars().any(char::is_control)
        {
            return Err(Error::Uncertain("start diagnostic is invalid".into()));
        }
        if diagnostic.operation_id == operation_id
            && latest
                .as_ref()
                .is_none_or(|(recorded, _)| diagnostic.recorded_at_ms >= *recorded)
        {
            latest = Some((diagnostic.recorded_at_ms, format!("diagnostics/{name}")));
        }
    }
    Ok(latest.map(|(_, reference)| reference))
}

async fn start(cfg: &Config, slot: Slot) -> Result<()> {
    let mut record = read(cfg)?;
    let expected = FileStore::new(&cfg.k_state)
        .version(slot)?
        .ok_or_else(|| invalid("start slot missing"))?;
    record.last_start = Some(Start {
        slot,
        version: expected.clone(),
        succeeded: false,
        failure_diagnostic: None,
    });
    save(cfg, &record)?;
    let attempt = async {
        // A newly appeared process must be settled before replacing bytes. A
        // stopped installation does not authorize stopping a user-started one.
        if !installed_product_processes(cfg)?.is_empty() {
            return Err(Error::Uncertain(
                "product became active before publication".into(),
            ));
        }
        publish(cfg, slot)?;
        let evidence = if record.running {
            match start_product_attempt(cfg).await {
                Ok(evidence) => evidence,
                Err(failure) => {
                    if let Ok(reference) =
                        preserve_start_failure(cfg, &record, slot, &expected, &failure.failure)
                    {
                        record
                            .last_start
                            .as_mut()
                            .expect("start intent written above")
                            .failure_diagnostic = Some(reference);
                        // Observability failure must not turn an ordinary
                        // candidate failure into an unresolved upgrade.
                        let _ = save(cfg, &record);
                    }
                    return Err(failure.error);
                }
            }
        } else {
            computer::self_report(&cfg.binary, cfg).await?
        };
        if slot == Slot::Stable && evidence.version != expected {
            return Err(invalid("restored stable did not report its version"));
        }
        Ok(())
    }
    .await;
    match attempt {
        Ok(()) => {
            record
                .last_start
                .as_mut()
                .expect("start intent written above")
                .succeeded = true;
            save(cfg, &record)
        }
        Err(error) if slot == Slot::Experiment && !error.is_uncertain() => {
            // Let K's probe judge an ordinary failed candidate start and enter
            // rollback. Stable restoration failure must remain unresolved.
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn probe(cfg: &Config) -> Result<Evidence> {
    let record = read(cfg)?;
    if let Some(start) = record.last_start.as_ref().filter(|start| !start.succeeded) {
        let suffix = start
            .failure_diagnostic
            .as_ref()
            .map(|reference| format!("; private diagnostic {reference}"))
            .unwrap_or_default();
        return Err(invalid(format!(
            "candidate was not started successfully{suffix}"
        )));
    }
    if record.running {
        live_evidence(cfg).await
    } else {
        if !installed_product_processes(cfg)?.is_empty() {
            return Err(Error::Uncertain(
                "unexpected service on a stopped installation".into(),
            ));
        }
        computer::self_report(&cfg.binary, cfg).await
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    protocol_version: u32,
    action: String,
    slot: Option<Slot>,
    artifact_path: Option<PathBuf>,
}

async fn dispatch(cfg: &Config, request: Request) -> Result<Value> {
    if request.protocol_version != 1 {
        return Err(invalid("unsupported controller protocol"));
    }
    let mut scoped = cfg.clone();
    if request.action != "fence" {
        scoped.waiting_caller = read(cfg)?.waiting_caller;
    }
    let cfg = &scoped;
    let slot_action = matches!(request.action.as_str(), "start" | "stop");
    if slot_action {
        let slot = request
            .slot
            .ok_or_else(|| invalid("controller slot missing"))?;
        if request.artifact_path.as_ref() != Some(&FileStore::new(&cfg.k_state).artifact(slot)) {
            return Err(invalid("controller artifact does not match slot"));
        }
    } else if request.slot.is_some() || request.artifact_path.is_some() {
        return Err(invalid("unexpected controller slot"));
    }
    match request.action.as_str() {
        "fence" => computer::fence(cfg).await?,
        "quiesce" | "resume" => {
            read(cfg)?;
        }
        "stop" => {
            let mut record = read(cfg)?;
            if record.running {
                record.forced_stops.extend(stop_product(cfg).await?);
                save(cfg, &record)?;
            } else if !installed_product_processes(cfg)?.is_empty() {
                return Err(Error::Uncertain(
                    "unexpected service on a stopped installation".into(),
                ));
            }
        }
        "start" => {
            start(
                cfg,
                request
                    .slot
                    .ok_or_else(|| invalid("controller slot missing"))?,
            )
            .await?
        }
        "probe" => return Ok(json!({"protocolVersion":1,"ok":true,"evidence":probe(cfg).await?})),
        _ => return Err(invalid("unknown controller action")),
    }
    Ok(json!({"protocolVersion":1,"ok":true}))
}

pub async fn serve(cfg: &Config) -> Result<u8> {
    k_carrier::host::isolate_standard_handles()?;
    let result = async {
        let mut bytes = Vec::new();
        timeout(
            Duration::from_secs(30),
            tokio::io::stdin().take(16385).read_to_end(&mut bytes),
        )
        .await
        .map_err(|_| invalid("controller request timeout"))??;
        if bytes.len() > 16384 {
            return Err(invalid("controller request too large"));
        }
        dispatch(cfg, serde_json::from_slice(&bytes)?).await
    }
    .await;
    let (code, value) = match result {
        Ok(value) => (0, value),
        Err(error) => (
            1,
            json!({"protocolVersion":1,"ok":false,"uncertain":error.is_uncertain(),"error":error.to_string()}),
        ),
    };
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    let mut output = tokio::io::stdout();
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(code)
}

#[cfg(all(test, target_os = "linux"))]
mod legacy_layout_tests {
    use super::*;
    use std::process::{Child, Command};

    struct Killed(Child);
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn config(root: &std::path::Path) -> Config {
        let state_home = root.join("state");
        Config {
            user_home: root.to_path_buf(),
            k_state: state_home.join("computer/k"),
            installer_dir: state_home.join("computer/installer"),
            state_home,
            install_dir: root.join("bin"),
            binary: root.join("bin/raft-computer"),
            sidecar: root.join("bin/photon_rs_bg.wasm"),
            hands_origin: String::new(),
            hands_app: String::new(),
            waiting_caller: None,
        }
    }

    // A legacy two-layer install keeps a launcher at the binary path that execs
    // into K's stable slot, so the live product runs from the slot artifact.
    #[test]
    fn product_running_from_the_stable_slot_is_found_and_attested() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let slot_artifact = FileStore::new(&cfg.k_state).artifact(Slot::Stable);
        fs::create_dir_all(slot_artifact.parent().unwrap()).unwrap();
        fs::create_dir_all(&cfg.install_dir).unwrap();
        fs::copy("/bin/sleep", &slot_artifact).unwrap();
        fs::write(&cfg.binary, b"#!/bin/sh\nexit 0\n").unwrap();
        let child = Killed(Command::new(&slot_artifact).arg("30").spawn().unwrap());
        let pid = child.0.id();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Ok(identity) = attest_product(&cfg, pid) {
                break identity;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "slot process was not attested"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(identity.pid, pid);
        assert!(
            installed_product_processes(&cfg)
                .unwrap()
                .iter()
                .any(|p| p.pid == pid),
            "a live slot process must count as a running product before quarantine",
        );
    }

    #[test]
    fn unrelated_process_is_not_attested() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let child = Killed(Command::new("/bin/sleep").arg("30").spawn().unwrap());
        std::thread::sleep(Duration::from_millis(50));
        assert!(attest_product(&cfg, child.0.id()).is_err());
        assert!(
            !installed_product_processes(&cfg)
                .unwrap()
                .iter()
                .any(|p| p.pid == child.0.id())
        );
    }
}
