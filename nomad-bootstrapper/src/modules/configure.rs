/// Configure Nomad
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, NodeRole, PhaseResult};
use crate::runner::CommandRunner;
use crate::system;
use anyhow::Result;
use log::warn;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::NamedTempFile;

pub struct Configure;

impl PhaseExecutor for Configure {
    fn execute(
        &self,
        runner: &CommandRunner,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let desired_config = render_config(config)?;
        let existing_config = system::read_nomad_config()?;

        let matches = existing_config
            .as_deref()
            .map(system::normalize_config)
            .map(|content| content == system::normalize_config(&desired_config))
            .unwrap_or(false);

        if matches {
            // Configuration matches, but update the hash in state for consistency
            let desired_hash = system::config_hash(&desired_config);
            ctx.state.update_config_hash(&desired_hash);
            return Ok(PhaseResult::unchanged(
                self.name(),
                "nomad configuration already matches desired state",
            ));
        }

        if runner.is_dry_run() {
            return Ok(PhaseResult::changed(
                self.name(),
                "would write nomad configuration and flag service restart",
            ));
        }

        let config_dir = Path::new(system::NOMAD_CONFIG_PATH)
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid config path: no parent directory"))?;
        fs::create_dir_all(config_dir)?;

        // Write to a temp file in the target directory (same filesystem guarantees
        // atomic rename), validate, then persist to the final path.
        let mut temp_file = NamedTempFile::new_in(config_dir)?;
        write!(temp_file, "{}", desired_config)?;
        temp_file.flush()?;

        maybe_validate_agent_config(runner, temp_file.path().to_str().unwrap_or_default())?;

        // Atomic replace: persist() renames the temp file to the final path.
        // If validation failed above, the temp file is dropped and cleaned up automatically.
        temp_file.persist(system::NOMAD_CONFIG_PATH)?;

        system::set_config_permissions(system::NOMAD_CONFIG_PATH)?;

        // Update state with new config hash
        let desired_hash = system::config_hash(&desired_config);
        ctx.state.update_config_hash(&desired_hash);
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

fn maybe_validate_agent_config(runner: &CommandRunner, config_path: &str) -> Result<()> {
    let help_output = runner
        .run_output("nomad", &["agent", "-h"])
        .unwrap_or_default();
    if help_output.contains("-validate") {
        let config_arg = format!("-config={}", config_path);
        runner.run("nomad", &["agent", "-validate", &config_arg])?;
    } else {
        warn!("nomad agent -validate is not supported by this version; skipping config validation");
    }

    Ok(())
}

fn render_config(config: &NodeConfig) -> Result<String> {
    let raft_multiplier = config.raft_multiplier();
    let base = [
        "data_dir = \"/opt/nomad\"".to_string(),
        "bind_addr = \"0.0.0.0\"".to_string(),
        String::new(),
        "advertise {".to_string(),
        "  http = \"{{ GetInterfaceIP \\\"default\\\" }}\"".to_string(),
        "  rpc  = \"{{ GetInterfaceIP \\\"default\\\" }}\"".to_string(),
        "  serf = \"{{ GetInterfaceIP \\\"default\\\" }}\"".to_string(),
        "}".to_string(),
        String::new(),
        format!("raft_multiplier = {}", raft_multiplier),
    ];

    let role_block = match config.role {
        NodeRole::Server => {
            let server = config.server_config()?;
            let retry_join = server
                .server_join_addresses
                .iter()
                .map(|addr| format!("  \"{}\",", addr))
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
        NodeRole::Client => {
            let client = config.client_config()?;
            let servers = client
                .server_addresses
                .iter()
                .map(|addr| format!("  \"{}\",", addr))
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
    };

    let latency_block = match config.latency_profile {
        crate::models::LatencyProfile::Standard => Vec::new(),
        crate::models::LatencyProfile::HighLatency => vec![
            String::new(),
            "server_join {".to_string(),
            "  retry_max = 12".to_string(),
            "  retry_interval = \"30s\"".to_string(),
            "}".to_string(),
        ],
    };

    Ok(base
        .into_iter()
        .chain(role_block)
        .chain(latency_block)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LatencyProfile, NodeConfig, NodeRole, ServerConfig};

    #[test]
    fn test_render_server_config() {
        let config = NodeConfig {
            version: "1.7.0".to_string(),
            role: NodeRole::Server,
            server_config: Some(ServerConfig {
                bootstrap_expect: 3,
                server_join_addresses: vec!["10.0.1.2:4648".to_string()],
            }),
            client_config: None,
            latency_profile: LatencyProfile::HighLatency,
        };

        let rendered = render_config(&config).expect("rendered config");
        assert!(rendered.contains("bootstrap_expect = 3"));
        assert!(rendered.contains("retry_join"));
        assert!(rendered.contains("raft_multiplier = 5"));
    }
}
