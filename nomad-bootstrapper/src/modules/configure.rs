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

        if !hcl_changed && !env_changed {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "nomad.hcl and nomad.env already match desired state",
            ));
        }

        if hcl_changed {
            host.write_config_validated(
                NOMAD_CONFIG_PATH,
                &desired_hcl,
                "nomad agent -validate -config \"$tmp\"",
            )?;
        }

        if env_changed {
            host.write_env_file(NOMAD_ENV_PATH, &desired_env)?;
        }

        ctx.mark_restart_required();

        let msg = match (hcl_changed, env_changed) {
            (true, true) => "wrote nomad.hcl and nomad.env, flagged service restart",
            (true, false) => "wrote nomad.hcl, flagged service restart",
            (false, true) => "wrote nomad.env, flagged service restart",
            (false, false) => unreachable!(),
        };

        Ok(PhaseResult::changed(self.name(), msg))
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
        "}".to_string(),
        String::new(),
        "servers = [".to_string(),
        servers,
        "]".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AdvertiseConfig, ClientConfig, LatencyProfile, NodeRole};

    fn server_node_config() -> NodeConfig {
        NodeConfig {
            name: "server-1".to_string(),
            datacenter: "homelab".to_string(),
            version: "1.7.6".to_string(),
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
        }
    }

    fn client_node_config() -> NodeConfig {
        NodeConfig {
            name: "client-1".to_string(),
            datacenter: "homelab".to_string(),
            version: "1.7.6".to_string(),
            roles: vec![NodeRole::Client],
            server_config: None,
            client_config: Some(ClientConfig {
                server_addresses: vec!["10.0.1.1:4647".to_string()],
            }),
            bind_addr: None,
            advertise: AdvertiseConfig::default(),
            latency_profile: LatencyProfile::Standard,
            env_vars: Default::default(),
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
        assert!(rendered.contains("servers = ["));
        assert!(rendered.contains("\"10.0.1.1:4647\""));
        assert!(!rendered.contains("server {"));
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
        vars.insert("KEY".to_string(), r#"has "quotes" and \slashes\"#.to_string());
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
        assert!(err.to_string().contains("invalid environment variable name"));
    }

    #[test]
    fn test_render_env_content_rejects_key_with_hyphen() {
        let mut vars = HashMap::new();
        vars.insert("INVALID-KEY".to_string(), "value".to_string());
        let err = render_env_content(&vars).expect_err("should reject hyphenated key");
        assert!(err.to_string().contains("invalid environment variable name"));
    }

    #[test]
    fn test_render_env_content_accepts_underscore_prefix() {
        let mut vars = HashMap::new();
        vars.insert("_PRIVATE".to_string(), "ok".to_string());
        let content = render_env_content(&vars).expect("underscore prefix is valid");
        assert_eq!(content, "_PRIVATE=\"ok\"\n");
    }
}
