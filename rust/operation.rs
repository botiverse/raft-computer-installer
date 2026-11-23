//! One product writer spans preparation, K, publication and repair. The gate
//! lives outside the directory repair quarantines, and belongs to the worker,
//! so losing its CLI supervisor cannot release a still-running transaction.
use crate::{
    Error, Result, artifact, computer,
    config::Config,
    host,
    presence::{Interaction, Presence},
    report::{self, FailureCode, Outcome, Receipt},
    request::{Reply, Request},
    runner, shell_path,
    source::{self, FrozenSource, Manifest, Source},
    version,
    world::{self, World},
};
use async_trait::async_trait;
use k_carrier::{
    artifact::{Release, ReleaseContext, ReleaseSource, sha256, verify},
    error::invalid,
    lock::UpgradeLock,
    protocol,
    state::{Operation as KOperation, OperationPhase, OperationRead, Slot},
    storage::{FileStore, ensure_dir, exists, now_ms, sync_dir, write_durable, write_json},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

/// A request id from a finished operation is refused. The usual cause is a
/// shell that still carries a finished web upgrade's variables (an old
/// installer passed them to the service and every agent): say how to drop
/// them (#proj-raft-computer:#32).
pub const REQUEST_CONFLICT_LINE: &str = "This request ID already belongs to a different operation. If this shell still carries a finished upgrade's variables, run the command again without them: env -u RAFT_COMPUTER_OPERATION_ID -u RAFT_COMPUTER_APPROVED_BY -u RAFT_COMPUTER_NON_INTERACTIVE raft-computer upgrade";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    Fresh,
    Adopt,
    Upgrade,
    Repair,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Phase {
    Preparing,
    Ready,
    Stopping,
    Quarantining,
    Seeding,
    Publishing,
    Verifying,
    Setup,
    Starting,
    Upgrading,
    Finished,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Plan {
    format_version: u32,
    request: Request,
    manifest: Manifest,
    kind: Kind,
    phase: Phase,
    from_version: Option<String>,
    inherited_unresolved: bool,
    running: Option<bool>,
    setup_succeeded: bool,
    next_step: Option<String>,
    detail: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Active {
    format_version: u32,
    id: String,
}

struct OfflineSource;
#[async_trait]
impl ReleaseSource for OfflineSource {
    async fn check(&self, _: &ReleaseContext) -> Result<Option<Release>> {
        Ok(None)
    }
    async fn fetch(&self, _: &str, _: &ReleaseContext) -> Result<Release> {
        Err(invalid("recovery cannot fetch a release"))
    }
}

fn directory(cfg: &Config, id: &str) -> PathBuf {
    cfg.installer_dir
        .join("operations")
        .join(sha256(id.as_bytes()))
}
fn active_path(cfg: &Config) -> PathBuf {
    cfg.installer_dir.join("active.json")
}
fn plan_path(cfg: &Config, id: &str) -> PathBuf {
    directory(cfg, id).join("plan.json")
}
fn damage_path(cfg: &Config) -> PathBuf {
    cfg.installer_dir.join("metadata-damage.json")
}

fn mark_damage(cfg: &Config) -> Result<()> {
    if !exists(&damage_path(cfg))? {
        write_json(
            &damage_path(cfg),
            &serde_json::json!({"formatVersion":1,"requiresRepair":true}),
        )?;
    }
    Ok(())
}

fn clear_damage(cfg: &Config) -> Result<()> {
    match fs::remove_file(damage_path(cfg)) {
        Ok(()) => sync_dir(&cfg.installer_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Keep the old pointer and its referenced metadata before publishing a repair
/// plan. Only installer-owned metadata paths are considered, never product data.
/// The original files remain until the replacement plan is durable.
fn preserve_metadata(cfg: &Config, request: &Request) -> Result<PathBuf> {
    let destination = cfg.installer_dir.join("quarantine").join(format!(
        "metadata-{}-{}",
        sha256(request.id.as_bytes()),
        uuid::Uuid::new_v4()
    ));
    ensure_dir(&destination)?;
    let mut paths = vec![
        active_path(cfg),
        damage_path(cfg),
        plan_path(cfg, &request.id),
        cfg.receipt_path(&request.id),
    ];
    if let Ok(previous) = json::<Active>(&active_path(cfg)) {
        paths.extend([plan_path(cfg, &previous.id), cfg.receipt_path(&previous.id)]);
    }
    paths.sort();
    paths.dedup();
    let mut index = BTreeMap::new();
    for (i, path) in paths.iter().enumerate() {
        if !exists(path)? {
            continue;
        }
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 * 1024 {
            return Err(Error::Uncertain(
                "installation records cannot be preserved safely".into(),
            ));
        }
        let name = format!("{i}.json");
        write_durable(&destination.join(&name), &fs::read(path)?, false)?;
        index.insert(name, path.to_string_lossy().into_owned());
    }
    write_json(&destination.join("index.json"), &index)?;
    Ok(destination)
}

fn json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("operation record too large"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn save(cfg: &Config, plan: &Plan) -> Result<()> {
    write_json(&plan_path(cfg, &plan.request.id), plan)
}

fn read_plan(cfg: &Config, id: &str) -> Result<Plan> {
    let plan: Plan = json(&plan_path(cfg, id))?;
    plan.request.validate()?;
    if plan.format_version != 1
        || plan.request.id != id
        || plan.manifest.version != plan.manifest.release.version
        || plan
            .request
            .version
            .as_ref()
            .is_some_and(|v| v != &plan.manifest.version)
    {
        return Err(invalid("operation identity mismatch"));
    }
    version::exact(&plan.manifest.version)?;
    plan.manifest.release.validate()?;
    Ok(plan)
}

/// What this installation holds for one request id besides its receipt.
pub(crate) enum Record {
    /// Nothing: no operation was ever started for it here.
    None,
    /// Its folder, without a plan of its own (it settled an earlier operation
    /// first, `settlement-failed.json`): nothing of its own was changed.
    Started,
    /// Its plan: the identity a continuation of it finishes with.
    Planned {
        request: Box<Request>,
        target: String,
        from: Option<String>,
    },
    /// A folder or plan that cannot be read.
    Unreadable,
}

pub(crate) fn record(cfg: &Config, id: &str) -> Record {
    match exists(&directory(cfg, id)) {
        Ok(false) => return Record::None,
        Ok(true) => {}
        Err(_) => return Record::Unreadable,
    }
    match exists(&plan_path(cfg, id)) {
        Ok(false) => Record::Started,
        Ok(true) => match read_plan(cfg, id) {
            Ok(plan) => Record::Planned {
                request: Box::new(plan.request),
                target: plan.manifest.version,
                from: plan.from_version,
            },
            Err(_) => Record::Unreadable,
        },
        Err(_) => Record::Unreadable,
    }
}

fn active(cfg: &Config) -> Result<Option<Plan>> {
    if !exists(&active_path(cfg))? {
        return Ok(None);
    }
    let active: Active = json(&active_path(cfg))?;
    if active.format_version != 1 {
        return Err(invalid("unsupported operation record format"));
    }
    if let Some(receipt) = report::read(cfg, &active.id)?
        && receipt.outcome != Outcome::Unresolved
    {
        // The old plan is no longer recovery input once the result is durable.
        // Even if housekeeping cannot be written yet, never replay its effects.
        settle_terminal_metadata(cfg, &receipt);
        return Ok(None);
    }
    read_plan(cfg, &active.id).map(Some)
}

fn clear_active(cfg: &Config, id: &str) -> Result<()> {
    if !exists(&active_path(cfg))? {
        return Ok(());
    }
    let current: Active = json(&active_path(cfg))?;
    if current.id == id {
        fs::remove_file(active_path(cfg))?;
        sync_dir(&cfg.installer_dir)?;
    }
    Ok(())
}

/// Reply lines hold at most 2048 bytes and no control characters.
fn bounded_line(line: String) -> String {
    let mut line: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if line.len() > 2048 {
        let mut end = 2045;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str("...");
    }
    line
}

fn receipt(
    request: &Request,
    target: Option<String>,
    from: Option<String>,
    outcome: Outcome,
    line: String,
) -> Receipt {
    Receipt::new(request, target, from, outcome, line)
}

fn finish(
    cfg: &Config,
    plan: &mut Plan,
    outcome: Outcome,
    failure: Option<(FailureCode, String)>,
    line: impl Into<String>,
) -> Result<Reply> {
    // Installation and account/workspace setup are separate product actions.
    // Keep the persisted field for backwards-compatible plan decoding, but
    // never carry a product-provided login/setup hint into a current receipt.
    plan.next_step = None;
    let mut line = line.into();
    if outcome.exit_code() == 0
        && plan.detail.get("readback").map(String::as_str) == Some("service")
    {
        line.push_str(" It is running.");
    }
    let mut result = receipt(
        &plan.request,
        Some(plan.manifest.version.clone()),
        plan.from_version.clone(),
        outcome,
        line,
    );
    if let Some((code, reason)) = failure {
        result.code = Some(code);
        result.reason = Some(reason);
    }
    result.detail = plan.detail.clone();
    result.preserve_unresolved(plan.inherited_unresolved);
    let result = result.finish(cfg)?;
    if result.outcome != Outcome::Unresolved {
        settle_terminal_metadata(cfg, &result);
    }
    Ok(Reply::receipt(result))
}

// A durable terminal result is authoritative. Failure to retire housekeeping
// metadata must preserve the active pointer for the next invocation, not turn
// an already successful installation into a failed lifecycle operation.
fn settle_terminal_metadata(cfg: &Config, receipt: &report::Receipt) {
    let settle = || -> Result<()> {
        crate::cleanup::request(cfg, &receipt.id)?;
        if receipt.outcome == Outcome::Repaired {
            clear_damage(cfg)?;
        }
        clear_active(cfg, &receipt.id)
    };
    let _ = settle();
}

/// Whether K's recovery ran (it may have changed the installation).
async fn settle_k(cfg: &Config) -> Result<bool> {
    // A settled K receipt must not restart a service the user later stopped.
    if let OperationRead::Observed { operation } = FileStore::new(&cfg.k_state).read_operation()
        && operation.outcome.is_none()
    {
        runner::create(
            cfg.clone(),
            Arc::new(OfflineSource),
            false,
            None,
            report::PhaseLog::default(),
        )?
        .recover(None)
        .await?;
        return Ok(true);
    }
    Ok(false)
}

fn cached_artifact(cfg: &Config, plan: &Plan) -> Result<PathBuf> {
    let path = directory(cfg, &plan.request.id).join("artifact.bin");
    let bytes = fs::read(&path)?;
    verify(
        &bytes,
        plan.manifest.release.size,
        &plan.manifest.release.sha256,
    )?;
    artifact::check_platform(&bytes)?;
    artifact::saved_sidecar(cfg, &plan.manifest.version)?;
    Ok(path)
}

fn transition(cfg: &Config, plan: &mut Plan, phase: Phase) -> Result<()> {
    plan.phase = phase;
    save(cfg, plan)
}

/// The receipt's reason and the human line share product-language state; raw
/// causes stay in detail.diagnostic and never become display copy.
fn recovery_hint_copy(previous_version: Option<&str>, running: Option<bool>) -> (String, String) {
    match (previous_version, running) {
        (Some(previous), Some(true)) => {
            let reason = format!("Raft Computer {previous} is running.");
            (
                format!("{reason} You can try the upgrade again later."),
                reason,
            )
        }
        (Some(_), Some(false)) => (
            "Raft Computer is not running. Run `raft-computer start`.".into(),
            "Raft Computer is not running.".into(),
        ),
        _ => (
            "Raft Computer's current status couldn't be confirmed. Run the same command again to continue."
                .into(),
            "Raft Computer's current status couldn't be confirmed.".into(),
        ),
    }
}

/// The published program's `--version`: a new file, so a first run.
pub(crate) async fn published_report(cfg: &Config) -> Result<k_carrier::state::Evidence> {
    computer::first_run_report(&cfg.binary, cfg).await
}

/// The reason of a download that ended because no data arrived (its line
/// names the file and the URL).
const DOWNLOAD_STALLED_REASON: &str = "The Raft Computer download stopped receiving data. Check this computer's internet connection and try again.";

/// The line and reason of a candidate that failed its first check, before
/// anything was stopped or replaced.
fn candidate_check_copy(code: FailureCode, target: &str) -> (String, String) {
    if code == FailureCode::CandidateCheckTimedOut {
        (
            format!(
                "The new Raft Computer {target} did not answer its first check in time, often because the operating system is still scanning the newly downloaded file. Nothing was changed. Run the same install command again."
            ),
            "The new Raft Computer version did not answer its first check in time; this is often the operating system scanning a newly downloaded file. Try again.".into(),
        )
    } else {
        (
            format!(
                "The downloaded Raft Computer {target} failed its first check. Nothing was changed. Run the same install command again."
            ),
            "The downloaded Raft Computer version failed its first check.".into(),
        )
    }
}

fn rollback_copy(
    target: &str,
    from_version: &str,
    was_running: Option<bool>,
    running: bool,
) -> (String, String) {
    if running {
        let reason = format!(
            "Raft Computer {target} didn't start correctly, so {from_version} is still running."
        );
        return (
            format!(
                "{reason} Nothing else was changed. Run the same install command again to retry."
            ),
            reason,
        );
    }
    if was_running == Some(false) {
        return (
            format!(
                "Raft Computer {target} didn't start correctly, so {from_version} is still installed. It was not running before the upgrade and is still not running."
            ),
            format!(
                "Raft Computer {target} didn't start correctly, so {from_version} is still installed and not running."
            ),
        );
    }
    // Read once, when the rollback finished: a service manager (launchd
    // KeepAlive, systemd Restart=) or a slow start can still bring the old
    // version back afterwards, so say when, not that it stays down.
    let reason = format!(
        "Raft Computer {target} didn't start correctly, so {from_version} is still installed. It was not running when the upgrade finished."
    );
    (
        format!("{reason} If it is not back within a few minutes, run `raft-computer start`."),
        reason,
    )
}

/// Refuse before stopping anything when the systemd unit that runs the
/// service would also stop this installer (it could not leave; cli.rs).
/// The unit recorded when a repair began (see `serviceUnit`).
fn service_unit(plan: &Plan) -> Option<crate::systemd::ServiceUnit> {
    plan.detail
        .get("serviceUnit")
        .and_then(|unit| serde_json::from_str(unit).ok())
}

/// The refusal's line and reason, or None to go on. Going on from inside the
/// unit is safe only where the unit's actual `KillMode=` (read from systemd,
/// never assumed) leaves every process but the main one running; that is
/// recorded in the receipt (`systemdKillMode`). Why moving out failed
/// (systemd-run's or the copy's stderr, from cli.rs) is a raw cause: it goes
/// to `detail.diagnostic`, never into the reason.
async fn systemd_confinement(cfg: &Config, plan: &mut Plan) -> Result<Option<String>> {
    let Some(unit) = crate::systemd::product_unit(cfg)
        .await?
        .filter(crate::systemd::self_inside)
    else {
        return Ok(None);
    };
    let note = plan.request.escape_failure.clone();
    if let Some(mode) = crate::systemd::kill_mode(&unit).await
        && crate::systemd::kill_mode_spares_others(&mode)
    {
        plan.detail.insert(
            "systemdKillMode".into(),
            format!(
                "{} has KillMode={mode}: stopping it does not stop this upgrade, which continued inside it.",
                unit.name
            ),
        );
        if let Some(note) = note {
            plan.detail.insert("systemdEscape".into(), note);
        }
        return Ok(None);
    }
    Ok(Some(confined(&unit, note, &mut plan.detail)))
}

/// The held reason for a run that stays inside the unit: product language
/// only, with the escape note kept as the raw cause.
fn confined(
    unit: &crate::systemd::ServiceUnit,
    note: Option<String>,
    detail: &mut BTreeMap<String, String>,
) -> String {
    if let Some(note) = note {
        detail.insert("diagnostic".into(), note);
    }
    crate::systemd::confinement_reason(unit)
}

/// The supervisor Raft Desktop registered when a repair began (see
/// `hostSupervisor`), used for its stop and start.
fn host_supervisor(plan: &Plan) -> Option<crate::host_supervisor::HostSupervisor> {
    plan.detail
        .get("hostSupervisor")
        .and_then(|supervisor| serde_json::from_str(supervisor).ok())
}

/// Refuse before stopping anything when Raft Desktop's service registration
/// forbids this operation: not understood, another binary path, a target
/// below its floor, or (Windows) this installer could not leave the job the
/// service runs in.
async fn supervisor_refusal(cfg: &Config, plan: &Plan) -> Result<Option<(FailureCode, String)>> {
    // The registration with its definition confirmed in the OS manager, before
    // anything is stopped (a missing definition is a stale registration).
    let registration = crate::host_supervisor::resolve(cfg).await;
    if let Some(refusal) =
        crate::host_supervisor::refusal_with_code(cfg, &registration, &plan.manifest.version)?
    {
        return Ok(Some(refusal));
    }
    Ok(crate::host_supervisor::confinement(&registration)
        .map(|reason| (FailureCode::InstallationHeld, reason)))
}

fn refuse_supervised(
    cfg: &Config,
    plan: &mut Plan,
    (code, reason): (FailureCode, String),
) -> Result<Reply> {
    finish(
        cfg,
        plan,
        Outcome::Held,
        Some((code, reason.clone())),
        &reason,
    )
}

/// After a failed operation: re-enable a supervisor this or an earlier run
/// disabled, best effort, when no product process runs. The durable marker
/// stays when this fails, so a later run still restores it.
async fn restore_after_failure(cfg: &Config, plan: &Plan) -> Option<String> {
    let running = !crate::host::installed_product_processes(cfg)
        .ok()?
        .is_empty();
    restore_after_failure_with(cfg, host_supervisor(plan), running).await
}

async fn restore_after_failure_with(
    cfg: &Config,
    supervisor: Option<crate::host_supervisor::HostSupervisor>,
    running: bool,
) -> Option<String> {
    if running || !crate::host_supervisor::disabled_by_earlier_run(cfg) {
        return None;
    }
    crate::host_supervisor::restore_after_stop(cfg, &supervisor?, true).await
}

/// A new plan inherits an earlier run's "disabled by a run" marker.
fn carry_over_disabled_marker(cfg: &Config, detail: &mut BTreeMap<String, String>) {
    if crate::host_supervisor::disabled_by_earlier_run(cfg) {
        detail.insert("supervisorDisabledByRun".into(), "true".into());
    }
}

fn refuse_confined(cfg: &Config, plan: &mut Plan, reason: String) -> Result<Reply> {
    finish(
        cfg,
        plan,
        Outcome::Held,
        Some((FailureCode::InstallationHeld, reason.clone())),
        &reason,
    )
}

async fn upgrade(cfg: &Config, plan: &mut Plan, recovery: bool) -> Result<Reply> {
    let preparing = Instant::now();
    let prepares = matches!(plan.phase, Phase::Preparing | Phase::Ready);
    if plan.phase == Phase::Preparing {
        if recovery {
            return finish(
                cfg,
                plan,
                Outcome::Failed,
                Some((
                    FailureCode::InterruptedBeforeStart,
                    "Installation was interrupted before the upgrade started.".into(),
                )),
                "Installation was interrupted before the upgrade started. Run the same install command again.",
            );
        }
        if already_installed(cfg, plan).await? {
            // Refused as after a download: the target does not suit Raft
            // Desktop's registration. Refusals that protect a stop do not
            // apply, since nothing is stopped.
            let registration = crate::host_supervisor::resolve(cfg).await;
            if let Some(refusal) =
                crate::host_supervisor::target_refusal(cfg, &registration, &plan.manifest.version)?
            {
                return refuse_supervised(cfg, plan, refusal);
            }
            return finish_up_to_date(cfg, plan).await;
        }
        artifact::acquire_sidecar(cfg, &plan.manifest, artifact::download_deadline()).await?;
        transition(cfg, plan, Phase::Ready)?;
    }
    if plan.phase == Phase::Ready {
        if let Some(reason) = systemd_confinement(cfg, plan).await? {
            return refuse_confined(cfg, plan, reason);
        }
        if let Some(refusal) = supervisor_refusal(cfg, plan).await? {
            return refuse_supervised(cfg, plan, refusal);
        }
        let _lock = UpgradeLock::acquire(&cfg.k_state)?;
        let from = plan
            .from_version
            .as_ref()
            .ok_or_else(|| invalid("adoption has no source version"))?;
        artifact::adopt_installed_sidecar(cfg, from)?;
        if plan.kind == Kind::Adopt {
            FileStore::new(&cfg.k_state).bootstrap_locked(from, &cfg.binary)?;
        }
        if prepares {
            report::add_phase(&mut plan.detail, "prepare", preparing.elapsed());
        }
        transition(cfg, plan, Phase::Upgrading)?;
    }
    let store = FileStore::new(&cfg.k_state);
    let phases = report::PhaseLog::default();
    let runner = runner::create(
        cfg.clone(),
        Arc::new(FrozenSource(plan.manifest.clone())),
        plan.request.allow_downgrade,
        Some(&plan.manifest.release),
        phases.clone(),
    )?;
    let response = execute_upgrade(cfg, plan, &store, &runner, recovery).await;
    phases.drain_into(&mut plan.detail);
    let response = match response? {
        Ok(response) => response,
        Err(reply) => return Ok(reply),
    };
    settle_upgrade(cfg, plan, response).await
}

/// Whether an upgrade has nothing to do, decided before any download: K
/// compares versions only after fetching the candidate. Only a plain upgrade
/// of a complete installation qualifies: the installed version is the
/// selected one, its sidecar matches its saved identity and the release's,
/// and K is at rest with that version as stable, consistent with its last
/// operation. Anything else (an adoption, a downgrade, a missing source
/// version, a busy or inconsistent K) takes the usual path, which downloads
/// first and reports what is wrong.
async fn already_installed(cfg: &Config, plan: &Plan) -> Result<bool> {
    let Some(from) = plan.from_version.as_deref() else {
        return Ok(false);
    };
    if plan.kind != Kind::Upgrade || from != plan.manifest.version {
        return Ok(false);
    }
    // Sidecar files are adopted under the K lock (`adopt_installed_sidecar`).
    let Ok(_lock) = UpgradeLock::acquire(&cfg.k_state) else {
        return Ok(false);
    };
    // A missing record is accepted by the check for legacy installations;
    // here it means the installation is not known to be complete.
    let Ok(saved) = artifact::saved_sidecar_identity(cfg, from) else {
        return Ok(false);
    };
    let release = plan.manifest.sidecar.as_ref();
    let same_sidecar = match (&saved, release) {
        (None, None) => true,
        (Some(saved), Some(release)) => {
            saved.sha256 == release.sha256 && saved.size == release.size
        }
        _ => false,
    };
    if !same_sidecar
        || artifact::saved_sidecar(cfg, from).is_err()
        || artifact::check_installed_sidecar(cfg, from).is_err()
    {
        return Ok(false);
    }
    let store = FileStore::new(&cfg.k_state);
    // K's own rule for a settled operation (its recovery fails one that
    // disagrees as SETTLED_OPERATION_STATE_MISMATCH).
    let expected = match store.read_operation() {
        OperationRead::Genesis => from.to_owned(),
        OperationRead::Observed { operation } => match operation.outcome {
            Some(k_carrier::state::Outcome::Promoted) => operation.target_version,
            Some(_) => operation.from_version,
            None => return Ok(false),
        },
        OperationRead::Unreadable { .. } => return Ok(false),
    };
    // The phase is the last journal entry's.
    let Ok(state) = store.transaction_state().await else {
        return Ok(false);
    };
    Ok(state.phase().at_rest() && state.stable() == from && state.stable() == expected)
}

/// The up-to-date receipt of `already_installed`, with the running
/// service's version read back as `settle_upgrade` does.
async fn finish_up_to_date(cfg: &Config, plan: &mut Plan) -> Result<Reply> {
    let live = if host::answer(cfg).await?.running {
        Some(host::live_evidence(cfg).await?.version)
    } else {
        None
    };
    finish_up_to_date_with(cfg, plan, live)
}

/// `live` is the running service's attested version, None when stopped.
fn finish_up_to_date_with(cfg: &Config, plan: &mut Plan, live: Option<String>) -> Result<Reply> {
    if live
        .as_ref()
        .is_some_and(|version| *version != plan.manifest.version)
    {
        return Err(Error::Uncertain(
            "running version does not match installed release".into(),
        ));
    }
    plan.detail.insert(
        "readback".into(),
        if live.is_some() {
            "service"
        } else {
            "candidate"
        }
        .into(),
    );
    let line = format!("{} is already installed.", plan.manifest.version);
    finish(cfg, plan, Outcome::UpToDate, None, line)
}

/// K's part of an upgrade: the transaction, or the recovery of one this
/// request left open. `Err(reply)` is a receipt this request already settled.
async fn execute_upgrade(
    cfg: &Config,
    plan: &mut Plan,
    store: &FileStore,
    runner: &k_carrier::runner::Runner,
    recovery: bool,
) -> Result<std::result::Result<protocol::Response, Reply>> {
    let response = if recovery {
        match store.read_operation() {
            OperationRead::Observed { operation }
                if operation.id == plan.request.id
                    && operation.target_version == plan.manifest.version =>
            {
                // Left open before the handover by an earlier run: settled as
                // failed, as below. K's recovery would close it as rolled
                // back, as if a candidate had run; it runs when the record
                // cannot be settled.
                let cause = recorded_cause(&plan.detail);
                let settling = settle_before_handover(&cfg.k_state, &operation, cause);
                if let Ok(Some(operation)) = settling.await {
                    protocol::Response {
                        protocol_version: 1,
                        action: "recover".into(),
                        exit_code: 1,
                        result: "failed".into(),
                        operation: OperationRead::Observed { operation },
                        error: None,
                    }
                } else {
                    runner
                        .execute(&protocol::Request::Recover {
                            protocol_version: 1,
                            expected: Some(protocol::Expected {
                                id: plan.request.id.clone(),
                                target_version: plan.manifest.version.clone(),
                            }),
                        })
                        .await?
                }
            }
            _ => {
                return finish(
                    cfg,
                    plan,
                    Outcome::Failed,
                    Some((
                        FailureCode::InterruptedBeforeStart,
                        "Installation was interrupted before the upgrade transaction started."
                            .into(),
                    )),
                    "Installation was interrupted before the upgrade transaction started. Run the same install command again.",
                )
                .map(Err);
            }
        }
    } else {
        runner
            .execute(&protocol::Request::Upgrade {
                protocol_version: 1,
                id: plan.request.id.clone(),
                target_version: plan.manifest.version.clone(),
                consented: true,
            })
            .await?
    };
    Ok(Ok(response))
}

async fn settle_upgrade(
    cfg: &Config,
    plan: &mut Plan,
    response: protocol::Response,
) -> Result<Reply> {
    if response.exit_code == 2 && response.result == "busy" {
        return finish(
            cfg,
            plan,
            Outcome::Held,
            Some((
                FailureCode::OperationBusy,
                "Another upgrade is running on this machine.".into(),
            )),
            "Another upgrade is running on this machine.",
        );
    }
    if let Some(error) = &response.error {
        // Preserve the initiating failure across recovery's later outcome.
        plan.detail
            .entry("diagnostic".into())
            .or_insert_with(|| error.clone());
        save(cfg, plan)?;
    }
    let OperationRead::Observed { operation } = response.operation else {
        return Err(Error::Uncertain(
            "upgrade returned no readable operation receipt".into(),
        ));
    };
    if operation.id != plan.request.id || operation.target_version != plan.manifest.version {
        return Err(Error::Uncertain(
            "upgrade returned a different operation receipt".into(),
        ));
    }
    // Left open before the handover: settled as failed, nothing changed.
    let settled = settle_before_handover(&cfg.k_state, &operation, recorded_cause(&plan.detail))
        .await
        .ok()
        .flatten();
    let operation = settled.unwrap_or(operation);
    let Some(outcome) = operation.outcome else {
        return Err(Error::Uncertain(STILL_REQUIRES_RECOVERY.into()));
    };
    let outcome = match outcome {
        k_carrier::state::Outcome::Promoted => Outcome::Promoted,
        k_carrier::state::Outcome::RolledBack => Outcome::RolledBack,
        k_carrier::state::Outcome::UpToDate => Outcome::UpToDate,
        k_carrier::state::Outcome::Held => Outcome::Held,
        k_carrier::state::Outcome::Failed => Outcome::Failed,
    };
    if let Some(reason) = operation.reason {
        plan.detail.insert("diagnostic".into(), reason);
    }
    if outcome == Outcome::RolledBack
        && let Ok(Some(reference)) = host::latest_start_failure_diagnostic(cfg, &plan.request.id)
    {
        // The receipt exposes only the opaque relative reference. The bounded
        // stderr tail and exit code remain in the local mode-0600 file.
        plan.detail
            .insert("startFailureDiagnostic".into(), reference);
    }
    let mut running = None;
    let mut was_running = None;
    if let Ok(state) = host::read(cfg) {
        running = Some(state.running);
        was_running = Some(!state.initial_processes.is_empty());
        if outcome == Outcome::UpToDate
            && state.running
            && host::live_evidence(cfg).await?.version != plan.manifest.version
        {
            return Err(Error::Uncertain(
                "running version does not match installed release".into(),
            ));
        }
        plan.detail.insert(
            "readback".into(),
            if state.running {
                "service"
            } else {
                "candidate"
            }
            .into(),
        );
        if outcome == Outcome::Promoted && state.running {
            let mut dead = Vec::new();
            for identity in &state.initial_processes {
                if crate::process::matches(identity)? {
                    return Err(Error::Uncertain(
                        "previous product process is still running".into(),
                    ));
                }
                dead.push(format!("pid:{}:created:{}", identity.pid, identity.created));
            }
            plan.detail.insert(
                "deadProcessIdentities".into(),
                serde_json::to_string(&dead)?,
            );
        }
    }
    let target = &plan.manifest.version;
    let (line, failure) = match outcome {
        Outcome::Promoted
            if plan.detail.get("readback").map(String::as_str) == Some("candidate") =>
        {
            (
                format!(
                    "Upgraded {} to {target}. The service remains stopped. Run raft-computer start to start it.",
                    operation.from_version
                ),
                None,
            )
        }
        Outcome::Promoted => (
            format!("Upgraded {} to {target}.", operation.from_version),
            None,
        ),
        Outcome::RolledBack => {
            let (line, reason) = rollback_copy(
                target,
                &operation.from_version,
                was_running,
                running == Some(true),
            );
            (line, Some((FailureCode::CandidateStartFailed, reason)))
        }
        Outcome::UpToDate => (format!("{target} is already installed."), None),
        Outcome::Held => (
            "Upgrade was not allowed. Check the selected version and --allow-downgrade.".into(),
            Some((
                FailureCode::InstallationHeld,
                "The requested upgrade was not allowed.".into(),
            )),
        ),
        // K fails an operation only before the handover (the candidate
        // could not be fetched, verified or prepared): nothing was changed.
        _ => {
            let (line, failure) =
                failed_before_handover(target, &plan.manifest.release, &plan.detail);
            (line, Some(failure))
        }
    };
    finish(cfg, plan, outcome, failure, line)
}

async fn install(cfg: &Config, plan: &mut Plan, recovery: bool) -> Result<Reply> {
    if plan.phase == Phase::Preparing {
        if recovery {
            return finish(
                cfg,
                plan,
                Outcome::Failed,
                Some((
                    FailureCode::InterruptedBeforeStart,
                    "Installation was interrupted before changing installed files.".into(),
                )),
                "Installation was interrupted before changing installed files. Run the same install command again.",
            );
        }
        let started = Instant::now();
        let phases = report::PhaseLog::default();
        let candidate = if plan.kind == Kind::Repair {
            artifact::acquire_repair(cfg, &plan.manifest, &phases).await
        } else {
            artifact::acquire(cfg, &plan.manifest, &phases).await
        };
        // Download, verification and staging; the check is its own phase.
        let prepare = started
            .elapsed()
            .saturating_sub(phases.total("candidate_check"));
        phases.add("prepare", prepare);
        phases.drain_into(&mut plan.detail);
        let candidate = candidate?;
        if let Some(path) = &candidate.sidecar_quarantine {
            plan.detail.insert(
                "sidecarQuarantine".into(),
                path.to_string_lossy().into_owned(),
            );
        }
        write_durable(
            &directory(cfg, &plan.request.id).join("artifact.bin"),
            &fs::read(candidate.path)?,
            true,
        )?;
        transition(cfg, plan, Phase::Ready)?;
    }
    // All later phases can finish using local, verified bytes only.
    let candidate = cached_artifact(cfg, plan)?;
    if plan.phase == Phase::Ready {
        let answer = host::answer(cfg).await?;
        let prior = if plan.inherited_unresolved {
            host::read(cfg).ok()
        } else {
            None
        };
        plan.running = plan
            .running
            .or(Some(prior.map_or(answer.running, |state| state.running)));
        if let Some(refusal) = supervisor_refusal(cfg, plan).await? {
            return refuse_supervised(cfg, plan, refusal);
        }
        if let Some(supervisor) = host::registered_supervisor(cfg).await? {
            plan.detail
                .insert("hostSupervisor".into(), serde_json::to_string(&supervisor)?);
        }
        if plan.kind == Kind::Repair {
            if let Some(reason) = systemd_confinement(cfg, plan).await? {
                return refuse_confined(cfg, plan, reason);
            }
            if let Some(unit) = crate::systemd::product_unit(cfg).await? {
                plan.detail
                    .insert("serviceUnit".into(), serde_json::to_string(&unit)?);
            }
        }
        transition(
            cfg,
            plan,
            if plan.kind == Kind::Repair {
                Phase::Stopping
            } else {
                Phase::Seeding
            },
        )?;
    }
    if plan.phase == Phase::Stopping {
        let started = Instant::now();
        // The K writer lock is held while the old controller and product effects
        // are fenced and the product is stopped. It is reacquired by quarantine.
        let _lock = UpgradeLock::acquire(&cfg.k_state)?;
        host::fence_controllers_for_repair(cfg).await?;
        let mut forced = Vec::new();
        if computer::wait_for_repair(cfg).await? {
            // A crashed helper might have left its native CLI child behind.
            // Explicit repair stops these verified product instances before
            // issuing a new graceful service-manager stop command.
            forced.extend(
                crate::process::terminate(&crate::host::installed_product_processes(cfg)?).await?,
            );
        }
        // Record what this run's stop is about to undo: a launchd agent or a
        // logon task is disabled by it. Only an enabled one is re-enabled at
        // the end (restore the previous state); a supervisor the user
        // stopped (`raft-computer stop` disables it) stays disabled.
        if let Some(supervisor) = host_supervisor(plan)
            && !plan.detail.contains_key("supervisorDisabledByRun")
        {
            let disables = crate::host_supervisor::note_stop(cfg, &supervisor).await?;
            plan.detail
                .insert("supervisorDisabledByRun".into(), disables.to_string());
            save(cfg, plan)?;
        }
        forced.extend(
            host::stop_product(
                cfg,
                service_unit(plan).as_ref(),
                host_supervisor(plan).as_ref(),
            )
            .await?,
        );
        if let Some(path) = computer::preserve_commands_after_stop(cfg, &plan.request.id).await? {
            plan.detail.insert(
                "commandQuarantine".into(),
                path.to_string_lossy().into_owned(),
            );
        }
        plan.detail
            .insert("forcedStops".into(), serde_json::to_string(&forced)?);
        report::add_phase(&mut plan.detail, "stop", started.elapsed());
        transition(cfg, plan, Phase::Quarantining)?;
    }
    // Quarantine, seeding and publication replace the installed program.
    let swapping = Instant::now();
    let swaps = matches!(
        plan.phase,
        Phase::Quarantining | Phase::Seeding | Phase::Publishing
    );
    if plan.phase == Phase::Quarantining {
        let destination = cfg
            .installer_dir
            .join("quarantine")
            .join(sha256(plan.request.id.as_bytes()));
        let proof = || -> Result<()> {
            if !crate::host::installed_product_processes(cfg)?.is_empty() {
                return Err(Error::Uncertain(
                    "product is still active before quarantine".into(),
                ));
            }
            Ok(())
        };
        proof()?;
        let kept = k_carrier::quarantine::quarantine_state(
            &cfg.k_state,
            &destination,
            now_ms(),
            true,
            Some(&proof),
        )?;
        plan.detail.insert(
            "quarantine".into(),
            kept.receipt.quarantine_path.to_string_lossy().into_owned(),
        );
        transition(cfg, plan, Phase::Seeding)?;
    }
    if plan.phase == Phase::Seeding {
        let _lock = UpgradeLock::acquire(&cfg.k_state)?;
        let store = FileStore::new(&cfg.k_state);
        store.bootstrap_locked(&plan.manifest.version, &candidate)?;
        if store.version(Slot::Stable)?.as_deref() != Some(plan.manifest.version.as_str())
            || sha256(&fs::read(store.artifact(Slot::Stable))?) != plan.manifest.release.sha256
        {
            return Err(Error::Uncertain(
                "bootstrap stable does not match the selected release".into(),
            ));
        }
        transition(cfg, plan, Phase::Publishing)?;
    }
    if plan.phase == Phase::Publishing {
        let _lock = UpgradeLock::acquire(&cfg.k_state)?;
        if !crate::host::installed_product_processes(cfg)?.is_empty() {
            return Err(Error::Uncertain(
                "product became active before publication".into(),
            ));
        }
        host::publish(cfg, Slot::Stable)?;
        if swaps {
            report::add_phase(&mut plan.detail, "swap", swapping.elapsed());
        }
        transition(cfg, plan, Phase::Verifying)?;
    }
    if plan.phase == Phase::Verifying {
        let started = Instant::now();
        // The published program is a new file too: its first run gets the
        // same allowance as the candidate's.
        let report = published_report(cfg).await;
        report::add_phase(&mut plan.detail, "verify", started.elapsed());
        if report?.version != plan.manifest.version {
            return Err(Error::Uncertain(
                "published program did not report the selected version".into(),
            ));
        }
        // A receipt line is one line; the Codex-style steps printed above it
        // travel separately (see cli.rs).
        let steps = shell_path::ensure(cfg).await?;
        plan.detail
            .insert("pathSteps".into(), serde_json::to_string(&steps)?);
        // Installation and account/workspace setup are separate product
        // actions. The surface that knows the target server and workspace
        // prints the exact setup command; the installer neither guesses it nor
        // starts an interactive login of its own.
        transition(cfg, plan, Phase::Starting)?;
    }
    if plan.phase == Phase::Setup {
        plan.setup_succeeded = computer::status(cfg)
            .await
            .is_ok_and(|s| s.next_step.is_none());
        transition(cfg, plan, Phase::Starting)?;
    }
    if plan.phase == Phase::Starting {
        let started = Instant::now();
        if plan.running == Some(true) || plan.setup_succeeded {
            let evidence = match host::live_evidence(cfg).await {
                Ok(evidence) => evidence,
                Err(_) => {
                    host::start_product(
                        cfg,
                        service_unit(plan).as_ref(),
                        host_supervisor(plan).as_ref(),
                    )
                    .await?
                }
            };
            if evidence.version != plan.manifest.version {
                return Err(Error::Uncertain(
                    "installed service reports a different version".into(),
                ));
            }
            plan.detail.insert("readback".into(), "service".into());
        } else {
            if !crate::host::installed_product_processes(cfg)?.is_empty() {
                return Err(Error::Uncertain(
                    "a stopped installation unexpectedly started".into(),
                ));
            }
            plan.detail.insert("readback".into(), "candidate".into());
            // A repair's stop disables a launchd agent or a logon task (a
            // systemd unit is only stopped, never disabled). The Computer was
            // not running and is not started: restore only what this run
            // changed, so an enabled supervisor is enabled again and one the
            // user stopped stays stopped. A failure here never fails the
            // installed bytes: it is a warning in the outcome.
            if let Some(supervisor) = host_supervisor(plan) {
                let disabled_by_run = plan
                    .detail
                    .get("supervisorDisabledByRun")
                    .map(String::as_str)
                    == Some("true");
                if let Some(warning) =
                    crate::host_supervisor::restore_after_stop(cfg, &supervisor, disabled_by_run)
                        .await
                {
                    plan.detail.insert("warning".into(), warning);
                }
            }
        }
        report::add_phase(&mut plan.detail, "start", started.elapsed());
        let outcome = if plan.kind == Kind::Repair {
            Outcome::Repaired
        } else {
            Outcome::Installed
        };
        let line = format!(
            "Raft Computer {} {} successfully.",
            plan.manifest.version,
            if plan.kind == Kind::Repair {
                "reinstalled"
            } else {
                "installed"
            }
        );
        return finish(cfg, plan, outcome, None, line);
    }
    Err(invalid("unexpected operation phase"))
}

/// Raised when K returns the operation still open. It names no cause of its
/// own: K's error, saved as the diagnostic just before, does.
const STILL_REQUIRES_RECOVERY: &str = "upgrade still requires recovery";

/// K's operation is still open in a phase before the handover: checking,
/// downloading, verifying or staging the candidate. K has not stopped the
/// service or replaced an installed file there. A failed candidate download
/// returns from K this way (#proj-raft-computer:#49): K records no outcome
/// for it, and the upgrade was reported unresolved although nothing changed.
fn open_before_handover(operation: &KOperation) -> bool {
    operation.outcome.is_none()
        && matches!(
            operation.phase,
            OperationPhase::Checking
                | OperationPhase::Downloading
                | OperationPhase::Verifying
                | OperationPhase::Staging
        )
}

/// The cause recorded for this upgrade (K's error), without the prefix an
/// earlier unresolved run added to it.
fn recorded_cause(detail: &BTreeMap<String, String>) -> Option<&str> {
    let cause = detail.get("diagnostic")?.as_str();
    let cause = cause
        .strip_prefix(STILL_REQUIRES_RECOVERY)
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(cause);
    (!cause.trim().is_empty()).then_some(cause)
}

/// Settles an operation K left open before the handover as K settles its
/// other failures there (phase and outcome `Failed`, the cause as its
/// reason), so the receipt says the upgrade failed and changed nothing, and
/// the record no longer reads as an upgrade in progress. Under K's lock, and
/// only while the record is still the one K returned and its transaction is
/// at rest on the operation's source version; otherwise None, and the
/// upgrade stays unresolved for recovery as before.
async fn settle_before_handover(
    k_state: &Path,
    operation: &KOperation,
    cause: Option<&str>,
) -> Result<Option<KOperation>> {
    if !open_before_handover(operation) {
        return Ok(None);
    }
    let _lock = UpgradeLock::acquire(k_state)?;
    let store = FileStore::new(k_state);
    let OperationRead::Observed { operation: current } = store.read_operation() else {
        return Ok(None);
    };
    if current != *operation {
        return Ok(None);
    }
    let state = store.transaction_state().await?;
    if !state.phase().at_rest() || state.stable() != operation.from_version {
        return Ok(None);
    }
    let mut settled = operation.clone();
    settled.phase = OperationPhase::Failed;
    settled.outcome = Some(k_carrier::state::Outcome::Failed);
    let reason = settled.reason.take().or_else(|| cause.map(str::to_owned));
    settled.reason = Some(reason.unwrap_or_else(|| "stopped before handover".into()));
    settled.updated_at_ms = now_ms();
    store.persist_operation(&settled)?;
    Ok(Some(settled))
}

/// The line, code and reason of an upgrade K failed before the handover,
/// from the cause recorded in `detail.diagnostic`.
fn failed_before_handover(
    target: &str,
    release: &Release,
    detail: &BTreeMap<String, String>,
) -> (String, (FailureCode, String)) {
    let cause = recorded_cause(detail);
    if let Some(code) = cause.and_then(artifact::candidate_check_failure) {
        let (line, reason) = candidate_check_copy(code, target);
        return (line, (code, reason));
    }
    let line = failed_before_handover_line(target, release, detail);
    if cause.is_some_and(artifact::download_stalled) {
        (
            line,
            (FailureCode::DownloadStalled, DOWNLOAD_STALLED_REASON.into()),
        )
    } else {
        (
            line,
            (
                FailureCode::InstallFailed,
                format!("Raft Computer could not be upgraded to {target}."),
            ),
        )
    }
}

/// The line for an upgrade K failed: why, when it was the download, and
/// that nothing was changed.
fn failed_before_handover_line(
    target: &str,
    release: &Release,
    detail: &BTreeMap<String, String>,
) -> String {
    let download = recorded_cause(detail)
        .and_then(|cause| artifact::named_download_failure("Raft Computer", release, cause));
    let line = match download {
        Some(download) => format!("{download}. Nothing was changed."),
        None => format!("Could not upgrade to {target}. Nothing was changed."),
    };
    bounded_line(format!("{line} Run the same install command again."))
}

/// Records a failed run's error as the receipt diagnostic. The generic
/// "still requires recovery" never replaces the more specific cause already
/// recorded (K's error from this run, or the initiating failure of an earlier
/// one); it is prefixed to it once instead, so every unresolved upgrade does
/// not report the same text.
fn record_failure_diagnostic(detail: &mut BTreeMap<String, String>, error: &Error) {
    let text = error.to_string();
    if error.is_uncertain()
        && text == STILL_REQUIRES_RECOVERY
        && let Some(cause) = detail.get("diagnostic")
        && !cause.trim().is_empty()
    {
        if !cause.starts_with(STILL_REQUIRES_RECOVERY) {
            let combined = format!("{STILL_REQUIRES_RECOVERY}: {cause}");
            detail.insert("diagnostic".into(), combined);
        }
        return;
    }
    detail.insert("diagnostic".into(), text);
}

async fn resume(cfg: &Config, plan: &mut Plan, recovery: bool) -> Result<Reply> {
    if let Some(old) = report::read(cfg, &plan.request.id)?
        && old.outcome != Outcome::Unresolved
    {
        settle_terminal_metadata(cfg, &old);
        return Ok(Reply::replay(old));
    }
    if recovery {
        // A previous controller may still write its captured ProductState.
        // K's existing fence settles both controller and product-command
        // lifetimes before rebinding. Explicit repair retains its separate
        // handling for abandoned command records.
        if !(plan.kind == Kind::Repair && matches!(plan.phase, Phase::Ready | Phase::Stopping)) {
            runner::fence(cfg).await?;
        }
        host::bind_recovery_caller(cfg, &plan.request.id).await?;
    }
    let result = match plan.kind {
        Kind::Fresh | Kind::Repair => install(cfg, plan, recovery).await,
        Kind::Adopt | Kind::Upgrade => upgrade(cfg, plan, recovery).await,
    };
    match result {
        Ok(reply) => Ok(reply),
        Err(error) => {
            record_failure_diagnostic(&mut plan.detail, &error);
            // A run that disabled the supervisor and failed before restoring
            // it: restore now, best effort (only when nothing runs; a running
            // service was started through the supervisor, which enabled it).
            if let Some(warning) = restore_after_failure(cfg, plan).await {
                plan.detail.insert("warning".into(), warning);
            }
            let untouched = plan.phase == Phase::Preparing && !error.is_uncertain();
            let (line, failure) = if untouched {
                failed_untouched(&error, &plan.manifest.version)
            } else {
                let (line, reason) = recovery_hint_copy(
                    plan.from_version.as_deref(),
                    plan.from_version
                        .as_ref()
                        .and_then(|_| host::read(cfg).ok().map(|state| state.running)),
                );
                (line, (FailureCode::RecoveryUnresolved, reason))
            };
            finish(
                cfg,
                plan,
                if untouched {
                    Outcome::Failed
                } else {
                    Outcome::Unresolved
                },
                Some(failure),
                line,
            )
        }
    }
}

/// The line, code and reason of a run that failed before it changed
/// anything (an install's or repair's download or candidate check).
fn failed_untouched(error: &Error, target: &str) -> (String, (FailureCode, String)) {
    let text = error.to_string();
    if let Some(code) = artifact::candidate_check_failure(&text) {
        let (line, reason) = candidate_check_copy(code, target);
        return (line, (code, reason));
    }
    if !artifact::is_download_failure(error) {
        return (
            "The installation could not finish. Nothing was changed. Run the same install command again."
                .into(),
            (
                FailureCode::InstallFailed,
                "The installation could not finish.".into(),
            ),
        );
    }
    let line = bounded_line(format!(
        "{text}. Nothing was changed. Run the same install command again."
    ));
    if artifact::download_stalled(&text) {
        (
            line,
            (FailureCode::DownloadStalled, DOWNLOAD_STALLED_REASON.into()),
        )
    } else {
        (
            line,
            (
                FailureCode::InstallFailed,
                "The installation could not finish.".into(),
            ),
        )
    }
}

/// An earlier installation's state as a refusal finds it.
#[derive(Clone, Copy)]
struct Earlier {
    unresolved: bool,
    /// This request already ran recovery: resumed the earlier operation, ran
    /// K's recovery, or is the supervisor's next worker after a failed one
    /// (its settlement record exists).
    recovery_attempted: bool,
}

impl Earlier {
    /// Refused before an operation of its own started. Only when this request
    /// has changed nothing is an earlier unresolved installation
    /// `earlier_recovery_pending`; after its own recovery attempt the result
    /// stays unresolved (exit 3), so the supervisor's next worker still
    /// enters the repair decision.
    fn settle(self, result: &mut Receipt) {
        if self.recovery_attempted {
            result.preserve_unresolved(self.unresolved);
        } else {
            result.refuse_for_earlier_recovery(self.unresolved);
        }
    }
}

fn reject(
    cfg: &Config,
    request: &Request,
    target: Option<String>,
    from: Option<String>,
    earlier: Earlier,
    failure: (FailureCode, String),
    line: &str,
) -> Result<Reply> {
    let mut result = receipt(request, target, from, Outcome::Held, line.into());
    result.code = Some(failure.0);
    result.reason = Some(failure.1);
    earlier.settle(&mut result);
    Ok(Reply::receipt(result.finish(cfg)?))
}

pub async fn execute(cfg: &Config, request: &Request) -> Result<Reply> {
    request.validate()?;
    let mut scoped = cfg.clone();
    scoped.waiting_caller = request.waiting_caller.clone();
    let cfg = &scoped;
    let _gate = match UpgradeLock::acquire(&cfg.installer_dir.join("gate")) {
        Ok(gate) => gate,
        Err(Error::Locked(_)) => {
            return Ok(Reply::failure(
                &request.id,
                2,
                "Another installation is running on this machine.",
                FailureCode::OperationBusy,
                "Another installation is running on this machine.",
            ));
        }
        Err(error) => return Err(error),
    };
    let mut interaction = Interaction::for_presence(if request.recovery_only {
        Presence::Unattended
    } else {
        request.presence
    })?;
    let own_receipt = match report::read(cfg, &request.id) {
        Ok(value) => value,
        Err(_) => {
            // Corruption removed the evidence needed to bind this ID to its
            // original target. Preserve it and refuse to repurpose the ID.
            // Current recovery inputs are checked below independently; an old
            // unreadable result alone does not make today's installation broken.
            if request.command != "status" {
                return Ok(Reply::failure(
                    &request.id,
                    3,
                    "This request's result is unreadable. Use status to inspect the current installation.",
                    FailureCode::ResultUnreadable,
                    "This request's result is unreadable.",
                ));
            }
            None
        }
    };
    if let Some(old) = own_receipt {
        if request.command != "status"
            && (request
                .version
                .as_ref()
                .is_some_and(|v| Some(v) != old.target_version.as_ref())
                || (request.command != "recover" && request.command != old.operation))
        {
            return Ok(Reply::failure(
                &request.id,
                2,
                REQUEST_CONFLICT_LINE,
                FailureCode::RequestConflict,
                "This request ID already belongs to a different operation.",
            ));
        }
        if request.command != "status" && old.outcome != Outcome::Unresolved {
            settle_terminal_metadata(cfg, &old);
            return Ok(Reply::replay(old));
        }
    }
    let settlement = directory(cfg, &request.id).join("settlement-failed.json");
    let deferred = exists(&settlement)?;
    let mut recovery_attempted = deferred;
    let mut unresolved = exists(&damage_path(cfg))?;
    let previous = match active(cfg) {
        Ok(previous) => previous,
        Err(_) => {
            mark_damage(cfg)?;
            unresolved = true;
            None
        }
    };
    let previous_running = previous.as_ref().and_then(|plan| plan.running);
    if let Some(mut previous) =
        previous.filter(|previous| !deferred || previous.request.id == request.id)
    {
        let same = previous.request.id == request.id;
        if same
            && !["status", "recover"].contains(&request.command.as_str())
            && (previous.request.command != request.command
                || request
                    .version
                    .as_ref()
                    .is_some_and(|v| v != &previous.manifest.version))
        {
            return Ok(Reply::failure(
                &request.id,
                2,
                REQUEST_CONFLICT_LINE,
                FailureCode::RequestConflict,
                "This request ID already belongs to a different operation.",
            ));
        }
        recovery_attempted = true;
        let resumed = resume(cfg, &mut previous, true).await;
        if matches!(&resumed, Err(Error::Locked(_))) {
            return resumed;
        }
        if same && request.command != "status" {
            return resumed;
        }
        unresolved = exists(&damage_path(cfg))?
            || resumed.as_ref().map_or(true, |reply| reply.exit_code == 3);
        if resumed.is_err() {
            mark_damage(cfg)?;
        }
        if unresolved {
            // K may have retained its claim in this worker. The next worker
            // must enter the repair decision, not repeat the same failed
            // recovery and then compete with its own retained claim again.
            write_json(&settlement, request)?;
            return Ok(Reply::failure(
                &request.id,
                3,
                "An earlier installation could not be recovered yet. Run the same install command again.",
                FailureCode::RecoveryUnresolved,
                "An earlier installation could not be recovered yet.",
            ));
        }
    }
    // A failed recovery may retain K's claim until worker exit. Record that it
    // was attempted, then let the supervisor's next worker enter the repair
    // decision with that claim's owner gone; do not spin on our own retained lock.
    if exists(&settlement)? {
        match json::<Request>(&settlement) {
            Ok(saved)
                if saved.id == request.id
                    && saved.command == request.command
                    && saved.version == request.version
                    && saved.channel == request.channel => {}
            _ => {
                mark_damage(cfg)?;
                return Ok(Reply::failure(
                    &request.id,
                    3,
                    "Recovery records are unreadable. Run the same install command again.",
                    FailureCode::ResultUnreadable,
                    "Recovery records are unreadable.",
                ));
            }
        }
        unresolved = true;
        recovery_attempted = true;
    } else {
        let settled = settle_k(cfg).await;
        recovery_attempted |= !matches!(settled, Ok(false));
        if let Err(error) = settled {
            if let Error::Locked(_) = error {
                return Ok(Reply::failure(
                    &request.id,
                    2,
                    "Another upgrade is running on this machine.",
                    FailureCode::OperationBusy,
                    "Another upgrade is running on this machine.",
                ));
            }
            unresolved = true;
            write_json(&settlement, request)?;
            if error.is_uncertain() {
                return Ok(Reply::failure(
                    &request.id,
                    3,
                    "An earlier upgrade could not be recovered yet. Run the same install command again.",
                    FailureCode::RecoveryUnresolved,
                    "An earlier upgrade could not be recovered yet.",
                ));
            }
        }
    }
    let observed = world::read(cfg).await?;
    unresolved |= matches!(observed, World::Broken { .. } | World::Upgrading { .. });
    if ["status", "recover"].contains(&request.command.as_str()) {
        let answer = host::answer(cfg).await?;
        let (code, line) = match &observed {
            World::Fresh => (0, "Nothing is installed.".into()),
            World::Managed { version } | World::Adopted { version } if !unresolved => (
                0,
                format!(
                    "{version} is installed and {}.",
                    if answer.running { "running" } else { "stopped" }
                ),
            ),
            World::Held { reason, next_step } => (
                2,
                format!(
                    "Not done: {reason}.{}",
                    next_step
                        .as_ref()
                        .map(|step| format!(" {step}"))
                        .unwrap_or_default()
                ),
            ),
            _ => (
                3,
                "Installation requires repair. Run the same install command again.".into(),
            ),
        };
        let mut reply = if code == 0 {
            Reply::plain(&request.id, code, line)
        } else {
            let (failure_code, reason) = match &observed {
                World::Held { reason, .. } => (FailureCode::InstallationHeld, reason.clone()),
                _ => (
                    FailureCode::RecoveryUnresolved,
                    "The installation requires repair.".into(),
                ),
            };
            Reply::failure(&request.id, code, line, failure_code, reason)
        };
        reply.world = Some(observed);
        return Ok(reply);
    }
    if let World::Held { reason, next_step } = &observed {
        let line = format!(
            "Not done: {reason}.{}",
            next_step
                .as_ref()
                .map(|step| format!(" {step}"))
                .unwrap_or_default()
        );
        return reject(
            cfg,
            request,
            request.version.clone(),
            None,
            Earlier {
                unresolved,
                recovery_attempted,
            },
            (FailureCode::InstallationHeld, reason.clone()),
            &line,
        );
    }
    let broken = unresolved || matches!(observed, World::Broken { .. } | World::Upgrading { .. });
    if request.command == "repair" && !broken {
        return reject(
            cfg,
            request,
            request.version.clone(),
            observed.version().map(str::to_owned),
            Earlier {
                unresolved: false,
                recovery_attempted,
            },
            (
                FailureCode::RepairNotNeeded,
                "This installation does not need repair.".into(),
            ),
            "This installation does not need repair. Use install or upgrade.",
        );
    }
    if request.recovery_only && !exists(&settlement)? {
        return Ok(Reply::failure(
            &request.id,
            if unresolved { 3 } else { 1 },
            "The installation was interrupted before it started. Run the same install command again.",
            if unresolved {
                FailureCode::RecoveryUnresolved
            } else {
                FailureCode::InterruptedBeforeStart
            },
            "The installation was interrupted before it started.",
        ));
    }
    let source = Source::new(cfg)?;
    // Status before the release resolution round trip, on stderr so --json
    // stdout stays clean; the entry scripts print their own lines before the
    // download that precedes this.
    eprintln!("Checking the Raft Computer release...");
    let manifest = match &request.version {
        Some(version) => source.manifest(version).await,
        None => {
            source
                .resolve(request.channel.as_deref().unwrap_or("main"))
                .await
        }
    };
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(error) => {
            // Say why: a retry cannot fix a network that redirects Hands
            // elsewhere, so that cause names what to allow instead.
            let line = if source::is_network_redirect(&error) {
                format!("Could not resolve the requested release. {error}")
            } else if source::is_unreachable(&error) {
                format!(
                    "Could not check for the release: {error}. Nothing was changed. Check this computer's internet connection, then run the same install command again."
                )
            } else {
                format!(
                    "Could not resolve the requested release ({error}). Run the same install command again."
                )
            };
            let mut result = receipt(
                request,
                request.version.clone(),
                observed.version().map(str::to_owned),
                Outcome::Failed,
                bounded_line(line),
            );
            result.code = Some(FailureCode::ReleaseResolutionFailed);
            result.reason = Some(if source::is_network_redirect(&error) {
                "The network redirected the release request to another host.".into()
            } else if source::is_unreachable(&error) {
                "The release server could not be reached.".into()
            } else {
                "The requested Raft Computer release could not be resolved.".into()
            });
            let diagnostic = match source.roots_fallback() {
                Some(fallback) => format!("{error}; {fallback}"),
                None => error.to_string(),
            };
            result.detail.insert("diagnostic".into(), diagnostic);
            Earlier {
                unresolved,
                recovery_attempted,
            }
            .settle(&mut result);
            return Ok(Reply::receipt(result.finish(cfg)?));
        }
    };
    let from = observed.version().map(str::to_owned);
    if !request.allow_downgrade
        && from.as_ref().is_some_and(|v| {
            version::compare(&manifest.version, v).is_ok_and(|order| order.is_lt())
        })
    {
        return reject(
            cfg,
            request,
            Some(manifest.version),
            from,
            Earlier {
                unresolved,
                recovery_attempted,
            },
            (
                FailureCode::DowngradeNotAllowed,
                "The selected version is older than the installed version.".into(),
            ),
            "The selected version is older. Add --allow-downgrade to install it.",
        );
    }
    if request.presence == Presence::Attended && !request.yes {
        if request.recovery_only {
            interaction = Interaction::for_presence(Presence::Attended)?;
        }
        let question = if broken {
            format!(
                "Reinstall {}? The previous installation will be kept aside; repair cannot be rolled back.",
                manifest.version
            )
        } else {
            format!("Install {} on this machine?", manifest.version)
        };
        if !interaction.ask(&question)? {
            return reject(
                cfg,
                request,
                Some(manifest.version),
                from,
                Earlier {
                    unresolved,
                    recovery_attempted,
                },
                (
                    FailureCode::InstallationDeclined,
                    "The installation was declined.".into(),
                ),
                "Installation declined.",
            );
        }
    }
    let kind = if broken {
        Kind::Repair
    } else {
        match observed {
            World::Fresh => Kind::Fresh,
            World::Adopted { .. } => Kind::Adopt,
            _ => Kind::Upgrade,
        }
    };
    let mut plan = Plan {
        format_version: 1,
        request: request.clone(),
        manifest,
        kind,
        phase: Phase::Preparing,
        from_version: from,
        inherited_unresolved: unresolved,
        running: if unresolved { previous_running } else { None },
        setup_succeeded: false,
        next_step: None,
        detail: BTreeMap::new(),
    };
    if let Some(fallback) = source.roots_fallback() {
        plan.detail.insert("tlsRoots".into(), fallback.to_owned());
    }
    carry_over_disabled_marker(cfg, &mut plan.detail);
    if plan.kind == Kind::Repair && exists(&active_path(cfg))? {
        mark_damage(cfg)?;
    }
    if exists(&damage_path(cfg))? {
        let kept = preserve_metadata(cfg, request)?;
        plan.detail.insert(
            "metadataQuarantine".into(),
            kept.to_string_lossy().into_owned(),
        );
    }
    save(cfg, &plan)?;
    write_json(
        &active_path(cfg),
        &Active {
            format_version: 1,
            id: request.id.clone(),
        },
    )?;
    resume(cfg, &mut plan, false).await
}

#[cfg(test)]
mod candidate_check_copy_tests {
    use super::*;

    #[test]
    fn a_candidate_check_failure_says_what_happened() {
        let (line, reason) = candidate_check_copy(FailureCode::CandidateCheckTimedOut, "1.25.3");
        assert_eq!(
            reason,
            "The new Raft Computer version did not answer its first check in time; this is often the operating system scanning a newly downloaded file. Try again."
        );
        assert_eq!(
            line,
            "The new Raft Computer 1.25.3 did not answer its first check in time, often because the operating system is still scanning the newly downloaded file. Nothing was changed. Run the same install command again."
        );
        let (line, reason) = candidate_check_copy(FailureCode::CandidateCheckFailed, "1.25.3");
        assert_eq!(
            reason,
            "The downloaded Raft Computer version failed its first check."
        );
        assert!(line.contains("Nothing was changed."));
    }

    /// The receipt an upgrade K failed in candidate preparation now carries:
    /// valid, with the specific code, and no code text in its line.
    #[test]
    fn a_failed_candidate_check_makes_a_valid_receipt() {
        for code in [
            FailureCode::CandidateCheckTimedOut,
            FailureCode::CandidateCheckFailed,
        ] {
            let (line, reason) = candidate_check_copy(code, "1.25.3");
            let request = Request {
                protocol_version: 1,
                id: "request".into(),
                command: "upgrade".into(),
                version: Some("1.25.3".into()),
                channel: None,
                yes: true,
                allow_downgrade: false,
                presence: Presence::Unattended,
                approved_by: "remote:request".into(),
                recovery_only: false,
                waiting_caller: None,
                escape_failure: None,
            };
            let mut result = receipt(
                &request,
                Some("1.25.3".into()),
                Some("1.25.2".into()),
                Outcome::Failed,
                line,
            );
            result.code = Some(code);
            result.reason = Some(reason);
            report::add_phase(
                &mut result.detail,
                "candidate_check",
                std::time::Duration::from_secs(120),
            );
            result.validate().unwrap();
            let reply = Reply::receipt(result);
            reply.validate("request").unwrap();
            assert_eq!(reply.exit_code, 1);
            assert_eq!(reply.code, Some(code));
        }
    }
}

#[cfg(test)]
mod earlier_tests {
    use super::*;

    fn refused() -> Receipt {
        let request = Request {
            protocol_version: 1,
            id: "request".into(),
            command: "upgrade".into(),
            version: Some("9.9.9".into()),
            channel: None,
            yes: true,
            allow_downgrade: false,
            presence: Presence::Unattended,
            approved_by: "remote:request".into(),
            recovery_only: false,
            waiting_caller: None,
            escape_failure: None,
        };
        let mut result = receipt(
            &request,
            Some("9.9.9".into()),
            None,
            Outcome::Failed,
            "Could not resolve the requested release.".into(),
        );
        result.code = Some(FailureCode::ReleaseResolutionFailed);
        result.reason = Some("The requested Raft Computer release could not be resolved.".into());
        result
    }

    #[test]
    fn a_request_that_changed_nothing_fails_behind_an_earlier_installation() {
        let mut result = refused();
        Earlier {
            unresolved: true,
            recovery_attempted: false,
        }
        .settle(&mut result);
        assert_eq!(result.outcome, Outcome::Failed);
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.code, Some(FailureCode::EarlierRecoveryPending));
    }

    /// After its own recovery attempt (K's recovery ran, an earlier operation
    /// was resumed, or this is the worker after a failed settlement) a
    /// refusal stays unresolved: exit 3 lets the supervisor start the next
    /// worker, which enters the repair decision.
    #[test]
    fn a_request_that_attempted_recovery_stays_unresolved() {
        let mut result = refused();
        Earlier {
            unresolved: true,
            recovery_attempted: true,
        }
        .settle(&mut result);
        assert_eq!(result.outcome, Outcome::Unresolved);
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.code, Some(FailureCode::RecoveryUnresolved));
        assert_eq!(
            result.detail.get("requestCode").map(String::as_str),
            Some("release_resolution_failed")
        );
    }

    #[test]
    fn nothing_unresolved_leaves_the_refusal_as_it_is() {
        for recovery_attempted in [false, true] {
            let mut result = refused();
            Earlier {
                unresolved: false,
                recovery_attempted,
            }
            .settle(&mut result);
            assert_eq!(result, refused());
        }
    }
}

#[cfg(test)]
mod recovery_hint_tests {
    use super::*;

    #[test]
    fn recovery_hint_reports_a_running_previous_version() {
        let (line, reason) = recovery_hint_copy(Some("1.0.31"), Some(true));
        assert_eq!(
            line,
            "Raft Computer 1.0.31 is running. You can try the upgrade again later."
        );
        assert_eq!(reason, "Raft Computer 1.0.31 is running.");
    }

    #[test]
    fn recovery_hint_reports_a_stopped_previous_version() {
        let (line, reason) = recovery_hint_copy(Some("1.0.31"), Some(false));
        assert_eq!(
            line,
            "Raft Computer is not running. Run `raft-computer start`."
        );
        assert_eq!(reason, "Raft Computer is not running.");
    }

    #[test]
    fn recovery_hint_falls_back_without_a_readable_previous_state() {
        let (line, reason) = recovery_hint_copy(None, None);
        assert_eq!(
            line,
            "Raft Computer's current status couldn't be confirmed. Run the same command again to continue."
        );
        assert_eq!(
            reason,
            "Raft Computer's current status couldn't be confirmed."
        );
    }
}

#[cfg(test)]
mod failure_diagnostic_tests {
    use super::*;

    fn diagnostic(detail: &BTreeMap<String, String>) -> Option<&str> {
        detail.get("diagnostic").map(String::as_str)
    }

    /// #proj-raft-computer:#49: an upgrade K left open used to report only
    /// "upgrade still requires recovery", losing K's error saved just before.
    #[test]
    fn an_open_operation_keeps_the_cause_k_reported() {
        let mut detail = BTreeMap::new();
        detail.insert(
            "diagnostic".into(),
            "artifact download timed out after 60s".into(),
        );
        let error = Error::Uncertain(STILL_REQUIRES_RECOVERY.into());
        record_failure_diagnostic(&mut detail, &error);
        assert_eq!(
            diagnostic(&detail),
            Some("upgrade still requires recovery: artifact download timed out after 60s")
        );
        // A second unresolved run does not stack the prefix.
        record_failure_diagnostic(&mut detail, &error);
        assert_eq!(
            diagnostic(&detail),
            Some("upgrade still requires recovery: artifact download timed out after 60s")
        );
    }

    #[test]
    fn an_open_operation_without_an_earlier_cause_says_so() {
        let mut detail = BTreeMap::new();
        record_failure_diagnostic(
            &mut detail,
            &Error::Uncertain(STILL_REQUIRES_RECOVERY.into()),
        );
        assert_eq!(diagnostic(&detail), Some(STILL_REQUIRES_RECOVERY));
        let mut blank = BTreeMap::from([("diagnostic".to_owned(), " ".to_owned())]);
        record_failure_diagnostic(
            &mut blank,
            &Error::Uncertain(STILL_REQUIRES_RECOVERY.into()),
        );
        assert_eq!(diagnostic(&blank), Some(STILL_REQUIRES_RECOVERY));
    }

    #[test]
    fn any_other_failure_names_itself() {
        let mut detail = BTreeMap::from([("diagnostic".to_owned(), "older cause".to_owned())]);
        record_failure_diagnostic(
            &mut detail,
            &Error::Uncertain("previous product process is still running".into()),
        );
        assert_eq!(
            diagnostic(&detail),
            Some("previous product process is still running")
        );
        record_failure_diagnostic(&mut detail, &invalid("disk full"));
        assert_eq!(diagnostic(&detail), Some("disk full"));
    }
}

#[cfg(test)]
mod rollback_copy_tests {
    use super::*;

    #[test]
    fn rollback_copy_reports_a_restored_running_service() {
        let (line, reason) = rollback_copy("1.0.32", "1.0.31", Some(true), true);
        assert_eq!(
            line,
            "Raft Computer 1.0.32 didn't start correctly, so 1.0.31 is still running. Nothing else was changed. Run the same install command again to retry.",
        );
        assert_eq!(
            reason,
            "Raft Computer 1.0.32 didn't start correctly, so 1.0.31 is still running."
        );
    }

    #[test]
    fn rollback_copy_reports_a_service_that_failed_to_return() {
        let (line, reason) = rollback_copy("1.0.32", "1.0.31", Some(true), false);
        assert_eq!(
            line,
            "Raft Computer 1.0.32 didn't start correctly, so 1.0.31 is still installed. It was not running when the upgrade finished. If it is not back within a few minutes, run `raft-computer start`.",
        );
        assert_eq!(
            reason,
            "Raft Computer 1.0.32 didn't start correctly, so 1.0.31 is still installed. It was not running when the upgrade finished."
        );
    }

    #[test]
    fn rollback_copy_preserves_an_already_stopped_service_without_a_next_step() {
        let (line, reason) = rollback_copy("1.0.32", "1.0.31", Some(false), false);
        assert_eq!(
            line,
            "Raft Computer 1.0.32 didn't start correctly, so 1.0.31 is still installed. It was not running before the upgrade and is still not running."
        );
        assert_eq!(
            reason,
            "Raft Computer 1.0.32 didn't start correctly, so 1.0.31 is still installed and not running."
        );
    }
}

#[cfg(test)]
mod supervisor_marker_tests {
    use super::*;
    use crate::host_supervisor::{self, HostSupervisor, Kind, test_manager};

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.state_home = dir.path().to_path_buf();
        cfg.installer_dir = dir.path().join("computer").join("installer");
        (dir, cfg)
    }

    fn agent() -> HostSupervisor {
        HostSupervisor {
            kind: Kind::LaunchdAgent,
            name: "build.raft.desktop.computer".into(),
            definition_path: Some(PathBuf::from("/x.plist")),
            binary_path: PathBuf::from("/x/raft-computer"),
            launcher_path: None,
            min_computer_version: "1.0.58".into(),
        }
    }

    /// archer #60 gap (a): a repair that stopped (disabling an enabled agent)
    /// and then failed restores right away, best effort.
    #[tokio::test]
    async fn a_failed_run_restores_what_it_disabled_right_away() {
        let (_dir, cfg) = cfg();
        test_manager::install(|program, args| match (program, args.first().copied()) {
            ("launchctl", Some("print-disabled")) => Some((true, String::new())),
            _ => Some((true, String::new())),
        });
        assert!(
            host_supervisor::note_stop(&cfg, &agent()).await.unwrap(),
            "it was enabled: this stop disables it"
        );
        assert!(
            host_supervisor::disabled_by_earlier_run(&cfg),
            "the durable marker"
        );
        // The service is running (started through the supervisor): nothing to do.
        assert_eq!(
            restore_after_failure_with(&cfg, Some(agent()), true).await,
            None
        );
        assert!(test_manager::commands().is_empty());
        // Not running after the failure: re-enabled now, marker cleared.
        assert_eq!(
            restore_after_failure_with(&cfg, Some(agent()), false).await,
            None
        );
        assert!(
            test_manager::commands()
                .iter()
                .any(|c| c.starts_with("launchctl enable"))
        );
        assert!(!host_supervisor::disabled_by_earlier_run(&cfg));
    }

    /// archer #60 gap (b): when the restore fails too, the marker survives
    /// into the next run's new plan, which restores it.
    #[tokio::test]
    async fn the_marker_survives_a_failed_restore_into_the_next_plan() {
        let (_dir, cfg) = cfg();
        test_manager::install(|_, _| Some((true, String::new())));
        assert!(host_supervisor::note_stop(&cfg, &agent()).await.unwrap());
        test_manager::fail_commands(true);
        assert!(
            restore_after_failure_with(&cfg, Some(agent()), false)
                .await
                .is_some(),
            "a warning"
        );
        test_manager::fail_commands(false);
        assert!(
            host_supervisor::disabled_by_earlier_run(&cfg),
            "kept for the next run"
        );
        // The next run builds a fresh plan: it inherits the marker.
        let mut detail = BTreeMap::new();
        carry_over_disabled_marker(&cfg, &mut detail);
        assert_eq!(
            detail.get("supervisorDisabledByRun").map(String::as_str),
            Some("true")
        );
        // Its own stop sees the agent disabled (by the earlier run) and still owns the restore.
        test_manager::install(|program, args| match (program, args.first().copied()) {
            ("launchctl", Some("print-disabled")) => Some((
                true,
                "\t\"build.raft.desktop.computer\" => disabled\n".into(),
            )),
            _ => Some((true, String::new())),
        });
        assert!(
            host_supervisor::note_stop(&cfg, &agent()).await.unwrap(),
            "an earlier run's marker counts"
        );
        assert_eq!(
            host_supervisor::restore_after_stop(&cfg, &agent(), false).await,
            None
        );
        assert!(
            test_manager::commands()
                .iter()
                .any(|c| c.starts_with("launchctl enable"))
        );
        assert!(!host_supervisor::disabled_by_earlier_run(&cfg));
        // Without any marker, a user's stop is never undone.
        let mut fresh = BTreeMap::new();
        carry_over_disabled_marker(&cfg, &mut fresh);
        assert!(fresh.is_empty());
    }
}

#[cfg(test)]
mod before_handover_tests {
    use super::*;

    /// A host K never needs here: no test below reaches the handover.
    struct NoHost;
    #[async_trait]
    impl k_carrier::host::Host for NoHost {
        async fn quiesce(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self, _: Slot) -> Result<()> {
            Ok(())
        }
        async fn start(&self, _: Slot) -> Result<()> {
            Ok(())
        }
        async fn probe(&self) -> Result<k_carrier::state::Evidence> {
            Err(invalid("no host in this test"))
        }
        async fn resume(&self) -> Result<()> {
            Ok(())
        }
    }

    fn operation(id: &str, from: &str, phase: OperationPhase) -> KOperation {
        KOperation {
            format_version: 1,
            id: id.into(),
            started_at_ms: 1,
            updated_at_ms: 1,
            from_version: from.into(),
            target_version: "1.1.0".into(),
            previous_stable_version: from.into(),
            phase,
            outcome: None,
            reason: None,
            provenance: None,
            metadata: BTreeMap::new(),
        }
    }

    /// K state with stable 1.0.0 and `open` as its operation record.
    fn k_state(open: &KOperation) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("stable.bin");
        fs::write(&source, b"stable").unwrap();
        let store = FileStore::new(dir.path().join("k"));
        store.bootstrap_locked("1.0.0", &source).unwrap();
        store.persist_operation(open).unwrap();
        dir
    }

    fn recorded(dir: &tempfile::TempDir) -> KOperation {
        match FileStore::new(dir.path().join("k")).read_operation() {
            OperationRead::Observed { operation } => operation,
            other => panic!("no operation record: {other:?}"),
        }
    }

    #[test]
    fn only_an_open_operation_before_the_handover_qualifies() {
        for phase in [
            OperationPhase::Checking,
            OperationPhase::Downloading,
            OperationPhase::Verifying,
            OperationPhase::Staging,
        ] {
            let open = operation("a", "1.0.0", phase);
            assert!(open_before_handover(&open), "{phase:?}");
        }
        for phase in [
            OperationPhase::HandingOver,
            OperationPhase::Probing,
            OperationPhase::Recovering,
        ] {
            let open = operation("a", "1.0.0", phase);
            assert!(!open_before_handover(&open), "{phase:?}");
        }
        let mut settled = operation("a", "1.0.0", OperationPhase::Downloading);
        settled.outcome = Some(k_carrier::state::Outcome::Failed);
        assert!(!open_before_handover(&settled));
    }

    // #proj-raft-computer:#49 (c3353daa): K's candidate download timed out,
    // nothing was stopped, and the upgrade was reported unresolved.
    #[tokio::test]
    async fn a_failed_download_is_settled_as_failed_and_the_next_upgrade_runs() {
        let open = operation("first", "1.0.0", OperationPhase::Downloading);
        let dir = k_state(&open);
        let k = dir.path().join("k");
        let cause = "DOWNLOAD_FAILED: overall timeout";
        let settled = settle_before_handover(&k, &open, Some(cause)).await;
        let settled = settled.unwrap().expect("settled");
        assert_eq!(settled.phase, OperationPhase::Failed);
        assert_eq!(settled.outcome, Some(k_carrier::state::Outcome::Failed));
        assert_eq!(settled.reason.as_deref(), Some(cause));
        assert_eq!(recorded(&dir), settled, "persisted");

        // The next upgrade request is accepted: K archives the settled
        // record and starts its own (failing here at the release lookup).
        let runner = k_carrier::runner::Runner::new(
            FileStore::new(&k),
            Arc::new(NoHost),
            Arc::new(OfflineSource),
        )
        .unwrap();
        let response = runner
            .execute(&protocol::Request::Upgrade {
                protocol_version: 1,
                id: "second".into(),
                target_version: "1.1.0".into(),
                consented: true,
            })
            .await
            .unwrap();
        assert_eq!(response.result, "failed", "{response:?}");
        let error = response.error.clone().unwrap_or_default();
        assert!(!error.contains("OPERATION_IN_PROGRESS"), "{response:?}");
        let OperationRead::Observed { operation } = &response.operation else {
            panic!("{response:?}");
        };
        assert_eq!(operation.id, "second");
        let archived = FileStore::new(k).read_archive("first");
        let OperationRead::Observed { operation } = archived else {
            panic!("the settled record is archived");
        };
        assert_eq!(operation.outcome, Some(k_carrier::state::Outcome::Failed));
    }

    #[tokio::test]
    async fn the_handover_or_later_is_left_for_recovery() {
        for phase in [OperationPhase::HandingOver, OperationPhase::Recovering] {
            let open = operation("first", "1.0.0", phase);
            let dir = k_state(&open);
            let k = dir.path().join("k");
            let settled = settle_before_handover(&k, &open, Some("cause")).await;
            assert!(settled.unwrap().is_none(), "{phase:?}");
            assert_eq!(recorded(&dir), open, "{phase:?} unchanged");
        }
    }

    #[tokio::test]
    async fn a_record_that_no_longer_matches_is_left_for_recovery() {
        // K's stable is not the operation's source version.
        let open = operation("first", "0.9.0", OperationPhase::Downloading);
        let dir = k_state(&open);
        let k = dir.path().join("k");
        let settled = settle_before_handover(&k, &open, None).await;
        assert!(settled.unwrap().is_none());
        assert_eq!(recorded(&dir), open);
        // The record on disk is another operation than the one K returned.
        let open = operation("first", "1.0.0", OperationPhase::Downloading);
        let dir = k_state(&operation("other", "1.0.0", OperationPhase::Downloading));
        let k = dir.path().join("k");
        let settled = settle_before_handover(&k, &open, None).await;
        assert!(settled.unwrap().is_none());
        assert_eq!(recorded(&dir).id, "other");
    }

    fn release() -> Release {
        Release {
            version: "1.1.0".into(),
            url: "https://hands.example/dl/raft-computer-cli/releases/1/linux-x64".into(),
            sha256: "0".repeat(64),
            size: 1,
            gzip: None,
        }
    }

    fn detail(diagnostic: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("diagnostic".to_owned(), diagnostic.to_owned())])
    }

    #[test]
    fn a_download_failure_names_its_cause_and_that_nothing_changed() {
        let from = "Could not download Raft Computer from https://hands.example/dl/raft-computer-cli/releases/1/linux-x64";
        let rest = "Nothing was changed. Run the same install command again.";
        for (diagnostic, cause) in [
            (
                "DOWNLOAD_FAILED: overall timeout",
                "the connection was too slow to finish the download in time",
            ),
            (
                "DOWNLOAD_FAILED: response timeout",
                "the download timed out",
            ),
            (
                "upgrade still requires recovery: DOWNLOAD_FAILED: idle timeout",
                "the download stopped receiving data for 30 s; check this computer's internet connection",
            ),
        ] {
            let line = failed_before_handover_line("1.1.0", &release(), &detail(diagnostic));
            assert_eq!(line, format!("{from}: {cause}. {rest}"));
        }
        let generic =
            "Could not upgrade to 1.1.0. Nothing was changed. Run the same install command again.";
        let line = failed_before_handover_line("1.1.0", &release(), &detail("check failed"));
        assert_eq!(line, generic);
        let line = failed_before_handover_line("1.1.0", &release(), &BTreeMap::new());
        assert_eq!(line, generic);
    }

    /// A fresh install's or repair's download that ended stalled after its
    /// retries fails as download_stalled; other failures keep their codes.
    #[test]
    fn a_stalled_install_download_fails_as_download_stalled() {
        let release = release();
        let stalled = artifact::named_download_failure(
            "Raft Computer",
            &release,
            "DOWNLOAD_FAILED: idle timeout",
        )
        .unwrap();
        let (line, (code, reason)) = failed_untouched(&invalid(stalled.clone()), "1.1.0");
        assert_eq!(code, FailureCode::DownloadStalled);
        assert_eq!(reason, DOWNLOAD_STALLED_REASON);
        assert_eq!(
            line,
            format!("{stalled}. Nothing was changed. Run the same install command again.")
        );
        let slow = artifact::named_download_failure(
            "Raft Computer",
            &release,
            "DOWNLOAD_FAILED: overall timeout",
        )
        .unwrap();
        let (line, (code, reason)) = failed_untouched(&invalid(slow), "1.1.0");
        assert_eq!(code, FailureCode::InstallFailed);
        assert_eq!(reason, "The installation could not finish.");
        assert!(line.contains("too slow"), "{line}");
        let (_, (code, _)) = failed_untouched(
            &invalid("CANDIDATE_CHECK_TIMED_OUT: product command timed out after 120 s"),
            "1.1.0",
        );
        assert_eq!(code, FailureCode::CandidateCheckTimedOut);
        let (line, (code, _)) = failed_untouched(&invalid("disk full"), "1.1.0");
        assert_eq!(code, FailureCode::InstallFailed);
        assert!(
            line.starts_with("The installation could not finish."),
            "{line}"
        );
    }

    /// 11-15: a candidate download that stops receiving data fails as
    /// download_stalled, with product-language copy and a valid receipt.
    #[test]
    fn a_stalled_candidate_download_fails_as_download_stalled() {
        for diagnostic in [
            "DOWNLOAD_FAILED: idle timeout",
            "upgrade still requires recovery: DOWNLOAD_FAILED: idle timeout",
        ] {
            let (line, (code, reason)) =
                failed_before_handover("1.1.0", &release(), &detail(diagnostic));
            assert_eq!(code, FailureCode::DownloadStalled);
            assert_eq!(reason, DOWNLOAD_STALLED_REASON);
            assert!(line.contains("stopped receiving data"), "{line}");
            assert!(line.contains("Nothing was changed."), "{line}");
            for text in [&line, &reason] {
                assert!(!text.to_lowercase().contains("installer"), "{text}");
                assert!(!text.contains(code.as_str()), "{text}");
            }
            let request = Request {
                protocol_version: 1,
                id: "request".into(),
                command: "upgrade".into(),
                version: Some("1.1.0".into()),
                channel: None,
                yes: true,
                allow_downgrade: false,
                presence: Presence::Unattended,
                approved_by: "remote:request".into(),
                recovery_only: false,
                waiting_caller: None,
                escape_failure: None,
            };
            let mut result = receipt(
                &request,
                Some("1.1.0".into()),
                Some("1.0.0".into()),
                Outcome::Failed,
                line,
            );
            result.code = Some(code);
            result.reason = Some(reason);
            result.validate().unwrap();
        }
        for (diagnostic, code) in [
            (
                "DOWNLOAD_FAILED: overall timeout",
                FailureCode::InstallFailed,
            ),
            (
                "DOWNLOAD_FAILED: HTTP 503 Service Unavailable",
                FailureCode::InstallFailed,
            ),
            (
                "CANDIDATE_CHECK_TIMED_OUT: product command timed out after 120 s",
                FailureCode::CandidateCheckTimedOut,
            ),
        ] {
            let (_, (found, _)) = failed_before_handover("1.1.0", &release(), &detail(diagnostic));
            assert_eq!(found, code, "{diagnostic}");
        }
    }
}

