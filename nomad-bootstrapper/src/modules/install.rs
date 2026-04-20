use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, NodeRole, PhaseResult};
use crate::transport::shell_quote;

/// Directory where CNI plugin binaries are installed.
const CNI_BIN_DIR: &str = "/opt/cni/bin";

/// Sentinel file written after a successful CNI install. Content is the installed version string.
const CNI_SENTINEL: &str = "/opt/cni/bin/.installed-version";

/// Binaries that must be present for CNI to be considered fully installed.
const CNI_REQUIRED_BINARIES: &[&str] = &["bridge", "host-local", "loopback"];

pub struct Install;

impl PhaseExecutor for Install {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let nomad_changed = ensure_nomad(host, config)?;

        let cni_changed = if config.has_role(NodeRole::Client) {
            ensure_cni_plugins(host, config)?
        } else {
            false
        };

        if !nomad_changed && !cni_changed {
            return Ok(PhaseResult::unchanged(
                self.name(),
                "already at desired state",
            ));
        }

        let mut parts = Vec::new();
        if nomad_changed {
            if config.version == "latest" {
                parts.push("installed nomad".to_string());
            } else {
                parts.push(format!("installed nomad={}", config.version));
            }
        }
        if cni_changed {
            parts.push(format!("installed CNI plugins {}", config.cni_version));
        }

        Ok(PhaseResult::changed(self.name(), parts.join("; ")))
    }

    fn name(&self) -> &'static str {
        "install"
    }
}

/// Installs or skips the Nomad apt package. Returns `true` if a change was made.
fn ensure_nomad(host: &DebianHost<'_>, config: &NodeConfig) -> Result<bool> {
    let is_latest = config.version == "latest";
    if is_latest {
        if host.installed_package_version("nomad")?.is_some()
            && !host.package_is_upgradable("nomad")?
        {
            return Ok(false);
        }
    } else if host.package_version_satisfies("nomad", &config.version)? {
        return Ok(false);
    }

    let package_spec = if is_latest {
        "nomad".to_string()
    } else {
        format!("nomad={}", config.version)
    };
    host.apt_install(std::slice::from_ref(&package_spec))?;

    Ok(true)
}

/// Downloads and extracts CNI plugin binaries to `/opt/cni/bin`, or skips if already converged.
///
/// Convergence is defined as: the sentinel file content matches the desired version string AND
/// all required binaries (`bridge`, `host-local`, `loopback`) are present. This double-check
/// handles partial installs where extraction succeeded but a previous run was interrupted.
/// The sentinel is written last so a failed extraction cannot leave a false-positive sentinel.
///
/// Returns `true` if a change was made.
fn ensure_cni_plugins(host: &DebianHost<'_>, config: &NodeConfig) -> Result<bool> {
    // In dry-run mode, skip state probes and use a placeholder arch. The install command
    // is logged but never executed, so the arch value only appears in the logged URL.
    let arch = if host.remote().is_dry_run() {
        "amd64"
    } else {
        let uname = host.command_output("uname -m")?;
        map_cni_arch(uname.trim())?
    };

    let version = &config.cni_version;

    // Convergence: sentinel content matches AND all required binaries exist.
    // Short-circuit: skip binary existence checks when the sentinel already disagrees.
    //
    // Note on intentional asymmetry: this convergence check uses `file_exists` (path
    // presence only). The install path below uses shell-side `test -x` (executability).
    // This is deliberate: `file_exists` is sufficient for convergence detection, while
    // `test -x` at install time guards against committing a sentinel after a bad extraction.
    let sentinel_ok = host
        .remote()
        .read_file(CNI_SENTINEL)?
        .as_deref()
        .map(str::trim)
        == Some(version.as_str());

    let converged = sentinel_ok
        && CNI_REQUIRED_BINARIES
            .iter()
            .map(|name| {
                host.remote()
                    .file_exists(&format!("{}/{}", CNI_BIN_DIR, name))
            })
            .collect::<Result<Vec<_>>>()?
            .iter()
            .all(|&exists| exists);

    if converged {
        return Ok(false);
    }

    // Download tarball, extract to /opt/cni/bin, then write sentinel on success.
    // Order is strict: extract → validate executability → write sentinel.
    // A `trap` ensures the temp file is removed even if an intermediate step fails.
    // The sentinel is written last so an interrupted extraction never leaves a stale marker.
    let url = format!(
        "https://github.com/containernetworking/plugins/releases/download/{version}/cni-plugins-linux-{arch}-{version}.tgz",
    );
    let binary_checks = CNI_REQUIRED_BINARIES
        .iter()
        .map(|name| {
            format!(
                "test -x {}",
                shell_quote(&format!("{}/{}", CNI_BIN_DIR, name))
            )
        })
        .collect::<Vec<_>>()
        .join(" && ");
    let install_cmd = format!(
        "set -eu; \
         tmp=$(mktemp /tmp/cni-plugins.XXXXXX.tgz); \
         trap 'rm -f \"$tmp\"' EXIT; \
         mkdir -p {bin_dir}; \
         curl -fsSL {url} -o \"$tmp\"; \
         tar -xzf \"$tmp\" -C {bin_dir}; \
         {binary_checks}; \
         printf '%s' {version_q} > {sentinel_q}",
        bin_dir = shell_quote(CNI_BIN_DIR),
        url = shell_quote(&url),
        binary_checks = binary_checks,
        version_q = shell_quote(version),
        sentinel_q = shell_quote(CNI_SENTINEL),
    );
    host.remote().run_privileged_checked(&install_cmd)?;

    Ok(true)
}

