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

pub fn create(
    cfg: Config,
    source: Arc<dyn ReleaseSource>,
    allow_downgrade: bool,
) -> Result<Runner> {
    let controller = controller(&cfg)?;
    let mut runner = Runner::new(FileStore::new(&cfg.k_state), Arc::new(controller), source)?;
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
        .map_err(|_| invalid("installer executable path is not Unicode"))?;
    CommandHost::new(
        vec![executable, "--host-controller".into()],
        &cfg.k_state,
        120_000,
    )
}

pub async fn fence(cfg: &Config) -> Result<()> {
    controller(cfg)?.fence().await
}
