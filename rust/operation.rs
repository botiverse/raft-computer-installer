//! One product writer spans preparation, K, publication and repair. The gate
//! lives outside the directory repair quarantines, and belongs to the worker,
//! so losing its CLI supervisor cannot release a still-running transaction.
use crate::{
    Error, Result, artifact, computer,
    config::Config,
    host,
    presence::{Interaction, Presence},
    report::{self, Outcome, Receipt},
    request::{Reply, Request},
    runner, shell_path,
    source::{FrozenSource, Manifest, Source},
    version,
    world::{self, World},
};
use async_trait::async_trait;
use k_carrier::{
    artifact::{Release, ReleaseContext, ReleaseSource, sha256, verify},
    error::invalid,
    lock::UpgradeLock,
    protocol,
    state::{OperationRead, Slot},
    storage::{FileStore, ensure_dir, exists, now_ms, sync_dir, write_durable, write_json},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

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
                "installer metadata cannot be preserved safely".into(),
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
        return Err(invalid("installer operation record too large"));
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
        return Err(invalid("installer operation identity mismatch"));
    }
    version::exact(&plan.manifest.version)?;
    plan.manifest.release.validate()?;
    Ok(plan)
}

fn active(cfg: &Config) -> Result<Option<Plan>> {
    if !exists(&active_path(cfg))? {
        return Ok(None);
    }
    let active: Active = json(&active_path(cfg))?;
    if active.format_version != 1 {
        return Err(invalid("unsupported installer operation format"));
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

fn receipt(
    request: &Request,
    target: Option<String>,
    from: Option<String>,
    outcome: Outcome,
    line: String,
) -> Receipt {
    Receipt {
        protocol: "raft-computer-installer/v3".into(),
        installer_version: env!("CARGO_PKG_VERSION").into(),
        id: request.id.clone(),
        operation: request.command.clone(),
        presence: request.presence,
        target_version: target,
        from_version: from,
        approved_by: Some(request.approved_by.clone()),
        outcome,
        exit_code: outcome.exit_code(),
        line,
        next_step: None,
        finished_at_ms: 0,
        detail: BTreeMap::new(),
    }
}

fn finish(
    cfg: &Config,
    plan: &mut Plan,
    outcome: Outcome,
    line: impl Into<String>,
) -> Result<Reply> {
    let mut line = line.into();
    if outcome.exit_code() == 0 || outcome == Outcome::RolledBack {
        if plan.detail.get("readback").map(String::as_str) == Some("service") {
            line.push_str(" It is running.");
        }
        if let Some(hint) = &plan.next_step {
            line.push_str(&format!(" Next: {hint}"));
        }
        if let Some(hint) = plan.detail.get("pathHint") {
            line.push(' ');
            line.push_str(hint);
        }
    }
    let mut result = receipt(
        &plan.request,
        Some(plan.manifest.version.clone()),
        plan.from_version.clone(),
        outcome,
        line,
    );
    result.detail = plan.detail.clone();
    result.next_step = plan.next_step.clone();
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

async fn settle_k(cfg: &Config) -> Result<()> {
    // A settled K receipt must not restart a service the user later stopped.
    if let OperationRead::Observed { operation } = FileStore::new(&cfg.k_state).read_operation()
        && operation.outcome.is_none()
    {
        runner::create(cfg.clone(), Arc::new(OfflineSource), false)?
            .recover(None)
            .await?;
    }
    Ok(())
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

/// The recovery hint carries the cause and tells the user to repeat the
/// command they already have. The native installer is an internal detail:
/// its path never appears in user-facing text, and the entry scripts resume
/// an unresolved operation themselves before this line is ever shown.
fn recovery_hint_line(error: &Error) -> String {
    format!(
        "Installation could not finish ({error}). Run the same install command again to continue where it left off."
    )
}

fn rollback_line(target: &str, from_version: &str, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => {
            format!("{target} failed its checks ({reason}); {from_version} was restored.")
        }
        None => format!("{target} failed its checks; {from_version} was restored."),
    }
}

async fn upgrade(cfg: &Config, plan: &mut Plan, recovery: bool) -> Result<Reply> {
    if plan.phase == Phase::Preparing {
        if recovery {
            return finish(
                cfg,
                plan,
                Outcome::Failed,
                "Installation was interrupted before the upgrade started. Run the same install command again.",
            );
        }
        artifact::acquire_sidecar(cfg, &plan.manifest).await?;
        transition(cfg, plan, Phase::Ready)?;
    }
    if plan.phase == Phase::Ready {
        let _lock = UpgradeLock::acquire(&cfg.k_state)?;
        let from = plan
            .from_version
            .as_ref()
            .ok_or_else(|| invalid("adoption has no source version"))?;
        artifact::adopt_installed_sidecar(cfg, from)?;
        if plan.kind == Kind::Adopt {
            FileStore::new(&cfg.k_state).bootstrap_locked(from, &cfg.binary)?;
        }
        transition(cfg, plan, Phase::Upgrading)?;
    }
    let store = FileStore::new(&cfg.k_state);
    let runner = runner::create(
        cfg.clone(),
        Arc::new(FrozenSource(plan.manifest.clone())),
        plan.request.allow_downgrade,
    )?;
    let response = if recovery {
        match store.read_operation() {
            OperationRead::Observed { operation }
                if operation.id == plan.request.id
                    && operation.target_version == plan.manifest.version =>
            {
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
            _ => {
                return finish(
                    cfg,
                    plan,
                    Outcome::Failed,
                    "Installation was interrupted before the upgrade transaction started. Run the same install command again.",
                );
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
    if response.exit_code == 2 && response.result == "busy" {
        return finish(
            cfg,
            plan,
            Outcome::Held,
            "Another upgrade is running on this machine.",
        );
    }
    if let Some(error) = &response.error {
        // Preserve the initiating failure across recovery's later outcome.
        plan.detail
            .entry("upgradeError".into())
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
    let Some(outcome) = operation.outcome else {
        return Err(Error::Uncertain("upgrade still requires recovery".into()));
    };
    let outcome = match outcome {
        k_carrier::state::Outcome::Promoted => Outcome::Promoted,
        k_carrier::state::Outcome::RolledBack => Outcome::RolledBack,
        k_carrier::state::Outcome::UpToDate => Outcome::UpToDate,
        k_carrier::state::Outcome::Held => Outcome::Held,
        k_carrier::state::Outcome::Failed => Outcome::Failed,
    };
    if let Some(reason) = operation.reason {
        plan.detail.insert("reason".into(), reason);
    }
    if let Ok(state) = host::read(cfg) {
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
        plan.next_step = state.next_step;
    }
    let target = &plan.manifest.version;
    let line = match outcome {
        Outcome::Promoted => format!("Upgraded {} to {target}.", operation.from_version),
        Outcome::RolledBack => rollback_line(
            target,
            &operation.from_version,
            plan.detail.get("reason").map(String::as_str),
        ),
        Outcome::UpToDate => format!("{target} is already installed."),
        Outcome::Held => {
            "Upgrade was not allowed. Check the selected version and --allow-downgrade.".into()
        }
        _ => format!(
            "Could not upgrade to {target}. Run the same install command again to check and continue."
        ),
    };
    finish(cfg, plan, outcome, line)
}

async fn install(
    cfg: &Config,
    plan: &mut Plan,
    recovery: bool,
    interaction: &Interaction,
) -> Result<Reply> {
    if plan.phase == Phase::Preparing {
        if recovery {
            return finish(
                cfg,
                plan,
                Outcome::Failed,
                "Installation was interrupted before changing installed files. Run the same install command again.",
            );
        }
        let candidate = if plan.kind == Kind::Repair {
            artifact::acquire_repair(cfg, &plan.manifest).await?
        } else {
            artifact::acquire(cfg, &plan.manifest).await?
        };
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
        plan.next_step = answer.next_step;
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
        forced.extend(host::stop_product(cfg).await?);
        if let Some(path) = computer::preserve_commands_after_stop(cfg, &plan.request.id).await? {
            plan.detail.insert(
                "commandQuarantine".into(),
                path.to_string_lossy().into_owned(),
            );
        }
        plan.detail
            .insert("forcedStops".into(), serde_json::to_string(&forced)?);
        transition(cfg, plan, Phase::Quarantining)?;
    }
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
        transition(cfg, plan, Phase::Verifying)?;
    }
    if plan.phase == Phase::Verifying {
        if computer::self_report(&cfg.binary, cfg).await?.version != plan.manifest.version {
            return Err(Error::Uncertain(
                "published program did not report the selected version".into(),
            ));
        }
        if let Some(hint) = shell_path::ensure(cfg).await? {
            plan.detail.insert("pathHint".into(), hint);
        }
        let status = computer::status(cfg).await.ok();
        plan.next_step = status.as_ref().and_then(|s| s.next_step.clone());
        if status.is_none() {
            plan.next_step = Some("Run raft-computer status to check first setup.".into());
        }
        if plan.next_step.is_some()
            && status.is_some()
            && plan.request.presence == Presence::Attended
            && !recovery
        {
            transition(cfg, plan, Phase::Setup)?;
            plan.setup_succeeded = computer::first_setup(cfg, interaction).await?;
            // Setup is intentionally not retried after a worker crash.
            transition(cfg, plan, Phase::Starting)?;
        } else {
            transition(cfg, plan, Phase::Starting)?;
        }
    }
    if plan.phase == Phase::Setup {
        plan.setup_succeeded = computer::status(cfg)
            .await
            .is_ok_and(|s| s.next_step.is_none());
        transition(cfg, plan, Phase::Starting)?;
    }
    if plan.phase == Phase::Starting {
        if plan.running == Some(true) || plan.setup_succeeded {
            let evidence = match host::live_evidence(cfg).await {
                Ok(evidence) => evidence,
                Err(_) => host::start_product(cfg).await?,
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
        }
        if let Ok(status) = computer::status(cfg).await {
            plan.next_step = status.next_step;
        }
        let outcome = if plan.kind == Kind::Repair {
            Outcome::Repaired
        } else {
            Outcome::Installed
        };
        let line = format!(
            "{} {}.",
            if plan.kind == Kind::Repair {
                "Reinstalled"
            } else {
                "Installed"
            },
            plan.manifest.version
        );
        return finish(cfg, plan, outcome, line);
    }
    Err(invalid("unexpected installer operation phase"))
}

async fn resume(
    cfg: &Config,
    plan: &mut Plan,
    recovery: bool,
    interaction: &Interaction,
) -> Result<Reply> {
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
        Kind::Fresh | Kind::Repair => install(cfg, plan, recovery, interaction).await,
        Kind::Adopt | Kind::Upgrade => upgrade(cfg, plan, recovery).await,
    };
    match result {
        Ok(reply) => Ok(reply),
        Err(error) => {
            plan.detail.insert("error".into(), error.to_string());
            let untouched = plan.phase == Phase::Preparing && !error.is_uncertain();
            finish(
                cfg,
                plan,
                if untouched {
                    Outcome::Failed
                } else {
                    Outcome::Unresolved
                },
                if untouched {
                    "Could not prepare the installation. Installed files were not changed.".into()
                } else {
                    recovery_hint_line(&error)
                },
            )
        }
    }
}

fn reject(
    cfg: &Config,
    request: &Request,
    target: Option<String>,
    from: Option<String>,
    unresolved: bool,
    line: &str,
) -> Result<Reply> {
    let mut result = receipt(request, target, from, Outcome::Held, line.into());
    result.preserve_unresolved(unresolved);
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
            return Ok(Reply::plain(
                &request.id,
                2,
                "Another installer is running on this machine.",
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
                return Ok(Reply::plain(
                    &request.id,
                    3,
                    "This request's result is unreadable. Use status to inspect the current installation.",
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
            return Ok(Reply::plain(
                &request.id,
                2,
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
            return Ok(Reply::plain(
                &request.id,
                2,
                "This request ID already belongs to a different operation.",
            ));
        }
        let resumed = resume(cfg, &mut previous, true, &interaction).await;
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
            return Ok(Reply::plain(
                &request.id,
                3,
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
                return Ok(Reply::plain(
                    &request.id,
                    3,
                    "Recovery records are unreadable. Start a new repair command.",
                ));
            }
        }
        unresolved = true;
    } else if let Err(error) = settle_k(cfg).await {
        if let Error::Locked(_) = error {
            return Ok(Reply::plain(
                &request.id,
                2,
                "Another upgrade is running on this machine.",
            ));
        }
        unresolved = true;
        write_json(&settlement, request)?;
        if error.is_uncertain() {
            return Ok(Reply::plain(
                &request.id,
                3,
                "An earlier upgrade could not be recovered yet.",
            ));
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
            World::Held { reason } => (2, format!("Not done: {reason}.")),
            _ => (
                3,
                "Installation requires repair. Run the same install command again.".into(),
            ),
        };
        let mut reply = Reply::plain(&request.id, code, line);
        reply.world = Some(observed);
        return Ok(reply);
    }
    if let World::Held { reason } = &observed {
        return reject(
            cfg,
            request,
            request.version.clone(),
            None,
            unresolved,
            &format!("Not done: {reason}."),
        );
    }
    let broken = unresolved || matches!(observed, World::Broken { .. } | World::Upgrading { .. });
    if request.command == "repair" && !broken {
        return reject(
            cfg,
            request,
            request.version.clone(),
            observed.version().map(str::to_owned),
            false,
            "This installation does not need repair. Use install or upgrade.",
        );
    }
    if request.recovery_only && !exists(&settlement)? {
        return Ok(Reply::plain(
            &request.id,
            if unresolved { 3 } else { 1 },
            "The request was interrupted before a recoverable installation started. Run the same install command again.",
        ));
    }
    let source = Source::new(cfg)?;
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
            let mut result = receipt(
                request,
                request.version.clone(),
                observed.version().map(str::to_owned),
                Outcome::Failed,
                "Could not resolve the requested release. Run the same install command again."
                    .into(),
            );
            result.detail.insert("error".into(), error.to_string());
            result.preserve_unresolved(unresolved);
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
            unresolved,
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
                unresolved,
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
    resume(cfg, &mut plan, false, &interaction).await
}

#[cfg(test)]
mod recovery_hint_tests {
    use super::*;

    #[test]
    fn recovery_hint_names_the_cause_and_never_the_installer() {
        let line = recovery_hint_line(&Error::Uncertain("digest mismatch".into()));
        assert_eq!(
            line,
            "Installation could not finish (digest mismatch). Run the same install command again to continue where it left off."
        );
        assert!(!line.contains("installer"), "{line}");
        assert!(!line.contains('/'), "{line}");
    }
}

#[cfg(test)]
mod rollback_line_tests {
    use super::*;

    #[test]
    fn rollback_line_with_reason_carries_it_inline() {
        assert_eq!(
            rollback_line("1.0.32", "1.0.31", Some("experiment probe failed: boom")),
            "1.0.32 failed its checks (experiment probe failed: boom); 1.0.31 was restored.",
        );
    }

    #[test]
    fn rollback_line_without_reason_is_the_legacy_verbatim_line() {
        assert_eq!(
            rollback_line("1.0.32", "1.0.31", None),
            "1.0.32 failed its checks; 1.0.31 was restored.",
        );
    }
}
