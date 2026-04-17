use anyhow::Result;
use clap::Parser;
use log::info;
use std::net::IpAddr;

mod executor;
mod models;
mod runner;
mod state;
mod system;

mod modules;

use executor::{DependencyGraph, PHASE_NAMES};
use models::NodeConfig;
use runner::CommandRunner;

/// Validate that a value is a valid IP or hostname, optionally with a :port suffix.
fn validate_address(val: &str) -> std::result::Result<String, String> {
    let val = val.trim();
    if val.is_empty() {
        return Err("address cannot be empty".to_string());
    }

    // Try parsing as a bare IP address first (covers IPv4 and IPv6 without port)
    if val.parse::<IpAddr>().is_ok() {
        return Ok(val.to_string());
    }

    // Handle [IPv6]:port notation
    if val.starts_with('[') {
        if let Some(bracket_end) = val.find(']') {
            let ip_part = &val[1..bracket_end];
            if ip_part.parse::<IpAddr>().is_err() {
                return Err(format!("invalid IPv6 address in '{}'", val));
            }
            if val.len() > bracket_end + 1 {
                // Expect ]:port
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

    // Split off optional port — only for non-IPv6 addresses
    let (host, port) = if let Some(idx) = val.rfind(':') {
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

    // Check if host is a valid IP address
    if host.parse::<IpAddr>().is_ok() {
        let _ = port;
        return Ok(val.to_string());
    }

    // Validate as hostname: RFC 952/1123 rules
    if host.is_empty() || host.len() > 253 {
        return Err(format!("invalid address '{}': hostname is empty or too long", val));
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

/// Nomad Bootstrap Tool - Idempotent state provisioner for Nomad on Debian systems
#[derive(Parser, Debug)]
#[command(name = "nomad-bootstrapper")]
#[command(about = "Bootstrap and configure Nomad on Debian-based Linux systems", long_about = None)]
#[command(version)]
#[command(author = "Clark Contributors")]
struct Args {
    /// Nomad version to install (exact upstream version, e.g. "1.7.0").
    /// Use "latest" to install/upgrade to the newest available package.
    #[arg(long, default_value = "latest")]
    nomad_version: String,

    /// Node role: server or client
    #[arg(long, value_parser = ["server", "client"])]
    role: Option<String>,

    /// For server mode: number of servers to bootstrap
    #[arg(long)]
    bootstrap_expect: Option<u32>,

    /// For server mode: another server to join (can be specified multiple times).
    /// Must be a valid IP or hostname, optionally with :port (e.g., 10.0.1.2:4648)
    #[arg(long, value_parser = validate_address)]
    server_join_address: Vec<String>,

    /// For client mode: a Nomad server address (can be specified multiple times).
    /// Must be a valid IP or hostname, optionally with :port (e.g., 10.0.1.1:4647)
    #[arg(long, value_parser = validate_address)]
    server_address: Vec<String>,

    /// Apply high-latency tuning (gossip interval, heartbeat timeouts, etc.)
    #[arg(long, default_value_t = false)]
    high_latency: bool,

    /// Run only this phase (for testing)
    #[arg(long, value_parser = PHASE_NAMES)]
    phase: Option<String>,

    /// Run up to and including this phase (for testing)
    #[arg(long, value_parser = PHASE_NAMES)]
    up_to: Option<String>,

    /// Show what would be done without making changes
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<()> {
    // Parse arguments first to set up logging
    let args = Args::parse();

    // Initialize logging
    env_logger::Builder::from_default_env()
        .filter_level(args.log_level.parse()?)
        .init();

    info!("Starting Nomad bootstrap");
    info!("Version: {}", args.nomad_version);

    // Build dependency graph
    let executor = DependencyGraph::new()?;

    // Filter phases based on --phase or --up-to flags
    let phases_to_run = executor.filter_phases(&args.phase, &args.up_to)?;
    info!("Running {} phases", phases_to_run.len());

    let requires_role_config = phases_to_run
        .iter()
        .any(|phase| phase.name() == "configure");

    // Build node configuration from arguments
    let config = NodeConfig::from_args_with_role_requirement(&args, requires_role_config)?;
    info!("Configuration: {:?}", config);

    // Escalate to root if needed (supports sudo, doas, pkexec).
    // Placed after validation so argument errors are reported without prompting.
    if !args.dry_run {
        sudo2::escalate_if_needed()
            .map_err(|e| anyhow::anyhow!("failed to escalate privileges: {}", e))?;
    }

    // Create command runner
    let runner = CommandRunner::new(args.dry_run);

    // Execute phases in order
    executor.execute_all(&runner, &config, phases_to_run)?;

    info!("Nomad bootstrap complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Valid addresses ---

    #[test]
    fn test_validate_ipv4() {
        assert_eq!(validate_address("10.0.1.1"), Ok("10.0.1.1".to_string()));
    }

    #[test]
    fn test_validate_ipv4_with_port() {
        assert_eq!(
            validate_address("10.0.1.1:4647"),
            Ok("10.0.1.1:4647".to_string())
        );
    }

    #[test]
    fn test_validate_hostname() {
        assert_eq!(
            validate_address("nomad-server"),
            Ok("nomad-server".to_string())
        );
    }

    #[test]
    fn test_validate_hostname_with_port() {
        assert_eq!(
            validate_address("nomad-server:4647"),
            Ok("nomad-server:4647".to_string())
        );
    }

    #[test]
    fn test_validate_fqdn() {
        assert_eq!(
            validate_address("node1.example.com:4647"),
            Ok("node1.example.com:4647".to_string())
        );
    }

    #[test]
    fn test_validate_localhost() {
        assert_eq!(
            validate_address("localhost"),
            Ok("localhost".to_string())
        );
    }

    #[test]
    fn test_validate_ipv6_loopback() {
        assert_eq!(validate_address("::1"), Ok("::1".to_string()));
    }

    // --- Invalid addresses ---

    #[test]
    fn test_reject_empty() {
        assert!(validate_address("").is_err());
    }

    #[test]
    fn test_reject_spaces_in_hostname() {
        assert!(validate_address("not valid").is_err());
    }

    #[test]
    fn test_reject_hostname_leading_hyphen() {
        assert!(validate_address("-bad").is_err());
    }

    #[test]
    fn test_reject_hostname_trailing_hyphen() {
        assert!(validate_address("bad-").is_err());
    }

    #[test]
    fn test_reject_port_zero() {
        assert!(validate_address("10.0.1.1:0").is_err());
    }

    #[test]
    fn test_reject_port_overflow() {
        assert!(validate_address("10.0.1.1:99999").is_err());
    }

    #[test]
    fn test_reject_special_characters() {
        assert!(validate_address("host!name:123").is_err());
    }

    #[test]
    fn test_reject_empty_label() {
        assert!(validate_address("host..name").is_err());
    }
}
