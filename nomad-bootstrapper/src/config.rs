use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use log::LevelFilter;
use serde::Deserialize;

use crate::executor::PHASE_NAMES;
use crate::models::{
    AdvertiseConfig, ClientConfig, LatencyProfile, NodeConfig, NodeRole, PluginInstallConfig,
    ResolvedNode, ResolvedTarget, ServerConfig,
};

#[derive(Parser, Debug)]
#[command(name = "nomad-bootstrapper")]
#[command(about = "Bootstrap Nomad on Debian hosts over SSH", long_about = None)]
#[command(version)]
#[command(author = "Clark Contributors")]
pub struct Args {
    /// Path to the cluster inventory TOML file.
    #[arg(long)]
    pub inventory: PathBuf,

    /// Run only this phase (for testing/debugging).
    #[arg(long, value_parser = PHASE_NAMES)]
    pub phase: Option<String>,

    /// Run all phases up to and including this one.
    #[arg(long, value_parser = PHASE_NAMES)]
    pub up_to: Option<String>,

    /// Run only the fleet-wide preflight gate and skip provisioning.
    #[arg(long, default_value_t = false)]
    pub preflight_only: bool,

    /// Show what would be done without making remote changes.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Delete unrecognized .hcl files found in /etc/nomad.d instead of failing.
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Log level (debug, info, warn, error).
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

impl Args {
    pub fn parse_and_init_logging() -> Result<Self> {
        let args = Self::parse().validated()?;
        env_logger::Builder::from_default_env()
            .filter_level(args.log_level.parse::<LevelFilter>()?)
            .init();
        Ok(args)
    }

