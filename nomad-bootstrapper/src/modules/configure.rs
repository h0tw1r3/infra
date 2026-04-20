use anyhow::Result;
use log::warn;
use std::collections::HashMap;

use crate::debian::{normalize_config, DebianHost};
use crate::executor::PhaseExecutor;
use crate::models::{
    ClientConfig, ExecutionContext, LatencyProfile, NodeConfig, NodeRole, PhaseResult, ServerConfig,
};

pub struct Configure;

const NOMAD_CONFIG_DIR: &str = "/etc/nomad.d";
const NOMAD_CONFIG_PATH: &str = "/etc/nomad.d/nomad.hcl";
const NOMAD_ENV_PATH: &str = "/etc/nomad.d/nomad.env";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0";
const DEFAULT_ADVERTISE_ADDR: &str = "{{ GetInterfaceIP \"default\" }}";

/// Kernel module persistence file managed by this bootstrapper.
const MODULES_LOAD_PATH: &str = "/etc/modules-load.d/nomad-br_netfilter.conf";
const MODULES_LOAD_CONTENT: &str = "br_netfilter\n";

/// Sysctl bridge settings file managed by this bootstrapper.
/// Only the two settings required by Nomad's CNI bridge networking mode are managed here.
/// `net.bridge.bridge-nf-call-arptables` is intentionally excluded: it is not required
/// by Nomad and may be absent on some kernels.
const SYSCTL_BRIDGE_PATH: &str = "/etc/sysctl.d/nomad-bridge.conf";
const SYSCTL_BRIDGE_CONTENT: &str = "net.bridge.bridge-nf-call-iptables = 1\n\
    net.bridge.bridge-nf-call-ip6tables = 1\n";

impl PhaseExecutor for Configure {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        audit_config_dir(host, ctx)?;

        let desired_hcl = render_config(config)?;
        let desired_env = render_env_content(&config.env_vars)?;

        let existing_hcl = host.read_privileged_file(NOMAD_CONFIG_PATH)?;
        let existing_env = host.read_privileged_file(NOMAD_ENV_PATH)?;

        let hcl_changed = existing_hcl
            .as_deref()
            .map(|current| normalize_config(current) != normalize_config(&desired_hcl))
            .unwrap_or(true);

        let env_changed = existing_env
            .as_deref()
            .map(|current| current != desired_env)
            .unwrap_or(true);

        let cni_changed = if config.has_role(NodeRole::Client) {
            ensure_br_netfilter(host)?
        } else {
            false
        };

        if !hcl_changed && !env_changed && !cni_changed {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "nomad.hcl, nomad.env, and bridge networking already match desired state",
            ));
        }

        if hcl_changed {
            host.write_config_validated(
                NOMAD_CONFIG_PATH,
                &desired_hcl,
                "nomad config validate \"$tmp\"",
            )?;
        }

        if env_changed {
            host.write_env_file(NOMAD_ENV_PATH, &desired_env)?;
        }

        if hcl_changed || env_changed {
            ctx.mark_restart_required();
        }

        let mut parts = Vec::new();
        match (hcl_changed, env_changed) {
            (true, true) => parts.push("wrote nomad.hcl and nomad.env".to_string()),
            (true, false) => parts.push("wrote nomad.hcl".to_string()),
            (false, true) => parts.push("wrote nomad.env".to_string()),
            _ => {}
        }
        if hcl_changed || env_changed {
            parts.push("flagged service restart".to_string());
        }
        if cni_changed {
            parts.push("applied bridge networking settings".to_string());
        }

        Ok(PhaseResult::changed(self.name(), parts.join(", ")))
    }

    fn name(&self) -> &'static str {
        "configure"
    }
}

/// Renders environment variables to systemd `EnvironmentFile=`-compatible content.
///
/// Keys are validated against `[A-Za-z_][A-Za-z0-9_]*`. Values are always
/// double-quoted, with `\` and `"` escaped, so spaces, `#`, and other special
/// characters are safe. Output is sorted deterministically by key.
///
/// # Errors
/// Returns an error if any key contains invalid characters.
pub fn render_env_content(vars: &HashMap<String, String>) -> Result<String> {
    let mut keys: Vec<&str> = vars.keys().map(String::as_str).collect();
    keys.sort_unstable();

    let mut out = String::new();
    for key in keys {
        if !is_valid_env_key(key) {
            anyhow::bail!(
                "invalid environment variable name {:?}: must match [A-Za-z_][A-Za-z0-9_]*",
                key
            );
        }
        let value = &vars[key];
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!("{}=\"{}\"\n", key, escaped));
    }
    Ok(out)
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checks `/etc/nomad.d` for unrecognized `.hcl` files that Nomad would silently load.
///
/// With `--force`, unknown files are deleted with a warning. Without it, the phase
/// fails with a list of offending paths.
fn audit_config_dir(host: &DebianHost<'_>, ctx: &ExecutionContext) -> Result<()> {
    let known: std::collections::HashSet<&str> = [NOMAD_CONFIG_PATH].into();
    let found = host.list_hcl_files(NOMAD_CONFIG_DIR)?;
    let unknown: Vec<String> = found
        .into_iter()
        .filter(|path| !known.contains(path.as_str()))
        .collect();

    if unknown.is_empty() {
        return Ok(());
    }

    if ctx.force {
        for path in &unknown {
            warn!("removing unrecognized config file: {}", path);
            host.remove_file(path)?;
        }
        Ok(())
    } else {
        anyhow::bail!(
            "unrecognized .hcl files found in {}; remove them or re-run with --force to delete automatically:\n  {}",
            NOMAD_CONFIG_DIR,
            unknown.join("\n  ")
        )
    }
}

