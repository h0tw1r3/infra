use anyhow::Result;

use crate::debian::{normalize_config, DebianHost};
use crate::executor::PhaseExecutor;
use crate::models::{
    ExecutionContext, LatencyProfile, NodeConfig, NodeRole, PhaseResult, ServerConfig,
};
use crate::state::config_hash;

pub struct Configure;

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
        let existing_config = host.read_nomad_config()?;

        let matches = existing_config
            .as_deref()
            .map(normalize_config)
            .map(|current| current == normalize_config(&desired_config))
            .unwrap_or(false);

        if matches {
            ctx.state.update_config_hash(&config_hash(&desired_config));
            return Ok(PhaseResult::unchanged(
                self.name(),
                "nomad configuration already matches desired state",
            ));
        }

        host.write_nomad_config(&desired_config)?;
        ctx.state.update_config_hash(&config_hash(&desired_config));
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

    match config.role {
        NodeRole::Server => {
            let server = config.server_config()?;
            lines.extend(render_server_block(server));
        }
        NodeRole::Client => {
            let client = config.client_config()?;
            let servers = client
                .server_addresses
                .iter()
                .map(|address| format!("  {},", render_hcl_string(address)))
                .collect::<Vec<_>>()
                .join("\n");
            lines.extend([
                String::new(),
                "client {".to_string(),
                "  enabled = true".to_string(),
                "}".to_string(),
                String::new(),
                "servers = [".to_string(),
                servers,
                "]".to_string(),
            ]);
        }
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
    use crate::models::{AdvertiseConfig, LatencyProfile, NodeRole};

    fn server_node_config() -> NodeConfig {
        NodeConfig {
            name: "server-1".to_string(),
            datacenter: "homelab".to_string(),
            version: "1.7.6".to_string(),
            role: NodeRole::Server,
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
}
