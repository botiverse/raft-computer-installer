//! Private, bounded diagnostics for failures that have no durable operation
//! receipt. Human output must never depend on or render these records.
use crate::{Error, Result, computer, config::Config};
use k_carrier::storage::{ensure_dir, now_ms, sync_dir, write_json};
use serde::{Deserialize, Serialize};
use std::fs;

const CLI_DIAGNOSTIC_LIMIT: usize = 16;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CliDiagnostic {
    format_version: u32,
    kind: String,
    diagnostic: String,
    truncated: bool,
    recorded_at_ms: u64,
}

pub fn safe_error(error: &Error) -> String {
    computer::diagnostic_stderr(error.to_string().as_bytes()).0
}

/// Preserve a last-resort launcher failure when configuration is usable. A
/// configuration error may make the state root unknowable; that case remains
/// fail-closed instead of guessing another user's or profile's directory.
pub fn preserve_top_level_failure(error: &Error) {
    let Ok(cfg) = Config::load() else {
        return;
    };
    let _ = write_cli_diagnostic(&cfg, error);
}

fn write_cli_diagnostic(cfg: &Config, error: &Error) -> Result<()> {
    let directory = cfg.installer_dir.join("diagnostics");
    ensure_dir(&directory)?;
    let raw = error.to_string();
    let (diagnostic, truncated) = computer::diagnostic_stderr(raw.as_bytes());
    let name = format!("cli-{}.json", uuid::Uuid::new_v4());
    write_json(
        &directory.join(&name),
        &CliDiagnostic {
            format_version: 1,
            kind: "top-level-error".into(),
            diagnostic,
            truncated,
            recorded_at_ms: now_ms(),
        },
    )?;
    prune_cli_diagnostics(&directory)
}

fn prune_cli_diagnostics(directory: &std::path::Path) -> Result<()> {
    let mut diagnostics = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix("cli-")
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if !entry.file_type()?.is_file() || uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        if bytes.len() > 65536 {
            continue;
        }
        let Ok(diagnostic) = serde_json::from_slice::<CliDiagnostic>(&bytes) else {
            continue;
        };
        diagnostics.push((diagnostic.recorded_at_ms, entry.path()));
    }
    diagnostics.sort_by_key(|(recorded_at_ms, _)| *recorded_at_ms);
    let excess = diagnostics.len().saturating_sub(CLI_DIAGNOSTIC_LIMIT);
    for (_, path) in diagnostics.into_iter().take(excess) {
        fs::remove_file(path)?;
    }
    if excess > 0 {
        sync_dir(directory)?;
    }
    Ok(())
}
