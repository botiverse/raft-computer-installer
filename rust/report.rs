//! Durable product receipts complement K's transaction receipts. Replaying a
//! completed request returns the stored result; live observation is `status`.
use crate::{Result, config::Config, presence::Presence, version};
use k_carrier::{
    error::invalid,
    storage::{now_ms, write_json},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs};

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
    CandidateStartFailed,
    DowngradeNotAllowed,
    InstallFailed,
    InstallationDeclined,
    InstallationHeld,
    InterruptedBeforeStart,
    LegacyFailure,
    OperationBusy,
    RecoveryUnresolved,
    ReleaseResolutionFailed,
    RepairNotNeeded,
    RequestConflict,
    ResultUnreadable,
}

impl FailureCode {
    pub const ALL: [Self; 14] = [
        Self::CallerNotComputer,
        Self::CandidateStartFailed,
        Self::DowngradeNotAllowed,
        Self::InstallFailed,
        Self::InstallationDeclined,
        Self::InstallationHeld,
        Self::InterruptedBeforeStart,
        Self::LegacyFailure,
        Self::OperationBusy,
        Self::RecoveryUnresolved,
        Self::ReleaseResolutionFailed,
        Self::RepairNotNeeded,
        Self::RequestConflict,
        Self::ResultUnreadable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallerNotComputer => "caller_not_computer",
            Self::CandidateStartFailed => "candidate_start_failed",
            Self::DowngradeNotAllowed => "downgrade_not_allowed",
            Self::InstallFailed => "install_failed",
            Self::InstallationDeclined => "installation_declined",
            Self::InstallationHeld => "installation_held",
            Self::InterruptedBeforeStart => "interrupted_before_start",
            Self::LegacyFailure => "legacy_failure",
            Self::OperationBusy => "operation_busy",
            Self::RecoveryUnresolved => "recovery_unresolved",
            Self::ReleaseResolutionFailed => "release_resolution_failed",
            Self::RepairNotNeeded => "repair_not_needed",
            Self::RequestConflict => "request_conflict",
            Self::ResultUnreadable => "result_unreadable",
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
    pub fn preserve_unresolved(&mut self, unresolved: bool) {
        if unresolved && self.outcome != Outcome::Repaired {
            self.outcome = Outcome::Unresolved;
            self.exit_code = 3;
            self.reason = Some("An earlier installation could not be recovered.".into());
            self.code = Some(FailureCode::RecoveryUnresolved);
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
            if old.target_version != self.target_version || old.operation != self.operation {
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
        assert_eq!(values.len(), 14);
        assert_eq!(values[0], "\"caller_not_computer\"");
        assert_eq!(values[13], "\"result_unreadable\"");
        for code in FailureCode::ALL {
            assert_eq!(
                serde_json::to_string(&code).unwrap(),
                format!("\"{}\"", code.as_str())
            );
        }
    }
}
