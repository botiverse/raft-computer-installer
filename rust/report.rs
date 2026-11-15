//! Durable product receipts complement K's transaction receipts. Replaying a
//! completed request returns the stored result; live observation is `status`.
use crate::{Result, config::Config, presence::Presence, version};
use k_carrier::{
    error::invalid,
    storage::{now_ms, write_json},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// The reason of an `earlier_recovery_pending` refusal. It does not contain
/// "An earlier installation could not be recovered": callers match that text
/// as an unresolved receipt that a later worker may still continue.
pub const EARLIER_RECOVERY_PENDING_REASON: &str = "An earlier installation on this computer is unfinished and needs a repair, so nothing was changed.";

/// `detail.phasesMs`: wall-clock milliseconds per phase of this request, as a
/// JSON object of names to whole numbers (`{"candidate_check":41250,...}`).
/// Inside `detail`, whose values are strings, so a reader that does not know
/// it is unaffected.
pub const PHASES_KEY: &str = "phasesMs";
/// The most phases one receipt carries, and the largest duration it states.
const PHASES_MAX: usize = 16;
pub const PHASE_MS_MAX: u64 = 7 * 24 * 60 * 60 * 1000;

fn valid_phase_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn phases(detail: &BTreeMap<String, String>) -> Option<BTreeMap<String, u64>> {
    let value = detail.get(PHASES_KEY)?;
    serde_json::from_str(value).ok()
}

fn valid_phases(detail: &BTreeMap<String, String>) -> bool {
    if !detail.contains_key(PHASES_KEY) {
        return true;
    }
    phases(detail).is_some_and(|phases| {
        phases.len() <= PHASES_MAX
            && phases
                .iter()
                .all(|(name, ms)| valid_phase_name(name) && *ms <= PHASE_MS_MAX)
    })
}

/// Adds `elapsed` to `phase` in `detail.phasesMs` (a phase resumed by a later
/// worker accumulates). Bounded: unknown-shaped names and phases beyond the
/// limit are dropped, durations are clamped.
pub fn add_phase(detail: &mut BTreeMap<String, String>, phase: &str, elapsed: Duration) {
    if !valid_phase_name(phase) {
        return;
    }
    let mut current = phases(detail).unwrap_or_default();
    if current.len() >= PHASES_MAX && !current.contains_key(phase) {
        return;
    }
    let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let total = current.entry(phase.into()).or_default();
    *total = total.saturating_add(ms).min(PHASE_MS_MAX);
    if let Ok(value) = serde_json::to_string(&current) {
        detail.insert(PHASES_KEY.into(), value);
    }
}

/// Phase timings collected where `detail` is not at hand (inside K's runner
/// hooks, inside artifact preparation), merged into it afterwards.
#[derive(Clone, Default)]
pub struct PhaseLog(Arc<Mutex<PhaseState>>);

#[derive(Default)]
struct PhaseState {
    done: Vec<(&'static str, Duration)>,
    open: BTreeMap<&'static str, Instant>,
}

impl PhaseLog {
    fn with<T>(&self, f: impl FnOnce(&mut PhaseState) -> T) -> T {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state)
    }
    pub fn add(&self, phase: &'static str, elapsed: Duration) {
        self.with(|state| state.done.push((phase, elapsed)));
    }
    pub fn begin(&self, phase: &'static str) {
        self.with(|state| {
            state.open.entry(phase).or_insert_with(Instant::now);
        });
    }
    pub fn end(&self, phase: &'static str) {
        self.with(|state| {
            if let Some(started) = state.open.remove(phase) {
                state.done.push((phase, started.elapsed()));
            }
        });
    }
    /// The total recorded so far for `phase` (open phases not included).
    pub fn total(&self, phase: &str) -> Duration {
        self.with(|state| {
            state
                .done
                .iter()
                .filter(|(name, _)| *name == phase)
                .map(|(_, elapsed)| *elapsed)
                .sum()
        })
    }
    /// Ends every open phase and moves all timings into `detail`.
    pub fn drain_into(&self, detail: &mut BTreeMap<String, String>) {
        let done = self.with(|state| {
            let open = std::mem::take(&mut state.open);
            let mut done = std::mem::take(&mut state.done);
            done.extend(
                open.into_iter()
                    .map(|(name, started)| (name, started.elapsed())),
            );
            done
        });
        for (phase, elapsed) in done {
            add_phase(detail, phase, elapsed);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Installed,
    Promoted,
    Repaired,
    UpToDate,
    Failed,
    RolledBack,
    Held,
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    CallerNotComputer,
    CandidateCheckFailed,
    CandidateCheckTimedOut,
    CandidateStartFailed,
    DowngradeNotAllowed,
    EarlierRecoveryPending,
    EscapeFailed,
    InstallFailed,
    InstallationDeclined,
    InstallationDirNotWritable,
    InstallationHeld,
    InstallationPathMismatch,
    InterruptedBeforeStart,
    LegacyFailure,
    OperationBusy,
    RecoveryUnresolved,
    ReleaseResolutionFailed,
    RepairNotNeeded,
    RequestConflict,
    ResultUnreadable,
    RunningAsRoot,
    UnexpectedError,
}

impl FailureCode {
    pub const ALL: [Self; 22] = [
        Self::CallerNotComputer,
        Self::CandidateCheckFailed,
        Self::CandidateCheckTimedOut,
        Self::CandidateStartFailed,
        Self::DowngradeNotAllowed,
        Self::EarlierRecoveryPending,
        Self::EscapeFailed,
        Self::InstallFailed,
        Self::InstallationDeclined,
        Self::InstallationDirNotWritable,
        Self::InstallationHeld,
        Self::InstallationPathMismatch,
        Self::InterruptedBeforeStart,
        Self::LegacyFailure,
        Self::OperationBusy,
        Self::RecoveryUnresolved,
        Self::ReleaseResolutionFailed,
        Self::RepairNotNeeded,
        Self::RequestConflict,
        Self::ResultUnreadable,
        Self::RunningAsRoot,
        Self::UnexpectedError,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallerNotComputer => "caller_not_computer",
            Self::CandidateCheckFailed => "candidate_check_failed",
            Self::CandidateCheckTimedOut => "candidate_check_timed_out",
            Self::CandidateStartFailed => "candidate_start_failed",
            Self::DowngradeNotAllowed => "downgrade_not_allowed",
            Self::EarlierRecoveryPending => "earlier_recovery_pending",
            Self::EscapeFailed => "escape_failed",
            Self::InstallFailed => "install_failed",
            Self::InstallationDeclined => "installation_declined",
            Self::InstallationDirNotWritable => "installation_dir_not_writable",
            Self::InstallationHeld => "installation_held",
            Self::InstallationPathMismatch => "installation_path_mismatch",
            Self::InterruptedBeforeStart => "interrupted_before_start",
            Self::LegacyFailure => "legacy_failure",
            Self::OperationBusy => "operation_busy",
            Self::RecoveryUnresolved => "recovery_unresolved",
            Self::ReleaseResolutionFailed => "release_resolution_failed",
            Self::RepairNotNeeded => "repair_not_needed",
            Self::RequestConflict => "request_conflict",
            Self::ResultUnreadable => "result_unreadable",
            Self::RunningAsRoot => "running_as_root",
            Self::UnexpectedError => "unexpected_error",
        }
    }
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Installed | Self::Promoted | Self::Repaired | Self::UpToDate => 0,
            Self::Failed | Self::RolledBack => 1,
            Self::Held => 2,
            Self::Unresolved => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub protocol: String,
    pub installer_version: String,
    pub id: String,
    pub operation: String,
    pub presence: Presence,
    pub target_version: Option<String>,
    pub from_version: Option<String>,
    pub approved_by: Option<String>,
    pub outcome: Outcome,
    pub exit_code: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<FailureCode>,
    pub line: String,
    pub next_step: Option<String>,
    pub finished_at_ms: u64,
    pub detail: BTreeMap<String, String>,
}

impl Receipt {
    /// This request's receipt, before its outcome's fields are filled in.
    pub fn new(
        request: &crate::request::Request,
        target: Option<String>,
        from: Option<String>,
        outcome: Outcome,
        line: String,
    ) -> Self {
        Self {
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
            reason: None,
            code: None,
            line,
            next_step: None,
            finished_at_ms: 0,
            detail: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.protocol != "raft-computer-installer/v3"
            || self.installer_version.is_empty()
            || self.id.is_empty()
            || self.id.trim() != self.id
            || self.id.encode_utf16().count() > 256
            || !["install", "upgrade", "repair", "recover"].contains(&self.operation.as_str())
            || self.exit_code != self.outcome.exit_code()
            || self.line.trim().is_empty()
            || self.line.len() > 2048
            || self.line.chars().any(char::is_control)
            || self.reason.as_ref().is_some_and(|reason| {
                reason.trim().is_empty()
                    || reason.len() > 2048
                    || reason.chars().any(char::is_control)
            })
            || self
                .code
                .is_some_and(|code| self.line.contains(code.as_str()))
            || matches!(
                (&self.reason, &self.code),
                (Some(_), None) | (None, Some(_))
            )
            || (self.outcome.exit_code() == 0 && (self.reason.is_some() || self.code.is_some()))
            || self
                .next_step
                .as_ref()
                .is_some_and(|s| s.len() > 512 || s.chars().any(char::is_control))
            || !valid_phases(&self.detail)
        {
            return Err(invalid("invalid receipt"));
        }
        if let Some(v) = &self.target_version {
            version::exact(v)?;
        }
        if let Some(v) = &self.from_version {
            version::exact(v)?;
        }
        Ok(())
    }

    /// A declined or failed repair cannot erase an earlier unresolved state.
    /// This request's own code, when it had a different one, stays in
    /// `detail.requestCode` (the line keeps its own words).
    pub fn preserve_unresolved(&mut self, unresolved: bool) {
        if unresolved && self.outcome != Outcome::Repaired {
            self.keep_request_failure(FailureCode::RecoveryUnresolved);
            self.outcome = Outcome::Unresolved;
            self.exit_code = 3;
            self.reason = Some("An earlier installation could not be recovered.".into());
            self.code = Some(FailureCode::RecoveryUnresolved);
        }
    }

    /// A request refused before it started an operation of its own (held,
    /// declined, release not resolved) while an earlier installation is still
    /// unresolved. Nothing was changed by this request and no later worker
    /// continues it, so it is `failed` with `earlier_recovery_pending`: the
    /// caller can settle it at once instead of waiting for a recovery that
    /// belongs to the earlier operation, whose records stay as they were.
    /// Any other outcome is preserved as unresolved as before.
    pub fn refuse_for_earlier_recovery(&mut self, unresolved: bool) {
        if !unresolved {
            return;
        }
        if !matches!(self.outcome, Outcome::Held | Outcome::Failed) {
            return self.preserve_unresolved(unresolved);
        }
        self.keep_request_failure(FailureCode::EarlierRecoveryPending);
        self.outcome = Outcome::Failed;
        self.exit_code = Outcome::Failed.exit_code();
        self.reason = Some(EARLIER_RECOVERY_PENDING_REASON.into());
        self.code = Some(FailureCode::EarlierRecoveryPending);
    }

    fn keep_request_failure(&mut self, replacement: FailureCode) {
        if let Some(code) = self.code.filter(|code| *code != replacement) {
            self.detail
                .entry("requestCode".into())
                .or_insert_with(|| code.as_str().into());
        }
    }

    pub fn finish(mut self, cfg: &Config) -> Result<Self> {
        self.finished_at_ms = now_ms();
        self.validate()?;
        let path = cfg.receipt_path(&self.id);
        // The caller holds the product operation gate. A terminal receipt is
        // immutable; an unresolved receipt may be replaced only by the same
        // request's successful continuation, after disk recovery proves it.
        if let Some(old) = read(cfg, &self.id)? {
            // An unresolved receipt written before the release was resolved
            // (settle.rs: a `--channel` request given up on) names no target;
            // the same request's continuation may still settle it.
            let target_unknown = old.outcome == Outcome::Unresolved && old.target_version.is_none();
            if (!target_unknown && old.target_version != self.target_version)
                || old.operation != self.operation
            {
                return Err(invalid("request identity conflict"));
            }
            if old.outcome != Outcome::Unresolved {
                if old == self {
                    return Ok(old);
                }
                return Err(invalid("request already completed"));
            }
        }
        write_json(&path, &self)?;
        Ok(self)
    }
}

/// Writes `receipt` only if this request has none yet: never replaces one,
/// whatever it says (a finished request's result, a running one's
/// continuation, or an unreadable file). The file appears complete or not at
/// all. Returns the receipt when this call wrote it. For a request that would
/// otherwise end without a durable result (settle.rs); an operation writes
/// its own through `Receipt::finish`.
pub fn create(cfg: &Config, mut receipt: Receipt) -> Result<Option<Receipt>> {
    use std::io::Write;
    receipt.finished_at_ms = now_ms();
    receipt.validate()?;
    let path = cfg.receipt_path(&receipt.id);
    if k_carrier::storage::exists(&path)? {
        return Ok(None);
    }
    let directory = path
        .parent()
        .ok_or_else(|| invalid("receipt path has no parent"))?;
    // Root in another user's home (refused up front, failure::root_refusal)
    // creates no folder there; a file it adds goes to the folder's owner.
    #[cfg(unix)]
    let owner = if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(directory) {
            Ok(meta) => Some((meta.uid(), meta.gid())),
            Err(_) => return Ok(None),
        }
    } else {
        None
    };
    k_carrier::storage::ensure_dir(directory)?;
    let mut file = tempfile::Builder::new()
        .prefix(".settle-")
        .tempfile_in(directory)?;
    #[cfg(unix)]
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::fchown(file.as_file(), Some(uid), Some(gid))?;
    }
    file.write_all(&serde_json::to_vec(&receipt)?)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(None);
        }
        Err(error) => return Err(error.error.into()),
    }
    k_carrier::storage::sync_dir(directory)?;
    Ok(Some(receipt))
}