    fn validated(self) -> Result<Self> {
        if self.preflight_only && (self.phase.is_some() || self.up_to.is_some()) {
            anyhow::bail!("--preflight-only cannot be used together with --phase or --up-to");
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    pub datacenter: String,
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Override the directory where Nomad looks for task driver plugin binaries.
    /// Defaults to `<data_dir>/plugins` = `/opt/nomad/plugins`.
    pub plugin_dir: Option<String>,
}

impl ClusterConfig {
    fn default_datacenter() -> String {
        "dc1".to_string()
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            datacenter: Self::default_datacenter(),
            env_vars: HashMap::new(),
            plugin_dir: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    pub nomad_version: Option<String>,
    pub high_latency: Option<bool>,
    pub cni_version: Option<String>,
    /// Default task driver plugin config applied to all client nodes.
    /// Deep-merged with per-node plugin overrides; node values win on conflict.
    #[serde(default)]
    pub plugins: HashMap<String, toml::Table>,
    /// Default driver plugin installation specs. Each entry is keyed by the
    /// driver name (e.g. "containerd-driver"). A per-node `plugin_install` entry
    /// for the same driver *replaces* the default entirely (no deep merge).
    #[serde(default)]
    pub plugin_install: HashMap<String, PluginInstallConfig>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ControllerConfig {
    pub concurrency: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    pub user: Option<String>,
    pub identity_file: Option<String>,
    pub port: Option<u16>,
    pub options: Vec<String>,
    pub privilege_escalation: Option<Vec<String>>,
}

impl SshConfig {
    fn merge(&self, override_config: Option<&SshConfig>) -> ResolvedTargetSsh {
        let mut options = self.options.clone();
        if let Some(override_config) = override_config {
            options.extend(override_config.options.iter().cloned());
        }

        ResolvedTargetSsh {
            user: override_config
                .and_then(|cfg| cfg.user.clone())
                .or_else(|| self.user.clone()),
            identity_file: override_config
                .and_then(|cfg| cfg.identity_file.clone())
                .or_else(|| self.identity_file.clone()),
            port: override_config.and_then(|cfg| cfg.port).or(self.port),
            options,
            privilege_escalation: normalize_escalation(
                override_config
                    .and_then(|cfg| cfg.privilege_escalation.clone())
                    .or_else(|| self.privilege_escalation.clone()),
            ),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ResolvedTargetSsh {
    user: Option<String>,
    identity_file: Option<String>,
    port: Option<u16>,
    options: Vec<String>,
    privilege_escalation: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdvertiseInventoryConfig {
    pub http: Option<String>,
    pub rpc: Option<String>,
    pub serf: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum AdvertiseInventory {
    Address(String),
    Addresses(AdvertiseInventoryConfig),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInventory {
    pub name: String,
    pub host: String,
    pub roles: Vec<NodeRole>,
    pub bootstrap_expect: Option<u32>,
    #[serde(default)]
    pub server_join_address: Vec<String>,
    #[serde(default)]
    pub server_address: Vec<String>,
    pub nomad_version: Option<String>,
    pub high_latency: Option<bool>,
    pub cni_version: Option<String>,
    pub datacenter: Option<String>,
    pub bind_addr: Option<String>,
    #[serde(default)]
    pub advertise: Option<AdvertiseInventory>,
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Per-node task driver plugin overrides. Deep-merged on top of
    /// `[defaults.plugins]`; node scalars win, nested tables recurse.
    /// Absent field defaults to an empty map for backward compatibility.
    #[serde(default)]
    pub plugins: HashMap<String, toml::Table>,
    /// Per-node driver plugin installation overrides. Each entry *replaces* the
    /// corresponding default entry entirely (no deep merge). Absent entries fall
    /// back to `[defaults.plugin_install]`.
    #[serde(default)]
    pub plugin_install: HashMap<String, PluginInstallConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    #[serde(default)]
    pub cluster: ClusterConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub controller: ControllerConfig,
    #[serde(default)]
    pub ssh: SshConfig,
    pub nodes: Vec<NodeInventory>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionConfig {
    pub concurrency: usize,
}

const DEFAULT_CONCURRENCY: usize = 3;

impl Inventory {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let inventory: Self = toml::from_str(&raw)?;
        if inventory.nodes.is_empty() {
            anyhow::bail!("inventory must contain at least one [[nodes]] entry");
        }
        Ok(inventory)
    }

    pub fn resolve_nodes(&self) -> Result<Vec<ResolvedNode>> {
        self.nodes
            .iter()
            .map(|node| self.resolve_node(node))
            .collect::<Result<Vec<_>>>()
    }

    pub fn resolve_execution(&self, host_count: usize) -> Result<ExecutionConfig> {
        if host_count == 0 {
            anyhow::bail!("execution requires at least one host");
        }

        if matches!(self.controller.concurrency, Some(0)) {
            anyhow::bail!("controller concurrency must be greater than 0");
        }

        let requested = self.controller.concurrency.unwrap_or(DEFAULT_CONCURRENCY);

        Ok(ExecutionConfig {
            concurrency: requested.min(host_count),
        })
    }

    fn resolve_node(&self, node: &NodeInventory) -> Result<ResolvedNode> {
        let name = node.name.trim();
        if name.is_empty() {
            anyhow::bail!("node names cannot be empty");
        }

        let host = node.host.trim();
        if host.is_empty() {
            anyhow::bail!("node '{}' is missing a host", name);
        }

        let merged_ssh = self.ssh.merge(node.ssh.as_ref());
        let datacenter = node
            .datacenter
            .clone()
            .unwrap_or_else(|| self.cluster.datacenter.clone());
        let version = node
            .nomad_version
            .clone()
            .or_else(|| self.defaults.nomad_version.clone())
            .unwrap_or_else(|| "latest".to_string());
        // CNI version: node-level override → cluster defaults → pinned known-good version.
        // Structurally present for all nodes; only consumed when the node has the client role.
        let cni_version = node
            .cni_version
            .clone()
            .or_else(|| self.defaults.cni_version.clone())
            .unwrap_or_else(|| "v1.6.2".to_string());
        let roles = validate_roles(&node.roles, name)?;
        let latency_profile = if node
            .high_latency
            .or(self.defaults.high_latency)
            .unwrap_or(false)
        {
            LatencyProfile::HighLatency
        } else {
            LatencyProfile::Standard
        };

        let server_join_addresses = node
            .server_join_address
            .iter()
            .map(|value| validate_address(value, 4648))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| anyhow::anyhow!("node '{}': {}", name, err))?;
        let server_addresses = node
            .server_address
            .iter()
            .map(|value| validate_address(value, 4647))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| anyhow::anyhow!("node '{}': {}", name, err))?;
        let bind_addr = normalize_nomad_config_value(node.bind_addr.as_deref(), name, "bind_addr")?;
        let advertise = resolve_advertise(node.advertise.as_ref(), name)?;
        let mut env_vars = self.cluster.env_vars.clone();
        env_vars.extend(node.env_vars.clone());

        // Plugins: start from defaults, deep-merge per-node overrides on top.
        // Each plugin is merged independently to preserve cross-plugin isolation.
        let mut plugins = self.defaults.plugins.clone();
        for (driver, node_plugin) in &node.plugins {
            let merged = match plugins.remove(driver) {
                Some(base) => deep_merge_plugin_config(base, node_plugin.clone()),
                None => node_plugin.clone(),
            };
            plugins.insert(driver.clone(), merged);
        }

        // Plugin installs: start from defaults, node entry replaces (not merges) per driver.
        let mut plugin_installs = self.defaults.plugin_install.clone();
        for (driver, node_install) in &node.plugin_install {
            plugin_installs.insert(driver.clone(), node_install.clone());
        }

        // plugin_dir: cluster-level override → default path under data_dir.
        let plugin_dir = self
            .cluster
            .plugin_dir
            .clone()
            .unwrap_or_else(|| "/opt/nomad/plugins".to_string());

        let server_config = if roles.contains(&NodeRole::Server) {
            let bootstrap_expect = node.bootstrap_expect.ok_or_else(|| {
                anyhow::anyhow!("node '{}' requires bootstrap_expect for server role", name)
            })?;
            if bootstrap_expect == 0 {
                anyhow::bail!("node '{}' bootstrap_expect must be greater than 0", name);
            }
            Some(ServerConfig {
                bootstrap_expect,
                server_join_addresses,
            })
        } else {
            None
        };

        let client_config = if roles.contains(&NodeRole::Client) {
            let addrs = if server_addresses.is_empty() {
                if roles.contains(&NodeRole::Server) {
                    vec!["127.0.0.1:4647".to_string()]
                } else {
                    anyhow::bail!("node '{}' requires at least one server_address", name);
                }
            } else {
                server_addresses
            };
            Some(ClientConfig {
                server_addresses: addrs,
            })
        } else {
            None
        };

        let config = NodeConfig {
            name: name.to_string(),
            datacenter,
            version,
            cni_version,
            roles,
            server_config,
            client_config,
            bind_addr,
            advertise,
            latency_profile,
            env_vars,
            plugins,
            plugin_dir,
            plugin_installs,
        };

        Ok(ResolvedNode {
            target: ResolvedTarget {
                name: name.to_string(),
                host: host.to_string(),
                user: merged_ssh.user,
                identity_file: merged_ssh.identity_file,
                port: merged_ssh.port,
                options: merged_ssh.options,
                privilege_escalation: validate_privilege_escalation(
                    name,
                    merged_ssh.privilege_escalation,
                )?,
            },
            config,
        })
    }
}

fn normalize_escalation(value: Option<Vec<String>>) -> Option<Vec<String>> {
    match value {
        Some(values) if values.is_empty() => None,
        other => other,
    }
}

/// Deep-merges two plugin config tables.
///
/// Rules:
/// - When both values for a key are tables, recurse.
/// - Otherwise, `override_table` wins regardless of type (scalar beats table,
///   table beats scalar, scalar beats scalar).
/// - Keys present only in `base` are inherited unchanged.
/// - Keys present only in `override_table` are added.
pub(crate) fn deep_merge_plugin_config(
    base: toml::Table,
    override_table: toml::Table,
) -> toml::Table {
    let mut result = base;
    for (key, override_val) in override_table {
        let merged = match (result.remove(&key), override_val) {
            (Some(toml::Value::Table(base_inner)), toml::Value::Table(over_inner)) => {
                toml::Value::Table(deep_merge_plugin_config(base_inner, over_inner))
            }
            // Override wins for all other type combinations (scalar/table mismatch,
            // scalar/scalar, or key absent in base).
            (_, val) => val,
        };
        result.insert(key, merged);
    }
    result
}

fn validate_privilege_escalation(
    node_name: &str,
    escalation: Option<Vec<String>>,
) -> Result<Option<Vec<String>>> {
    escalation
        .map(|values| {
            values
                .into_iter()
                .map(|value| {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        anyhow::bail!(
                            "node '{}' privilege escalation entries cannot be empty",
                            node_name
                        );
                    }
                    Ok(trimmed.to_string())
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()
}

fn validate_roles(roles: &[NodeRole], node_name: &str) -> Result<Vec<NodeRole>> {
    if roles.is_empty() {
        anyhow::bail!("node '{}' must define at least one role", node_name);
    }

    let mut normalized = Vec::with_capacity(roles.len());
    for role in roles {
        if normalized.contains(role) {
            anyhow::bail!("node '{}' role '{}' is duplicated", node_name, role);
        }
        normalized.push(*role);
    }

    Ok(normalized)
}

fn normalize_nomad_config_value(
    value: Option<&str>,
    node_name: &str,
    field_name: &str,
) -> Result<Option<String>> {
    value
        .map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                anyhow::bail!("node '{}' {} cannot be empty", node_name, field_name);
            }
            Ok(trimmed.to_string())
        })
        .transpose()
}

fn resolve_advertise(
    advertise: Option<&AdvertiseInventory>,
    node_name: &str,
) -> Result<AdvertiseConfig> {
    match advertise {
        None => Ok(AdvertiseConfig::default()),
        Some(AdvertiseInventory::Address(value)) => Ok(AdvertiseConfig {
            address: normalize_nomad_config_value(Some(value.as_str()), node_name, "advertise")?,
            ..AdvertiseConfig::default()
        }),
        Some(AdvertiseInventory::Addresses(config)) => {
            let advertise = AdvertiseConfig {
                address: None,
                http: normalize_nomad_config_value(
                    config.http.as_deref(),
                    node_name,
                    "advertise.http",
                )?,
                rpc: normalize_nomad_config_value(
                    config.rpc.as_deref(),
                    node_name,
                    "advertise.rpc",
                )?,
                serf: normalize_nomad_config_value(
                    config.serf.as_deref(),
                    node_name,
                    "advertise.serf",
                )?,
            };

            if advertise.http.is_none() && advertise.rpc.is_none() && advertise.serf.is_none() {
                anyhow::bail!(
                    "node '{}' advertise must set at least one of http, rpc, or serf",
                    node_name
                );
            }

            Ok(advertise)
        }
    }
}

fn validate_address(val: &str, default_port: u16) -> std::result::Result<String, String> {
    let val = val.trim();
    if val.is_empty() {
        return Err("address cannot be empty".to_string());
    }

    // Bare IP with no port — append the default.
    if val.parse::<IpAddr>().is_ok() {
        return Ok(format!("{}:{}", val, default_port));
    }

    // Bracketed IPv6: [::1] or [::1]:port
    if val.starts_with('[') {
        if let Some(bracket_end) = val.find(']') {
            let ip_part = &val[1..bracket_end];
            if ip_part.parse::<IpAddr>().is_err() {
                return Err(format!("invalid IPv6 address in '{}'", val));
            }
            if val.len() > bracket_end + 1 {
                if !val[bracket_end + 1..].starts_with(':') {
                    return Err(format!("expected ':port' after ']' in '{}'", val));
                }
                let port_str = &val[bracket_end + 2..];
                let port: u16 = port_str
                    .parse()
                    .map_err(|_| format!("invalid port in '{}': port must be 1-65535", val))?;
                if port == 0 {
                    return Err(format!("invalid port in '{}': port must be 1-65535", val));
                }
                return Ok(val.to_string());
            }
            return Ok(format!("{}:{}", val, default_port));
        }
        return Err(format!("missing closing ']' in '{}'", val));
    }

    let (host, has_port) = if let Some(idx) = val.rfind(':') {
        let maybe_port = &val[idx + 1..];
        if maybe_port.chars().all(|c| c.is_ascii_digit()) && !maybe_port.is_empty() {
            let port: u16 = maybe_port
                .parse()
                .map_err(|_| format!("invalid port in '{}': port must be 1-65535", val))?;
            if port == 0 {
                return Err(format!("invalid port in '{}': port must be 1-65535", val));
            }
            (&val[..idx], true)
        } else {
            (val, false)
        }
    } else {
        (val, false)
    };

    if host.parse::<IpAddr>().is_ok() {
        return if has_port {
            Ok(val.to_string())
        } else {
            Ok(format!("{}:{}", val, default_port))
        };
    }

    if host.is_empty() || host.len() > 253 {
        return Err(format!(
            "invalid address '{}': hostname is empty or too long",
            val
        ));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!(
                "invalid address '{}': hostname label '{}' is empty or too long",
                val, label
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!(
                "invalid address '{}': hostname contains invalid characters",
                val
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "invalid address '{}': hostname label cannot start or end with a hyphen",
                val
            ));
        }
    }

    if has_port {
        Ok(val.to_string())
    } else {
        Ok(format!("{}:{}", val, default_port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVENTORY: &str = r#"
        [cluster]
        datacenter = "homelab"

        [defaults]
        nomad_version = "1.7.6"
        high_latency = true

        [controller]
        concurrency = 8

        [ssh]
        user = "admin"
        identity_file = "~/.ssh/id_ed25519"
        options = ["StrictHostKeyChecking=accept-new"]
        privilege_escalation = ["sudo", "-n"]

        [[nodes]]
        name = "server-1"
        host = "server-1.example.com"
        roles = ["server"]
        bootstrap_expect = 3
        server_join_address = ["10.0.1.2:4648", "10.0.1.3:4648"]
        bind_addr = "10.0.1.10"
        advertise = "10.0.1.20"

        [nodes.ssh]
        user = "root"
        port = 2222
        privilege_escalation = ["doas"]

        [[nodes]]
        name = "client-1"
        host = "client-1.example.com"
        roles = ["client"]
        server_address = ["10.0.1.1:4647"]
        high_latency = false

        [nodes.advertise]
        http = "10.0.2.20"
        rpc = "10.0.2.21"
        serf = "10.0.2.22:4648"
    "#;

    #[test]
    fn test_inventory_resolves_ssh_defaults_and_overrides() {
        let inventory: Inventory = toml::from_str(INVENTORY).expect("inventory parses");
        let nodes = inventory.resolve_nodes().expect("nodes resolve");

        assert_eq!(nodes.len(), 2);
        let server = &nodes[0];
        assert_eq!(server.target.user.as_deref(), Some("root"));
        assert_eq!(server.target.port, Some(2222));
        assert_eq!(
            server.target.identity_file.as_deref(),
            Some("~/.ssh/id_ed25519")
        );
        assert_eq!(
            server.target.options,
            vec!["StrictHostKeyChecking=accept-new".to_string()]
        );
        assert_eq!(
            server.target.privilege_escalation,
            Some(vec!["doas".to_string()])
        );

        let client = &nodes[1];
        assert_eq!(
            client.target.privilege_escalation,
            Some(vec!["sudo".to_string(), "-n".to_string()])
        );
    }

    #[test]
    fn test_inventory_applies_cluster_and_default_values() {
        let inventory: Inventory = toml::from_str(INVENTORY).expect("inventory parses");
        let nodes = inventory.resolve_nodes().expect("nodes resolve");

        let server = &nodes[0].config;
        assert_eq!(server.roles, vec![NodeRole::Server]);
        assert_eq!(server.datacenter, "homelab");
        assert_eq!(server.version, "1.7.6");
        assert_eq!(server.bind_addr.as_deref(), Some("10.0.1.10"));
        assert_eq!(server.advertise.address.as_deref(), Some("10.0.1.20"));
        assert_eq!(server.latency_profile, LatencyProfile::HighLatency);

        let client = &nodes[1].config;
        assert_eq!(client.roles, vec![NodeRole::Client]);
        assert_eq!(client.bind_addr, None);
        assert_eq!(client.advertise.http.as_deref(), Some("10.0.2.20"));
        assert_eq!(client.advertise.rpc.as_deref(), Some("10.0.2.21"));
        assert_eq!(client.advertise.serf.as_deref(), Some("10.0.2.22:4648"));
        assert_eq!(client.latency_profile, LatencyProfile::Standard);
    }

    #[test]
    fn test_inventory_resolves_execution_from_controller_defaults() {
        let inventory: Inventory = toml::from_str(INVENTORY).expect("inventory parses");

        let execution = inventory.resolve_execution(2).expect("execution resolves");

        assert_eq!(execution.concurrency, 2);
    }

    #[test]
    fn test_inventory_uses_default_concurrency_when_controller_missing() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let execution = inventory.resolve_execution(5).expect("execution resolves");
        assert_eq!(execution.concurrency, 3);
    }

    #[test]
    fn test_inventory_rejects_zero_controller_concurrency() {
        let inventory: Inventory = toml::from_str(
            r#"
            [controller]
            concurrency = 0

            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_execution(1)
            .expect_err("expected validation error");
        assert!(err.to_string().contains("greater than 0"));
    }

    #[test]
    fn test_empty_node_privilege_escalation_disables_inherited_default() {
        let inventory: Inventory = toml::from_str(
            r#"
            [ssh]
            privilege_escalation = ["sudo", "-n"]

            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1

            [nodes.ssh]
            privilege_escalation = []
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].target.privilege_escalation, None);
    }

    #[test]
    fn test_args_reject_preflight_only_with_phase_selection() {
        let err = Args {
            inventory: PathBuf::from("inventory.toml"),
            phase: Some("ensure-deps".to_string()),
            up_to: None,
            preflight_only: true,
            dry_run: false,
            force: false,
            log_level: "info".to_string(),
        }
        .validated()
        .expect_err("expected invalid flag combination");

        assert!(err
            .to_string()
            .contains("--preflight-only cannot be used together"));
    }

    #[test]
    fn test_client_requires_server_addresses() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err.to_string().contains("server_address"));
    }

    #[test]
    fn test_server_requires_bootstrap_expect() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err.to_string().contains("bootstrap_expect"));
    }

    #[test]
    fn test_legacy_role_field_is_rejected() {
        let err = toml::from_str::<Inventory>(
            r#"
            [[nodes]]
            name = "node-1"
            host = "node-1.example.com"
            role = "server"
            bootstrap_expect = 1
        "#,
        )
        .expect_err("expected schema validation error");

        assert!(err.to_string().contains("unknown field `role`"));
    }

    #[test]
    fn test_node_requires_at_least_one_role() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "node-1"
            host = "node-1.example.com"
            roles = []
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err.to_string().contains("must define at least one role"));
    }

    #[test]
    fn test_node_rejects_duplicate_roles() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "node-1"
            host = "node-1.example.com"
            roles = ["server", "server"]
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err.to_string().contains("is duplicated"));
    }

    #[test]
    fn test_dual_role_node_resolves_both_role_configs() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "node-1"
            host = "node-1.example.com"
            roles = ["server", "client"]
            bootstrap_expect = 1
            server_join_address = ["10.0.1.2:4648"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        let node = &nodes[0].config;
        assert_eq!(node.roles, vec![NodeRole::Server, NodeRole::Client]);
        assert_eq!(
            node.server_config()
                .expect("server config")
                .bootstrap_expect,
            1
        );
        assert_eq!(
            node.client_config()
                .expect("client config")
                .server_addresses,
            vec!["127.0.0.1:4647".to_string()]
        );
    }

    #[test]
    fn test_dual_role_node_accepts_explicit_server_addresses() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "node-1"
            host = "node-1.example.com"
            roles = ["server", "client"]
            bootstrap_expect = 1
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(
            nodes[0]
                .config
                .client_config()
                .expect("client config")
                .server_addresses,
            vec!["10.0.1.1:4647".to_string()]
        );
    }

