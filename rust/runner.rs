use crate::{Result, config::Config, host, report::PhaseLog, version};
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
    phases: PhaseLog,
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
        host::prepare(&self.cfg, artifact, release, &self.phases).await
    }
    /// K's stages, as phases: `download` until the candidate is verified;
    /// `handover` from the handover (stop, replace, start, read back) until
    /// K reports its end, or the caller drains the log when K returns.
    fn progress(&self, stage: &str, _version: Option<&str>, _bytes: Option<(u64, u64)>) {
        match stage {
            "downloading" => self.phases.begin("download"),
            "verifying" => self.phases.end("download"),
            "handing-over" => self.phases.begin("handover"),
            "promoted" | "rolled-back" => self.phases.end("handover"),
            _ => {}
        }
    }
}

/// K bounds `prepare_candidate` (and each host call) by the runner's host
/// budget, 120 s by default. Preparation runs the candidate's first check
/// (computer::FIRST_RUN_BUDGET, 120 s, plus a bounded wait for a killed
/// process), copies the ~150 MB candidate beside its sidecar and captures the
/// service state (`status --json`, up to 30 s). With 120 s, K's deadline
/// would end the check first and report only CANDIDATE_PREPARATION_TIMEOUT.
/// K's host calls keep their own 120 s limit (CommandHost), so for them this
/// outer bound changes nothing.
pub const UPGRADE_HOST_BUDGET_MS: u64 = 300_000;

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
    phases: PhaseLog,
) -> Result<Runner> {
    let controller = controller(&cfg)?;
    let mut runner = Runner::new(FileStore::new(&cfg.k_state), Arc::new(controller), source)?;
    if let Some(release) = release {
        runner.downloader = crate::artifact::release_downloader(release)?;
        runner.host_budget_ms = UPGRADE_HOST_BUDGET_MS;
    }
    runner.policy = Policy::Confirm;
    runner.hooks = Arc::new(ProductHooks {
        cfg,
        allow_downgrade,
        phases,
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
            PhaseLog::default(),
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
            PhaseLog::default(),
        )
        .unwrap();
        assert_eq!(
            recovery.downloader.policy.budgets(size).unwrap().overall_ms,
            TransferPolicy::default().budgets(size).unwrap().overall_ms
        );
        assert_eq!(recovery.host_budget_ms, 120_000);
    }

    /// K's deadline around candidate preparation must outlast the
    /// candidate's own first-run allowance, its kill wait and the service
    /// state capture after it, or K reports CANDIDATE_PREPARATION_TIMEOUT
    /// instead of the check's own result. The first worker's deadline in
    /// turn leaves room for the preparation and the transfer around it.
    #[test]
    fn an_upgrade_runner_outlasts_the_candidate_check() {
        let (_dir, cfg) = cfg();
        let candidate = release(1024);
        let runner = create(
            cfg,
            Arc::new(crate::source::FrozenSource(crate::source::Manifest {
                version: candidate.version.clone(),
                release: candidate.clone(),
                sidecar: None,
            })),
            false,
            Some(&candidate),
            PhaseLog::default(),
        )
        .unwrap();
        assert_eq!(runner.host_budget_ms, UPGRADE_HOST_BUDGET_MS);
        let check = crate::computer::FIRST_RUN_BUDGET + std::time::Duration::from_secs(5);
        let status = std::time::Duration::from_secs(30 + 5);
        assert!(
            std::time::Duration::from_millis(UPGRADE_HOST_BUDGET_MS) >= check + status * 2,
            "{UPGRADE_HOST_BUDGET_MS}"
        );
        assert!(
            crate::supervisor::FIRST_ATTEMPT_BUDGET
                >= std::time::Duration::from_millis(UPGRADE_HOST_BUDGET_MS) * 3,
        );
    }

    #[test]
    fn k_stages_are_recorded_as_phases() {
        let (_dir, cfg) = cfg();
        let phases = PhaseLog::default();
        let hooks = ProductHooks {
            cfg,
            allow_downgrade: false,
            phases: phases.clone(),
        };
        for stage in [
            "checking",
            "downloading",
            "verifying",
            "staging",
            "handing-over",
            "promoted",
        ] {
            hooks.progress(stage, Some("1.0.64"), None);
        }
        let mut detail = std::collections::BTreeMap::new();
        phases.drain_into(&mut detail);
        let recorded: std::collections::BTreeMap<String, u64> =
            serde_json::from_str(&detail[crate::report::PHASES_KEY]).unwrap();
        assert_eq!(
            recorded.keys().map(String::as_str).collect::<Vec<_>>(),
            ["download", "handover"]
        );
    }
}
