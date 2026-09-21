//! Classification is read after recovery, under the product operation gate.
//! A usable stable slot alone does not prove that the installed file is intact.
use crate::{Result, artifact, computer, config::Config, version};
use k_carrier::{
    artifact::sha256,
    state::{OperationRead, Slot},
    storage::FileStore,
};
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum World {
    Fresh,
    Adopted { version: String },
    Managed { version: String },
    Upgrading { id: String },
    Broken { reason: String },
    Held { reason: String },
}

impl World {
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Adopted { version } | Self::Managed { version } => Some(version),
            _ => None,
        }
    }
}

fn foreign_manager(binary: &Path) -> Result<Option<&'static str>> {
    let resolved = fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    let path = resolved.to_string_lossy().replace('\\', "/");
    if path.contains("/Cellar/") || path.starts_with("/opt/homebrew/") {
        return Ok(Some("Homebrew"));
    }
    if path.contains("/node_modules/") {
        return Ok(Some("npm"));
    }
    if path.contains("/.bun/") {
        return Ok(Some("bun"));
    }
    let mut head = [0u8; 256];
    let read = match fs::File::open(binary) {
        Ok(mut file) => file.read(&mut head)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if head[..read].starts_with(b"#!") {
        let first = String::from_utf8_lossy(&head[..read]);
        if first.lines().next().is_some_and(|line| {
            line.split_whitespace()
                .any(|token| token.ends_with("node") || token.ends_with("bun"))
        }) {
            return Ok(Some("a script package manager"));
        }
    }
    Ok(None)
}

fn exists(path: &Path) -> Result<bool> {
    k_carrier::storage::exists(path)
}

fn empty_genesis(cfg: &Config) -> Result<bool> {
    if !exists(&cfg.k_state)? {
        return Ok(true);
    }
    for entry in fs::read_dir(&cfg.k_state)? {
        let entry = entry?;
        let name = entry.file_name();
        match name.to_str() {
            Some("upgrade.lock") => {} // Lock ownership is checked before classification.
            Some("upgrade.lock.claims" | "controllers") if entry.file_type()?.is_dir() => {
                if fs::read_dir(entry.path())?.next().is_some() {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

pub async fn read(cfg: &Config) -> Result<World> {
    // Foreign ownership wins even when old or corrupt K records also exist.
    if let Some(manager) = foreign_manager(&cfg.binary)? {
        return Ok(World::Held {
            reason: format!(
                "{} belongs to {manager}; remove it with that manager before retrying",
                cfg.binary.display()
            ),
        });
    }
    if fs::symlink_metadata(&cfg.binary).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(World::Held {
            reason: format!(
                "{} is a link owned by another manager; remove it with that manager before retrying",
                cfg.binary.display()
            ),
        });
    }
    if fs::symlink_metadata(&cfg.sidecar).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(World::Held {
            reason: format!(
                "{} is a link owned by another manager; remove it with that manager before retrying",
                cfg.sidecar.display()
            ),
        });
    }
    if exists(&cfg.installer_dir.join("metadata-damage.json"))? {
        return Ok(World::Broken {
            reason: "installation records require repair".into(),
        });
    }
    let store = FileStore::new(&cfg.k_state);
    let operation = store.read_operation();
    match &operation {
        OperationRead::Unreadable { .. } => {
            return Ok(World::Broken {
                reason: "installation records are unreadable".into(),
            });
        }
        OperationRead::Observed { operation } if operation.outcome.is_none() => {
            return Ok(World::Upgrading {
                id: operation.id.clone(),
            });
        }
        _ => {}
    }
    let stable = match store.version(Slot::Stable) {
        Ok(stable) => stable,
        Err(_) => {
            return Ok(World::Broken {
                reason: "stable installation is incomplete".into(),
            });
        }
    };
    match stable {
        Some(stable) => {
            if version::exact(&stable).is_err() {
                return Ok(World::Broken {
                    reason: "stable version is invalid".into(),
                });
            }
            let state = match store.transaction_state().await {
                Ok(state) => state,
                Err(_) => {
                    return Ok(World::Broken {
                        reason: "transaction state is unreadable".into(),
                    });
                }
            };
            if !state.phase().at_rest() {
                return Ok(World::Broken {
                    reason: "transaction journal has unfinished work without an active receipt"
                        .into(),
                });
            }
            let installed = match fs::read(&cfg.binary) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Ok(World::Broken {
                        reason: "installed executable is missing or unreadable".into(),
                    });
                }
            };
            if artifact::check_platform(&installed).is_err()
                || !fs::read(store.artifact(Slot::Stable))
                    .is_ok_and(|bytes| sha256(&installed) == sha256(&bytes))
            {
                return Ok(World::Broken {
                    reason: "installed executable differs from the stable slot".into(),
                });
            }
            if artifact::check_installed_sidecar(cfg, &stable).is_err() {
                return Ok(World::Broken {
                    reason: "installed sidecar differs from its saved identity".into(),
                });
            }
            match computer::self_report(&cfg.binary, cfg).await {
                Ok(evidence) if evidence.version == stable => {
                    Ok(World::Managed { version: stable })
                }
                Err(error) if error.is_uncertain() => Err(error),
                _ => Ok(World::Broken {
                    reason: "installed executable does not report the stable version".into(),
                }),
            }
        }
        None => {
            if !matches!(operation, OperationRead::Genesis) || !empty_genesis(cfg)? {
                return Ok(World::Broken {
                    reason: "installation records have no usable stable slot".into(),
                });
            }
            if !exists(&cfg.binary)? {
                return Ok(World::Fresh);
            }
            let bytes = fs::read(&cfg.binary)?;
            if artifact::check_platform(&bytes).is_err() {
                return Ok(World::Broken {
                    reason: "installed program is not a compatible native executable".into(),
                });
            }
            match computer::self_report(&cfg.binary, cfg).await {
                Ok(evidence)
                    if artifact::check_installed_sidecar(cfg, &evidence.version).is_ok() =>
                {
                    Ok(World::Adopted {
                        version: evidence.version,
                    })
                }
                Ok(_) => Ok(World::Broken {
                    reason: "installed sidecar is incomplete".into(),
                }),
                Err(error) if error.is_uncertain() => Err(error),
                Err(_) => Ok(World::Broken {
                    reason: "installed program does not answer".into(),
                }),
            }
        }
    }
}
