use anyhow::Result;
use log::info;

use crate::config::{Args, Inventory};
use crate::debian::DebianHost;
use crate::executor::{DependencyGraph, PhaseExecutor};
use crate::models::{ExecutionContext, ResolvedNode};
use crate::state::ProvisionedState;
use crate::transport::{RemoteHost, SshTransport, Transport};

pub fn run(args: &Args) -> Result<()> {
    let inventory = Inventory::load(&args.inventory)?;
    let nodes = inventory.resolve_nodes()?;
    let executor = DependencyGraph::new();
    let phases = executor.filter_phases(&args.phase, &args.up_to)?;
    let transport = SshTransport::new(args.dry_run);

    run_nodes(&nodes, &phases, &transport)
}

fn run_nodes(
    nodes: &[ResolvedNode],
    phases: &[&dyn PhaseExecutor],
    transport: &dyn Transport,
) -> Result<()> {
    info!(
        "Starting Nomad controller run for {} host(s) with {} phase(s)",
        nodes.len(),
        phases.len()
    );

    for node in nodes {
        info!("Starting host: {}", node.target.label());
        let remote = RemoteHost::new(transport, &node.target);
        let host = DebianHost::new(remote);
        host.ensure_supported_platform()?;

        let mut ctx = ExecutionContext::default();
        ctx.state = ProvisionedState::load_optional(host.remote());

        for phase in phases {
            info!(
                "Host {}: starting phase {}",
                host.remote().label(),
                phase.name()
            );
            let result = phase
                .execute(&host, &node.config, &mut ctx)
                .map_err(|err| {
                    anyhow::anyhow!(
                        "host {} phase {} failed: {}",
                        host.remote().label(),
                        phase.name(),
                        err
                    )
                })?;
            info!(
                "Host {}: completed phase {} (changed: {}) - {}",
                host.remote().label(),
                result.phase_name,
                result.changes_made,
                result.message
            );
        }

        ctx.state.save_optional(host.remote());
        info!("Completed host: {}", host.remote().label());
    }

    info!("Nomad controller run complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::models::{LatencyProfile, NodeConfig, NodeRole, ResolvedTarget, ServerConfig};
    use crate::transport::RemoteOutput;

    struct FakeTransport {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for FakeTransport {
        fn is_dry_run(&self) -> bool {
            false
        }

        fn exec(
            &self,
            target: &ResolvedTarget,
            command: &str,
            _input: Option<&[u8]>,
        ) -> Result<RemoteOutput> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(format!("{}:{}", target.label(), command));

            if command == "cat /etc/os-release" {
                let stdout = if target.label() == "node-1" {
                    "ID=ubuntu\nVERSION_CODENAME=jammy\n"
                } else {
                    "ID=debian\nVERSION_CODENAME=bookworm\n"
                };
                return Ok(RemoteOutput {
                    status: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                });
            }

            Ok(RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn test_controller_stops_on_first_host_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = FakeTransport {
            calls: Arc::clone(&calls),
        };
        let nodes = vec![
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
        ];
        let phases: Vec<&dyn PhaseExecutor> = Vec::new();

        let err = run_nodes(&nodes, &phases, &transport).expect_err("expected first host failure");
        assert!(err.to_string().contains("not supported"));

        let calls = calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("node-1:"));
    }
}
