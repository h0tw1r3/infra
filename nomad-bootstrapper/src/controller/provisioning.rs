use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use log::info;

use super::{render_run_summary, HostStatus, RunAbortReason};
use crate::config::ExecutionConfig;
use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, ResolvedNode};
use crate::state::ProvisionedState;
use crate::transport::{RemoteHost, Transport};

pub(super) fn run(
    nodes: &[ResolvedNode],
    phases: &[&dyn PhaseExecutor],
    transport: &dyn Transport,
    execution: ExecutionConfig,
    initial_statuses: Vec<HostStatus>,
) -> Result<()> {
    let queue = Arc::new(Mutex::new((0..nodes.len()).collect::<VecDeque<_>>()));
    let statuses = Arc::new(Mutex::new(initial_statuses));
    let abort_reason = Arc::new(Mutex::new(None::<RunAbortReason>));

    thread::scope(|scope| {
        for _ in 0..execution.concurrency.min(nodes.len()) {
            let queue = Arc::clone(&queue);
            let statuses = Arc::clone(&statuses);
            let abort_reason = Arc::clone(&abort_reason);

            scope.spawn(move || loop {
                if current_abort(&abort_reason).is_some() {
                    break;
                }

                let index = {
                    let mut pending = queue.lock().expect("provisioning queue lock");
                    pending.pop_front()
                };
                let Some(index) = index else {
                    break;
                };

                let node = &nodes[index];
                let status = run_host(node, phases, transport, &abort_reason);
                statuses.lock().expect("provisioning results lock")[index] = status;
            });
        }
    });

    let abort_reason = current_abort(&abort_reason);
    let mut statuses = statuses.lock().expect("provisioning results lock").clone();

    if matches!(
        abort_reason,
        Some(RunAbortReason::ProvisioningFailure { .. } | RunAbortReason::GateInvalidation { .. })
    ) {
        for status in &mut statuses {
            if matches!(status, HostStatus::PreflightPassed) {
                *status = HostStatus::SkippedAfterAbort { after_phase: None };
            }
        }
    }

    match abort_reason {
        Some(reason) => anyhow::bail!(
            "{}",
            render_run_summary(nodes, phases, &statuses, Some(&reason))
        ),
        None => {
            println!("{}", render_run_summary(nodes, phases, &statuses, None));
            Ok(())
        }
    }
}

fn run_host(
    node: &ResolvedNode,
    phases: &[&dyn PhaseExecutor],
    transport: &dyn Transport,
    abort_reason: &Arc<Mutex<Option<RunAbortReason>>>,
) -> HostStatus {
    if current_abort(abort_reason).is_some() {
        return HostStatus::PreflightPassed;
    }

    if !transport.is_dry_run() {
        if let Err(err) = transport.check_session(&node.target) {
            let message = err.to_string();
            set_abort_once(
                abort_reason,
                RunAbortReason::GateInvalidation {
                    host: node.target.label().to_string(),
                    message: message.clone(),
                },
            );
            return HostStatus::GateInvalidated(message);
        }
    }

    info!("Starting host: {}", node.target.label());
    let remote = RemoteHost::new(transport, &node.target);
    let host = DebianHost::new(remote);

    let mut ctx = ExecutionContext::default();
    ctx.state = ProvisionedState::load_optional(host.remote());

    let mut last_completed_phase = None;
    for (index, phase) in phases.iter().enumerate() {
        if index > 0 && current_abort(abort_reason).is_some() {
            return HostStatus::SkippedAfterAbort {
                after_phase: last_completed_phase,
            };
        }

        info!(
            "Host {}: starting phase {}",
            host.remote().label(),
            phase.name()
        );
        match phase.execute(&host, &node.config, &mut ctx) {
            Ok(result) => {
                info!(
                    "Host {}: completed phase {} (changed: {}) - {}",
                    host.remote().label(),
                    result.phase_name,
                    result.changes_made,
                    result.message
                );
                last_completed_phase = Some(result.phase_name);
            }
            Err(err) => {
                let message = err.to_string();
                set_abort_once(
                    abort_reason,
                    RunAbortReason::ProvisioningFailure {
                        host: host.remote().label().to_string(),
                        phase: phase.name().to_string(),
                        message: message.clone(),
                    },
                );
                return HostStatus::ProvisioningFailed {
                    phase: phase.name().to_string(),
                    message,
                };
            }
        }
    }

    ctx.state.save_optional(host.remote());
    info!("Completed host: {}", host.remote().label());
    HostStatus::ProvisioningSucceeded
}

