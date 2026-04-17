use anyhow::Result;
use log::info;

use crate::models::{ExecutionContext, NodeConfig, PhaseResult};
use crate::modules::configure::Configure;
use crate::modules::ensure_deps::EnsureDeps;
use crate::modules::install::Install;
use crate::modules::setup_repo::SetupRepo;
use crate::modules::verify::Verify;
use crate::runner::CommandRunner;

pub const PHASE_NAMES: [&str; 5] = [
    "ensure-deps",
    "setup-repo",
    "install",
    "configure",
    "verify",
];

/// Trait for phase executors
pub trait PhaseExecutor: Send + Sync {
    fn execute(
        &self,
        runner: &CommandRunner,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult>;
    fn name(&self) -> &'static str;
}

/// Manages phase execution order and dependencies
pub struct DependencyGraph {
    phases: Vec<Box<dyn PhaseExecutor>>,
}

impl DependencyGraph {
    pub fn new() -> Result<Self> {
        let phases: Vec<Box<dyn PhaseExecutor>> = vec![
            Box::new(EnsureDeps),
            Box::new(SetupRepo),
            Box::new(Install),
            Box::new(Configure),
            Box::new(Verify),
        ];

        Ok(DependencyGraph { phases })
    }

    /// Filter phases based on --phase or --up-to flags
    pub fn filter_phases(
        &self,
        phase: &Option<String>,
        up_to: &Option<String>,
    ) -> Result<Vec<&dyn PhaseExecutor>> {
        if phase.is_some() && up_to.is_some() {
            anyhow::bail!("--phase and --up-to cannot be used together");
        }

        if let Some(p) = phase {
            let found = self
                .phases
                .iter()
                .find(|ph| ph.name() == p)
                .ok_or_else(|| anyhow::anyhow!("Unknown phase: {}", p))?;
            return Ok(vec![found.as_ref()]);
        }

        if let Some(p) = up_to {
            let index = self
                .phases
                .iter()
                .position(|ph| ph.name() == p)
                .ok_or_else(|| anyhow::anyhow!("Unknown phase: {}", p))?;
            return Ok(self.phases[..=index].iter().map(Box::as_ref).collect());
        }

        Ok(self.phases.iter().map(Box::as_ref).collect())
    }

    /// Execute phases in dependency order
    pub fn execute_all(
        &self,
        runner: &CommandRunner,
        config: &NodeConfig,
        phases: Vec<&dyn PhaseExecutor>,
    ) -> Result<()> {
        let mut ctx = ExecutionContext::default();

        // Load existing provisioned state
        if let Ok(existing_state) = crate::state::ProvisionedState::load() {
            ctx.state = existing_state;
        }

        for phase in phases {
            info!("Starting phase: {}", phase.name());
            let result = phase.execute(runner, config, &mut ctx)?;
            info!(
                "Completed phase: {} (changed: {}) - {}",
                result.phase_name, result.changes_made, result.message
            );
        }

        // Save updated state after all phases complete
        if !runner.is_dry_run() {
            ctx.state.save()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dependency_graph_creation() {
        let graph = DependencyGraph::new();
        assert!(graph.is_ok());
    }

    #[test]
    fn test_phase_filtering_all() {
        let graph = DependencyGraph::new().expect("Failed to create graph");
        let phases = graph.filter_phases(&None, &None);
        assert!(phases.is_ok());
        let phases = phases.unwrap();
        assert_eq!(phases.len(), 5); // 5 phases total
    }

    #[test]
    fn test_phase_filtering_single() {
        let graph = DependencyGraph::new().expect("Failed to create graph");
        let phases = graph.filter_phases(&Some("ensure-deps".to_string()), &None);
        assert!(phases.is_ok());
        let phases = phases.unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name(), "ensure-deps");
    }

    #[test]
    fn test_phase_filtering_up_to() {
        let graph = DependencyGraph::new().expect("Failed to create graph");
        let phases = graph.filter_phases(&None, &Some("install".to_string()));
        assert!(phases.is_ok());
        let phases = phases.unwrap();
        assert_eq!(phases.len(), 3); // ensure-deps, setup-repo, install
    }

    #[test]
    fn test_invalid_phase() {
        let graph = DependencyGraph::new().expect("Failed to create graph");
        let phases = graph.filter_phases(&Some("invalid-phase".to_string()), &None);
        assert!(phases.is_err());
    }

    #[test]
    fn test_phase_and_up_to_are_mutually_exclusive() {
        let graph = DependencyGraph::new().expect("Failed to create graph");
        let phases = graph.filter_phases(
            &Some("ensure-deps".to_string()),
            &Some("verify".to_string()),
        );
        assert!(phases.is_err());
        let err = match phases {
            Err(err) => err.to_string(),
            Ok(_) => String::new(),
        };
        assert!(err.contains("cannot be used together"));
    }
}
