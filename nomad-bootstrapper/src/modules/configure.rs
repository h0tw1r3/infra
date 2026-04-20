use anyhow::Result;

use crate::debian::{normalize_config, DebianHost};
use crate::executor::PhaseExecutor;
use crate::models::{
    ClientConfig, ExecutionContext, LatencyProfile, NodeConfig, NodeRole, PhaseResult, ServerConfig,
};

pub struct Configure;

const NOMAD_CONFIG_PATH: &str = "/etc/nomad.d/nomad.hcl";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0";
const DEFAULT_ADVERTISE_ADDR: &str = "{{ GetInterfaceIP \"default\" }}";

impl PhaseExecutor for Configure {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let desired_config = render_config(config)?;
        let existing_config = host.read_privileged_file(NOMAD_CONFIG_PATH)?;

        let matches = existing_config
            .as_deref()
            .map(normalize_config)
            .map(|current| current == normalize_config(&desired_config))
            .unwrap_or(false);

        if matches {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "nomad configuration already matches desired state",
            ));
        }

        host.write_config_validated(
            NOMAD_CONFIG_PATH,
            &desired_config,
            "nomad agent -validate -config \"$tmp\"",
        )?;
        ctx.mark_restart_required();

        Ok(PhaseResult::changed(
            self.name(),
            "wrote nomad configuration and flagged service restart",
        ))
    }

    fn name(&self) -> &'static str {
        "configure"
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
}
