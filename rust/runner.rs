use crate::{Result, config::Config, host, version};
use async_trait::async_trait;
use k_carrier::{
    artifact::{Release, ReleaseSource},
    error::invalid,
    host::{CommandHost, Host},
    runner::{Hooks, Policy, Runner},
    storage::FileStore,
};
use std::{path::Path, sync::Arc};

struct ProductHooks {
    cfg: Config,
    allow_downgrade: bool,
}

#[async_trait]
impl Hooks for ProductHooks {
    async fn compatibility(&self, from: &str, to: &str) -> Result<Option<String>> {
        if !self.allow_downgrade && version::compare(to, from)?.is_lt() {
            return Ok(Some("downgrade requires --allow-downgrade".into()));
        }
        Ok(None)
    }
    async fn prepare_candidate(&self, artifact: &Path, release: &Release) -> Result<()> {
        host::prepare(&self.cfg, artifact, release).await
    }
}

/// `release` is the one this runner may download (an upgrade's frozen
/// selection). Its candidate is then fetched with the installer's downloader
/// (its roots, proxy settings and minimum attempt budget) instead of K's
/// default one; K still makes a single attempt. A recovery runner downloads
/// nothing and keeps K's default.
pub fn create(
    cfg: Config,
    source: Arc<dyn ReleaseSource>,
    allow_downgrade: bool,
    release: Option<&Release>,
) -> Result<Runner> {
    let controller = controller(&cfg)?;
    let mut runner = Runner::new(FileStore::new(&cfg.k_state), Arc::new(controller), source)?;
    if let Some(release) = release {
        runner.downloader = crate::artifact::release_downloader(release)?;
    }
    runner.policy = Policy::Confirm;
    runner.hooks = Arc::new(ProductHooks {
        cfg,
        allow_downgrade,
    });
    Ok(runner)
}

// Reuse K's lifetime fence before changing metadata that surviving controllers
// can still write. The installer gate alone does not mean those writers exited.
fn controller(cfg: &Config) -> Result<CommandHost> {
    let executable = std::env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| invalid("executable path is not Unicode"))?;
    CommandHost::new(
        vec![executable, "--host-controller".into()],
        &cfg.k_state,
        120_000,
    )
}

pub async fn fence(cfg: &Config) -> Result<()> {
    controller(cfg)?.fence().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use k_carrier::artifact::TransferPolicy;

    fn release(size: u64) -> Release {
        Release {
            version: "1.0.64".into(),
            url: "https://hands.example/raft-computer".into(),
            sha256: "0".repeat(64),
            size,
            gzip: None,
        }
    }

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.state_home = dir.path().to_path_buf();
        cfg.k_state = dir.path().join("computer").join("k");
        cfg.installer_dir = dir.path().join("computer").join("installer");
        (dir, cfg)
    }

    /// #proj-raft-computer:#49: an upgrade's candidate was downloaded with K's
    /// `Downloader::new()`: none of the installer's roots, proxy settings or
    /// minimum attempt budget.
    #[test]
    fn an_upgrade_runner_downloads_with_the_installers_downloader() {
        let (_dir, cfg) = cfg();
        let size = 1024 * 1024;
        let candidate = release(size);
        let runner = create(
            cfg.clone(),
            Arc::new(crate::source::FrozenSource(crate::source::Manifest {
                version: candidate.version.clone(),
                release: candidate.clone(),
                sidecar: None,
            })),
            false,
            Some(&candidate),
        )
        .unwrap();
        let expected = crate::artifact::release_downloader(&candidate)
            .unwrap()
            .policy
            .budgets(size)
            .unwrap();
        let budgets = runner.downloader.policy.budgets(size).unwrap();
        assert_eq!(budgets.overall_ms, expected.overall_ms);
        assert!(budgets.overall_ms >= 120_000, "{budgets:?}");
        // K's default would give this transfer well under a minute.
        assert!(TransferPolicy::default().budgets(size).unwrap().overall_ms < 60_000);
        // A recovery runner downloads nothing and keeps K's default.
        let recovery = create(
            cfg,
            Arc::new(crate::source::FrozenSource(crate::source::Manifest {
                version: candidate.version.clone(),
                release: candidate.clone(),
                sidecar: None,
            })),
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            recovery.downloader.policy.budgets(size).unwrap().overall_ms,
            TransferPolicy::default().budgets(size).unwrap().overall_ms
        );
    }
}
