use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use serde::Deserialize;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvertiseConfig {
    pub address: Option<String>,
    pub http: Option<String>,
    pub rpc: Option<String>,
    pub serf: Option<String>,
}

/// Arch-keyed URL map or a single URL string (optionally containing `{arch}`).
///
/// ## Single URL
/// ```toml
/// url = "https://example.com/driver_{arch}_linux.tar.gz"
/// ```
/// The `{arch}` placeholder is substituted with `amd64` or `arm64` at install time.
///
/// ## Arch-keyed map
/// Use this when the amd64 and arm64 URLs differ in structure (e.g. different filenames):
/// ```toml
/// [plugin.url]
/// amd64 = "https://example.com/driver"
/// arm64 = "https://example.com/driver-arm64"
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum UrlSpec {
    Single(String),
    ArchMap(HashMap<String, String>),
}

/// Resolved installation config for a single Nomad task driver plugin.
///
/// Each variant describes how to obtain the plugin binary and where it lives
/// after installation, so the install phase can place it into `plugin_dir`.
///
/// ## `binary` field semantics differ by variant
/// - **`Archive`**: `binary` is the **exact relative path of the target executable
///   within the archive** after extraction (e.g. `"nomad-driver-containerd"` or
///   `"linux-amd64/nomad-driver-foo"`). The binary is moved to
///   `plugin_dir/<basename(binary)>`.
/// - **`Binary`**: `binary` is the **destination filename** in `plugin_dir`
///   (e.g. `"nomad-driver-containerd"`). The file is downloaded directly from `url`.
/// - **`Apt`**: `binary` is the **absolute on-disk path** where the package
///   installs the executable (e.g. `"/usr/sbin/nomad-driver-lxc"`). A symlink
///   `plugin_dir/<basename(binary)>` → `binary` is created or corrected.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum PluginInstallConfig {
    /// Download a release archive and extract a named binary from it.
    ///
    /// `url` may be a single URL (with optional `{arch}` placeholder) or an arch-keyed
    /// map. Use version-pinned, immutable URLs — mutable "latest" URLs defeat idempotency.
    ///
    /// `binary` is the path of the binary *inside* the archive after extraction
    /// (can be a bare filename or a relative path like `linux-amd64/nomad-driver-foo`).
    Archive { url: UrlSpec, binary: String },
    /// Download a single binary file directly (no archive extraction).
    ///
    /// `url` may be a single URL (with optional `{arch}` placeholder) or an arch-keyed
    /// map. Use version-pinned, immutable URLs — mutable "latest" URLs defeat idempotency.
    ///
    /// `binary` is the destination **filename** in `plugin_dir` (e.g. `"containerd-driver"`).
    Binary { url: UrlSpec, binary: String },
    /// Install via `apt-get` and symlink the installed binary into `plugin_dir`.
    ///
    /// `binary` is the *full absolute path* where apt drops the executable
    /// (e.g. `/usr/sbin/nomad-driver-lxc`). A symlink
    /// `plugin_dir/<filename>` → `binary` is created or corrected if the target drifts.
    Apt {
        package: String,
        version: Option<String>,
        binary: String,
    },
}

