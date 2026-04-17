/// Install Nomad
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};
use crate::runner::CommandRunner;
use crate::system;
use anyhow::Result;
use log::info;

pub struct Install;

impl PhaseExecutor for Install {
    fn execute(
        &self,
        runner: &CommandRunner,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let is_latest = config.version == "latest";

        // For exact versions, check if already satisfied.
        // For "latest", always proceed through APT (let the package manager
        // handle idempotency and upgrade detection).
        if !is_latest && system::nomad_version_satisfies(&config.version) {
            ctx.state.update_provision(&config.version);
            return Ok(PhaseResult::unchanged(
                self.name(),
                format!("nomad {} is already installed", config.version),
            ));
        }

        let package_spec = if is_latest {
            "nomad".to_string()
        } else {
            format!("nomad={}", config.version)
        };

        if runner.is_dry_run() {
            let msg = if is_latest {
                "would run apt-get install nomad (latest available)".to_string()
            } else {
                format!("would install {}", package_spec)
            };
            return Ok(PhaseResult::changed(self.name(), msg));
        }

        info!("Installing package {}", package_spec);
        runner.run("apt-get", &["install", "-y", &package_spec])?;

        ctx.state.update_provision(&config.version);

        Ok(PhaseResult::changed(
            self.name(),
            format!("installed {}", package_spec),
        ))
    }

    fn name(&self) -> &'static str {
        "install"
    }
}
