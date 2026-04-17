use std::fmt;

use anyhow::Result;
use serde::Deserialize;

use crate::state::ProvisionedState;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub bootstrap_expect: u32,
    pub server_join_addresses: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    pub server_addresses: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    pub name: String,
    pub datacenter: String,
    pub version: String,
    pub role: NodeRole,
    pub server_config: Option<ServerConfig>,
    pub client_config: Option<ClientConfig>,
    pub latency_profile: LatencyProfile,
}

impl NodeConfig {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    pub identity_file: Option<String>,
    pub port: Option<u16>,
    pub options: Vec<String>,
    pub privilege_escalation: Option<Vec<String>>,
}

impl ResolvedTarget {
    pub fn label(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedNode {
    pub target: ResolvedTarget,
    pub config: NodeConfig,
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionContext {
    restart_required: bool,
    pub state: ProvisionedState,
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

#[derive(Clone, Debug, PartialEq, Eq)]
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

    #[test]
    fn test_node_role_display() {
        assert_eq!(NodeRole::Server.to_string(), "server");
        assert_eq!(NodeRole::Client.to_string(), "client");
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
}