/// Resolved per-node Nomad intent from the inventory.
///
/// Invariant: `roles` is the authoritative list of intended capabilities, and
/// the role-specific config payloads must stay aligned with it:
/// - `server_config` must be `Some` iff `roles` contains `NodeRole::Server`
/// - `client_config` must be `Some` iff `roles` contains `NodeRole::Client`
///
/// `Inventory::resolve_node` is the canonical constructor, and renderer/test
/// fixtures should preserve this invariant because configuration rendering
/// branches on `roles` and then reads the matching role-specific payload.
///
/// `cni_version`, `plugins`, `plugin_dir`, and `plugin_installs` are
/// structurally present for all nodes but only consumed when the node has the
/// `client` role.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeConfig {
    pub name: String,
    pub datacenter: String,
    pub version: String,
    pub cni_version: String,
    pub roles: Vec<NodeRole>,
    pub server_config: Option<ServerConfig>,
    pub client_config: Option<ClientConfig>,
    pub bind_addr: Option<String>,
    pub advertise: AdvertiseConfig,
    pub latency_profile: LatencyProfile,
    pub env_vars: HashMap<String, String>,
    /// Task driver plugin HCL config, deep-merged from inventory defaults and
    /// per-node overrides. Present for all nodes; rendered into `nomad.hcl` only
    /// when the client role is present.
    pub plugins: HashMap<String, toml::Table>,
    /// Directory where Nomad looks for task driver plugin binaries.
    /// Defaults to `<data_dir>/plugins` = `/opt/nomad/plugins`.
    pub plugin_dir: String,
    /// Driver plugin installation specs, merged from inventory defaults and
    /// per-node overrides. Node entry fully replaces the default for that driver.
    /// Only consumed for client nodes during the install phase.
    pub plugin_installs: HashMap<String, PluginInstallConfig>,
}

impl NodeConfig {
    pub fn has_role(&self, role: NodeRole) -> bool {
        self.roles.contains(&role)
    }

    fn roles_label(&self) -> String {
        self.roles
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn server_config(&self) -> Result<&ServerConfig> {
        self.server_config.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "server configuration is not available for roles {}",
                self.roles_label()
            )
        })
    }

    pub fn client_config(&self) -> Result<&ClientConfig> {
        self.client_config.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "client configuration is not available for roles {}",
                self.roles_label()
            )
        })
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

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedNode {
    pub target: ResolvedTarget,
    pub config: NodeConfig,
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionContext {
    restart_required: bool,
    pub force: bool,
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

    fn dual_role_node_config() -> NodeConfig {
        NodeConfig {
            name: "node-1".to_string(),
            datacenter: "dc1".to_string(),
            version: "latest".to_string(),
            cni_version: "v1.6.2".to_string(),
            roles: vec![NodeRole::Server, NodeRole::Client],
            server_config: Some(ServerConfig {
                bootstrap_expect: 1,
                server_join_addresses: vec!["10.0.1.2:4648".to_string()],
            }),
            client_config: Some(ClientConfig {
                server_addresses: vec!["10.0.1.1:4647".to_string()],
            }),
            bind_addr: None,
            advertise: AdvertiseConfig::default(),
            latency_profile: LatencyProfile::Standard,
            env_vars: Default::default(),
            plugins: Default::default(),
            plugin_dir: "/opt/nomad/plugins".to_string(),
            plugin_installs: Default::default(),
        }
    }

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

    #[test]
    fn test_advertise_config_defaults_to_no_overrides() {
        let advertise = AdvertiseConfig::default();
        assert_eq!(advertise.address, None);
        assert_eq!(advertise.http, None);
        assert_eq!(advertise.rpc, None);
        assert_eq!(advertise.serf, None);
    }

    #[test]
    fn test_dual_role_node_config_exposes_both_role_payloads() {
        let config = dual_role_node_config();
        assert!(config.has_role(NodeRole::Server));
        assert!(config.has_role(NodeRole::Client));
        assert_eq!(
            config
                .server_config()
                .expect("server config")
                .bootstrap_expect,
            1
        );
        assert_eq!(
            config
                .client_config()
                .expect("client config")
                .server_addresses,
            vec!["10.0.1.1:4647".to_string()]
        );
    }

    #[test]
    fn test_server_config_error_mentions_active_roles() {
        let config = NodeConfig {
            roles: vec![NodeRole::Server, NodeRole::Client],
            server_config: None,
            ..dual_role_node_config()
        };

        let err = config
            .server_config()
            .expect_err("expected invariant failure");
        assert!(err
            .to_string()
            .contains("server configuration is not available for roles server, client"));
    }
}
