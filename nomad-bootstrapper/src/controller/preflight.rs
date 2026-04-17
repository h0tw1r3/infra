use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use log::info;

use super::{render_run_summary, HostStatus, RunAbortReason};
use crate::config::ExecutionConfig;
use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::ResolvedNode;
use crate::transport::{RemoteHost, Transport};

pub(super) fn run(
    nodes: &[ResolvedNode],
    phases: &[&dyn PhaseExecutor],
    transport: &dyn Transport,
    execution: ExecutionConfig,
) -> Result<Vec<HostStatus>> {
    if transport.is_dry_run() {
        return Ok(vec![HostStatus::PreflightPassed; nodes.len()]);
    }

    let queue = Arc::new(Mutex::new((0..nodes.len()).collect::<VecDeque<_>>()));
    let statuses = Arc::new(Mutex::new(vec![None; nodes.len()]));

    thread::scope(|scope| {
        for _ in 0..execution.concurrency.min(nodes.len()) {
            let queue = Arc::clone(&queue);
            let statuses = Arc::clone(&statuses);
            scope.spawn(move || loop {
                let index = {
                    let mut pending = queue.lock().expect("preflight queue lock");
                    pending.pop_front()
                };
                let Some(index) = index else {
                    break;
                };

                let node = &nodes[index];
                let status = match run_host_preflight(node, phases, transport) {
                    Ok(()) => HostStatus::PreflightPassed,
                    Err(err) => HostStatus::PreflightFailed(err.to_string()),
                };

                statuses.lock().expect("preflight results lock")[index] = Some(status);
            });
        }
    });

    let statuses = statuses
        .lock()
        .expect("preflight results lock")
        .iter()
        .map(|status| status.clone().expect("every host has preflight status"))
        .collect::<Vec<_>>();

    if statuses
        .iter()
        .any(|status| matches!(status, HostStatus::PreflightFailed(_)))
    {
        anyhow::bail!(
            "{}",
            render_run_summary(nodes, &statuses, Some(&RunAbortReason::PreflightFailure))
        );
    }

    info!("Preflight gate passed for {} host(s)", nodes.len());
    Ok(statuses)
}

fn run_host_preflight(
    node: &ResolvedNode,
    phases: &[&dyn PhaseExecutor],
    transport: &dyn Transport,
) -> Result<()> {
    info!("Starting preflight for host {}", node.target.label());

    let remote = RemoteHost::new(transport, &node.target);
    let host = DebianHost::new(remote);
    host.remote().run_checked("true")?;
    host.ensure_supported_platform()?;
    validate_privileges(&host, phases)?;

    info!("Completed preflight for host {}", node.target.label());
    Ok(())
}

fn validate_privileges(host: &DebianHost<'_>, phases: &[&dyn PhaseExecutor]) -> Result<()> {
    if !requires_privileged_checks(phases) {
        return Ok(());
    }

    host.remote().run_checked(
        "set -eu; test \"$(id -u)\" -eq 0 && command -v apt-get >/dev/null && command -v systemctl >/dev/null && command -v mktemp >/dev/null && command -v mkdir >/dev/null && command -v chmod >/dev/null && command -v mv >/dev/null",
    )?;
    Ok(())
}

fn requires_privileged_checks(phases: &[&dyn PhaseExecutor]) -> bool {
    phases.iter().any(|phase| {
        matches!(
            phase.name(),
            "ensure-deps" | "setup-repo" | "install" | "configure"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        ExecutionContext, LatencyProfile, NodeConfig, NodeRole, PhaseResult, ResolvedTarget,
        ServerConfig,
    };
    use crate::transport::RemoteOutput;

    struct FakeTransport {
        failing_host: &'static str,
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
            if target.label() == self.failing_host && command == "true" {
                anyhow::bail!("ssh authentication failed");
            }

            if command == "cat /etc/os-release" {
                return Ok(RemoteOutput {
                    status: 0,
                    stdout: "ID=debian\nVERSION_CODENAME=bookworm\n".to_string(),
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

    struct FakePhase(&'static str);

    impl PhaseExecutor for FakePhase {
        fn execute(
            &self,
            _host: &DebianHost<'_>,
            _config: &NodeConfig,
            _ctx: &mut ExecutionContext,
        ) -> Result<PhaseResult> {
            unreachable!("preflight tests do not execute provisioning phases");
        }

        fn name(&self) -> &'static str {
            self.0
        }
    }

    fn nodes() -> Vec<ResolvedNode> {
        ["node-1", "node-2"]
            .iter()
            .map(|name| ResolvedNode {
                target: ResolvedTarget {
                    name: (*name).to_string(),
                    host: format!("{}.example.com", name),
                    user: None,
                    identity_file: None,
                    port: None,
                    options: Vec::new(),
                },
                config: NodeConfig {
                    name: (*name).to_string(),
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
            })
            .collect()
    }

    #[test]
    fn test_preflight_failure_is_aggregated_in_host_order() {
        let nodes = nodes();
        let phase = FakePhase("install");
        let phases: Vec<&dyn PhaseExecutor> = vec![&phase];
        let err = run(
            &nodes,
            &phases,
            &FakeTransport {
                failing_host: "node-2",
            },
            ExecutionConfig { concurrency: 2 },
        )
        .expect_err("expected aggregated preflight failure");

        let message = err.to_string();
        assert!(message.contains("Run aborted: preflight failure"));
        assert!(message.contains("node-1: preflight_passed"));
        assert!(message.contains("node-2: preflight_failed (ssh authentication failed)"));
    }
}
