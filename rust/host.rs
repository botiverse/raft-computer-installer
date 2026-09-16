//! Product lifecycle controller, launched separately by K's CommandHost.
//! The durable mode belongs to one operation and is never inferred anew during
//! recovery. Running images live at the installed path, outside K's slots.
use crate::{Error, Result, artifact, computer, config::Config, process, version};
use k_carrier::{error::invalid, state::{Evidence, OperationRead, Slot}, storage::{FileStore, ensure_dir, sync_dir, write_durable, write_json}};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, time::{Instant, sleep, timeout}};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Start {
    pub slot: Slot,
    pub version: String,
    pub succeeded: bool,
}

pub struct Answer {
    pub running: bool,
    pub setup_known: bool,
    pub next_step: Option<String>,
    pub processes: Vec<process::Identity>,
}

pub async fn answer(cfg: &Config) -> Result<Answer> {
    let status = match computer::status(cfg).await {
        Ok(status) => Some(status),
        Err(error) if error.is_uncertain() => return Err(error),
        Err(_) => None,
    };
    let mut processes = process::installed(&cfg.binary)?;
    if let Some(evidence) = status.as_ref().and_then(|s| s.evidence.as_ref()) {
        let identity = process::attest(evidence.pid, &cfg.binary)?;
        if !processes.contains(&identity) { processes.push(identity); }
    }
    Ok(Answer {
        running: !processes.is_empty(), setup_known: status.is_some(),
        next_step: status.and_then(|s| s.next_step), processes,
    })
}

fn state_path(cfg: &Config) -> PathBuf { cfg.installer_dir.join("product-state.json") }

fn operation(cfg: &Config) -> Result<k_carrier::state::Operation> {
    match FileStore::new(&cfg.k_state).read_operation() {
        OperationRead::Observed { operation } => Ok(operation),
        _ => Err(Error::Uncertain("product state has no readable transaction identity".into())),
    }
}

pub async fn prepare(cfg: &Config, path: &std::path::Path, release: &k_carrier::artifact::Release) -> Result<()> {
    artifact::check_candidate(cfg, path, release).await?;
    let operation = operation(cfg)?;
    if operation.target_version != release.version || operation.outcome.is_some() {
        return Err(invalid("candidate preparation transaction mismatch"));
    }
    // Both sides of a rollback must have locally available, verified sidecars.
    artifact::saved_sidecar(cfg, &operation.from_version)?;
    let answer = answer(cfg).await?;
    save(cfg, &ProductState {
        format_version: 1, operation_id: operation.id, target_version: operation.target_version,
        from_version: operation.from_version, running: answer.running, setup_known: answer.setup_known,
        next_step: answer.next_step, initial_processes: answer.processes,
        forced_stops: vec![], last_start: None,
    })
}

pub fn read(cfg: &Config) -> Result<ProductState> {
    let operation = operation(cfg)?;
    let bytes = fs::read(state_path(cfg)).map_err(|_| Error::Uncertain("product state is missing or unreadable".into()))?;
    if bytes.len() > 65536 { return Err(Error::Uncertain("product state is too large".into())); }
    let record: ProductState = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Uncertain("product state is invalid".into()))?;
    if record.format_version != 1 || record.operation_id != operation.id
        || record.target_version != operation.target_version || record.from_version != operation.from_version
    { return Err(Error::Uncertain("product state belongs to a different transaction".into())); }
    Ok(record)
}

fn save(cfg: &Config, record: &ProductState) -> Result<()> { write_json(&state_path(cfg), record) }

pub async fn live_evidence(cfg: &Config) -> Result<Evidence> {
    let status = computer::status(cfg).await?;
    let evidence = status.evidence.ok_or_else(|| invalid("product returned no live attestation"))?;
    process::attest(evidence.pid, &cfg.binary)?;
    Ok(evidence)
}

pub async fn stop_product(cfg: &Config) -> Result<Vec<u32>> {
    // The product gets the first opportunity to unregister or stop its service.
    // The bounded fallback acts only on OS-identified instances of this binary.
    let _ = computer::lifecycle(cfg, "stop").await?;
    let processes = process::installed(&cfg.binary)?;
    let remaining = process::wait_gone(&processes, Duration::from_secs(3)).await?;
    let forced = process::terminate(&remaining).await?;
    if !process::installed(&cfg.binary)?.is_empty() {
        return Err(Error::Uncertain("product service restarted while stopping".into()));
    }
    Ok(forced)
}

