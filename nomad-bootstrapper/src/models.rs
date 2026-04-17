use anyhow::Result;
use std::fmt;

/// Node role in the Nomad cluster
#[derive(Clone, Debug)]
pub enum NodeRole {
    Server,
    Client,
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeRole::Server => write!(f, "server"),
            NodeRole::Client => write!(f, "client"),
        }
    }
}

/// Latency profile for Nomad tuning
#[derive(Clone, Copy, Debug)]
pub enum LatencyProfile {
    Standard,
    HighLatency,
}

impl fmt::Display for LatencyProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LatencyProfile::Standard => write!(f, "standard"),
            LatencyProfile::HighLatency => write!(f, "high-latency"),
        }
    }
}

/// Server-specific configuration
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bootstrap_expect: u32,
    pub server_join_addresses: Vec<String>,
}

/// Client-specific configuration
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub server_addresses: Vec<String>,
}

/// Complete node configuration
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub version: String,
    pub role: NodeRole,
    pub server_config: Option<ServerConfig>,
    pub client_config: Option<ClientConfig>,
    pub latency_profile: LatencyProfile,
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionContext {
    restart_required: bool,
    pub state: crate::state::ProvisionedState,
}

impl ExecutionContext {
    pub fn mark_restart_required(&mut self) {
        self.restart_required = true;
    }

    pub fn restart_required(&self) -> bool {
        self.restart_required
    }

    pub fn clear_restart_required(&mut self) {
        self.restart_required = false;
    }
}

impl NodeConfig {
    pub fn from_args_with_role_requirement(args: &crate::Args, require_role: bool) -> Result<Self> {
        let role = match args.role.as_deref() {
            Some("server") => NodeRole::Server,
            Some("client") => NodeRole::Client,
            None if require_role => anyhow::bail!("--role must be specified (server or client)"),
            None => NodeRole::Server,
            Some(r) => anyhow::bail!("Unknown role: {}", r),
        };

        let latency_profile = if args.high_latency {
            LatencyProfile::HighLatency
        } else {
            LatencyProfile::Standard
        };

        let server_config = match &role {
            NodeRole::Server => {
                if !require_role && args.role.is_none() {
                    None
                } else {
                    let bootstrap_expect = args.bootstrap_expect.ok_or_else(|| {
                        anyhow::anyhow!("--bootstrap-expect required for server role")
                    })?;

                    if bootstrap_expect == 0 {
                        anyhow::bail!("--bootstrap-expect must be greater than 0");
                    }

                    let server_join_addresses = args
                        .server_join_addresses
                        .iter()
                        .map(|s| s.trim().to_string())
                        .collect();

                    Some(ServerConfig {
                        bootstrap_expect,
                        server_join_addresses,
                    })
                }
            }
            NodeRole::Client => None,
        };

        let client_config = match &role {
            NodeRole::Client => {
                if !require_role && args.role.is_none() {
                    None
                } else {
                    if args.server_addresses.is_empty() {
                        anyhow::bail!("--server-addresses required for client role");
                    }

                    let server_addresses = args
                        .server_addresses
                        .iter()
                        .map(|s| s.trim().to_string())
                        .collect();

                    Some(ClientConfig { server_addresses })
                }
            }
            NodeRole::Server => None,
        };

        Ok(NodeConfig {
            version: args.nomad_version.clone(),
            role,
            server_config,
            client_config,
            latency_profile,
        })
    }

    pub fn server_config(&self) -> Result<&ServerConfig> {
        self.server_config.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "server configuration is not available for role {}",
                self.role
            )
        })
    }

    pub fn client_config(&self) -> Result<&ClientConfig> {
        self.client_config.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "client configuration is not available for role {}",
                self.role
            )
        })
    }

    pub fn raft_multiplier(&self) -> u8 {
        match self.latency_profile {
            LatencyProfile::Standard => 1,
            LatencyProfile::HighLatency => 5,
        }
    }
}

/// Result of a phase execution
#[derive(Clone, Debug)]
pub struct PhaseResult {
    pub phase_name: String,
    pub changes_made: bool,
    pub message: String,
}

impl PhaseResult {
    pub fn changed(phase_name: &str, message: impl Into<String>) -> Self {
        Self {
            phase_name: phase_name.to_string(),
            changes_made: true,
            message: message.into(),
        }
    }

    pub fn unchanged(phase_name: &str, message: impl Into<String>) -> Self {
        Self {
            phase_name: phase_name.to_string(),
            changes_made: false,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;

    fn base_args() -> Args {
        Args {
            nomad_version: "1.6.0".to_string(),
            role: Some("server".to_string()),
            bootstrap_expect: Some(1),
            server_join_addresses: Vec::new(),
            server_addresses: Vec::new(),
            high_latency: false,
            phase: None,
            up_to: None,
            dry_run: true,
            log_level: "info".to_string(),
        }
    }

    #[test]
    fn test_node_role_display() {
        assert_eq!(NodeRole::Server.to_string(), "server");
        assert_eq!(NodeRole::Client.to_string(), "client");
    }

    #[test]
    fn test_latency_profile() {
        let _standard = LatencyProfile::Standard;
        let _high_latency = LatencyProfile::HighLatency;
    }

    #[test]
    fn test_phase_result_helpers() {
        let changed = PhaseResult::changed("install", "installed nomad");
        assert!(changed.changes_made);
        assert_eq!(changed.phase_name, "install");

        let unchanged = PhaseResult::unchanged("verify", "already healthy");
        assert!(!unchanged.changes_made);
        assert_eq!(unchanged.message, "already healthy");
    }

    #[test]
    fn test_server_requires_bootstrap_expect_when_role_required() {
        let mut args = base_args();
        args.bootstrap_expect = None;

        let result = NodeConfig::from_args_with_role_requirement(&args, true);
        assert!(result.is_err());
        assert!(result
            .expect_err("expected error")
            .to_string()
            .contains("--bootstrap-expect"));
    }

    #[test]
    fn test_server_rejects_zero_bootstrap_expect() {
        let mut args = base_args();
        args.bootstrap_expect = Some(0);

        let result = NodeConfig::from_args_with_role_requirement(&args, true);
        assert!(result.is_err());
        assert!(result
            .expect_err("expected error")
            .to_string()
            .contains("greater than 0"));
    }

    #[test]
    fn test_client_requires_server_addresses_when_role_required() {
        let mut args = base_args();
        args.role = Some("client".to_string());
        args.bootstrap_expect = None;
        args.server_addresses = Vec::new();

        let result = NodeConfig::from_args_with_role_requirement(&args, true);
        assert!(result.is_err());
        assert!(result
            .expect_err("expected error")
            .to_string()
            .contains("--server-addresses"));
    }

    #[test]
    fn test_role_is_optional_when_not_required() {
        let mut args = base_args();
        args.role = None;
        args.bootstrap_expect = None;

        let config = NodeConfig::from_args_with_role_requirement(&args, false)
            .expect("config should parse without role");
        assert_eq!(config.version, "1.6.0");
    }
}
