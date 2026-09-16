use crate::{
    Result,
    presence::Presence,
    report::{Outcome, Receipt},
    source, version,
    world::World,
};
use k_carrier::error::invalid;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub id: String,
    pub command: String,
    pub version: Option<String>,
    pub channel: Option<String>,
    pub yes: bool,
    pub allow_downgrade: bool,
    pub presence: Presence,
    pub approved_by: String,
    pub recovery_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_caller: Option<crate::process::Identity>,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.protocol_version != 1
            || self.id.is_empty()
            || self.id.trim() != self.id
            || self.id.encode_utf16().count() > 256
            || self.id.chars().any(char::is_control)
            || !["install", "upgrade", "repair", "status", "recover"]
                .contains(&self.command.as_str())
            || self.approved_by.trim().is_empty()
            || self.approved_by.len() > 256
            || self.approved_by.chars().any(char::is_control)
            || (self.version.is_some() && self.channel.is_some())
        {
            return Err(invalid("invalid installer request"));
        }
        if let Some(caller) = &self.waiting_caller
            && (caller.pid <= 1
                || caller.created.is_empty()
                || caller.created.len() > 256
                || !caller.executable.is_absolute())
        {
            return Err(invalid("invalid waiting caller identity"));
        }
        if let Some(version) = &self.version {
            version::exact(version)?;
        }
        if let Some(channel) = &self.channel {
            source::parse_channel(channel)?;
        }
        if ["status", "recover"].contains(&self.command.as_str())
            && (self.version.is_some() || self.channel.is_some())
        {
            return Err(invalid("status and recover do not select a release"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reply {
    pub protocol_version: u32,
    pub id: String,
    pub exit_code: u8,
    pub line: String,
    pub receipt: Option<Receipt>,
    pub world: Option<World>,
}

impl Reply {
    pub fn plain(id: &str, exit_code: u8, line: impl Into<String>) -> Self {
        Self {
            protocol_version: 1,
            id: id.into(),
            exit_code,
            line: line.into(),
            receipt: None,
            world: None,
        }
    }
    pub fn receipt(receipt: Receipt) -> Self {
        Self {
            protocol_version: 1,
            id: receipt.id.clone(),
            exit_code: receipt.exit_code,
            line: receipt.line.clone(),
            receipt: Some(receipt),
            world: None,
        }
    }
    fn replay_line(receipt: &Receipt) -> String {
        let result = match receipt.outcome {
            Outcome::Installed => "installed",
            Outcome::Promoted => "upgraded",
            Outcome::Repaired => "reinstalled",
            Outcome::UpToDate => "already up to date",
            Outcome::Failed => "failed",
            Outcome::RolledBack => "rolled back",
            Outcome::Held => "held",
            Outcome::Unresolved => "unresolved",
        };
        format!(
            "Previous operation: {result}. Run raft-computer-installer status for the current installation and service state."
        )
    }
    pub fn replay(receipt: Receipt) -> Self {
        let line = Self::replay_line(&receipt);
        Self {
            line,
            ..Self::receipt(receipt)
        }
    }
    pub fn validate(&self, expected: &str) -> Result<()> {
        if self.protocol_version != 1
            || self.id != expected
            || self.exit_code > 3
            || self.line.trim().is_empty()
            || self.line.len() > 2048
            || self.line.chars().any(char::is_control)
        {
            return Err(invalid("invalid installer response"));
        }
        if let Some(receipt) = &self.receipt {
            receipt.validate()?;
            if receipt.id != self.id
                || receipt.exit_code != self.exit_code
                || (receipt.line != self.line && Self::replay_line(receipt) != self.line)
            {
                return Err(invalid("installer response receipt mismatch"));
            }
        }
        Ok(())
    }
}
