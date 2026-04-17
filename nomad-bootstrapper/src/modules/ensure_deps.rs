/// Ensure required system dependencies are installed
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};
use crate::runner::CommandRunner;
use crate::system;
use anyhow::Result;
use log::info;

pub struct EnsureDeps;

impl PhaseExecutor for EnsureDeps {
    fn execute(
        &self,
        runner: &CommandRunner,
        _config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let required_packages = ["curl", "gnupg", "ca-certificates"];
        let missing_packages = required_packages
            .into_iter()
            .filter(|pkg| !system::check_pkg(pkg))
            .collect::<Vec<_>>();

        if missing_packages.is_empty() {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "all required packages are already installed",
            ));
        }

        info!(
            "Installing missing packages: {}",
            missing_packages.join(", ")
        );
        runner.run("apt-get", &["update", "-qq"])?;

        let mut install_args = vec!["install", "-y", "-qq"];
        install_args.extend(missing_packages.iter().copied());
        runner.run("apt-get", &install_args)?;

        Ok(PhaseResult::changed(
            self.name(),
            format!(
                "installed missing packages: {}",
                missing_packages.join(", ")
            ),
        ))
    }

    fn name(&self) -> &'static str {
        "ensure-deps"
    }
}