    #[test]
    fn test_validate_address_rejects_invalid_hostname() {
        let err = validate_address("bad host", 4647).expect_err("expected invalid hostname");
        assert!(err.contains("invalid characters"));
    }

    #[test]
    fn test_validate_address_appends_default_port_bare_ip() {
        assert_eq!(
            validate_address("172.16.20.8", 4647).unwrap(),
            "172.16.20.8:4647"
        );
        assert_eq!(
            validate_address("172.16.20.8", 4648).unwrap(),
            "172.16.20.8:4648"
        );
    }

    #[test]
    fn test_validate_address_preserves_explicit_port() {
        assert_eq!(
            validate_address("172.16.20.8:9999", 4647).unwrap(),
            "172.16.20.8:9999"
        );
    }

    #[test]
    fn test_validate_address_appends_default_port_hostname() {
        assert_eq!(
            validate_address("server-1.example.com", 4647).unwrap(),
            "server-1.example.com:4647"
        );
    }

    #[test]
    fn test_server_address_without_port_gets_default_4647() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["172.16.20.8"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(
            nodes[0]
                .config
                .client_config()
                .expect("client config")
                .server_addresses,
            vec!["172.16.20.8:4647".to_string()]
        );
    }

    #[test]
    fn test_server_join_address_without_port_gets_default_4648() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
            server_join_address = ["10.0.1.2"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(
            nodes[0]
                .config
                .server_config()
                .expect("server config")
                .server_join_addresses,
            vec!["10.0.1.2:4648".to_string()]
        );
    }