/// Repair uses the same controller lifetime fence without asking a new
/// controller to accept an abandoned product-command record as completed.
pub async fn fence_controllers_for_repair(cfg: &Config) -> Result<()> {
    let directory = cfg.k_state.join("controllers");
    if !k_carrier::storage::exists(&directory)? { return Ok(()); }
    let deadline = Instant::now() + Duration::from_secs(110);
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let record: Value = serde_json::from_slice(&fs::read(entry.path())?)?;
        let pid = record.get("pid").and_then(Value::as_u64).and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 1).ok_or_else(|| Error::Uncertain("controller lifetime record is unreadable".into()))?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| name.starts_with(&format!("{pid}-"))) {
            return Err(Error::Uncertain("controller lifetime identity mismatch".into()));
        }
        while k_carrier::lock::process_alive(pid) {
            if Instant::now() >= deadline { return Err(Error::Uncertain("controller is still active during repair".into())); }
            sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

fn regular_destination(path: &std::path::Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => Err(invalid("installed destination is not a regular file")),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn publish(cfg: &Config, slot: Slot) -> Result<String> {
    let store = FileStore::new(&cfg.k_state);
    let version = store.version(slot)?.ok_or_else(|| invalid("slot is missing"))?;
    version::exact(&version)?;
    let binary = fs::read(store.artifact(slot))?;
    artifact::check_platform(&binary)?;
    let sidecar = artifact::saved_sidecar(cfg, &version)?.map(fs::read).transpose()?;
    regular_destination(&cfg.binary)?;
    regular_destination(&cfg.sidecar)?;
    let directory = cfg.binary.parent().ok_or_else(|| invalid("installed directory is missing"))?;
    ensure_dir(directory)?;
    // With service stopped and intent durable, interruption between these two
    // writes is recovered by republishing a complete slot and its sidecar.
    match sidecar {
        Some(bytes) => write_durable(&cfg.sidecar, &bytes, false)?,
        None => match fs::remove_file(&cfg.sidecar) {
            Ok(()) => sync_dir(directory)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        },
    }
    write_durable(&cfg.binary, &binary, true)?;
    Ok(version)
}

pub async fn start_product(cfg: &Config) -> Result<Evidence> {
    if !computer::lifecycle(cfg, "start").await? { return Err(invalid("product start failed")); }
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match live_evidence(cfg).await {
            Ok(evidence) => return Ok(evidence),
            Err(error) if error.is_uncertain() => return Err(error),
            Err(_) => {},
        }
        if Instant::now() >= deadline { return Err(invalid("product did not become ready")); }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn start(cfg: &Config, slot: Slot) -> Result<()> {
    let mut record = read(cfg)?;
    let expected = FileStore::new(&cfg.k_state).version(slot)?.ok_or_else(|| invalid("start slot missing"))?;
    record.last_start = Some(Start { slot, version: expected.clone(), succeeded: false });
    save(cfg, &record)?;
    let attempt = async {
        // A newly appeared process must be settled before replacing bytes. A
        // stopped installation does not authorize stopping a user-started one.
        if !process::installed(&cfg.binary)?.is_empty() {
            return Err(Error::Uncertain("product became active before publication".into()));
        }
        publish(cfg, slot)?;
        let evidence = if record.running { start_product(cfg).await? } else { computer::self_report(&cfg.binary, cfg).await? };
        if slot == Slot::Stable && evidence.version != expected { return Err(invalid("restored stable did not report its version")); }
        Ok(())
    }.await;
    match attempt {
        Ok(()) => {
            record.last_start.as_mut().expect("start intent written above").succeeded = true;
            save(cfg, &record)
        },
        Err(error) if slot == Slot::Experiment && !error.is_uncertain() => {
            // Let K's probe judge an ordinary failed candidate start and enter
            // rollback. Stable restoration failure must remain unresolved.
            Ok(())
        },
        Err(error) => Err(error),
    }
}

async fn probe(cfg: &Config) -> Result<Evidence> {
    let record = read(cfg)?;
    if record.last_start.as_ref().is_some_and(|start| !start.succeeded) {
        return Err(invalid("candidate was not started successfully"));
    }
    if record.running { live_evidence(cfg).await } else {
        if !process::installed(&cfg.binary)?.is_empty() { return Err(Error::Uncertain("unexpected service on a stopped installation".into())); }
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
    if request.protocol_version != 1 { return Err(invalid("unsupported controller protocol")); }
    let slot_action = matches!(request.action.as_str(), "start" | "stop");
    if slot_action {
        let slot = request.slot.ok_or_else(|| invalid("controller slot missing"))?;
        if request.artifact_path.as_ref() != Some(&FileStore::new(&cfg.k_state).artifact(slot)) {
            return Err(invalid("controller artifact does not match slot"));
        }
    } else if request.slot.is_some() || request.artifact_path.is_some() {
        return Err(invalid("unexpected controller slot"));
    }
    match request.action.as_str() {
        "fence" => computer::fence(cfg).await?,
        "quiesce" | "resume" => { read(cfg)?; },
        "stop" => {
            let mut record = read(cfg)?;
            if record.running {
                record.forced_stops.extend(stop_product(cfg).await?);
                save(cfg, &record)?;
            } else if !process::installed(&cfg.binary)?.is_empty() {
                return Err(Error::Uncertain("unexpected service on a stopped installation".into()));
            }
        },
        "start" => start(cfg, request.slot.ok_or_else(|| invalid("controller slot missing"))?).await?,
        "probe" => return Ok(json!({"protocolVersion":1,"ok":true,"evidence":probe(cfg).await?})),
        _ => return Err(invalid("unknown controller action")),
    }
    Ok(json!({"protocolVersion":1,"ok":true}))
}

pub async fn serve(cfg: &Config) -> Result<u8> {
    k_carrier::host::isolate_standard_handles()?;
    let result = async {
        let mut bytes = Vec::new();
        timeout(Duration::from_secs(30), tokio::io::stdin().take(16385).read_to_end(&mut bytes)).await
            .map_err(|_| invalid("controller request timeout"))??;
        if bytes.len() > 16384 { return Err(invalid("controller request too large")); }
        dispatch(cfg, serde_json::from_slice(&bytes)?).await
    }.await;
    let (code, value) = match result {
        Ok(value) => (0, value),
        Err(error) => (1, json!({"protocolVersion":1,"ok":false,"uncertain":error.is_uncertain(),"error":error.to_string()})),
    };
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    tokio::io::stdout().write_all(&bytes).await?;
    Ok(code)
}
