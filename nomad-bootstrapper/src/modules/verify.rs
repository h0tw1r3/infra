/// Verify Nomad installation
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};
use crate::runner::CommandRunner;
use anyhow::Result;

pub struct Verify;

impl PhaseExecutor for Verify {
    fn execute(
        &self,
        runner: &CommandRunner,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let mut actions = Vec::new();

        if ctx.restart_required() {
            runner.run("systemctl", &["restart", "nomad"])?;
            ctx.clear_restart_required();
            actions.push("restarted nomad service".to_string());
        }

        let version_output = runner.run_output("nomad", &["version"])?;
        if !version_output.is_empty()
            && config.version != "latest"
            && !version_output.contains(&config.version)
        {
            anyhow::bail!(
                "installed nomad version does not match requested version {}: {}",
                config.version,
                version_output
            );
        }

        actions.push("verified nomad version".to_string());

        Ok(PhaseResult {
            phase_name: self.name().to_string(),
            changes_made: actions.iter().any(|action| action.contains("restarted")),
            message: actions.join(", "),
        })
    }

    fn name(&self) -> &'static str {
        "verify"
    }
}
