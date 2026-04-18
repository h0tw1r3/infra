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
            if let Some(installed_version) = host.installed_package_version("nomad")? {
                if !host.package_is_upgradable("nomad")? {
                    ctx.state.update_provision(&installed_version);
                    return Ok(PhaseResult::unchanged(
                        self.name(),
                        "nomad is already at the latest available package version",
                    ));
                }
            }
        } else if host.package_version_satisfies("nomad", &config.version)? {
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
            .installed_package_version("nomad")?
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
    use super::*;
    use crate::models::{AdvertiseConfig, LatencyProfile, NodeRole};
    use crate::test_helpers::{recording_target, RecordingTransport};
    use crate::transport::{RemoteHost, RemoteOutput};

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
                stdout: "nomad:\n  Installed: 1.8.0-1\n  Candidate: 1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
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
        assert_eq!(commands[1], "apt-cache policy nomad");
        assert!(!commands
            .iter()
            .any(|command| command.contains("apt-get install")));
    }

    #[test]
    fn test_latest_installs_when_nomad_is_upgradable() {
        let transport = RecordingTransport::new(vec![
            // installed_package_version → "1.7.0-1" (nomad is present)
            RemoteOutput {
                status: 0,
                stdout: "1.7.0-1\n".to_string(),
                stderr: String::new(),
            },
            // package_is_upgradable → upgradable from 1.7.0 to 1.8.0
            RemoteOutput {
                status: 0,
                stdout: "nomad:\n  Installed: 1.7.0-1\n  Candidate: 1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            // privilege check for apt-get
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // apt-get install
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // installed_package_version post-install → "1.8.0-1"
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
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
        assert_eq!(commands[1], "apt-cache policy nomad");
        assert_eq!(commands[2], "id -u");
        assert_eq!(commands[3], "apt-get install -y -qq nomad");
        assert_eq!(
            commands[4],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
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
        let target = recording_target();
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
