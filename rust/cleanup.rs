//! Delete installer-owned recovery payloads only after a durable terminal
//! receipt and a settled K state. Receipts remain small replay records; they
//! are not a reason to retain old executable or sidecar copies forever.
use crate::{
    Error, Result,
    config::Config,
    report::{self, Outcome},
};
use k_carrier::{
    error::invalid,
    lock::UpgradeLock,
    state::OperationRead,
    storage::{FileStore, exists, remove_dir, sync_dir, write_json},
};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pending {
    format_version: u32,
    receipt_id: String,
}

pub fn request(cfg: &Config, id: &str) -> Result<()> {
    write_json(
        &cfg.installer_dir.join("cleanup.json"),
        &Pending {
            format_version: 1,
            receipt_id: id.into(),
        },
    )
}

pub async fn run(cfg: &Config) -> Result<()> {
    let pending = cfg.installer_dir.join("cleanup.json");
    if !exists(&pending)? {
        return Ok(());
    }
    let _gate = match UpgradeLock::acquire(&cfg.installer_dir.join("gate")) {
        Ok(gate) => gate,
        Err(Error::Locked(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    if exists(&cfg.installer_dir.join("active.json"))?
        || exists(&cfg.installer_dir.join("metadata-damage.json"))?
    {
        return Ok(());
    }
    let saved: Pending = serde_json::from_slice(&fs::read(&pending)?)?;
    if saved.format_version != 1 {
        return Err(invalid("invalid cleanup request"));
    }
    let receipt = report::read(cfg, &saved.receipt_id)?
        .ok_or_else(|| invalid("cleanup has no terminal receipt"))?;
    if receipt.outcome == Outcome::Unresolved {
        return Ok(());
    }
    let _lock = match UpgradeLock::acquire(&cfg.k_state) {
        Ok(lock) => lock,
        Err(Error::Locked(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    let store = FileStore::new(&cfg.k_state);
    match store.read_operation() {
        OperationRead::Unreadable { .. } => return Ok(()),
        OperationRead::Observed { operation } if operation.outcome.is_none() => return Ok(()),
        _ => {}
    }
    let state = store.transaction_state().await?;
    if !state.phase().at_rest() {
        return Ok(());
    }
    let stable = state.stable();
    crate::version::exact(stable)?;
    // Paths are derived only from our owned layout, never receipt-supplied paths.
    remove_dir(&cfg.installer_dir.join("quarantine"))?;
    remove_dir(&cfg.installer_dir.join("operations"))?;
    remove_dir(&cfg.k_state.join("incoming"))?;
    let sidecars = cfg.installer_dir.join("sidecars");
    if exists(&sidecars)? {
        for entry in fs::read_dir(&sidecars)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(version) = name.to_str() else {
                continue;
            };
            if version != stable && crate::version::exact(version).is_ok() {
                remove_dir(&entry.path())?;
            }
        }
    }
    fs::remove_file(pending)?;
    sync_dir(&cfg.installer_dir)
}
