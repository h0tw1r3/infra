use anyhow::Result;

use crate::debian::{desired_repo_contents, normalize_config, DebianHost};
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};

pub struct SetupRepo;

impl PhaseExecutor for SetupRepo {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        _config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let codename = host.get_codename()?;
        let desired_repo = desired_repo_contents(&codename);
        let mut changed = false;

        if !host.keyring_exists()? {
            host.fetch_hashicorp_keyring()?;
            changed = true;
        }

        let existing_repo = host.read_repo_file()?;
        let repo_matches = existing_repo
            .as_deref()
            .map(normalize_config)
            .map(|current| current == normalize_config(&desired_repo))
            .unwrap_or(false);

        if !repo_matches {
            host.write_repo_file(&desired_repo)?;
            changed = true;
        }

        if changed {
            host.apt_update()?;
            return Ok(PhaseResult::changed(
                self.name(),
                format!("configured HashiCorp repository for {}", codename),
            ));
        }

        Ok(PhaseResult::unchanged(
            self.name(),
            format!("HashiCorp repository already configured for {}", codename),
        ))
    }

    fn name(&self) -> &'static str {
        "setup-repo"
    }
}
