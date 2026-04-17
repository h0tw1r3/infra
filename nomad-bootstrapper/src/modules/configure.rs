use anyhow::Result;

use crate::debian::{normalize_config, DebianHost};
use crate::executor::PhaseExecutor;
use crate::models::{
    ExecutionContext, LatencyProfile, NodeConfig, NodeRole, PhaseResult, ServerConfig,
};
use crate::state::config_hash;

pub struct Configure;

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
    let mut lines = vec![
        format!("name = \"{}\"", config.name),
        format!("datacenter = \"{}\"", config.datacenter),
        "data_dir = \"/opt/nomad\"".to_string(),
        "bind_addr = \"0.0.0.0\"".to_string(),
        String::new(),
        "advertise {".to_string(),
        "  http = \"{{ GetInterfaceIP \\\"default\\\" }}\"".to_string(),
        "  rpc  = \"{{ GetInterfaceIP \\\"default\\\" }}\"".to_string(),
        "  serf = \"{{ GetInterfaceIP \\\"default\\\" }}\"".to_string(),
        "}".to_string(),
        String::new(),
        format!("raft_multiplier = {}", config.raft_multiplier()),
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
                .map(|address| format!("  \"{}\",", address))
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
        .map(|address| format!("  \"{}\",", address))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LatencyProfile, NodeRole};

    #[test]
    fn test_render_server_config() {
        let config = NodeConfig {
            name: "server-1".to_string(),
            datacenter: "homelab".to_string(),
            version: "1.7.6".to_string(),
            role: NodeRole::Server,
            server_config: Some(ServerConfig {
                bootstrap_expect: 3,
                server_join_addresses: vec!["10.0.1.2:4648".to_string()],
            }),
            client_config: None,
            latency_profile: LatencyProfile::HighLatency,
        };

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("name = \"server-1\""));
        assert!(rendered.contains("datacenter = \"homelab\""));
        assert!(rendered.contains("bootstrap_expect = 3"));
        assert!(rendered.contains("retry_join"));
        assert!(rendered.contains("raft_multiplier = 5"));
    }
}
