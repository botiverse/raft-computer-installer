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
    /// The systemd unit running the service when the operation began; the
    /// product is then stopped and started through `systemctl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_unit: Option<crate::systemd::ServiceUnit>,
    /// The supervisor Raft Desktop registered for this home when the
    /// operation began (host_supervisor.rs); it then stops and starts the
    /// product, ahead of any systemd unit found at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_supervisor: Option<crate::host_supervisor::HostSupervisor>,
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
pub fn product_executables(cfg: &Config) -> [PathBuf; 3] {
    let store = FileStore::new(&cfg.k_state);
    [
        cfg.binary.clone(),
        store.artifact(Slot::Stable),
        store.artifact(Slot::Experiment),
    ]
}

/// Several state homes (`RAFT_HOME`) can run one installed binary, but each
/// has its own K state and service. A process on the shared binary whose
/// environment names another home is that home's Computer, not this
/// installation's: stopping it for this upgrade would leave it down, and it
/// keeps running its old image after publication. Slot artifacts live under
/// this home's K state, and a process whose home cannot be read counts as
/// this one.
fn of_this_state_home(cfg: &Config, identity: &process::Identity) -> Result<bool> {
    if !process::same_path(&identity.executable, &cfg.binary) {
        return Ok(true);
    }
    let Some(environment) = process::environment(identity)? else {
        return Ok(true);
    };
    Ok(crate::config::state_home_of(&environment)
        .is_none_or(|home| process::same_path(&home, &cfg.state_home)))
}

pub fn installed_product_processes(cfg: &Config) -> Result<Vec<process::Identity>> {
    let caller = cfg.waiting_caller.as_ref();
    let mut processes = Vec::new();
    for executable in product_executables(cfg) {
        for identity in process::installed(&executable)? {
            if !caller.is_some_and(|c| process::same_instance(c, &identity))
                && !processes.contains(&identity)
                && of_this_state_home(cfg, &identity)?
            {
                processes.push(identity);
            }
        }
    }
    processes.sort_by_key(|p| p.pid);
    Ok(processes)
}

/// How far up the process tree to look for this installation's Computer.
const ANCESTOR_LIMIT: usize = 64;

/// This installation's running Computer, when it is an ancestor of `caller`
/// (the waiting CLI). That is a local command started from inside the
/// Computer, for example by one of its agents: stopping the service stops
/// everything the Computer started, this installer included, so the upgrade
/// would die halfway and leave the service down.
pub fn product_ancestor(
    cfg: &Config,
    caller: &process::Identity,
) -> Result<Option<process::Identity>> {
    let products = installed_product_processes(cfg)?;
    if products.is_empty() {
        return Ok(None);
    }
    let mut pid = caller.pid;
    let mut seen = vec![pid];
    for _ in 0..ANCESTOR_LIMIT {
        let Some(parent) = process::parent_of(pid)? else {
            return Ok(None);
        };
        if seen.contains(&parent) {
            return Ok(None);
        }
        seen.push(parent);
        if let Some(product) = products.iter().find(|product| product.pid == parent)
            && process::observe(parent)?
                .is_some_and(|identity| process::same_instance(&identity, product))
        {
            return Ok(Some(product.clone()));
        }
        pid = parent;
    }
    Ok(None)
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
    let service_unit = crate::systemd::product_unit(cfg).await?;
    let host_supervisor = registered_supervisor(cfg)?;
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
            service_unit,
            host_supervisor,
        },
    )
}