fn render_config(config: &NodeConfig) -> Result<String> {
    let bind_addr = config.bind_addr.as_deref().unwrap_or(DEFAULT_BIND_ADDR);
    let advertise = render_advertise(config, bind_addr);
    let mut lines = vec![
        format!("name = {}", render_hcl_string(&config.name)),
        format!("datacenter = {}", render_hcl_string(&config.datacenter)),
        "data_dir = \"/opt/nomad\"".to_string(),
        format!("plugin_dir = {}", render_hcl_string(&config.plugin_dir)),
        format!("bind_addr = {}", render_hcl_string(bind_addr)),
        String::new(),
        "advertise {".to_string(),
        format!("  http = {}", render_hcl_string(&advertise.http)),
        format!("  rpc  = {}", render_hcl_string(&advertise.rpc)),
        format!("  serf = {}", render_hcl_string(&advertise.serf)),
        "}".to_string(),
    ];

    if config.has_role(NodeRole::Server) {
        lines.extend(render_server_block(config.server_config()?));
    }

    if config.has_role(NodeRole::Client) {
        lines.extend(render_client_block(config.client_config()?));
        if !config.plugins.is_empty() {
            lines.extend(render_plugin_blocks(&config.plugins)?);
        }
    } else if !config.plugins.is_empty() {
        // Intentional design: plugin config is only meaningful on client nodes.
        // Non-client nodes skip rendering entirely and emit a named warning so
        // operators can identify and fix inventory misconfigurations. This means
        // invalid plugin keys on non-client nodes produce a warning rather than
        // an error, because rendering (and therefore key validation) is never
        // reached. This asymmetry is expected behaviour.
        let plugin_names: Vec<&str> = {
            let mut names: Vec<&str> = config.plugins.keys().map(String::as_str).collect();
            names.sort_unstable();
            names
        };
        warn!(
            "node \"{}\" has plugins configured ({}) but has no client role; skipping plugin rendering",
            config.name,
            plugin_names.join(", ")
        );
    }

    if config.latency_profile == LatencyProfile::HighLatency {
        lines.extend([
            String::new(),
            "server_join {".to_string(),
            "  retry_max = 12".to_string(),
            "  retry_interval = \"30s\"".to_string(),
            "}".to_string(),
        ]);
    }

    Ok(lines.join("\n") + "\n")
}

fn render_server_block(server: &ServerConfig) -> Vec<String> {
    let retry_join = server
        .server_join_addresses
        .iter()
        .map(|address| format!("  {},", render_hcl_string(address)))
        .collect::<Vec<_>>()
        .join("\n");

    let mut lines = vec![
        String::new(),
        "server {".to_string(),
        "  enabled = true".to_string(),
        format!("  bootstrap_expect = {}", server.bootstrap_expect),
    ];

    if !retry_join.is_empty() {
        lines.push("  retry_join = [".to_string());
        lines.push(retry_join);
        lines.push("  ]".to_string());
    }

    lines.push("}".to_string());
    lines
}

fn render_client_block(client: &ClientConfig) -> Vec<String> {
    let servers = client
        .server_addresses
        .iter()
        .map(|address| format!("  {},", render_hcl_string(address)))
        .collect::<Vec<_>>()
        .join("\n");

    vec![
        String::new(),
        "client {".to_string(),
        "  enabled = true".to_string(),
        "  servers = [".to_string(),
        servers
            .lines()
            .map(|l| format!("  {}", l))
            .collect::<Vec<_>>()
            .join("\n"),
        "  ]".to_string(),
        "}".to_string(),
    ]
}

struct RenderedAdvertise {
    http: String,
    rpc: String,
    serf: String,
}

fn render_advertise(config: &NodeConfig, bind_addr: &str) -> RenderedAdvertise {
    let fallback = advertise_fallback(config, bind_addr);
    let advertise = &config.advertise;

    RenderedAdvertise {
        http: advertise
            .http
            .as_deref()
            .or(advertise.address.as_deref())
            .unwrap_or(fallback)
            .to_string(),
        rpc: advertise
            .rpc
            .as_deref()
            .or(advertise.address.as_deref())
            .unwrap_or(fallback)
            .to_string(),
        serf: advertise
            .serf
            .as_deref()
            .or(advertise.address.as_deref())
            .unwrap_or(fallback)
            .to_string(),
    }
}

fn advertise_fallback<'a>(config: &'a NodeConfig, bind_addr: &'a str) -> &'a str {
    match config.bind_addr.as_deref() {
        Some(addr) if addr != DEFAULT_BIND_ADDR => bind_addr,
        _ => DEFAULT_ADVERTISE_ADDR,
    }
}

fn render_hcl_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }

    format!("\"{}\"", escaped)
}

/// Renders all plugin stanzas for a client node.
///
/// Plugin names are sorted alphabetically for deterministic output. Each plugin
/// is wrapped as:
/// ```hcl
/// plugin "<name>" {
///   config {
///     <key = value pairs, sorted alphabetically at every nesting level>
///   }
/// }
/// ```
///
/// Plugins with an empty merged config table are skipped entirely — Nomad
/// does not require an explicit config block and empty `config {}` adds noise.
fn render_plugin_blocks(plugins: &HashMap<String, toml::Table>) -> Result<Vec<String>> {
    let mut plugin_names: Vec<&str> = plugins.keys().map(String::as_str).collect();
    plugin_names.sort_unstable();

    let mut lines = Vec::new();
    for name in plugin_names {
        let table = &plugins[name];
        // Skip plugins whose merged config is empty; no stanza needed.
        if table.is_empty() {
            continue;
        }
        lines.push(String::new());
        lines.push(format!("plugin {} {{", render_hcl_string(name)));
        lines.push("  config {".to_string());
        for line in render_plugin_table(table, 4)? {
            lines.push(line);
        }
        lines.push("  }".to_string());
        lines.push("}".to_string());
    }
    Ok(lines)
}