/// Maps `uname -m` output to the arch label used in CNI plugin release tarballs.
fn map_cni_arch(uname: &str) -> Result<&'static str> {
    match uname {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => anyhow::bail!(
            "unsupported architecture '{}' for CNI plugins; supported: x86_64 (amd64), aarch64 (arm64)",
            other
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AdvertiseConfig, ClientConfig, LatencyProfile, NodeRole};
    use crate::test_helpers::{recording_target, RecordingTransport};
    use crate::transport::{RemoteHost, RemoteOutput};

    fn server_node_config(version: &str) -> NodeConfig {
        NodeConfig {
            name: "node-1".to_string(),
            datacenter: "dc1".to_string(),
            version: version.to_string(),
            cni_version: "v1.6.2".to_string(),
            roles: vec![NodeRole::Server],
            server_config: None,
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
            datacenter: "dc1".to_string(),
            version: "latest".to_string(),
            cni_version: "v1.6.2".to_string(),
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

    // ── Nomad-only (server role) ──────────────────────────────────────────────

    #[test]
    fn test_latest_skips_install_when_nomad_is_not_upgradable() {
        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "nomad:\n  Installed: 1.8.0-1\n  Candidate: 1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &server_node_config("latest"), &mut ctx)
            .expect("install succeeds");

        assert!(!result.changes_made);
        assert!(result.message.contains("already at desired state"));

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(
            commands[0],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
        assert_eq!(commands[1], "apt-cache policy nomad");
        assert!(!commands
            .iter()
            .any(|command| command.contains("apt-get install")));
    }

    #[test]
    fn test_latest_installs_when_nomad_is_upgradable() {
        let transport = RecordingTransport::new(vec![
            // installed_package_version → "1.7.0-1" (nomad is present)
            RemoteOutput {
                status: 0,
                stdout: "1.7.0-1\n".to_string(),
                stderr: String::new(),
            },
            // package_is_upgradable → upgradable from 1.7.0 to 1.8.0
            RemoteOutput {
                status: 0,
                stdout: "nomad:\n  Installed: 1.7.0-1\n  Candidate: 1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            // privilege check for apt-get
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // apt-get install
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &server_node_config("latest"), &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        assert_eq!(result.message, "installed nomad");

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(
            commands[0],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
        assert_eq!(commands[1], "apt-cache policy nomad");
        assert_eq!(commands[2], "id -u");
        assert_eq!(commands[3], "apt-get install -y -qq nomad");
    }

    #[test]
    fn test_latest_installs_when_nomad_is_missing() {
        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &server_node_config("latest"), &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        assert_eq!(result.message, "installed nomad");

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(
            commands[0],
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi"
        );
        assert_eq!(commands[1], "id -u");
        assert_eq!(commands[2], "apt-get install -y -qq nomad");
    }

    // ── CNI plugins (client role) ─────────────────────────────────────────────

    /// Helper: recording responses for a Nomad "latest, already up-to-date" check.
    fn nomad_latest_current_responses() -> Vec<RemoteOutput> {
        vec![
            // installed_package_version → installed
            RemoteOutput {
                status: 0,
                stdout: "1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
            // package_is_upgradable → not upgradable
            RemoteOutput {
                status: 0,
                stdout: "nomad:\n  Installed: 1.8.0-1\n  Candidate: 1.8.0-1\n".to_string(),
                stderr: String::new(),
            },
        ]
    }

    #[test]
    fn test_cni_skipped_for_server_role() {
        // Server nodes must never trigger any CNI commands.
        let transport = RecordingTransport::new(nomad_latest_current_responses());
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &server_node_config("latest"), &mut ctx)
            .expect("install succeeds");

        assert!(!result.changes_made);

        let commands = transport.commands.lock().expect("commands lock");
        // Only the two Nomad apt-cache commands; no uname, no sentinel check, no install.
        assert_eq!(commands.len(), 2);
        assert!(!commands.iter().any(|c| c.contains("uname")));
        assert!(!commands.iter().any(|c| c.contains("cni")));
    }

    #[test]
    fn test_cni_skipped_when_already_converged() {
        // Client node: Nomad current, CNI sentinel matches, all binaries present.
        let mut responses = nomad_latest_current_responses();
        responses.extend([
            // uname -m
            RemoteOutput {
                status: 0,
                stdout: "x86_64\n".to_string(),
                stderr: String::new(),
            },
            // read sentinel → matches version
            RemoteOutput {
                status: 0,
                stdout: "v1.6.2".to_string(),
                stderr: String::new(),
            },
            // file_exists bridge
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // file_exists host-local
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // file_exists loopback
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &client_node_config(), &mut ctx)
            .expect("install succeeds");

        assert!(!result.changes_made);

        let commands = transport.commands.lock().expect("commands lock");
        assert!(!commands.iter().any(|c| c.contains("apt-get install")));
        assert!(!commands.iter().any(|c| c.contains("curl")));
    }

    #[test]
    fn test_cni_installs_when_sentinel_missing() {
        // Client node: Nomad current, CNI sentinel absent → download and extract.
        let mut responses = nomad_latest_current_responses();
        responses.extend([
            // uname -m
            RemoteOutput {
                status: 0,
                stdout: "x86_64\n".to_string(),
                stderr: String::new(),
            },
            // read sentinel → not found (empty stdout)
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // id -u (privilege check for install command)
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // install command
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &client_node_config(), &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        assert!(result.message.contains("CNI plugins v1.6.2"));

        // Verify the install command references the correct arch and version.
        let commands = transport.commands.lock().expect("commands lock");
        let install_cmd = commands.last().expect("install command present");
        assert!(install_cmd.contains("cni-plugins-linux-amd64-v1.6.2.tgz"));
        assert!(install_cmd.contains("/opt/cni/bin"));
        assert!(install_cmd.contains(".installed-version"));
    }

    #[test]
    fn test_cni_installs_when_version_mismatch() {
        // Sentinel exists but has a stale version → re-download.
        let mut responses = nomad_latest_current_responses();
        responses.extend([
            // uname -m
            RemoteOutput {
                status: 0,
                stdout: "x86_64\n".to_string(),
                stderr: String::new(),
            },
            // read sentinel → old version
            RemoteOutput {
                status: 0,
                stdout: "v1.5.0".to_string(),
                stderr: String::new(),
            },
            // id -u
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // install command
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &client_node_config(), &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);

        // Binary existence checks must NOT run when the sentinel already disagrees.
        let commands = transport.commands.lock().expect("commands lock");
        assert!(!commands
            .iter()
            .any(|c| c.contains("[ -e") && c.contains("bridge")));
    }

    #[test]
    fn test_cni_installs_when_binary_missing() {
        // Sentinel matches version but a required binary is absent → re-download.
        let mut responses = nomad_latest_current_responses();
        responses.extend([
            // uname -m
            RemoteOutput {
                status: 0,
                stdout: "aarch64\n".to_string(),
                stderr: String::new(),
            },
            // read sentinel → matches
            RemoteOutput {
                status: 0,
                stdout: "v1.6.2".to_string(),
                stderr: String::new(),
            },
            // file_exists bridge → present
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // file_exists host-local → present
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // file_exists loopback → MISSING (non-zero exit)
            RemoteOutput {
                status: 1,
                stdout: String::new(),
                stderr: String::new(),
            },
            // id -u
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // install command (should use arm64 for aarch64)
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &client_node_config(), &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);

        // Install command must reference arm64 for aarch64.
        let commands = transport.commands.lock().expect("commands lock");
        let install_cmd = commands.last().expect("install command present");
        assert!(install_cmd.contains("cni-plugins-linux-arm64-v1.6.2.tgz"));
    }

    #[test]
    fn test_cni_errors_on_unsupported_arch() {
        let mut responses = nomad_latest_current_responses();
        responses.push(RemoteOutput {
            status: 0,
            stdout: "armv7l\n".to_string(),
            stderr: String::new(),
        });

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let err = Install
            .execute(&host, &client_node_config(), &mut ctx)
            .expect_err("should fail on unsupported arch");
        assert!(err.to_string().contains("unsupported architecture"));
        assert!(err.to_string().contains("armv7l"));
    }

    #[test]
    fn test_map_cni_arch_x86_64() {
        assert_eq!(map_cni_arch("x86_64").expect("mapped"), "amd64");
    }

    #[test]
    fn test_map_cni_arch_aarch64() {
        assert_eq!(map_cni_arch("aarch64").expect("mapped"), "arm64");
    }

    #[test]
    fn test_map_cni_arch_unsupported() {
        let err = map_cni_arch("armv7l").expect_err("should error");
        assert!(err.to_string().contains("armv7l"));
    }
}
