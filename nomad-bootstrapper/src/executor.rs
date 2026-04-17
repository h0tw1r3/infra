use anyhow::Result;

use crate::debian::DebianHost;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};
use crate::modules::configure::Configure;
use crate::modules::ensure_deps::EnsureDeps;
use crate::modules::install::Install;
use crate::modules::setup_repo::SetupRepo;
use crate::modules::verify::Verify;

pub const PHASE_NAMES: [&str; 5] = [
    "ensure-deps",
    "setup-repo",
    "install",
    "configure",
    "verify",
];

pub trait PhaseExecutor: Send + Sync {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult>;
    fn name(&self) -> &'static str;
}

pub struct DependencyGraph {
    phases: Vec<Box<dyn PhaseExecutor>>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self {
            phases: vec![
                Box::new(EnsureDeps),
                Box::new(SetupRepo),
                Box::new(Install),
                Box::new(Configure),
                Box::new(Verify),
            ],
        }
    }

    pub fn filter_phases(
        &self,
        phase: &Option<String>,
        up_to: &Option<String>,
    ) -> Result<Vec<&dyn PhaseExecutor>> {
        if phase.is_some() && up_to.is_some() {
            anyhow::bail!("--phase and --up-to cannot be used together");
        }

        if let Some(phase_name) = phase {
            let phase = self
                .phases
                .iter()
                .find(|candidate| candidate.name() == phase_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown phase: {}", phase_name))?;
            return Ok(vec![phase.as_ref()]);
        }

        if let Some(phase_name) = up_to {
            let index = self
                .phases
                .iter()
                .position(|candidate| candidate.name() == phase_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown phase: {}", phase_name))?;
            return Ok(self.phases[..=index].iter().map(Box::as_ref).collect());
        }

        Ok(self.phases.iter().map(Box::as_ref).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dependency_graph_creation() {
        let graph = DependencyGraph::new();
        assert_eq!(graph.phases.len(), 5);
    }

    #[test]
    fn test_phase_filtering_all() {
        let graph = DependencyGraph::new();
        let phases = graph.filter_phases(&None, &None).expect("all phases");
        assert_eq!(phases.len(), 5);
    }

    #[test]
    fn test_phase_filtering_single() {
        let graph = DependencyGraph::new();
        let phases = graph
            .filter_phases(&Some("ensure-deps".to_string()), &None)
            .expect("single phase");
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name(), "ensure-deps");
    }

    #[test]
    fn test_phase_filtering_up_to() {
        let graph = DependencyGraph::new();
        let phases = graph
            .filter_phases(&None, &Some("install".to_string()))
            .expect("up to install");
        assert_eq!(phases.len(), 3);
    }

    #[test]
    fn test_invalid_phase() {
        let graph = DependencyGraph::new();
        let phases = graph.filter_phases(&Some("invalid-phase".to_string()), &None);
        assert!(phases.is_err());
    }

    #[test]
    fn test_phase_and_up_to_are_mutually_exclusive() {
        let graph = DependencyGraph::new();
        let phases = graph.filter_phases(
            &Some("ensure-deps".to_string()),
            &Some("verify".to_string()),
        );
        assert!(phases.is_err());
        let err = match phases {
            Ok(_) => String::new(),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("cannot be used together"));
    }
}
