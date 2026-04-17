mod preflight;
mod provisioning;

use anyhow::Result;
use log::info;

use crate::config::{Args, Inventory};
use crate::executor::DependencyGraph;
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
    GateInvalidation { host: String, message: String },
    ProvisioningFailure {
        host: String,
        phase: String,
        message: String,
    },
}

pub fn run(args: &Args) -> Result<()> {
    let inventory = Inventory::load(&args.inventory)?;
    let nodes = inventory.resolve_nodes()?;
    let execution = inventory.resolve_execution(args, nodes.len())?;
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
    provisioning::run(&nodes, &phases, &transport, execution, statuses)?;

    info!("Nomad controller run complete");
    Ok(())
}

fn render_run_summary(
    nodes: &[ResolvedNode],
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
        lines.push(format!("{}: {}", node.target.label(), render_host_status(status, abort_reason)));
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
        } => format!("provisioning failure on {} during {}: {}", host, phase, message),
    }
}

fn render_host_status(status: &HostStatus, abort_reason: Option<&RunAbortReason>) -> String {
    match status {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        LatencyProfile, NodeConfig, NodeRole, ResolvedTarget, ServerConfig,
    };

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
                },
                config: NodeConfig {
                    name: "node-1".to_string(),
                    datacenter: "dc1".to_string(),
                    version: "latest".to_string(),
                    role: NodeRole::Server,
                    server_config: Some(ServerConfig {
                        bootstrap_expect: 1,
                        server_join_addresses: Vec::new(),
                    }),
                    client_config: None,
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
                },
                config: NodeConfig {
                    name: "node-2".to_string(),
                    datacenter: "dc1".to_string(),
                    version: "latest".to_string(),
                    role: NodeRole::Server,
                    server_config: Some(ServerConfig {
                        bootstrap_expect: 1,
                        server_join_addresses: Vec::new(),
                    }),
                    client_config: None,
                    latency_profile: LatencyProfile::Standard,
                },
            },
        ]
    }

    #[test]
    fn test_render_run_summary_orders_hosts_deterministically() {
        let summary = render_run_summary(
            &nodes(),
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
        assert!(summary.contains("node-1: preflight_passed"));
        assert!(summary.contains("node-2: provisioning_failed [install] (boom)"));
    }
}
