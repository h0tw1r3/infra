use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};

pub struct Verify;

impl PhaseExecutor for Verify {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let mut actions = Vec::new();

        if ctx.restart_required() {
            host.restart_nomad()?;
            ctx.clear_restart_required();
            actions.push("restarted nomad service".to_string());
        }

        let version_output = host.nomad_version_output()?;
        if config.version != "latest" && !version_output.contains(&config.version) {
            anyhow::bail!(
                "installed nomad version does not match requested version {}: {}",
                config.version,
                version_output.trim()
            );
        }
        actions.push("verified nomad version".to_string());

        Ok(PhaseResult {
            phase_name: self.name().to_string(),
            changes_made: actions.iter().any(|entry| entry.contains("restarted")),
            message: actions.join(", "),
        })
    }

    fn name(&self) -> &'static str {
        "verify"
    }
}