/// Recursively renders a `toml::Table` as indented HCL key/value lines.
///
/// Keys within a table are sorted alphabetically at every level for deterministic
/// output. Nested tables become HCL block syntax. Arrays must contain only scalar
/// values; arrays containing tables or mixed types return an error.
///
/// All keys are validated as HCL bare identifiers via [`render_hcl_key`]; keys
/// containing characters that are invalid in unquoted HCL attribute names return
/// an error rather than emitting potentially invalid config.
fn render_plugin_table(table: &toml::Table, indent: usize) -> Result<Vec<String>> {
    let prefix = " ".repeat(indent);
    let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
    keys.sort_unstable();

    let mut lines = Vec::new();
    for key in keys {
        // Validate key before branching so the rule applies to both block
        // labels (foo { }) and assignment keys (foo = ...).
        let validated_key = render_hcl_key(key)?;
        let val = &table[key];
        match val {
            toml::Value::Table(inner) => {
                lines.push(format!("{}{} {{", prefix, validated_key));
                for line in render_plugin_table(inner, indent + 2)? {
                    lines.push(line);
                }
                lines.push(format!("{}}}", prefix));
            }
            toml::Value::Array(arr) => {
                let rendered = render_plugin_array(arr, key)?;
                lines.push(format!("{}{} = {}", prefix, validated_key, rendered));
            }
            scalar => {
                let rendered = render_plugin_scalar(scalar, key)?;
                lines.push(format!("{}{} = {}", prefix, validated_key, rendered));
            }
        }
    }
    Ok(lines)
}

/// Renders an array of scalar values as an HCL array literal `[v1, v2, ...]`.
///
/// Arrays containing tables or mixed types (non-homogeneous scalars are fine)
/// return an error because Nomad/HCL does not support arrays-of-tables in
/// plugin config and we must not silently produce malformed output.
fn render_plugin_array(arr: &[toml::Value], key: &str) -> Result<String> {
    let mut parts = Vec::with_capacity(arr.len());
    for item in arr {
        match item {
            toml::Value::Table(_) => {
                anyhow::bail!(
                    "plugin config key '{}': arrays of tables are not supported; \
                     use a nested table block instead",
                    key
                );
            }
            toml::Value::Array(_) => {
                anyhow::bail!(
                    "plugin config key '{}': nested arrays are not supported",
                    key
                );
            }
            scalar => {
                parts.push(render_plugin_scalar(scalar, key)?);
            }
        }
    }
    Ok(format!("[{}]", parts.join(", ")))
}

/// Renders a single TOML scalar value to its HCL representation.
///
/// Floats use Rust's default `f64` Display, which produces stable human-readable
/// output (e.g. `1`, `0.5`, `1e10`). Non-finite values (`NaN`, `inf`, `-inf`)
/// are not valid HCL numeric literals and are rejected with an error.
fn render_plugin_scalar(val: &toml::Value, key: &str) -> Result<String> {
    match val {
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => {
            if !f.is_finite() {
                anyhow::bail!(
                    "plugin config key '{}': non-finite float values (NaN, inf) \
                     are not valid HCL literals",
                    key
                );
            }
            Ok(f.to_string())
        }
        toml::Value::String(s) => Ok(render_hcl_string(s)),
        toml::Value::Datetime(dt) => Ok(render_hcl_string(&dt.to_string())),
        toml::Value::Table(_) | toml::Value::Array(_) => {
            anyhow::bail!(
                "plugin config key '{}': unexpected composite value in scalar context",
                key
            );
        }
    }
}

/// Validates and returns `key` as a bare HCL attribute name.
///
/// HCL bare identifiers must match `[a-zA-Z_][a-zA-Z0-9_-]*`. Keys that do not
/// conform are rejected with an error rather than being silently quoted, because
/// Nomad's HCL parser only accepts bare attribute names in configuration body
/// context, and a quoted name that Nomad won't accept would be misleading.
///
/// Valid: `allow_privileged`, `max-files`, `_internal`
/// Invalid: `allow.privileged`, `3count`, `key with spaces`
fn render_hcl_key(key: &str) -> Result<&str> {
    let mut chars = key.chars();
    let valid = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }
        _ => false,
    };
    if valid {
        Ok(key)
    } else {
        anyhow::bail!(
            "plugin config key '{}' is not a valid HCL identifier; \
             keys must match [a-zA-Z_][a-zA-Z0-9_-]*",
            key
        )
    }
}

