use anyhow::Result;

mod config;
mod controller;
mod debian;
mod executor;
mod models;
mod modules;
mod state;
mod transport;

#[cfg(test)]
mod test_helpers;

fn main() -> Result<()> {
    let args = config::Args::parse_and_init_logging()?;
    controller::run(&args)
}
