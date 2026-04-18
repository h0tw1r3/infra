mod preflight;
mod provisioning;

use anyhow::Result;
use log::info;

use crate::config::{Args, Inventory};
use crate::executor::{DependencyGraph, PhaseExecutor};
use crate::models::ResolvedNode;
use crate::transport::SshTransport;

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostStatus {
    PreflightPassed,
    PreflightFailed(String),
    ProvisioningSucceeded,
    ProvisioningFailed { phase: String, message: String },
    GateInvalidated(String),
    SkippedAfterAbort { after_phase: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RunAbortReason {
    PreflightFailure,
    GateInvalidation {
        host: String,
        message: String,
    },
    ProvisioningFailure {
        host: String,
        phase: String,
        message: String,
    },
}

pub fn run(args: &Args) -> Result<()> {
    let inventory = Inventory::load(&args.inventory)?;
    let nodes = inventory.resolve_nodes()?;
    let execution = inventory.resolve_execution(nodes.len())?;
    let executor = DependencyGraph::new();
    let phases = executor.filter_phases(&args.phase, &args.up_to)?;
    let transport = SshTransport::new(args.dry_run)?;

    info!(
        "Starting Nomad controller run for {} host(s) with {} phase(s) and concurrency limit {}",
        nodes.len(),
        phases.len(),
        execution.concurrency
    );

    let statuses = preflight::run(&nodes, &phases, &transport, execution)?;
    if args.preflight_only {
        println!("{}", render_run_summary(&nodes, &[], &statuses, None));
        info!("Nomad preflight-only run complete");
        return Ok(());
    }

    provisioning::run(&nodes, &phases, &transport, execution, statuses)?;

    info!("Nomad controller run complete");
    Ok(())
}

fn render_run_summary(
    nodes: &[ResolvedNode],
    phases: &[&dyn PhaseExecutor],
    statuses: &[HostStatus],
    abort_reason: Option<&RunAbortReason>,
) -> String {
    let mut lines = Vec::new();
    if let Some(reason) = abort_reason {
        lines.push(format!("Run aborted: {}", render_abort_reason(reason)));
    } else {
        lines.push("Run succeeded".to_string());
    }

    for (node, status) in nodes.iter().zip(statuses.iter()) {
        lines.push(format!(
            "{}: {}",
            node.target.label(),
            render_host_status(status, phases, abort_reason)
        ));
    }

    lines.join("\n")
}

fn render_abort_reason(reason: &RunAbortReason) -> String {
    match reason {
        RunAbortReason::PreflightFailure => "preflight failure".to_string(),
        RunAbortReason::GateInvalidation { host, message } => {
            format!("gate invalidation on {}: {}", host, message)
        }
        RunAbortReason::ProvisioningFailure {
            host,
            phase,
            message,
        } => format!(
            "provisioning failure on {} during {}: {}",
            host, phase, message
        ),
    }
}

fn render_host_status(
    status: &HostStatus,
    phases: &[&dyn PhaseExecutor],
    abort_reason: Option<&RunAbortReason>,
) -> String {
    let status_label = match status {
        HostStatus::PreflightPassed => "preflight_passed".to_string(),
        HostStatus::PreflightFailed(message) => format!("preflight_failed ({})", message),
        HostStatus::ProvisioningSucceeded => "provisioning_succeeded".to_string(),
        HostStatus::ProvisioningFailed { phase, message } => {
            format!("provisioning_failed [{}] ({})", phase, message)
        }
        HostStatus::GateInvalidated(message) => format!("gate_invalidated ({})", message),
        HostStatus::SkippedAfterAbort { after_phase } => {
            let label = match abort_reason {
                Some(RunAbortReason::ProvisioningFailure { .. }) => "skipped_after_peer_failure",
                Some(RunAbortReason::GateInvalidation { .. }) => "skipped_after_abort",
                _ => "skipped_after_abort",
            };
            if let Some(phase) = after_phase {
                format!("{} (after {})", label, phase)
            } else {
                label.to_string()
            }
        }
    };
    let state_path = render_state_path(status, phases, abort_reason);
    format!("{} [states: {}]", status_label, state_path)
}

fn render_state_path(
    status: &HostStatus,
    phases: &[&dyn PhaseExecutor],
    abort_reason: Option<&RunAbortReason>,
) -> String {
    let mut states = vec!["pending_preflight".to_string()];

    match status {
        HostStatus::PreflightPassed => states.push("preflight_passed".to_string()),
        HostStatus::PreflightFailed(_) => states.push("preflight_failed".to_string()),
        HostStatus::ProvisioningSucceeded => {
            states.push("preflight_passed".to_string());
            states.push("queued_for_provisioning".to_string());
            states.extend(render_running_phases(all_phase_names(phases)));
            states.push("provisioning_succeeded".to_string());
        }
        HostStatus::ProvisioningFailed { phase, .. } => {
            states.push("preflight_passed".to_string());
            states.push("queued_for_provisioning".to_string());
            states.extend(render_running_phases(phase_names_through(phases, phase)));
            states.push("provisioning_failed".to_string());
        }
        HostStatus::GateInvalidated(_) => {
            states.push("preflight_passed".to_string());
            states.push("queued_for_provisioning".to_string());
            states.push("gate_invalidated".to_string());
        }
        HostStatus::SkippedAfterAbort { after_phase } => {
            states.push("preflight_passed".to_string());
            states.push("queued_for_provisioning".to_string());
            if let Some(phase) = after_phase {
                states.extend(render_running_phases(phase_names_through(phases, phase)));
            }
            states.push(
                match abort_reason {
                    Some(RunAbortReason::ProvisioningFailure { .. }) => {
                        "skipped_after_peer_failure"
                    }
                    _ => "skipped_after_abort",
                }
                .to_string(),
            );
        }
    }

    states.join(" -> ")
}

fn all_phase_names(phases: &[&dyn PhaseExecutor]) -> Vec<String> {
    phases
        .iter()
        .map(|phase| phase.name().to_string())
        .collect()
}

fn phase_names_through(phases: &[&dyn PhaseExecutor], stop_after: &str) -> Vec<String> {
    let mut names = Vec::new();
    for phase in phases {
        names.push(phase.name().to_string());
        if phase.name() == stop_after {
            return names;
        }
    }

    names.push(stop_after.to_string());
    names
}

fn render_running_phases(phases: Vec<String>) -> Vec<String> {
    phases
        .into_iter()
        .map(|phase| format!("running_phase({})", phase))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::models::{
        AdvertiseConfig, ExecutionContext, LatencyProfile, NodeConfig, NodeRole, PhaseResult,
        ResolvedTarget, ServerConfig,
    };
    use crate::transport::{RemoteOutput, Transport};

    fn nodes() -> Vec<ResolvedNode> {
        vec![
            ResolvedNode {
                target: ResolvedTarget {
                    name: "node-1".to_string(),
                    host: "node-1.example.com".to_string(),
                    user: None,
                    identity_file: None,
                    port: None,
                    options: Vec::new(),
                    privilege_escalation: None,
                },
                config: NodeConfig {
                    name: "node-1".to_string(),
                    datacenter: "dc1".to_string(),
                    version: "latest".to_string(),
                    roles: vec![NodeRole::Server],
                    server_config: Some(ServerConfig {
                        bootstrap_expect: 1,
                        server_join_addresses: Vec::new(),
                    }),
                    client_config: None,
                    bind_addr: None,
                    advertise: AdvertiseConfig::default(),
                    latency_profile: LatencyProfile::Standard,
                },
            },
            ResolvedNode {
                target: ResolvedTarget {
                    name: "node-2".to_string(),
                    host: "node-2.example.com".to_string(),
                    user: None,
                    identity_file: None,
                    port: None,
                    options: Vec::new(),
                    privilege_escalation: None,
                },
                config: NodeConfig {
                    name: "node-2".to_string(),
                    datacenter: "dc1".to_string(),
                    version: "latest".to_string(),
                    roles: vec![NodeRole::Server],
                    server_config: Some(ServerConfig {
                        bootstrap_expect: 1,
                        server_join_addresses: Vec::new(),
                    }),
                    client_config: None,
                    bind_addr: None,
                    advertise: AdvertiseConfig::default(),
                    latency_profile: LatencyProfile::Standard,
                },
            },
        ]
    }

    #[test]
    fn test_render_run_summary_orders_hosts_deterministically() {
        struct FixedPhase(&'static str);

        impl PhaseExecutor for FixedPhase {
            fn execute(
                &self,
                _host: &crate::debian::DebianHost<'_>,
                _config: &NodeConfig,
                _ctx: &mut ExecutionContext,
            ) -> Result<PhaseResult> {
                unreachable!("summary test does not execute phases");
            }

            fn name(&self) -> &'static str {
                self.0
            }
        }

        let install = FixedPhase("install");
        let verify = FixedPhase("verify");
        let phases: Vec<&dyn PhaseExecutor> = vec![&install, &verify];
        let summary = render_run_summary(
            &nodes(),
            &phases,
            &[
                HostStatus::PreflightPassed,
                HostStatus::ProvisioningFailed {
                    phase: "install".to_string(),
                    message: "boom".to_string(),
                },
            ],
            Some(&RunAbortReason::ProvisioningFailure {
                host: "node-2".to_string(),
                phase: "install".to_string(),
                message: "boom".to_string(),
            }),
        );

        assert!(summary.contains("Run aborted: provisioning failure on node-2 during install"));
        assert!(summary
            .contains("node-1: preflight_passed [states: pending_preflight -> preflight_passed]"));
        assert!(summary.contains(
            "node-2: provisioning_failed [install] (boom) [states: pending_preflight -> preflight_passed -> queued_for_provisioning -> running_phase(install) -> provisioning_failed]"
        ));
    }

    struct IdleSessionTransport {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for IdleSessionTransport {
        fn is_dry_run(&self) -> bool {
            false
        }

        fn check_session(&self, target: &ResolvedTarget) -> Result<()> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(format!("check-session:{}", target.label()));
            Ok(())
        }

        fn exec(
            &self,
            target: &ResolvedTarget,
            command: &str,
            _input: Option<&[u8]>,
        ) -> Result<RemoteOutput> {
            self.calls.lock().expect("calls lock").push(format!(
                "exec:{}:{}",
                target.label(),
                command
            ));

            let stdout = if command == "cat /etc/os-release" {
                "ID=debian\nVERSION_CODENAME=bookworm\n".to_string()
            } else {
                String::new()
            };

            Ok(RemoteOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            })
        }
    }

    struct RemoteExecPhase;

    impl PhaseExecutor for RemoteExecPhase {
        fn execute(
            &self,
            host: &crate::debian::DebianHost<'_>,
            _config: &NodeConfig,
            _ctx: &mut ExecutionContext,
        ) -> Result<PhaseResult> {
            host.remote().run_checked("nomad version")?;
            Ok(PhaseResult::unchanged("verify", "ok"))
        }

        fn name(&self) -> &'static str {
            "verify"
        }
    }

    #[test]
    fn test_preflight_sessions_are_reused_after_idle_before_provisioning() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = IdleSessionTransport {
            calls: Arc::clone(&calls),
        };
        let phase = RemoteExecPhase;
        let phases: Vec<&dyn PhaseExecutor> = vec![&phase];
        let execution = crate::config::ExecutionConfig { concurrency: 1 };
        let nodes = nodes();

        let statuses =
            preflight::run(&nodes, &phases, &transport, execution).expect("preflight should pass");
        thread::sleep(Duration::from_millis(10));
        provisioning::run(&nodes, &phases, &transport, execution, statuses)
            .expect("provisioning should pass");

        let calls = calls.lock().expect("calls lock");
        let node_one_check = calls
            .iter()
            .position(|entry| entry == "check-session:node-1")
            .expect("expected check-session for node-1");
        let node_one_exec = calls
            .iter()
            .position(|entry| entry == "exec:node-1:nomad version")
            .expect("expected provisioning command for node-1");
        assert!(node_one_check > 0);
        assert!(node_one_check < node_one_exec);
    }
}
