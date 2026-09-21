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