    #[test]
    fn test_inventory_rejects_empty_bind_addr() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
            bind_addr = "   "
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err.to_string().contains("bind_addr cannot be empty"));
    }

    #[test]
    fn test_inventory_rejects_empty_advertise_block() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1

            [nodes.advertise]
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err
            .to_string()
            .contains("advertise must set at least one of http, rpc, or serf"));
    }

    #[test]
    fn test_env_vars_node_overrides_cluster() {
        let inventory: Inventory = toml::from_str(
            r#"
            [cluster]
            env_vars = { SHARED = "cluster", CLUSTER_ONLY = "yes" }

            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
            env_vars = { SHARED = "node", NODE_ONLY = "yes" }
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("resolve nodes");
        let env = &nodes[0].config.env_vars;

        assert_eq!(env.get("SHARED").map(String::as_str), Some("node"));
        assert_eq!(env.get("CLUSTER_ONLY").map(String::as_str), Some("yes"));
        assert_eq!(env.get("NODE_ONLY").map(String::as_str), Some("yes"));
    }

    #[test]
    fn test_cni_version_falls_back_to_default_when_absent() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].config.cni_version, "v1.6.2");
    }

    #[test]
    fn test_cni_version_inherits_from_defaults() {
        let inventory: Inventory = toml::from_str(
            r#"
            [defaults]
            cni_version = "v1.5.0"

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].config.cni_version, "v1.5.0");
    }

    #[test]
    fn test_cni_version_node_override_takes_precedence() {
        let inventory: Inventory = toml::from_str(
            r#"
            [defaults]
            cni_version = "v1.5.0"

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
            cni_version = "v1.4.0"
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].config.cni_version, "v1.4.0");
    }

    #[test]
    fn test_cni_version_present_on_server_only_node_with_default() {
        // cni_version is structurally present for all nodes; only consumed for client role.
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].config.cni_version, "v1.6.2");
    }

    // ── Plugin config resolution tests ──────────────────────────────────────

    fn make_table(pairs: &[(&str, toml::Value)]) -> toml::Table {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_deep_merge_plugin_config_empty_base_returns_override() {
        let base = toml::Table::new();
        let over = make_table(&[("enabled", toml::Value::Boolean(true))]);
        let result = deep_merge_plugin_config(base, over);
        assert_eq!(result["enabled"], toml::Value::Boolean(true));
    }

    #[test]
    fn test_deep_merge_plugin_config_empty_override_returns_base() {
        let base = make_table(&[("enabled", toml::Value::Boolean(false))]);
        let over = toml::Table::new();
        let result = deep_merge_plugin_config(base, over);
        assert_eq!(result["enabled"], toml::Value::Boolean(false));
    }

    #[test]
    fn test_deep_merge_plugin_config_scalar_override_wins() {
        let base = make_table(&[("enabled", toml::Value::Boolean(false))]);
        let over = make_table(&[("enabled", toml::Value::Boolean(true))]);
        let result = deep_merge_plugin_config(base, over);
        assert_eq!(result["enabled"], toml::Value::Boolean(true));
    }

    #[test]
    fn test_deep_merge_plugin_config_nested_tables_recurse() {
        let inner_base = make_table(&[
            ("enabled", toml::Value::Boolean(true)),
            ("selinuxlabel", toml::Value::String("z".to_string())),
        ]);
        let base = make_table(&[("volumes", toml::Value::Table(inner_base))]);

        let inner_over = make_table(&[("enabled", toml::Value::Boolean(false))]);
        let over = make_table(&[("volumes", toml::Value::Table(inner_over))]);

        let result = deep_merge_plugin_config(base, over);
        let volumes = result["volumes"].as_table().expect("volumes is table");
        // Override wins for the key it sets.
        assert_eq!(volumes["enabled"], toml::Value::Boolean(false));
        // Base key not in override is inherited.
        assert_eq!(
            volumes["selinuxlabel"],
            toml::Value::String("z".to_string())
        );
    }

    #[test]
    fn test_deep_merge_plugin_config_type_conflict_override_wins() {
        // Default is a table; node override is a scalar — override replaces base.
        let inner = make_table(&[("nested", toml::Value::Boolean(true))]);
        let base = make_table(&[("key", toml::Value::Table(inner))]);
        let over = make_table(&[("key", toml::Value::Boolean(false))]);
        let result = deep_merge_plugin_config(base, over);
        assert_eq!(result["key"], toml::Value::Boolean(false));

        // Default is a scalar; node override is a table — override replaces base.
        let base2 = make_table(&[("key", toml::Value::Boolean(true))]);
        let inner2 = make_table(&[("nested", toml::Value::Boolean(false))]);
        let over2 = make_table(&[("key", toml::Value::Table(inner2))]);
        let result2 = deep_merge_plugin_config(base2, over2);
        assert!(result2["key"].is_table());
    }

    #[test]
    fn test_plugins_resolved_defaults_only() {
        let inventory: Inventory = toml::from_str(
            r#"
            [defaults.plugins.raw_exec]
            enabled = true

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        let plugins = &nodes[0].config.plugins;
        assert_eq!(plugins["raw_exec"]["enabled"], toml::Value::Boolean(true));
    }

    #[test]
    fn test_plugins_resolved_node_only() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]

            [nodes.plugins.docker]
            allow_privileged = false
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        let plugins = &nodes[0].config.plugins;
        assert_eq!(
            plugins["docker"]["allow_privileged"],
            toml::Value::Boolean(false)
        );
    }

    #[test]
    fn test_plugins_resolved_deep_merge_with_multi_plugin_boundary() {
        // Defaults: docker (allow_privileged=false, volumes table) + raw_exec (enabled=false).
        // Node overrides: raw_exec enabled=true (docker untouched), docker.allow_privileged=true.
        let inventory: Inventory = toml::from_str(
            r#"
            [defaults.plugins.docker]
            allow_privileged = false
            [defaults.plugins.docker.volumes]
            enabled = true
            selinuxlabel = "z"

            [defaults.plugins.raw_exec]
            enabled = false

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]

            [nodes.plugins.raw_exec]
            enabled = true

            [nodes.plugins.docker]
            allow_privileged = true
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        let plugins = &nodes[0].config.plugins;

        // raw_exec: node override wins.
        assert_eq!(plugins["raw_exec"]["enabled"], toml::Value::Boolean(true));
        // docker.allow_privileged: node override wins.
        assert_eq!(
            plugins["docker"]["allow_privileged"],
            toml::Value::Boolean(true)
        );
        // docker.volumes: inherited from defaults (node didn't override it).
        let volumes = plugins["docker"]["volumes"]
            .as_table()
            .expect("volumes table");
        assert_eq!(volumes["enabled"], toml::Value::Boolean(true));
        assert_eq!(
            volumes["selinuxlabel"],
            toml::Value::String("z".to_string())
        );
    }

    #[test]
    fn test_plugins_empty_when_not_configured() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert!(nodes[0].config.plugins.is_empty());
    }

    #[test]
    fn test_plugins_deserialization_absent_field_defaults_to_empty() {
        // An inventory with no `plugins` key must still deserialize cleanly.
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses without plugins key");

        assert!(inventory.defaults.plugins.is_empty());
        let node = &inventory.nodes[0];
        assert!(node.plugins.is_empty());
    }

    #[test]
    fn test_plugin_dir_defaults_to_opt_nomad_plugins() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].config.plugin_dir, "/opt/nomad/plugins");
    }

    #[test]
    fn test_plugin_dir_cluster_override_propagates_to_nodes() {
        let inventory: Inventory = toml::from_str(
            r#"
            [cluster]
            plugin_dir = "/custom/plugins"

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert_eq!(nodes[0].config.plugin_dir, "/custom/plugins");
    }

    #[test]
    fn test_plugin_install_defaults_resolved_when_no_node_override() {
        let inventory: Inventory = toml::from_str(
            r#"
            [defaults.plugin_install.containerd-driver]
            method = "tarball"
            url = "https://example.com/containerd_{arch}.tar.gz"
            binary = "nomad-driver-containerd"

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        let install = &nodes[0].config.plugin_installs;
        assert!(matches!(
            install.get("containerd-driver"),
            Some(PluginInstallConfig::Tarball { url, binary })
                if url == "https://example.com/containerd_{arch}.tar.gz"
                && binary == "nomad-driver-containerd"
        ));
    }

    #[test]
    fn test_plugin_install_node_entry_replaces_default() {
        let inventory: Inventory = toml::from_str(
            r#"
            [defaults.plugin_install.lxc]
            method = "apt"
            package = "nomad-driver-lxc"
            version = "1.0.0"
            binary = "/usr/sbin/nomad-driver-lxc"

            [[nodes]]
            name = "client-1"
            host = "client-1.example.com"
            roles = ["client"]
            server_address = ["10.0.1.1:4647"]

            [nodes.plugin_install.lxc]
            method = "apt"
            package = "nomad-driver-lxc"
            version = "2.0.0"
            binary = "/usr/sbin/nomad-driver-lxc"
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        let install = &nodes[0].config.plugin_installs;
        // Node version overrides default version; entire entry replaced.
        assert!(matches!(
            install.get("lxc"),
            Some(PluginInstallConfig::Apt { version: Some(v), .. }) if v == "2.0.0"
        ));
    }

    #[test]
    fn test_plugin_install_empty_when_not_configured() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            roles = ["server"]
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let nodes = inventory.resolve_nodes().expect("nodes resolve");
        assert!(nodes[0].config.plugin_installs.is_empty());
    }
}
