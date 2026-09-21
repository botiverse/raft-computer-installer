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

/// How the printed failure hints can invoke this installer again. The
/// bootstrap downloads the binary into a temporary directory it deletes on
/// exit, and the supervisor's worker copy lives in scratch that is cleaned
/// after a settled operation — neither survives. Hints therefore name the
/// durable copy (see `Config::durable_binary`), quoted so the printed command
/// is copy-pasteable even with spaces in the path.
pub fn installer_invocation(durable_binary: &std::path::Path) -> String {
    shell_invocation(&durable_binary.display().to_string(), cfg!(windows))
}

/// Render a path as a complete, paste-ready command for the entry shell.
/// POSIX sh single-quotes the literal (no expansion of `$`, backticks or
/// spaces; an inner `'` becomes the standard `'"'"'` splice). PowerShell
/// needs the call operator and single-quoted literals double an inner `'`.
fn shell_invocation(path: &str, windows: bool) -> String {
    if windows {
        format!("& '{}'", path.replace('\'', "''"))
    } else {
        format!("'{}'", path.replace('\'', "'\"'\"'"))
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
            return Err(invalid("invalid installer receipt"));
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
                return Err(invalid("installer request identity conflict"));
            }
            if old.outcome != Outcome::Unresolved {
                if old == self {
                    return Ok(old);
                }
                return Err(invalid("installer request already completed"));
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
        return Err(invalid("installer receipt too large"));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    receipt.validate()?;
    if receipt.id != id {
        return Err(invalid("installer receipt identity mismatch"));
    }
    Ok(Some(receipt))
}

#[cfg(test)]
mod shell_invocation_tests {
    use super::*;

    #[test]
    fn posix_invocation_is_a_single_quoted_literal() {
        assert_eq!(
            shell_invocation("/opt/slot/installer", false),
            "'/opt/slot/installer'"
        );
        assert_eq!(
            shell_invocation("/opt/a b/$HOME/`id`", false),
            "'/opt/a b/$HOME/`id`'"
        );
        assert_eq!(
            shell_invocation("/opt/it's here/installer", false),
            "'/opt/it'\"'\"'s here/installer'",
        );
    }

    #[test]
    fn powershell_invocation_uses_the_call_operator_with_doubled_quotes() {
        assert_eq!(
            shell_invocation("C:\\slot space\\installer.exe", true),
            "& 'C:\\slot space\\installer.exe'"
        );
        assert_eq!(
            shell_invocation("C:\\it's\\installer.exe", true),
            "& 'C:\\it''s\\installer.exe'"
        );
    }
}
