use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};

pub struct Install;

impl PhaseExecutor for Install {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let is_latest = config.version == "latest";
        if !is_latest && host.nomad_version_satisfies(&config.version)? {
            ctx.state.update_provision(&config.version);
            return Ok(PhaseResult::unchanged(
                self.name(),
                format!("nomad {} is already installed", config.version),
            ));
        }

        let package_spec = if is_latest {
            "nomad".to_string()
        } else {
            format!("nomad={}", config.version)
        };
        host.apt_install(std::slice::from_ref(&package_spec))?;

        let provisioned_version = host
            .installed_nomad_version()?
            .unwrap_or_else(|| config.version.clone());
        ctx.state.update_provision(&provisioned_version);

        Ok(PhaseResult::changed(
            self.name(),
            format!("installed {}", package_spec),
        ))
    }

    fn name(&self) -> &'static str {
        "install"
    }
}
