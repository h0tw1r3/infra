use anyhow::Result;
use clap::Parser;
use log::info;

mod executor;
mod models;
mod runner;
mod state;
mod system;

mod modules;

use executor::{DependencyGraph, PHASE_NAMES};
use models::NodeConfig;
use runner::CommandRunner;

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

    /// For server mode: other servers to join (can be specified multiple times)
    #[arg(long)]
    server_join_addresses: Vec<String>,

    /// For client mode: server addresses (can be specified multiple times)
    #[arg(long)]
    server_addresses: Vec<String>,

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

    // Validate root privileges before making system changes.
    if !system::is_root() {
        anyhow::bail!("This tool must be run as root or with sudo");
    }

    // Create command runner
    let runner = CommandRunner::new(args.dry_run);

    // Execute phases in order
    executor.execute_all(&runner, &config, phases_to_run)?;

    info!("Nomad bootstrap complete");
    Ok(())
}
