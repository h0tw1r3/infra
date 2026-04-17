/// Setup HashiCorp repository
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};
use crate::runner::CommandRunner;
use crate::system;
use anyhow::Result;
use log::info;
use std::fs;
use std::path::Path;

pub struct SetupRepo;

impl PhaseExecutor for SetupRepo {
    fn execute(
        &self,
        runner: &CommandRunner,
        _config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let codename = system::get_codename()?;
        let desired_repo = system::desired_repo_contents(&codename);
        let mut changed = false;

        if runner.is_dry_run() {
            return Ok(PhaseResult::changed(
                self.name(),
                format!("would configure HashiCorp repository for {}", codename),
            ));
        }

        if !system::file_exists(system::HASHICORP_KEYRING_PATH) {
            if let Some(parent) = Path::new(system::HASHICORP_KEYRING_PATH).parent() {
                fs::create_dir_all(parent)?;
            }

            runner.run(
                "sh",
                &[
                    "-c",
                    "curl -fsSL https://apt.releases.hashicorp.com/gpg | gpg --dearmor -o /usr/share/keyrings/hashicorp-archive-keyring.gpg",
                ],
            )?;
            changed = true;
        }

        let existing_repo = system::read_file_if_exists(system::HASHICORP_SOURCE_LIST_PATH)?;
        let repo_matches = existing_repo
            .as_deref()
            .map(system::normalize_config)
            .map(|content| content == system::normalize_config(&desired_repo))
            .unwrap_or(false);

        if !repo_matches {
            if let Some(parent) = Path::new(system::HASHICORP_SOURCE_LIST_PATH).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(system::HASHICORP_SOURCE_LIST_PATH, desired_repo)?;
            changed = true;
        }

        if changed {
            info!("HashiCorp apt repository updated for codename {}", codename);
            runner.run("apt-get", &["update", "-qq"])?;
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