fn current_abort(abort_reason: &Arc<Mutex<Option<RunAbortReason>>>) -> Option<RunAbortReason> {
    abort_reason.lock().expect("abort reason lock").clone()
}

fn set_abort_once(abort_reason: &Arc<Mutex<Option<RunAbortReason>>>, reason: RunAbortReason) {
    let mut current = abort_reason.lock().expect("abort reason lock");
    if current.is_none() {
        *current = Some(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use crate::models::{
        AdvertiseConfig, LatencyProfile, NodeConfig, NodeRole, PhaseResult, ResolvedTarget,
        ServerConfig,
    };
    use crate::transport::RemoteOutput;

    struct FakeTransport {
        invalidated_host: Option<&'static str>,
    }

    impl Transport for FakeTransport {
        fn is_dry_run(&self) -> bool {
            false
        }

        fn check_session(&self, target: &ResolvedTarget) -> Result<()> {
            if self.invalidated_host == Some(target.label()) {
                anyhow::bail!("retained SSH session is unhealthy for {}", target.label());
            }

            Ok(())
        }

        fn exec(
            &self,
            _target: &ResolvedTarget,
            _command: &str,
            _input: Option<&[u8]>,
        ) -> Result<RemoteOutput> {
            Ok(RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    struct SuccessPhase(&'static str);

    impl PhaseExecutor for SuccessPhase {
        fn execute(
            &self,
            _host: &DebianHost<'_>,
            _config: &NodeConfig,
            _ctx: &mut ExecutionContext,
        ) -> Result<PhaseResult> {
            Ok(PhaseResult::unchanged(self.0, "ok"))
        }

        fn name(&self) -> &'static str {
            self.0
        }
    }

    struct CoordinatedPhaseOne {
        barrier: Arc<Barrier>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl PhaseExecutor for CoordinatedPhaseOne {
        fn execute(
            &self,
            host: &DebianHost<'_>,
            _config: &NodeConfig,
            _ctx: &mut ExecutionContext,
        ) -> Result<PhaseResult> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(format!("{}:phase-one", host.remote().label()));
            self.barrier.wait();

            if host.remote().label() == "node-1" {
                anyhow::bail!("phase one failed");
            }

            thread::sleep(Duration::from_millis(20));

            Ok(PhaseResult::changed("phase-one", "ok"))
        }

        fn name(&self) -> &'static str {
            "phase-one"
        }
    }

    struct TrackingPhaseTwo {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl PhaseExecutor for TrackingPhaseTwo {
        fn execute(
            &self,
            host: &DebianHost<'_>,
            _config: &NodeConfig,
            _ctx: &mut ExecutionContext,
        ) -> Result<PhaseResult> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(format!("{}:phase-two", host.remote().label()));
            Ok(PhaseResult::unchanged("phase-two", "ok"))
        }

        fn name(&self) -> &'static str {
            "phase-two"
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
                    privilege_escalation: None,
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
                    bind_addr: None,
                    advertise: AdvertiseConfig::default(),
                    latency_profile: LatencyProfile::Standard,
                },
            })
            .collect()
    }

    #[test]
    fn test_gate_invalidation_reports_run_level_abort() {
        let nodes = nodes();
        let phase = SuccessPhase("install");
        let phases: Vec<&dyn PhaseExecutor> = vec![&phase];
        let err = run(
            &nodes,
            &phases,
            &FakeTransport {
                invalidated_host: Some("node-2"),
            },
            ExecutionConfig { concurrency: 1 },
            vec![HostStatus::PreflightPassed; 2],
        )
        .expect_err("expected gate invalidation");

        let message = err.to_string();
        assert!(message.contains("Run aborted: gate invalidation on node-2"));
        assert!(message.contains("node-1: provisioning_succeeded"));
        assert!(message.contains("node-2: gate_invalidated"));
    }

    #[test]
    fn test_gate_invalidation_marks_unstarted_hosts_as_skipped() {
        let nodes = nodes();
        let phase = SuccessPhase("install");
        let phases: Vec<&dyn PhaseExecutor> = vec![&phase];
        let err = run(
            &nodes,
            &phases,
            &FakeTransport {
                invalidated_host: Some("node-1"),
            },
            ExecutionConfig { concurrency: 1 },
            vec![HostStatus::PreflightPassed; 2],
        )
        .expect_err("expected gate invalidation");

        let message = err.to_string();
        assert!(message.contains("Run aborted: gate invalidation on node-1"));
        assert!(message.contains("node-1: gate_invalidated"));
        assert!(message.contains("node-2: skipped_after_abort"));
    }

    #[test]
    fn test_peer_failure_only_allows_current_phase_to_finish() {
        let nodes = nodes();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let phase_one = CoordinatedPhaseOne {
            barrier: Arc::new(Barrier::new(2)),
            calls: Arc::clone(&calls),
        };
        let phase_two = TrackingPhaseTwo {
            calls: Arc::clone(&calls),
        };
        let phases: Vec<&dyn PhaseExecutor> = vec![&phase_one, &phase_two];

        let err = run(
            &nodes,
            &phases,
            &FakeTransport {
                invalidated_host: None,
            },
            ExecutionConfig { concurrency: 2 },
            vec![HostStatus::PreflightPassed; 2],
        )
        .expect_err("expected provisioning failure");

        let message = err.to_string();
        assert!(message.contains("Run aborted: provisioning failure on node-1 during phase-one"));
        assert!(message.contains("node-1: provisioning_failed [phase-one] (phase one failed)"));
        assert!(message.contains("node-2: skipped_after_peer_failure (after phase-one)"));
        assert!(!calls
            .lock()
            .expect("calls lock")
            .iter()
            .any(|entry| entry == "node-2:phase-two"));
    }

    struct TrackingPhase {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl PhaseExecutor for TrackingPhase {
        fn execute(
            &self,
            host: &DebianHost<'_>,
            _config: &NodeConfig,
            _ctx: &mut ExecutionContext,
        ) -> Result<PhaseResult> {
            self.calls.lock().expect("calls lock").push(format!(
                "{}:{}",
                host.remote().label(),
                self.name
            ));
            Ok(PhaseResult::unchanged(self.name, "ok"))
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    #[test]
    fn test_serial_provisioning_preserves_host_then_phase_order() {
        let nodes = nodes();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let phase_one = TrackingPhase {
            name: "phase-one",
            calls: Arc::clone(&calls),
        };
        let phase_two = TrackingPhase {
            name: "phase-two",
            calls: Arc::clone(&calls),
        };
        let phases: Vec<&dyn PhaseExecutor> = vec![&phase_one, &phase_two];

        run(
            &nodes,
            &phases,
            &FakeTransport {
                invalidated_host: None,
            },
            ExecutionConfig { concurrency: 1 },
            vec![HostStatus::PreflightPassed; 2],
        )
        .expect("serial provisioning should succeed");

        assert_eq!(
            *calls.lock().expect("calls lock"),
            vec![
                "node-1:phase-one".to_string(),
                "node-1:phase-two".to_string(),
                "node-2:phase-one".to_string(),
                "node-2:phase-two".to_string(),
            ]
        );
    }
}
