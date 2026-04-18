use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};

pub struct Install;

impl PhaseExecutor for Install {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let is_latest = config.version == "latest";
        if is_latest {
            if !host.latest_nomad_needs_install()? {
                let installed_version = host
                    .installed_nomad_version()?
                    .unwrap_or_else(|| "latest".to_string());
                ctx.state.update_provision(&installed_version);
                return Ok(PhaseResult::unchanged(
                    self.name(),
                    "nomad is already at the latest available package version",
                ));
            }
        } else if host.nomad_version_satisfies(&config.version)? {
            ctx.state.update_provision(&config.version);
            return Ok(PhaseResult::unchanged(
                self.name(),
                format!("nomad {} is already installed", config.version),
            ));
        }

        let package_spec = if is_latest {
            "nomad".to_string()
        } else {
            format!("nomad={}", config.version)
        };
        host.apt_install(std::slice::from_ref(&package_spec))?;

        let provisioned_version = host
            .installed_nomad_version()?
            .unwrap_or_else(|| config.version.clone());
        ctx.state.update_provision(&provisioned_version);

        Ok(PhaseResult::changed(
            self.name(),
            format!("installed {}", package_spec),
        ))
    }

    fn name(&self) -> &'static str {
        "install"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::models::{AdvertiseConfig, LatencyProfile, NodeRole, ResolvedTarget};
    use crate::transport::{RemoteHost, RemoteOutput, Transport};

    struct RecordingTransport {
        outputs: Mutex<Vec<RemoteOutput>>,
        commands: Mutex<Vec<String>>,
    }

    impl RecordingTransport {
        fn new(outputs: Vec<RemoteOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into_iter().rev().collect()),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    impl Transport for RecordingTransport {
        fn is_dry_run(&self) -> bool {
            false
        }

        fn exec(
            &self,
            _target: &ResolvedTarget,
            command: &str,
            _input: Option<&[u8]>,
        ) -> Result<RemoteOutput> {
            self.commands
                .lock()
                .expect("commands lock")
                .push(command.to_string());
            Ok(self
                .outputs
                .lock()
                .expect("outputs lock")
                .pop()
                .expect("output"))
        }
    }

    fn node_config(version: &str) -> NodeConfig {
        NodeConfig {
            name: "node-1".to_string(),
            datacenter: "dc1".to_string(),
            version: version.to_string(),
            roles: vec![NodeRole::Server],
            server_config: None,
            client_config: None,
            bind_addr: None,
            advertise: AdvertiseConfig::default(),
            latency_profile: LatencyProfile::Standard,
        }
    }

    fn remote_target() -> ResolvedTarget {
        ResolvedTarget {
            name: "node-1".to_string(),
            host: "node-1.example.com".to_string(),
            user: None,
            identity_file: None,
            port: None,
            options: Vec::new(),
            privilege_escalation: None,
        }
    }

    #[test]
    fn test_latest_skips_install_when_nomad_is_not_upgradable() {
        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "Listing...\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = remote_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &node_config("latest"), &mut ctx)
            .expect("install succeeds");

        assert_eq!(result.changes_made, false);
        assert!(result
            .message
            .contains("already at the latest available package version"));
        assert_eq!(ctx.state.provisioned_version.as_deref(), Some("1.8.0-1"));

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(
            commands[0],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
        assert_eq!(commands[1], "apt list --upgradable nomad 2>&1");
        assert_eq!(
            commands[2],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
        assert!(!commands
            .iter()
            .any(|command| command.contains("apt-get install")));
    }

    #[test]
    fn test_latest_installs_when_nomad_is_missing() {
        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = remote_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &node_config("latest"), &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        assert_eq!(result.message, "installed nomad");
        assert_eq!(ctx.state.provisioned_version.as_deref(), Some("1.8.0-1"));

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(
            commands[0],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
        assert_eq!(commands[1], "id -u");
        assert_eq!(commands[2], "apt-get install -y -qq nomad");
        assert_eq!(
            commands[3],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
    }
}