pub fn read(cfg: &Config, id: &str) -> Result<Option<Receipt>> {
    let bytes = match fs::read(cfg.receipt_path(id)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if bytes.len() > 65536 {
        return Err(invalid("receipt too large"));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    receipt.validate()?;
    if receipt.id != id {
        return Err(invalid("receipt identity mismatch"));
    }
    Ok(Some(receipt))
}

#[cfg(test)]
mod failure_code_tests {
    use super::*;

    #[test]
    fn fixed_failure_codes_round_trip_to_the_public_strings() {
        let values: Vec<String> = FailureCode::ALL
            .into_iter()
            .map(|code| serde_json::to_string(&code).unwrap())
            .collect();
        assert_eq!(values.len(), 22);
        assert_eq!(values[0], "\"caller_not_computer\"");
        assert_eq!(values[1], "\"candidate_check_failed\"");
        assert_eq!(values[2], "\"candidate_check_timed_out\"");
        assert_eq!(values[5], "\"earlier_recovery_pending\"");
        assert_eq!(values[6], "\"escape_failed\"");
        assert_eq!(values[19], "\"result_unreadable\"");
        assert_eq!(values[20], "\"running_as_root\"");
        assert_eq!(values[21], "\"unexpected_error\"");
        for code in FailureCode::ALL {
            assert_eq!(
                serde_json::to_string(&code).unwrap(),
                format!("\"{}\"", code.as_str())
            );
        }
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    fn receipt(outcome: Outcome, code: FailureCode, reason: &str, line: &str) -> Receipt {
        Receipt {
            protocol: "raft-computer-installer/v3".into(),
            installer_version: "0.0.0".into(),
            id: "request".into(),
            operation: "upgrade".into(),
            presence: Presence::Unattended,
            target_version: Some("1.25.3".into()),
            from_version: Some("1.25.2".into()),
            approved_by: Some("remote:request".into()),
            outcome,
            exit_code: outcome.exit_code(),
            reason: Some(reason.into()),
            code: Some(code),
            line: line.into(),
            next_step: None,
            finished_at_ms: 1,
            detail: BTreeMap::new(),
        }
    }

    /// A refusal while an earlier installation is unresolved used to read
    /// exactly like this request's own unresolved outcome: `unresolved`,
    /// `recovery_unresolved`, "An earlier installation could not be
    /// recovered.". It is now a settled failure with its own code.
    #[test]
    fn a_refusal_behind_an_earlier_unresolved_installation_is_its_own_failure() {
        for (outcome, code) in [
            (Outcome::Held, FailureCode::InstallationHeld),
            (Outcome::Held, FailureCode::DowngradeNotAllowed),
            (Outcome::Failed, FailureCode::ReleaseResolutionFailed),
        ] {
            let mut refused = receipt(outcome, code, "Refused.", "Not done: refused.");
            refused.refuse_for_earlier_recovery(true);
            assert_eq!(refused.outcome, Outcome::Failed);
            assert_eq!(refused.exit_code, 1);
            assert_eq!(refused.code, Some(FailureCode::EarlierRecoveryPending));
            assert_eq!(
                refused.reason.as_deref(),
                Some(EARLIER_RECOVERY_PENDING_REASON)
            );
            assert_eq!(
                refused.detail.get("requestCode").map(String::as_str),
                Some(code.as_str())
            );
            // The line keeps this request's own words.
            assert_eq!(refused.line, "Not done: refused.");
            // Callers keep matching this text as an unresolved receipt.
            assert!(
                !EARLIER_RECOVERY_PENDING_REASON
                    .contains("An earlier installation could not be recovered")
            );
            refused.validate().unwrap();
        }
    }

    #[test]
    fn a_refusal_without_an_earlier_unresolved_installation_is_unchanged() {
        let original = receipt(
            Outcome::Held,
            FailureCode::DowngradeNotAllowed,
            "Older.",
            "Not done.",
        );
        let mut refused = original.clone();
        refused.refuse_for_earlier_recovery(false);
        assert_eq!(refused, original);
    }

    #[test]
    fn an_unresolved_or_finished_request_stays_unresolved_behind_an_earlier_one() {
        let mut own = receipt(
            Outcome::Unresolved,
            FailureCode::RecoveryUnresolved,
            "Raft Computer's current status couldn't be confirmed.",
            "Run the same command again.",
        );
        own.refuse_for_earlier_recovery(true);
        assert_eq!(own.outcome, Outcome::Unresolved);
        assert_eq!(own.code, Some(FailureCode::RecoveryUnresolved));
        assert!(!own.detail.contains_key("requestCode"));

        // An operation of this request's own (a repair plan) keeps today's
        // meaning, and now names the code it replaced.
        let mut repair = receipt(
            Outcome::Failed,
            FailureCode::CandidateCheckTimedOut,
            "Timed out.",
            "Nothing was changed.",
        );
        repair.preserve_unresolved(true);
        assert_eq!(repair.outcome, Outcome::Unresolved);
        assert_eq!(repair.exit_code, 3);
        assert_eq!(repair.code, Some(FailureCode::RecoveryUnresolved));
        assert_eq!(
            repair.reason.as_deref(),
            Some("An earlier installation could not be recovered.")
        );
        assert_eq!(
            repair.detail.get("requestCode").map(String::as_str),
            Some("candidate_check_timed_out")
        );
        repair.validate().unwrap();
    }

    #[test]
    fn phase_durations_accumulate_and_stay_bounded() {
        let mut detail = BTreeMap::new();
        add_phase(
            &mut detail,
            "candidate_check",
            Duration::from_millis(41_250),
        );
        add_phase(&mut detail, "candidate_check", Duration::from_millis(750));
        add_phase(&mut detail, "stop", Duration::from_secs(u64::MAX / 2));
        add_phase(&mut detail, "Bad-Name", Duration::from_secs(1));
        assert_eq!(
            detail.get(PHASES_KEY).map(String::as_str),
            Some(format!("{{\"candidate_check\":42000,\"stop\":{PHASE_MS_MAX}}}").as_str())
        );
        for i in 0..40 {
            add_phase(
                &mut detail,
                &"x".repeat(i % 30 + 1),
                Duration::from_millis(1),
            );
        }
        let mut ok = receipt(
            Outcome::Promoted,
            FailureCode::InstallFailed,
            "x",
            "Upgraded.",
        );
        ok.reason = None;
        ok.code = None;
        ok.detail = detail;
        ok.validate().unwrap();
        assert_eq!(phases(&ok.detail).unwrap().len(), PHASES_MAX);
    }

    #[test]
    fn a_receipt_with_unbounded_phase_durations_is_invalid() {
        for value in [
            "not json",
            "{\"stop\":-1}",
            "{\"stop\":1.5}",
            "{\"Stop\":1}",
            "{\"stop\":\"1\"}",
            &format!("{{\"stop\":{}}}", PHASE_MS_MAX + 1),
        ] {
            let mut bad = receipt(Outcome::Failed, FailureCode::InstallFailed, "x", "Failed.");
            bad.detail.insert(PHASES_KEY.into(), value.into());
            assert!(bad.validate().is_err(), "{value}");
        }
    }

    #[test]
    fn a_phase_log_closes_open_phases_when_drained() {
        let log = PhaseLog::default();
        log.add("candidate_check", Duration::from_millis(5));
        log.begin("handover");
        log.begin("download");
        log.end("download");
        log.end("never_begun");
        assert_eq!(log.total("candidate_check"), Duration::from_millis(5));
        let mut detail = BTreeMap::new();
        log.drain_into(&mut detail);
        let phases = phases(&detail).unwrap();
        assert_eq!(phases.get("candidate_check"), Some(&5));
        assert!(phases.contains_key("handover"));
        assert!(phases.contains_key("download"));
        assert!(!phases.contains_key("never_begun"));
        // Drained: a second drain adds nothing.
        let before = detail.clone();
        log.drain_into(&mut detail);
        assert_eq!(detail, before);
    }
}