/// The registered supervisor, or None. A registration that is not understood
/// never reads as "none" (operation.rs holds it before K starts; finding one
/// here means it changed mid-operation).
pub fn registered_supervisor(
    cfg: &Config,
) -> Result<Option<crate::host_supervisor::HostSupervisor>> {
    match crate::host_supervisor::read(cfg) {
        crate::host_supervisor::Registration::Absent => Ok(None),
        crate::host_supervisor::Registration::Registered(supervisor) => Ok(Some(supervisor)),
        crate::host_supervisor::Registration::Unknown(detail) => Err(Error::Uncertain(format!(
            "the service registration is not understood: {detail}"
        ))),
    }
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

pub async fn stop_product(
    cfg: &Config,
    unit: Option<&crate::systemd::ServiceUnit>,
    supervisor: Option<&crate::host_supervisor::HostSupervisor>,
) -> Result<Vec<u32>> {
    // The product gets the first opportunity to unregister or stop its service;
    // a supervisor (Raft Desktop's registration, or a systemd unit) is stopped
    // through its manager, disabled first so it does not restart it.
    // The bounded fallback acts only on OS-identified instances of this binary.
    match (supervisor, unit) {
        (Some(supervisor), _) => crate::host_supervisor::stop(cfg, supervisor).await?,
        (None, Some(unit)) => crate::systemd::systemctl(unit, "stop").await?,
        (None, None) => {
            let _ = computer::lifecycle(cfg, "stop").await?;
        }
    }
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
    publish_launcher(cfg);
    Ok(version)
}

pub const LAUNCHER_NAME: &str = "raft-computer-launch.exe";

/// Keep `raft-computer-launch.exe` next to the published binary at this
/// installer's version. Best effort: the product is stopped (so the launcher
/// has exited), and a launcher that could not be replaced is an older
/// working one, never a reason to fail or roll back the Computer.
#[cfg(not(windows))]
fn publish_launcher(_: &Config) {}

#[cfg(windows)]
#[allow(clippy::const_is_empty)] // empty in builds that embedded no launcher
fn publish_launcher(cfg: &Config) {
    // Embedded at build time (build.rs).
    const LAUNCHER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/raft-computer-launch.bin"));
    if LAUNCHER.is_empty() {
        return;
    }
    let Some(directory) = cfg.binary.parent() else {
        return;
    };
    let destination = directory.join(LAUNCHER_NAME);
    if fs::read(&destination).is_ok_and(|existing| existing == LAUNCHER) {
        return;
    }
    let _ = write_durable(&destination, LAUNCHER, true);
}

async fn start_product_attempt(
    cfg: &Config,
    unit: Option<&crate::systemd::ServiceUnit>,
    supervisor: Option<&crate::host_supervisor::HostSupervisor>,
) -> std::result::Result<Evidence, ProductStartError> {
    if let Some(supervisor) = supervisor {
        crate::host_supervisor::start(cfg, supervisor)
            .await
            .map_err(|error| ProductStartError {
                failure: computer::LifecycleFailure {
                    kind: "command-error".into(),
                    argv: vec!["start".into()],
                    exit_code: None,
                    stderr_tail: error.to_string(),
                    stderr_truncated: false,
                },
                error,
            })?;
        return wait_ready(cfg, vec!["start".into()]).await;
    }
    if let Some(unit) = unit {
        crate::systemd::systemctl(unit, "start")
            .await
            .map_err(|error| ProductStartError {
                failure: computer::LifecycleFailure {
                    kind: "command-error".into(),
                    argv: vec!["start".into()],
                    exit_code: None,
                    stderr_tail: error.to_string(),
                    stderr_truncated: false,
                },
                error,
            })?;
        return wait_ready(cfg, vec!["start".into()]).await;
    }
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
    wait_ready(cfg, vec!["start".into()]).await
}

async fn wait_ready(
    cfg: &Config,
    argv: Vec<String>,
) -> std::result::Result<Evidence, ProductStartError> {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match live_evidence(cfg).await {
            Ok(evidence) => return Ok(evidence),
            Err(error) if error.is_uncertain() => {
                return Err(ProductStartError {
                    error,
                    failure: computer::LifecycleFailure {
                        kind: "readiness-uncertain".into(),
                        argv: argv.clone(),
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
                    argv,
                    exit_code: Some(0),
                    stderr_tail: String::new(),
                    stderr_truncated: false,
                },
            });
        }
        sleep(Duration::from_millis(200)).await;
    }
}

pub async fn start_product(
    cfg: &Config,
    unit: Option<&crate::systemd::ServiceUnit>,
    supervisor: Option<&crate::host_supervisor::HostSupervisor>,
) -> Result<Evidence> {
    start_product_attempt(cfg, unit, supervisor)
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
            match start_product_attempt(
                cfg,
                record.service_unit.as_ref(),
                record.host_supervisor.as_ref(),
            )
            .await
            {
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
                record.forced_stops.extend(
                    stop_product(
                        cfg,
                        record.service_unit.as_ref(),
                        record.host_supervisor.as_ref(),
                    )
                    .await?,
                );
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
        // A parallel test's fork can briefly inherit the writable handle from
        // the copy above, so exec may see ETXTBSY until that child execs too.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let child = Killed(loop {
            match Command::new(&slot_artifact).arg("30").spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("slot fixture did not start: {error}"),
            }
        });
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

    fn spawn_binary_copy(cfg: &Config, home: Option<&std::path::Path>) -> Killed {
        if !cfg.binary.exists() {
            fs::create_dir_all(&cfg.install_dir).unwrap();
            fs::copy("/bin/sleep", &cfg.binary).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        Killed(loop {
            let mut command = Command::new(&cfg.binary);
            command
                .arg("30")
                .env_remove("RAFT_HOME")
                .env_remove("SLOCK_HOME");
            command.env("HOME", &cfg.user_home);
            if let Some(home) = home {
                command.env("RAFT_HOME", home);
            }
            match command.spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("binary fixture did not start: {error}"),
            }
        })
    }

    fn found(cfg: &Config, pid: u32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if installed_product_processes(cfg)
                .unwrap()
                .iter()
                .any(|p| p.pid == pid)
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // Two Computers on one machine share ~/.local/bin/raft-computer with
    // different RAFT_HOMEs; upgrading one stopped the other (2026-10-19).
    #[test]
    fn a_computer_of_another_state_home_on_the_shared_binary_is_not_this_installation() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let other = spawn_binary_copy(&cfg, Some(&root.path().join("other-home")));
        let this = spawn_binary_copy(&cfg, Some(&cfg.state_home));
        assert!(
            found(&cfg, this.0.id()),
            "this home's Computer is this installation's product"
        );
        assert!(
            !installed_product_processes(&cfg)
                .unwrap()
                .iter()
                .any(|p| p.pid == other.0.id()),
            "another home's Computer must not be stopped or block this upgrade",
        );
    }

    #[test]
    fn a_computer_without_raft_home_resolves_the_default_home() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        // No RAFT_HOME: the process resolves ~/.raft under its HOME.
        cfg.state_home = root.path().join(".raft");
        let default = spawn_binary_copy(&cfg, None);
        assert!(found(&cfg, default.0.id()));
        cfg.state_home = root.path().join("state");
        assert!(!found(&cfg, default.0.id()));
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