#[cfg(test)]
mod confinement_tests {
    use super::*;
    use crate::systemd::{confinement_reason, unit_of_cgroup};

    #[test]
    fn the_escape_note_is_a_diagnostic_and_the_reason_stays_product_language() {
        let unit = unit_of_cgroup("/system.slice/raft-computer.service").unwrap();
        let note = "systemd-run --scope exited with status 1: Access denied";
        let mut detail = BTreeMap::new();
        let reason = confined(&unit, Some(note.into()), &mut detail);
        assert_eq!(reason, confinement_reason(&unit));
        assert!(!reason.contains("systemd-run"));
        assert_eq!(detail.get("diagnostic").map(String::as_str), Some(note));

        let mut untouched = BTreeMap::new();
        assert_eq!(confined(&unit, None, &mut untouched), reason);
        assert!(untouched.is_empty());
    }
}

#[cfg(test)]
mod already_installed_tests {
    use super::*;
    use k_carrier::storage::Effects;
    use std::{
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
    };

    /// Every connection made to the release server, closed at once so a
    /// download fails fast instead of waiting for a reply.
    struct Server {
        url: String,
        connections: Arc<AtomicUsize>,
    }

    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/computer", listener.local_addr().unwrap());
            let connections = Arc::new(AtomicUsize::new(0));
            let counted = connections.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    counted.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
            });
            Self { url, connections }
        }
        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    const SIDECAR: &[u8] = b"installed sidecar";

    /// A managed installation of `installed` (K stable, saved sidecar
    /// identity) whose binary is not running.
    fn machine(installed: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.state_home = dir.path().to_path_buf();
        cfg.k_state = dir.path().join("computer").join("k");
        cfg.installer_dir = dir.path().join("computer").join("installer");
        cfg.install_dir = dir.path().join("bin");
        cfg.binary = cfg.install_dir.join("raft-computer");
        cfg.sidecar = cfg.install_dir.join("raft-computer-sidecar");
        cfg.waiting_caller = None;
        fs::create_dir_all(&cfg.install_dir).unwrap();
        fs::write(&cfg.binary, b"installed computer").unwrap();
        fs::write(&cfg.sidecar, SIDECAR).unwrap();
        FileStore::new(&cfg.k_state)
            .bootstrap_locked(installed, &cfg.binary)
            .unwrap();
        artifact::adopt_installed_sidecar(&cfg, installed).unwrap();
        (dir, cfg)
    }

    fn plan(server: &Server, kind: Kind, from: Option<&str>, target: &str) -> Plan {
        let release = Release {
            version: target.into(),
            url: server.url.clone(),
            sha256: "0".repeat(64),
            size: 1,
            gzip: None,
        };
        Plan {
            format_version: 1,
            request: Request {
                protocol_version: 1,
                id: "request".into(),
                command: "upgrade".into(),
                version: Some(target.into()),
                channel: None,
                yes: true,
                allow_downgrade: true,
                presence: Presence::Unattended,
                approved_by: "remote:request".into(),
                recovery_only: false,
                waiting_caller: None,
                escape_failure: None,
            },
            manifest: Manifest {
                version: target.into(),
                // The identity `machine` saved for the installed sidecar.
                sidecar: Some(Release {
                    url: format!("{}-sidecar", server.url),
                    sha256: sha256(SIDECAR),
                    size: SIDECAR.len() as u64,
                    ..release.clone()
                }),
                release,
            },
            kind,
            phase: Phase::Preparing,
            from_version: from.map(str::to_owned),
            inherited_unresolved: false,
            running: None,
            setup_succeeded: false,
            next_step: None,
            detail: BTreeMap::new(),
        }
    }

    /// Nothing to do: up to date from local state alone, without a single
    /// connection to the release server or a download.
    #[tokio::test]
    async fn an_installed_version_is_up_to_date_without_a_download() {
        let (_dir, cfg) = machine("1.1.0");
        let server = Server::start();
        let mut plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        save(&cfg, &plan).unwrap();
        let reply = upgrade(&cfg, &mut plan, false).await;
        assert_eq!(server.connections(), 0, "no network call");
        assert_eq!(
            plan.phase,
            Phase::Preparing,
            "never got ready to download: {reply:?}"
        );
        let reply = reply.unwrap();
        reply.validate("request").unwrap();
        let receipt = reply.receipt.expect("a receipt");
        assert_eq!(receipt.outcome, Outcome::UpToDate);
        assert_eq!(receipt.exit_code, 0);
        assert_eq!(receipt.line, "1.1.0 is already installed.");
        assert_eq!(receipt.code, None);
        assert_eq!(receipt.target_version.as_deref(), Some("1.1.0"));
        assert_eq!(receipt.from_version.as_deref(), Some("1.1.0"));
        assert_eq!(
            receipt.detail.get("readback").map(String::as_str),
            Some("candidate"),
            "not running"
        );
        assert!(!exists(&cfg.scratch()).unwrap(), "no download");
        assert!(
            matches!(
                FileStore::new(&cfg.k_state).read_operation(),
                OperationRead::Genesis
            ),
            "K was not asked"
        );
    }

    #[tokio::test]
    async fn a_complete_installation_qualifies() {
        let (_dir, cfg) = machine("1.1.0");
        let server = Server::start();
        let plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        assert!(already_installed(&cfg, &plan).await.unwrap());
        // A settled earlier operation does not hold it back.
        let mut operation = open_operation();
        operation.outcome = Some(k_carrier::state::Outcome::Promoted);
        FileStore::new(&cfg.k_state)
            .persist_operation(&operation)
            .unwrap();
        assert!(already_installed(&cfg, &plan).await.unwrap());
    }

    fn open_operation() -> KOperation {
        KOperation {
            format_version: 1,
            id: "earlier".into(),
            started_at_ms: 1,
            updated_at_ms: 1,
            from_version: "1.0.0".into(),
            target_version: "1.1.0".into(),
            previous_stable_version: "1.0.0".into(),
            phase: OperationPhase::Downloading,
            outcome: None,
            reason: None,
            provenance: None,
            metadata: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn another_kind_or_version_downloads_first() {
        let (_dir, cfg) = machine("1.1.0");
        let server = Server::start();
        for (kind, from, target) in [
            (Kind::Repair, Some("1.1.0"), "1.1.0"),
            (Kind::Adopt, Some("1.1.0"), "1.1.0"),
            (Kind::Fresh, None, "1.1.0"),
            (Kind::Upgrade, None, "1.1.0"),
            // An upgrade and a downgrade (allowed in `plan`).
            (Kind::Upgrade, Some("1.1.0"), "1.2.0"),
            (Kind::Upgrade, Some("1.1.0"), "1.0.0"),
        ] {
            let plan = plan(&server, kind, from, target);
            assert!(
                !already_installed(&cfg, &plan).await.unwrap(),
                "{kind:?} {from:?} -> {target}"
            );
        }
    }

    #[tokio::test]
    async fn k_on_another_version_downloads_first() {
        // The binary reports 1.1.0 but K's stable is 1.0.0.
        let (_dir, cfg) = machine("1.0.0");
        artifact::adopt_installed_sidecar(&cfg, "1.1.0").unwrap();
        let server = Server::start();
        let plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        assert!(!already_installed(&cfg, &plan).await.unwrap());
    }

    #[tokio::test]
    async fn an_incomplete_sidecar_downloads_first() {
        let server = Server::start();
        let plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        // Missing: the installed file is gone.
        let (_dir, cfg) = machine("1.1.0");
        fs::remove_file(&cfg.sidecar).unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap(), "missing");
        // Corrupt: the installed file differs from its saved identity.
        let (_dir, cfg) = machine("1.1.0");
        fs::write(&cfg.sidecar, b"other bytes").unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap(), "corrupt");
        // Corrupt: the saved copy no longer matches its record.
        let (_dir, cfg) = machine("1.1.0");
        let saved = artifact::saved_sidecar(&cfg, "1.1.0").unwrap().unwrap();
        fs::write(&saved, b"other bytes").unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap(), "saved copy");
        // No saved record: not known to be complete.
        let (_dir, cfg) = machine("1.1.0");
        fs::remove_dir_all(cfg.sidecar_dir("1.1.0").unwrap()).unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap(), "no record");
    }

    #[tokio::test]
    async fn k_not_at_rest_downloads_first() {
        let server = Server::start();
        let plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        // A transaction staged in K's journal.
        let (dir, cfg) = machine("1.1.0");
        let store = FileStore::new(&cfg.k_state);
        let candidate = dir.path().join("candidate");
        fs::write(&candidate, b"candidate").unwrap();
        store.stage("1.2.0", &candidate).await.unwrap();
        store
            .append(&k_carrier::state::JournalEntry {
                seq: 0,
                timestamp_ms: 1,
                intent: k_carrier::state::Phase::Staged,
                detail: BTreeMap::from([
                    ("formatVersion".to_owned(), "1".to_owned()),
                    ("version".to_owned(), "1.2.0".to_owned()),
                ]),
            })
            .await
            .unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap(), "staged");
        // An operation still open before K's journal moves (downloading).
        let (_dir, cfg) = machine("1.1.0");
        FileStore::new(&cfg.k_state)
            .persist_operation(&open_operation())
            .unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap(), "open");
    }

    /// K's rule for a settled operation: stable is a promotion's target,
    /// otherwise its source version.
    #[tokio::test]
    async fn k_disagreeing_with_its_last_operation_downloads_first() {
        let server = Server::start();
        let plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        let settled = |from: &str, target: &str, outcome| KOperation {
            from_version: from.into(),
            target_version: target.into(),
            previous_stable_version: from.into(),
            phase: OperationPhase::Promoted,
            outcome: Some(outcome),
            ..open_operation()
        };
        use k_carrier::state::Outcome as K;
        for (operation, qualifies) in [
            // Stable is 1.1.0 in every case.
            (settled("1.0.0", "1.2.0", K::Promoted), false),
            (settled("1.0.0", "1.1.0", K::RolledBack), false),
            (settled("1.0.0", "1.1.0", K::Promoted), true),
            (settled("1.1.0", "1.2.0", K::RolledBack), true),
            (settled("1.1.0", "1.1.0", K::UpToDate), true),
        ] {
            let (_dir, cfg) = machine("1.1.0");
            FileStore::new(&cfg.k_state)
                .persist_operation(&operation)
                .unwrap();
            assert_eq!(
                already_installed(&cfg, &plan).await.unwrap(),
                qualifies,
                "{operation:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_sidecar_unlike_the_release_downloads_first() {
        let server = Server::start();
        // A record of no sidecar against a release that has one.
        let (_dir, cfg) = machine("1.1.0");
        fs::remove_file(&cfg.sidecar).unwrap();
        fs::remove_dir_all(cfg.sidecar_dir("1.1.0").unwrap()).unwrap();
        artifact::adopt_installed_sidecar(&cfg, "1.1.0").unwrap();
        assert_eq!(
            artifact::saved_sidecar_identity(&cfg, "1.1.0").unwrap(),
            None
        );
        let mut with_none = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        assert!(!already_installed(&cfg, &with_none).await.unwrap());
        with_none.manifest.sidecar = None;
        assert!(
            already_installed(&cfg, &with_none).await.unwrap(),
            "both absent"
        );
        // A sidecar republished with another hash, or size.
        let (_dir, cfg) = machine("1.1.0");
        let mut republished = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        republished.manifest.sidecar.as_mut().unwrap().sha256 = "1".repeat(64);
        assert!(
            !already_installed(&cfg, &republished).await.unwrap(),
            "hash"
        );
        let mut resized = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        resized.manifest.sidecar.as_mut().unwrap().size += 1;
        assert!(!already_installed(&cfg, &resized).await.unwrap(), "size");
    }

    #[tokio::test]
    async fn a_k_lock_held_elsewhere_downloads_first() {
        let (_dir, cfg) = machine("1.1.0");
        let server = Server::start();
        let plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        let held = UpgradeLock::acquire(&cfg.k_state).unwrap();
        assert!(!already_installed(&cfg, &plan).await.unwrap());
        drop(held);
        assert!(already_installed(&cfg, &plan).await.unwrap());
    }

    #[test]
    fn a_running_service_on_another_version_is_uncertain() {
        let (_dir, cfg) = machine("1.1.0");
        let server = Server::start();
        let mut stale = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        save(&cfg, &stale).unwrap();
        let error = finish_up_to_date_with(&cfg, &mut stale, Some("1.0.0".into())).unwrap_err();
        assert!(error.is_uncertain(), "{error}");
        assert!(!exists(&cfg.receipt_path("request")).unwrap(), "no receipt");

        let mut running = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        let receipt = finish_up_to_date_with(&cfg, &mut running, Some("1.1.0".into()))
            .unwrap()
            .receipt
            .unwrap();
        assert_eq!(receipt.outcome, Outcome::UpToDate);
        assert_eq!(receipt.line, "1.1.0 is already installed. It is running.");
        assert_eq!(
            receipt.detail.get("readback").map(String::as_str),
            Some("service")
        );
    }

    /// Raft Desktop's registration, with its definition file present.
    #[cfg(target_os = "linux")]
    fn register(cfg: &Config, binary: &Path, minimum: &str) {
        let definition = cfg
            .state_home
            .join("config/systemd/user/raft-desktop-computer.service");
        fs::create_dir_all(definition.parent().unwrap()).unwrap();
        fs::write(&definition, b"[Service]\n").unwrap();
        let record = serde_json::json!({
            "formatVersion": 2, "owner": "app",
            "supervisor": {
                "kind": "systemd-user", "name": "raft-desktop-computer.service",
                "definitionPath": definition, "binaryPath": binary,
                "launcherPath": null, "registeredBy": "desktop",
                "registeredAt": "t", "minComputerVersion": minimum
            }
        });
        let path = crate::host_supervisor::registration_path(cfg);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, record.to_string()).unwrap();
    }

    /// The refusals about the target still come back as after a download,
    /// and still without one.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_target_raft_desktop_refuses_is_still_refused() {
        let server = Server::start();
        for (binary, minimum, code) in [
            (
                Some("/elsewhere/raft-computer"),
                "1.0.0",
                FailureCode::InstallationPathMismatch,
            ),
            (None, "1.2.0", FailureCode::InstallationHeld),
        ] {
            let (_dir, cfg) = machine("1.1.0");
            let binary = binary.map_or(cfg.binary.clone(), PathBuf::from);
            register(&cfg, &binary, minimum);
            let mut plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
            save(&cfg, &plan).unwrap();
            let reply = upgrade(&cfg, &mut plan, false).await.unwrap();
            reply.validate("request").unwrap();
            let receipt = reply.receipt.unwrap();
            assert_eq!(receipt.outcome, Outcome::Held, "{minimum}");
            assert_eq!(receipt.code, Some(code), "{minimum}");
            assert_eq!(plan.phase, Phase::Preparing);
        }
        // A registration that allows it: up to date.
        let (_dir, cfg) = machine("1.1.0");
        register(&cfg, &cfg.binary, "1.1.0");
        let mut plan = plan(&server, Kind::Upgrade, Some("1.1.0"), "1.1.0");
        save(&cfg, &plan).unwrap();
        let reply = upgrade(&cfg, &mut plan, false).await.unwrap();
        assert_eq!(reply.receipt.unwrap().outcome, Outcome::UpToDate);
        assert_eq!(server.connections(), 0, "no network call");
    }
}