/// Ensures `br_netfilter` is loaded and the sysctl bridge settings are in place.
///
/// Steps (ordered; each is idempotent):
/// 1. `modprobe br_netfilter` — fail immediately if the kernel module is unavailable.
/// 2. Persist the module via `/etc/modules-load.d/nomad-br_netfilter.conf` (loaded on boot).
/// 3. Write `/etc/sysctl.d/nomad-bridge.conf` with the three `net.bridge.*` settings if
///    the file content differs from the desired state.
/// 4. If the sysctl file was (re)written, apply it with `sysctl -p` scoped to that file.
///
/// Returns `true` if any persistent files were written.
fn ensure_br_netfilter(host: &DebianHost<'_>) -> Result<bool> {
    // Always load the module immediately; bail if the kernel does not support it.
    host.load_kernel_module("br_netfilter")?;

    // Persist the module name so systemd-modules-load loads it automatically on boot.
    let module_file_changed = host
        .read_privileged_file(MODULES_LOAD_PATH)?
        .as_deref()
        .map(|current| current != MODULES_LOAD_CONTENT)
        .unwrap_or(true);

    if module_file_changed {
        host.write_config(MODULES_LOAD_PATH, MODULES_LOAD_CONTENT)?;
    }

    // Write and optionally apply the bridge sysctl settings.
    let sysctl_file_changed = host
        .read_privileged_file(SYSCTL_BRIDGE_PATH)?
        .as_deref()
        .map(|current| normalize_config(current) != normalize_config(SYSCTL_BRIDGE_CONTENT))
        .unwrap_or(true);

    if sysctl_file_changed {
        host.write_config(SYSCTL_BRIDGE_PATH, SYSCTL_BRIDGE_CONTENT)?;
        host.apply_sysctl_file(SYSCTL_BRIDGE_PATH)?;
    }

    Ok(module_file_changed || sysctl_file_changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debian::normalize_config;
    use crate::models::{AdvertiseConfig, ClientConfig, LatencyProfile, NodeRole};

    fn server_node_config() -> NodeConfig {
        NodeConfig {
            name: "server-1".to_string(),
            datacenter: "homelab".to_string(),
            version: "1.7.6".to_string(),
            cni_version: "v1.6.2".to_string(),
            roles: vec![NodeRole::Server],
            server_config: Some(ServerConfig {
                bootstrap_expect: 3,
                server_join_addresses: Vec::new(),
            }),
            client_config: None,
            bind_addr: None,
            advertise: AdvertiseConfig::default(),
            latency_profile: LatencyProfile::Standard,
            env_vars: Default::default(),
            plugins: Default::default(),
            plugin_dir: "/opt/nomad/plugins".to_string(),
            plugin_installs: Default::default(),
        }
    }

    fn client_node_config() -> NodeConfig {
        NodeConfig {
            name: "client-1".to_string(),
            datacenter: "homelab".to_string(),
            version: "1.7.6".to_string(),
            cni_version: "v1.6.2".to_string(),
            roles: vec![NodeRole::Client],
            server_config: None,
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
    fn test_render_server_config() {
        let mut config = server_node_config();
        config.server_config = Some(ServerConfig {
            bootstrap_expect: 3,
            server_join_addresses: vec!["10.0.1.2:4648".to_string()],
        });
        config.latency_profile = LatencyProfile::HighLatency;

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("name = \"server-1\""));
        assert!(rendered.contains("datacenter = \"homelab\""));
        assert!(rendered.contains("bootstrap_expect = 3"));
        assert!(rendered.contains("retry_join"));
    }

    #[test]
    fn test_render_config_preserves_default_network_behavior_without_overrides() {
        let rendered = render_config(&server_node_config()).expect("rendered config");
        let expected = concat!(
            "bind_addr = \"0.0.0.0\"\n\n",
            "advertise {\n",
            "  http = \"{{ GetInterfaceIP \\\"default\\\" }}\"\n",
            "  rpc  = \"{{ GetInterfaceIP \\\"default\\\" }}\"\n",
            "  serf = \"{{ GetInterfaceIP \\\"default\\\" }}\"\n",
            "}\n"
        );

        assert!(rendered.contains(expected));
    }

    #[test]
    fn test_render_config_uses_default_interface_fallback_for_wildcard_bind_addr() {
        let mut config = server_node_config();
        config.bind_addr = Some("0.0.0.0".to_string());

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("bind_addr = \"0.0.0.0\""));
        assert!(rendered.contains("  http = \"{{ GetInterfaceIP \\\"default\\\" }}\""));
        assert!(!rendered.contains("  http = \"0.0.0.0\""));
    }

    #[test]
    fn test_render_config_uses_bind_addr_for_default_advertise() {
        let mut config = server_node_config();
        config.bind_addr = Some("10.0.1.10".to_string());

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("bind_addr = \"10.0.1.10\""));
        assert!(rendered.contains("  http = \"10.0.1.10\""));
        assert!(rendered.contains("  rpc  = \"10.0.1.10\""));
        assert!(rendered.contains("  serf = \"10.0.1.10\""));
    }

    #[test]
    fn test_render_config_combines_shared_and_protocol_advertise_values() {
        let mut config = server_node_config();
        config.bind_addr = Some("10.0.1.10".to_string());
        config.advertise = AdvertiseConfig {
            address: Some("10.0.1.20".to_string()),
            http: None,
            rpc: Some("10.0.1.21".to_string()),
            serf: None,
        };

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("  http = \"10.0.1.20\""));
        assert!(rendered.contains("  rpc  = \"10.0.1.21\""));
        assert!(rendered.contains("  serf = \"10.0.1.20\""));
    }

    #[test]
    fn test_render_config_preserves_template_passthrough_and_escapes_for_hcl() {
        let mut config = server_node_config();
        config.bind_addr = Some("host\\name".to_string());
        config.advertise = AdvertiseConfig {
            address: Some("{{ GetInterfaceIP \"eth0\" }}".to_string()),
            http: Some("{{ GetInterfaceIP \"eth0\" }}".to_string()),
            rpc: None,
            serf: None,
        };

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("bind_addr = \"host\\\\name\""));
        assert!(rendered.contains("  http = \"{{ GetInterfaceIP \\\"eth0\\\" }}\""));
        assert!(rendered.contains("  rpc  = \"{{ GetInterfaceIP \\\"eth0\\\" }}\""));
        assert!(rendered.contains("  serf = \"{{ GetInterfaceIP \\\"eth0\\\" }}\""));
    }

    #[test]
    fn test_render_client_config() {
        let rendered = render_config(&client_node_config()).expect("rendered config");
        assert!(rendered.contains("client {"));
        assert!(rendered.contains("enabled = true"));
        assert!(rendered.contains("\"10.0.1.1:4647\""));
        assert!(!rendered.contains("server {"));

        // servers must be nested inside the client block, not at the top level
        let client_pos = rendered.find("client {").unwrap();
        let servers_pos = rendered.find("servers = [").unwrap();
        let close_pos = rendered[client_pos..].find('}').unwrap() + client_pos;
        assert!(
            servers_pos > client_pos && servers_pos < close_pos,
            "servers should be inside client {{ }} block"
        );
    }

    #[test]
    fn test_render_config_includes_plugin_dir() {
        let rendered = render_config(&server_node_config()).expect("rendered config");
        assert!(rendered.contains("plugin_dir = \"/opt/nomad/plugins\""));
    }

    #[test]
    fn test_render_config_uses_custom_plugin_dir() {
        let mut config = server_node_config();
        config.plugin_dir = "/custom/plugins".to_string();

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("plugin_dir = \"/custom/plugins\""));
    }

    #[test]
    fn test_render_dual_role_config() {
        let mut config = server_node_config();
        config.roles.push(NodeRole::Client);
        config.client_config = Some(ClientConfig {
            server_addresses: vec!["10.0.1.1:4647".to_string()],
        });

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("server {"));
        assert!(rendered.contains("client {"));
        assert!(rendered.contains("bootstrap_expect = 3"));
        assert!(rendered.contains("\"10.0.1.1:4647\""));
    }

    #[test]
    fn test_render_config_rejects_server_role_without_server_payload() {
        let mut config = server_node_config();
        config.server_config = None;

        let err = render_config(&config).expect_err("expected invariant failure");
        assert!(err
            .to_string()
            .contains("server configuration is not available for roles server"));
    }

    #[test]
    fn test_render_config_rejects_client_role_without_client_payload() {
        let mut config = client_node_config();
        config.client_config = None;

        let err = render_config(&config).expect_err("expected invariant failure");
        assert!(err
            .to_string()
            .contains("client configuration is not available for roles client"));
    }

    #[test]
    fn test_audit_config_dir_passes_when_only_known_files_present() {
        use crate::debian::DebianHost;
        use crate::test_helpers::{recording_target, RecordingTransport};
        use crate::transport::{RemoteHost, RemoteOutput};

        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "/etc/nomad.d/nomad.hcl\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let ctx = ExecutionContext::default();

        audit_config_dir(&host, &ctx).expect("known file should pass audit");
    }

    #[test]
    fn test_audit_config_dir_fails_on_unknown_hcl_file() {
        use crate::debian::DebianHost;
        use crate::test_helpers::{recording_target, RecordingTransport};
        use crate::transport::{RemoteHost, RemoteOutput};

        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "/etc/nomad.d/nomad.hcl\n/etc/nomad.d/extra.hcl\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let ctx = ExecutionContext::default();

        let err = audit_config_dir(&host, &ctx).expect_err("unknown file should fail audit");
        assert!(err.to_string().contains("unrecognized .hcl files found"));
        assert!(err.to_string().contains("extra.hcl"));
    }

    #[test]
    fn test_audit_config_dir_removes_unknown_file_with_force() {
        use crate::debian::DebianHost;
        use crate::test_helpers::{recording_target, RecordingTransport};
        use crate::transport::{RemoteHost, RemoteOutput};

        let transport = RecordingTransport::new(vec![
            // id -u for list_files_privileged
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // find output: known + unknown
            RemoteOutput {
                status: 0,
                stdout: "/etc/nomad.d/nomad.hcl\n/etc/nomad.d/stray.hcl\n".to_string(),
                stderr: String::new(),
            },
            // id -u for remove_file
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // rm -f
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();
        ctx.force = true;

        audit_config_dir(&host, &ctx).expect("force should delete and succeed");

        let commands = transport.commands.lock().expect("commands lock");
        assert!(commands.iter().any(|c| c.contains("rm -f")));
        assert!(commands.iter().any(|c| c.contains("stray.hcl")));
    }

    // --- render_env_content tests ---

    #[test]
    fn test_render_env_content_empty_vars_produces_empty_string() {
        let content = render_env_content(&HashMap::new()).expect("render empty");
        assert_eq!(content, "");
    }

    #[test]
    fn test_render_env_content_sorts_and_quotes_values() {
        let mut vars = HashMap::new();
        vars.insert("ZEBRA".to_string(), "last".to_string());
        vars.insert("APPLE".to_string(), "first".to_string());
        let content = render_env_content(&vars).expect("render");
        assert_eq!(content, "APPLE=\"first\"\nZEBRA=\"last\"\n");
    }

    #[test]
    fn test_render_env_content_escapes_backslash_and_double_quote() {
        let mut vars = HashMap::new();
        vars.insert(
            "KEY".to_string(),
            r#"has "quotes" and \slashes\"#.to_string(),
        );
        let content = render_env_content(&vars).expect("render");
        assert!(content.contains(r#"KEY="has \"quotes\" and \\slashes\\""#));
    }

    #[test]
    fn test_render_env_content_handles_value_with_spaces_and_hash() {
        let mut vars = HashMap::new();
        vars.insert("MSG".to_string(), "hello world # not a comment".to_string());
        let content = render_env_content(&vars).expect("render");
        assert_eq!(content, "MSG=\"hello world # not a comment\"\n");
    }

    #[test]
    fn test_render_env_content_handles_empty_value() {
        let mut vars = HashMap::new();
        vars.insert("EMPTY".to_string(), String::new());
        let content = render_env_content(&vars).expect("render");
        assert_eq!(content, "EMPTY=\"\"\n");
    }

    #[test]
    fn test_render_env_content_rejects_invalid_key_starting_with_digit() {
        let mut vars = HashMap::new();
        vars.insert("1INVALID".to_string(), "value".to_string());
        let err = render_env_content(&vars).expect_err("should reject invalid key");
        assert!(err
            .to_string()
            .contains("invalid environment variable name"));
    }

    #[test]
    fn test_render_env_content_rejects_key_with_hyphen() {
        let mut vars = HashMap::new();
        vars.insert("INVALID-KEY".to_string(), "value".to_string());
        let err = render_env_content(&vars).expect_err("should reject hyphenated key");
        assert!(err
            .to_string()
            .contains("invalid environment variable name"));
    }

    #[test]
    fn test_render_env_content_accepts_underscore_prefix() {
        let mut vars = HashMap::new();
        vars.insert("_PRIVATE".to_string(), "ok".to_string());
        let content = render_env_content(&vars).expect("underscore prefix is valid");
        assert_eq!(content, "_PRIVATE=\"ok\"\n");
    }

    // ── ensure_br_netfilter ───────────────────────────────────────────────────

    use crate::debian::DebianHost;
    use crate::test_helpers::{recording_target, RecordingTransport};
    use crate::transport::{RemoteHost, RemoteOutput};

    /// Recording responses for a successful `modprobe br_netfilter` call (uid=0).
    fn modprobe_ok_responses() -> Vec<RemoteOutput> {
        vec![
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
        ]
    }

    #[test]
    fn test_ensure_br_netfilter_writes_both_files_when_absent() {
        let mut responses = modprobe_ok_responses();
        responses.extend([
            // id -u + read modules-load file → absent
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
            // id -u + write modules-load file
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
            // id -u + read sysctl file → absent
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
            // id -u + write sysctl file
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
            // id -u + sysctl -p
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
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        let changed = ensure_br_netfilter(&host).expect("ensure_br_netfilter succeeds");
        assert!(changed);

        let commands = transport.commands.lock().expect("commands lock");
        assert!(commands.iter().any(|c| c.contains("modprobe br_netfilter")));
        assert!(commands
            .iter()
            .any(|c| c.contains("nomad-br_netfilter.conf")));
        assert!(commands
            .iter()
            .any(|c| c.contains("nomad-bridge.conf") && c.contains("cat >")));
        assert!(commands
            .iter()
            .any(|c| c.contains("sysctl -p") && c.contains("nomad-bridge.conf")));
    }

    #[test]
    fn test_ensure_br_netfilter_skips_writes_when_already_configured() {
        let mut responses = modprobe_ok_responses();
        responses.extend([
            // id -u + read modules-load file → matches
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "br_netfilter\n".to_string(),
                stderr: String::new(),
            },
            // id -u + read sysctl file → matches
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "net.bridge.bridge-nf-call-iptables = 1\nnet.bridge.bridge-nf-call-ip6tables = 1\n".to_string(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        let changed = ensure_br_netfilter(&host).expect("ensure_br_netfilter succeeds");
        assert!(!changed);

        let commands = transport.commands.lock().expect("commands lock");
        // modprobe still runs (load on every run), but no file writes and no sysctl -p
        assert!(commands.iter().any(|c| c.contains("modprobe")));
        assert!(!commands.iter().any(|c| c.contains("sysctl -p")));
        assert!(!commands
            .iter()
            .any(|c| c.contains("mv") && c.contains("nomad-bridge")));
    }

    #[test]
    fn test_ensure_br_netfilter_fails_when_modprobe_fails() {
        let transport = RecordingTransport::new(vec![
            // id -u
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // modprobe → fails
            RemoteOutput {
                status: 1,
                stdout: String::new(),
                stderr: "modprobe: FATAL: Module br_netfilter not found".to_string(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        let err = ensure_br_netfilter(&host).expect_err("should fail if modprobe fails");
        assert!(err.to_string().contains("command failed"));
    }

    #[test]
    fn test_ensure_br_netfilter_rewrites_sysctl_when_content_differs() {
        let mut responses = modprobe_ok_responses();
        responses.extend([
            // id -u + read modules-load file → matches
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "br_netfilter\n".to_string(),
                stderr: String::new(),
            },
            // id -u + read sysctl file → stale content
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "net.bridge.bridge-nf-call-iptables = 0\n".to_string(),
                stderr: String::new(),
            },
            // id -u + write sysctl
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
            // id -u + sysctl -p
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
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        let changed = ensure_br_netfilter(&host).expect("ensure_br_netfilter succeeds");
        assert!(changed);

        let commands = transport.commands.lock().expect("commands lock");
        assert!(commands
            .iter()
            .any(|c| c.contains("sysctl -p") && c.contains("nomad-bridge.conf")));
    }

    // ── Plugin rendering tests ───────────────────────────────────────────────

    fn make_plugin_table(pairs: &[(&str, toml::Value)]) -> toml::Table {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_render_plugin_blocks_empty_produces_no_lines() {
        let plugins = HashMap::new();
        let lines = render_plugin_blocks(&plugins).expect("render succeeds");
        assert!(lines.is_empty());
    }

    #[test]
    fn test_render_plugin_blocks_single_flat_plugin() {
        let table = make_plugin_table(&[("enabled", toml::Value::Boolean(true))]);
        let mut plugins = HashMap::new();
        plugins.insert("raw_exec".to_string(), table);

        let output = render_plugin_blocks(&plugins)
            .expect("render succeeds")
            .join("\n");

        assert!(output.contains("plugin \"raw_exec\" {"), "plugin wrapper");
        assert!(output.contains("config {"), "config block");
        assert!(output.contains("    enabled = true"), "scalar value");
    }

    #[test]
    fn test_render_plugin_blocks_nested_table() {
        let volumes = make_plugin_table(&[
            ("enabled", toml::Value::Boolean(true)),
            ("selinuxlabel", toml::Value::String("z".to_string())),
        ]);
        let table = make_plugin_table(&[
            ("allow_privileged", toml::Value::Boolean(false)),
            ("volumes", toml::Value::Table(volumes)),
        ]);
        let mut plugins = HashMap::new();
        plugins.insert("docker".to_string(), table);

        let output = render_plugin_blocks(&plugins)
            .expect("render succeeds")
            .join("\n");

        assert!(output.contains("plugin \"docker\" {"));
        assert!(output.contains("allow_privileged = false"));
        assert!(output.contains("volumes {"));
        assert!(output.contains("enabled = true"));
        assert!(output.contains("selinuxlabel = \"z\""));
    }

    #[test]
    fn test_render_plugin_blocks_multi_plugin_sorted_output() {
        let mut plugins = HashMap::new();
        plugins.insert(
            "raw_exec".to_string(),
            make_plugin_table(&[("enabled", toml::Value::Boolean(true))]),
        );
        plugins.insert(
            "docker".to_string(),
            make_plugin_table(&[("allow_privileged", toml::Value::Boolean(false))]),
        );

        let output = render_plugin_blocks(&plugins)
            .expect("render succeeds")
            .join("\n");

        let docker_pos = output.find("plugin \"docker\"").expect("docker present");
        let raw_exec_pos = output
            .find("plugin \"raw_exec\"")
            .expect("raw_exec present");
        assert!(
            docker_pos < raw_exec_pos,
            "docker must come before raw_exec (alphabetical)"
        );
    }

    #[test]
    fn test_render_plugin_blocks_keys_sorted_within_block() {
        let table = make_plugin_table(&[
            ("z_key", toml::Value::Boolean(true)),
            ("a_key", toml::Value::Boolean(false)),
        ]);
        let mut plugins = HashMap::new();
        plugins.insert("raw_exec".to_string(), table);

        let output = render_plugin_blocks(&plugins)
            .expect("render succeeds")
            .join("\n");

        let a_pos = output.find("a_key").expect("a_key present");
        let z_pos = output.find("z_key").expect("z_key present");
        assert!(a_pos < z_pos, "a_key must come before z_key (alphabetical)");
    }

    #[test]
    fn test_render_plugin_blocks_array_of_scalars() {
        let table = make_plugin_table(&[(
            "allowed_images",
            toml::Value::Array(vec![
                toml::Value::String("nginx:latest".to_string()),
                toml::Value::String("redis:7".to_string()),
            ]),
        )]);
        let mut plugins = HashMap::new();
        plugins.insert("docker".to_string(), table);

        let output = render_plugin_blocks(&plugins)
            .expect("render succeeds")
            .join("\n");

        assert!(output.contains("allowed_images = [\"nginx:latest\", \"redis:7\"]"));
    }

    #[test]
    fn test_render_plugin_blocks_array_of_tables_returns_error() {
        let inner = make_plugin_table(&[("key", toml::Value::Boolean(true))]);
        let table = make_plugin_table(&[(
            "bad_array",
            toml::Value::Array(vec![toml::Value::Table(inner)]),
        )]);
        let mut plugins = HashMap::new();
        plugins.insert("docker".to_string(), table);

        let err = render_plugin_blocks(&plugins).expect_err("should fail");
        assert!(
            err.to_string()
                .contains("arrays of tables are not supported"),
            "error message must explain the restriction: {}",
            err
        );
    }

    // ── Role gating tests ────────────────────────────────────────────────────

    #[test]
    fn test_render_hcl_key_accepts_valid_identifiers() {
        assert_eq!(render_hcl_key("enabled").unwrap(), "enabled");
        assert_eq!(
            render_hcl_key("allow_privileged").unwrap(),
            "allow_privileged"
        );
        assert_eq!(render_hcl_key("max-files").unwrap(), "max-files");
        assert_eq!(render_hcl_key("_internal").unwrap(), "_internal");
        assert_eq!(render_hcl_key("key123").unwrap(), "key123");
    }

    #[test]
    fn test_render_hcl_key_rejects_invalid_identifiers() {
        // Starts with digit
        assert!(render_hcl_key("3count").is_err());
        // Contains dot
        assert!(render_hcl_key("allow.privileged").is_err());
        // Contains space
        assert!(render_hcl_key("key with spaces").is_err());
        // Empty string
        assert!(render_hcl_key("").is_err());
    }

    #[test]
    fn test_render_plugin_blocks_invalid_key_returns_error() {
        // End-to-end: invalid key in plugin table must bubble up as an error.
        let table = make_plugin_table(&[("allow.privileged", toml::Value::Boolean(false))]);
        let mut plugins = HashMap::new();
        plugins.insert("docker".to_string(), table);

        let err = render_plugin_blocks(&plugins).expect_err("invalid key must fail");
        assert!(
            err.to_string().contains("not a valid HCL identifier"),
            "error must explain key validation: {}",
            err
        );
    }

    #[test]
    fn test_render_plugin_blocks_empty_table_is_skipped() {
        // A plugin with an empty merged config produces no stanza.
        let mut plugins = HashMap::new();
        plugins.insert("raw_exec".to_string(), toml::Table::new());

        let lines = render_plugin_blocks(&plugins).expect("render succeeds");
        assert!(
            lines.is_empty(),
            "empty plugin config table must produce no output lines"
        );
    }

    #[test]
    fn test_render_plugin_blocks_mixed_empty_and_nonempty() {
        // Only the non-empty plugin renders; the empty one is silently skipped.
        let mut plugins = HashMap::new();
        plugins.insert("raw_exec".to_string(), toml::Table::new());
        plugins.insert(
            "docker".to_string(),
            make_plugin_table(&[("enabled", toml::Value::Boolean(true))]),
        );

        let output = render_plugin_blocks(&plugins)
            .expect("render succeeds")
            .join("\n");

        assert!(output.contains("plugin \"docker\""), "docker must render");
        assert!(
            !output.contains("plugin \"raw_exec\""),
            "empty raw_exec must be skipped"
        );
    }

    #[test]
    fn test_render_plugin_scalar_rejects_nan() {
        let err = render_plugin_scalar(&toml::Value::Float(f64::NAN), "timeout")
            .expect_err("NaN must fail");
        assert!(err.to_string().contains("non-finite float"), "{}", err);
    }

    #[test]
    fn test_render_plugin_scalar_rejects_inf() {
        let err = render_plugin_scalar(&toml::Value::Float(f64::INFINITY), "ratio")
            .expect_err("inf must fail");
        assert!(err.to_string().contains("non-finite float"), "{}", err);
    }

    #[test]
    fn test_render_plugin_scalar_finite_float_renders() {
        assert_eq!(
            render_plugin_scalar(&toml::Value::Float(1.5), "ratio").unwrap(),
            "1.5"
        );
        assert_eq!(
            render_plugin_scalar(&toml::Value::Float(0.0), "ratio").unwrap(),
            "0"
        );
    }

    // Non-client node + invalid key → warning only (rendering skipped, no error).
    // Client node + invalid key → error from render_plugin_blocks.
    #[test]
    fn test_non_client_invalid_key_warns_not_errors() {
        let table = make_plugin_table(&[("allow.privileged", toml::Value::Boolean(false))]);
        let mut config = server_node_config();
        config.plugins.insert("docker".to_string(), table);

        // render_config must succeed even though the key is invalid, because
        // rendering is never attempted for non-client nodes.
        let rendered = render_config(&config).expect("server-only node must not error");
        assert!(
            !rendered.contains("plugin \""),
            "no plugin stanza must appear for server-only node"
        );
    }

    #[test]
    fn test_client_invalid_key_returns_error() {
        let table = make_plugin_table(&[("allow.privileged", toml::Value::Boolean(false))]);
        let mut config = client_node_config();
        config.plugins.insert("docker".to_string(), table);

        let err = render_config(&config).expect_err("client node with invalid key must fail");
        assert!(
            err.to_string().contains("not a valid HCL identifier"),
            "error must name the invalid key: {}",
            err
        );
    }

    #[test]
    fn test_render_config_client_with_plugins_renders_plugin_blocks() {
        let table = make_plugin_table(&[("enabled", toml::Value::Boolean(true))]);
        let mut config = client_node_config();
        config.plugins.insert("raw_exec".to_string(), table);

        let rendered = render_config(&config).expect("render succeeds");
        assert!(
            rendered.contains("plugin \"raw_exec\""),
            "plugin block must appear in rendered HCL"
        );
        assert!(rendered.contains("enabled = true"));
    }

    #[test]
    fn test_render_config_server_only_with_plugins_omits_plugin_blocks() {
        let table = make_plugin_table(&[("enabled", toml::Value::Boolean(true))]);
        let mut config = server_node_config();
        config.plugins.insert("raw_exec".to_string(), table);

        let rendered = render_config(&config).expect("render succeeds");
        assert!(
            !rendered.contains("plugin \"raw_exec\""),
            "server-only node must not render plugin blocks"
        );
    }

    #[test]
    fn test_render_config_dual_role_with_plugins_renders_plugin_blocks() {
        let table = make_plugin_table(&[("allow_privileged", toml::Value::Boolean(false))]);
        let mut config = client_node_config();
        // Elevate to dual role.
        config.roles = vec![NodeRole::Server, NodeRole::Client];
        config.server_config = Some(ServerConfig {
            bootstrap_expect: 1,
            server_join_addresses: Vec::new(),
        });
        config.plugins.insert("docker".to_string(), table);

        let rendered = render_config(&config).expect("render succeeds");
        assert!(
            rendered.contains("plugin \"docker\""),
            "dual-role node must render plugin blocks"
        );
    }

    #[test]
    fn test_render_config_empty_plugins_produces_no_plugin_stanzas() {
        let rendered = render_config(&client_node_config()).expect("render succeeds");
        assert!(
            !rendered.contains("plugin "),
            "no plugins configured → no plugin stanzas"
        );
    }

    // ── Restart semantics tests ──────────────────────────────────────────────

    #[test]
    fn test_render_config_plugin_determinism_same_input_same_normalized_output() {
        let table = make_plugin_table(&[
            ("enabled", toml::Value::Boolean(true)),
            ("allow_privileged", toml::Value::Boolean(false)),
        ]);
        let mut config = client_node_config();
        config.plugins.insert("docker".to_string(), table);

        let first = render_config(&config).expect("first render");
        let second = render_config(&config).expect("second render");
        assert_eq!(
            normalize_config(&first),
            normalize_config(&second),
            "identical input must produce identical normalized output"
        );
    }

    #[test]
    fn test_render_config_plugin_diff_sensitivity_changed_value_triggers_diff() {
        let table_before = make_plugin_table(&[("allow_privileged", toml::Value::Boolean(false))]);
        let table_after = make_plugin_table(&[("allow_privileged", toml::Value::Boolean(true))]);

        let mut config = client_node_config();
        config.plugins.insert("docker".to_string(), table_before);
        let before = render_config(&config).expect("before render");

        config.plugins.insert("docker".to_string(), table_after);
        let after = render_config(&config).expect("after render");

        assert_ne!(
            normalize_config(&before),
            normalize_config(&after),
            "changed plugin value must produce different normalized output"
        );
    }
}
