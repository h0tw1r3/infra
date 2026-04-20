use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult, PluginInstallConfig, UrlSpec};

pub struct EnsureDeps;

impl PhaseExecutor for EnsureDeps {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let mut required_packages: Vec<&str> = vec!["curl", "gnupg", "ca-certificates"];

        // Require `unzip` if any tarball plugin uses a .zip archive URL.
        if needs_unzip(config) {
            required_packages.push("unzip");
        }

        let mut missing_packages = Vec::new();
        for package in required_packages {
            if !host.package_installed(package)? {
                missing_packages.push(package.to_string());
            }
        }

        if missing_packages.is_empty() {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "all required packages are already installed",
            ));
        }

        host.apt_update()?;
        host.apt_install(&missing_packages)?;

        Ok(PhaseResult::changed(
            self.name(),
            format!(
                "installed missing packages: {}",
                missing_packages.join(", ")
            ),
        ))
    }

    fn name(&self) -> &'static str {
        "ensure-deps"
    }
}

/// Returns `true` if any tarball plugin install config uses a `.zip` archive URL.
fn needs_unzip(config: &NodeConfig) -> bool {
    config.plugin_installs.values().any(|p| match p {
        PluginInstallConfig::Tarball { url, .. } => url_spec_has_zip(url),
        _ => false,
    })
}

/// Returns `true` if any URL in the spec ends with `.zip`.
fn url_spec_has_zip(url_spec: &UrlSpec) -> bool {
    match url_spec {
        UrlSpec::Single(url) => url.ends_with(".zip"),
        UrlSpec::ArchMap(map) => map.values().any(|url| url.ends_with(".zip")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::models::{AdvertiseConfig, LatencyProfile, NodeRole};

    fn base_config() -> NodeConfig {
        NodeConfig {
            name: "node".to_string(),
            datacenter: "dc1".to_string(),
            version: "1.8.0".to_string(),
            cni_version: "v1.6.2".to_string(),
            roles: vec![NodeRole::Client],
            server_config: None,
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

    #[test]
    fn test_needs_unzip_false_when_no_plugins() {
        let config = base_config();
        assert!(!needs_unzip(&config));
    }

    #[test]
    fn test_needs_unzip_false_for_tgz_tarball() {
        let mut config = base_config();
        config.plugin_installs.insert(
            "driver".to_string(),
            PluginInstallConfig::Tarball {
                url: UrlSpec::Single("https://example.com/driver_linux_amd64.tar.gz".to_string()),
                binary: "driver".to_string(),
            },
        );
        assert!(!needs_unzip(&config));
    }

    #[test]
    fn test_needs_unzip_true_for_zip_tarball_single() {
        let mut config = base_config();
        config.plugin_installs.insert(
            "exec2".to_string(),
            PluginInstallConfig::Tarball {
                url: UrlSpec::Single(
                    "https://releases.hashicorp.com/nomad-driver-exec2/0.1.1/nomad-driver-exec2_0.1.1_linux_amd64.zip".to_string(),
                ),
                binary: "nomad-driver-exec2".to_string(),
            },
        );
        assert!(needs_unzip(&config));
    }

    #[test]
    fn test_needs_unzip_true_for_zip_in_arch_map() {
        let mut config = base_config();
        let mut map = HashMap::new();
        map.insert(
            "amd64".to_string(),
            "https://example.com/driver.zip".to_string(),
        );
        map.insert(
            "arm64".to_string(),
            "https://example.com/driver-arm64.zip".to_string(),
        );
        config.plugin_installs.insert(
            "exec2".to_string(),
            PluginInstallConfig::Tarball {
                url: UrlSpec::ArchMap(map),
                binary: "nomad-driver-exec2".to_string(),
            },
        );
        assert!(needs_unzip(&config));
    }

    #[test]
    fn test_needs_unzip_false_for_apt_plugin() {
        let mut config = base_config();
        config.plugin_installs.insert(
            "lxc".to_string(),
            PluginInstallConfig::Apt {
                package: "nomad-driver-lxc".to_string(),
                version: None,
                binary: "/usr/sbin/nomad-driver-lxc".to_string(),
            },
        );
        assert!(!needs_unzip(&config));
    }
}
