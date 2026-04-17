use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};

pub struct EnsureDeps;

impl PhaseExecutor for EnsureDeps {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        _config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let required_packages = ["curl", "gnupg", "ca-certificates"];
        let mut missing_packages = Vec::new();
        for package in required_packages {
            if !host.check_pkg(package)? {
                missing_packages.push(package.to_string());
            }
        }

        if missing_packages.is_empty() {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "all required packages are already installed",
            ));
        }

        host.apt_update()?;
        host.apt_install(&missing_packages)?;

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
