use std::fs;
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use log::LevelFilter;
use serde::Deserialize;

use crate::executor::PHASE_NAMES;
use crate::models::{
    AdvertiseConfig, ClientConfig, LatencyProfile, NodeConfig, NodeRole, ResolvedNode,
    ResolvedTarget, ServerConfig,
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

    /// Log level (debug, info, warn, error).
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Override the inventory host concurrency limit with a positive value.
    #[arg(long)]
    pub concurrency: Option<NonZeroUsize>,
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
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    pub nomad_version: Option<String>,
    pub high_latency: Option<bool>,
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
    pub role: NodeRole,
    pub bootstrap_expect: Option<u32>,
    #[serde(default)]
    pub server_join_address: Vec<String>,
    #[serde(default)]
    pub server_address: Vec<String>,
    pub nomad_version: Option<String>,
    pub high_latency: Option<bool>,
    pub datacenter: Option<String>,
    pub bind_addr: Option<String>,
    #[serde(default)]
    pub advertise: Option<AdvertiseInventory>,
    #[serde(default)]
    pub ssh: Option<SshConfig>,
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

    pub fn resolve_execution(&self, args: &Args, host_count: usize) -> Result<ExecutionConfig> {
        if host_count == 0 {
            anyhow::bail!("execution requires at least one host");
        }

        if matches!(self.controller.concurrency, Some(0)) {
            anyhow::bail!("controller concurrency must be greater than 0");
        }

        let requested = args
            .concurrency
            .map(NonZeroUsize::get)
            .or(self.controller.concurrency)
            .unwrap_or(DEFAULT_CONCURRENCY);

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
            .map(|value| validate_address(value))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| anyhow::anyhow!("node '{}': {}", name, err))?;
        let server_addresses = node
            .server_address
            .iter()
            .map(|value| validate_address(value))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| anyhow::anyhow!("node '{}': {}", name, err))?;
        let bind_addr = normalize_nomad_config_value(node.bind_addr.as_deref(), name, "bind_addr")?;
        let advertise = resolve_advertise(node.advertise.as_ref(), name)?;

        let config = match node.role {
            NodeRole::Server => {
                let bootstrap_expect = node.bootstrap_expect.ok_or_else(|| {
                    anyhow::anyhow!("node '{}' requires bootstrap_expect for server role", name)
                })?;
                if bootstrap_expect == 0 {
                    anyhow::bail!("node '{}' bootstrap_expect must be greater than 0", name);
                }

                NodeConfig {
                    name: name.to_string(),
                    datacenter,
                    version,
                    role: NodeRole::Server,
                    server_config: Some(ServerConfig {
                        bootstrap_expect,
                        server_join_addresses,
                    }),
                    client_config: None,
                    bind_addr,
                    advertise,
                    latency_profile,
                }
            }
            NodeRole::Client => {
                if server_addresses.is_empty() {
                    anyhow::bail!("node '{}' requires at least one server_address", name);
                }

                NodeConfig {
                    name: name.to_string(),
                    datacenter,
                    version,
                    role: NodeRole::Client,
                    server_config: None,
                    client_config: Some(ClientConfig { server_addresses }),
                    bind_addr,
                    advertise,
                    latency_profile,
                }
            }
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

fn validate_address(val: &str) -> std::result::Result<String, String> {
    let val = val.trim();
    if val.is_empty() {
        return Err("address cannot be empty".to_string());
    }

    if val.parse::<IpAddr>().is_ok() {
        return Ok(val.to_string());
    }

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
            }
            return Ok(val.to_string());
        }
        return Err(format!("missing closing ']' in '{}'", val));
    }

    let (host, _) = if let Some(idx) = val.rfind(':') {
        let maybe_port = &val[idx + 1..];
        if maybe_port.chars().all(|c| c.is_ascii_digit()) && !maybe_port.is_empty() {
            let port: u16 = maybe_port
                .parse()
                .map_err(|_| format!("invalid port in '{}': port must be 1-65535", val))?;
            if port == 0 {
                return Err(format!("invalid port in '{}': port must be 1-65535", val));
            }
            (&val[..idx], Some(port))
        } else {
            (val, None)
        }
    } else {
        (val, None)
    };

    if host.parse::<IpAddr>().is_ok() {
        return Ok(val.to_string());
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

    Ok(val.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn args_with_concurrency(concurrency: Option<usize>) -> Args {
        Args {
            inventory: PathBuf::from("inventory.toml"),
            phase: None,
            up_to: None,
            preflight_only: false,
            dry_run: false,
            log_level: "info".to_string(),
            concurrency: concurrency.and_then(NonZeroUsize::new),
        }
    }

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
        role = "server"
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
        role = "client"
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
        assert_eq!(server.datacenter, "homelab");
        assert_eq!(server.version, "1.7.6");
        assert_eq!(server.bind_addr.as_deref(), Some("10.0.1.10"));
        assert_eq!(server.advertise.address.as_deref(), Some("10.0.1.20"));
        assert_eq!(server.latency_profile, LatencyProfile::HighLatency);

        let client = &nodes[1].config;
        assert_eq!(client.bind_addr, None);
        assert_eq!(client.advertise.http.as_deref(), Some("10.0.2.20"));
        assert_eq!(client.advertise.rpc.as_deref(), Some("10.0.2.21"));
        assert_eq!(client.advertise.serf.as_deref(), Some("10.0.2.22:4648"));
        assert_eq!(client.latency_profile, LatencyProfile::Standard);
    }

    #[test]
    fn test_inventory_resolves_execution_from_controller_defaults() {
        let inventory: Inventory = toml::from_str(INVENTORY).expect("inventory parses");

        let execution = inventory
            .resolve_execution(&args_with_concurrency(None), 2)
            .expect("execution resolves");

        assert_eq!(execution.concurrency, 2);
    }

    #[test]
    fn test_cli_concurrency_override_takes_precedence() {
        let inventory: Inventory = toml::from_str(INVENTORY).expect("inventory parses");

        let execution = inventory
            .resolve_execution(&args_with_concurrency(Some(1)), 2)
            .expect("execution resolves");

        assert_eq!(execution.concurrency, 1);
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
            role = "server"
            bootstrap_expect = 1
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_execution(&args_with_concurrency(None), 1)
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
            role = "server"
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
            log_level: "info".to_string(),
            concurrency: None,
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
            role = "client"
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
            role = "server"
        "#,
        )
        .expect("inventory parses");

        let err = inventory
            .resolve_nodes()
            .expect_err("expected validation error");
        assert!(err.to_string().contains("bootstrap_expect"));
    }

    #[test]
    fn test_validate_address_rejects_invalid_hostname() {
        let err = validate_address("bad host").expect_err("expected invalid hostname");
        assert!(err.contains("invalid characters"));
    }

    #[test]
    fn test_inventory_rejects_empty_bind_addr() {
        let inventory: Inventory = toml::from_str(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
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
            role = "server"
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
}
