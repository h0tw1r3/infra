use std::collections::HashMap;

use anyhow::Result;

use crate::debian::DebianHost;
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, NodeRole, PhaseResult, PluginInstallConfig};
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

        let plugins_changed = if config.has_role(NodeRole::Client) {
            ensure_driver_plugins(host, config)?
        } else {
            false
        };

        if !nomad_changed && !cni_changed && !plugins_changed {
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
        if plugins_changed {
            parts.push("installed driver plugins".to_string());
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

/// Maps `uname -m` output to the arch label used in driver plugin tarball URLs.
///
/// Supports the same `{arch}` substitution as CNI plugins.
fn map_plugin_arch(uname: &str) -> Result<&'static str> {
    match uname {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => anyhow::bail!(
            "unsupported architecture '{}' for driver plugins; supported: x86_64 (amd64), aarch64 (arm64)",
            other
        ),
    }
}

/// Installs all driver plugins declared in `config.plugin_installs` into `config.plugin_dir`.
///
/// Returns `true` if any plugin was installed or changed.
fn ensure_driver_plugins(host: &DebianHost<'_>, config: &NodeConfig) -> Result<bool> {
    if config.plugin_installs.is_empty() {
        return Ok(false);
    }

    // Preflight: detect duplicate target basenames — two plugins resolving to the same filename
    // would silently overwrite each other in plugin_dir.
    let mut seen_basenames: HashMap<&str, &str> = HashMap::new();
    for (driver_name, install_config) in &config.plugin_installs {
        let bin = match install_config {
            PluginInstallConfig::Tarball { binary, .. } => binary.as_str(),
            PluginInstallConfig::Apt { binary, .. } => binary.as_str(),
        };
        let bname = basename(bin);
        if let Some(existing) = seen_basenames.insert(bname, driver_name.as_str()) {
            anyhow::bail!(
                "driver plugins '{}' and '{}' both resolve to the same target filename '{}' in \
                 plugin_dir; rename one to avoid silent overwrites",
                existing,
                driver_name,
                bname
            );
        }
    }

    // Resolve arch once — shared across all tarball plugins on this node.
    // In dry-run mode we skip the remote probe and fall back to 'amd64'; warn so operators on
    // ARM hosts know the displayed URLs may not reflect their actual target architecture.
    let needs_arch = config
        .plugin_installs
        .values()
        .any(|p| matches!(p, PluginInstallConfig::Tarball { url, .. } if url.contains("{arch}")));
    let arch_cache: Option<&'static str> = if needs_arch {
        if host.remote().is_dry_run() {
            log::warn!(
                "dry-run: {{arch}} placeholder in tarball URL resolved to 'amd64'; \
                 actual architecture may differ on the target host"
            );
            Some("amd64")
        } else {
            let uname = host.command_output("uname -m")?;
            Some(map_plugin_arch(uname.trim())?)
        }
    } else {
        None
    };

    let mut any_changed = false;

    // Iterate in sorted key order for deterministic command output / logs.
    let mut sorted_installs: Vec<(&String, &PluginInstallConfig)> =
        config.plugin_installs.iter().collect();
    sorted_installs.sort_by_key(|(name, _)| *name);

    for (driver_name, install_config) in sorted_installs {
        let changed = match install_config {
            PluginInstallConfig::Tarball { url, binary } => {
                let arch = arch_cache.unwrap_or("amd64");
                let resolved_url = url.replace("{arch}", arch);
                ensure_tarball_plugin(host, driver_name, &resolved_url, binary, &config.plugin_dir)?
            }
            PluginInstallConfig::Apt {
                package,
                version,
                binary,
            } => ensure_apt_plugin(
                host,
                driver_name,
                package,
                version.as_deref(),
                binary,
                &config.plugin_dir,
            )?,
        };
        if changed {
            any_changed = true;
        }
    }

    Ok(any_changed)
}

/// Installs a driver plugin binary from a release tarball into `plugin_dir`.
///
/// ## Idempotency
/// A sentinel file `plugin_dir/.installed-<driver_name>` stores two lines:
/// - line 1: the resolved download URL
/// - line 2: the configured archive-internal binary path
///
/// Convergence is skipped if the sentinel matches both values, the destination binary
/// exists, and it is executable. A malformed or missing sentinel triggers reinstall.
///
/// ## `binary` semantics
/// `binary` must be the **exact relative path of the binary within the archive** after
/// extraction (e.g. `"nomad-driver-containerd"` or `"linux-amd64/nomad-driver-foo"`).
/// The binary is moved to `plugin_dir/<basename(binary)>` after extraction.
///
/// Use version-pinned, immutable URLs. Mutable "latest" URLs can cause the sentinel to
/// match while the underlying content has changed, defeating idempotency.
///
/// ## Atomicity
/// The tarball is extracted into a temp directory created inside `plugin_dir` (same
/// filesystem), so the final `mv` into `plugin_dir` is a guaranteed atomic rename.
///
/// Returns `true` if a change was made.
fn ensure_tarball_plugin(
    host: &DebianHost<'_>,
    driver_name: &str,
    url: &str,
    binary: &str,
    plugin_dir: &str,
) -> Result<bool> {
    let sentinel = format!("{}/.installed-{}", plugin_dir, driver_name);
    let dest = format!("{}/{}", plugin_dir, basename(binary));

    // Convergence: sentinel matches URL + binary path, dest exists, dest is executable.
    let sentinel_content = host.remote().read_file(&sentinel)?.unwrap_or_default();
    let sentinel_ok = parse_tarball_sentinel(&sentinel_content)
        .map(|(s_url, s_binary)| s_url == url && s_binary == binary)
        .unwrap_or(false);

    let dest_ok = sentinel_ok
        && host.remote().file_exists(&dest)?
        && host
            .remote()
            .run(&format!("[ -x {} ]", shell_quote(&dest)))?
            .success();

    if dest_ok {
        return Ok(false);
    }

    // Download tarball to /tmp, extract into a temp dir inside plugin_dir (same FS),
    // validate the binary at its configured archive-internal path, chmod +x, then
    // atomically move it into place before writing the sentinel last.
    let install_cmd = format!(
        "set -eu; \
         mkdir -p {plugin_dir_q}; \
         tmp_tgz=$(mktemp /tmp/plugin-{driver_name_q}.XXXXXX.tgz); \
         tmp_dir=$(mktemp -d {plugin_dir_q}/.plugin-{driver_name_q}.XXXXXX); \
         trap 'rm -rf \"$tmp_tgz\" \"$tmp_dir\"' EXIT; \
         curl -fsSL {url_q} -o \"$tmp_tgz\"; \
         tar -xzf \"$tmp_tgz\" -C \"$tmp_dir\"; \
         extracted=\"$tmp_dir\"/{binary_q}; \
         [ -f \"$extracted\" ] || {{ echo 'ERROR: binary not found at archive path {binary_display}' >&2; exit 1; }}; \
         chmod +x \"$extracted\"; \
         mv \"$extracted\" {dest_q}; \
         printf '%s\\n%s' {url_store_q} {binary_store_q} > {sentinel_q}",
        driver_name_q = shell_quote(driver_name),
        plugin_dir_q = shell_quote(plugin_dir),
        url_q = shell_quote(url),
        binary_q = shell_quote(binary),
        binary_display = binary,
        dest_q = shell_quote(&dest),
        url_store_q = shell_quote(url),
        binary_store_q = shell_quote(binary),
        sentinel_q = shell_quote(&sentinel),
    );
    host.remote().run_privileged_checked(&install_cmd)?;

    Ok(true)
}

/// Parses the two-line tarball sentinel format.
///
/// Returns `Some((url, binary))` when both lines are non-empty.
/// Returns `None` for any malformed input (empty, one line, whitespace-only),
/// which causes the caller to treat the plugin as not yet converged.
fn parse_tarball_sentinel(content: &str) -> Option<(&str, &str)> {
    let mut lines = content.lines();
    let url = lines.next()?.trim();
    let binary = lines.next()?.trim();
    if url.is_empty() || binary.is_empty() {
        None
    } else {
        Some((url, binary))
    }
}

/// Installs a driver plugin via apt and ensures a symlink in `plugin_dir` points to it.
///
/// ## `binary` semantics
/// `binary` is the **absolute on-disk path** where apt installs the executable
/// (e.g. `/usr/sbin/nomad-driver-lxc`). A symlink `plugin_dir/<basename(binary)>` → `binary`
/// is created or corrected.
///
/// ## Idempotency
/// - apt: `package_version_satisfies` / `installed_package_version` gates reinstall.
/// - symlink: `readlink` compares the current target; the link is created or replaced if wrong.
///   A directory at `plugin_dir/<basename>` is an error; a regular file is replaced.
///
/// Returns `true` if a change was made.
fn ensure_apt_plugin(
    host: &DebianHost<'_>,
    driver_name: &str,
    package: &str,
    version: Option<&str>,
    binary: &str,
    plugin_dir: &str,
) -> Result<bool> {
    let mut changed = false;

    // Install/upgrade the apt package if needed.
    let needs_install = if let Some(ver) = version {
        !host.package_version_satisfies(package, ver)?
    } else {
        // No version pinned: install if not present; skip if any version is installed.
        host.installed_package_version(package)?.is_none()
    };

    if needs_install {
        let package_spec = match version {
            Some(ver) => format!("{}={}", package, ver),
            None => package.to_string(),
        };
        host.apt_install(std::slice::from_ref(&package_spec))?;

        // Verify the installed binary path exists and is executable.
        let bin_ok = host
            .remote()
            .run(&format!(
                "[ -f {bin_q} ] && [ -x {bin_q} ]",
                bin_q = shell_quote(binary)
            ))?
            .success();
        if !bin_ok {
            anyhow::bail!(
                "driver plugin '{}': binary '{}' not found or not executable after installing \
                 package '{}'; verify the 'binary' path in your inventory",
                driver_name,
                binary,
                package
            );
        }
        changed = true;
    }

    // Ensure `plugin_dir/<basename>` is a symlink pointing to `binary`.
    //
    // Query the current state with one shell expression to minimise round-trips:
    //   - prints "DIR"  if a directory is in the way (operator must resolve manually)
    //   - prints the symlink target if it is a symlink (may match or may need replacement)
    //   - prints "FILE" if a regular (non-symlink) file exists (will be replaced)
    //   - prints nothing if the path does not exist (will be created)
    let link_path = format!("{}/{}", plugin_dir, basename(binary));
    let link_status = host.command_output(&format!(
        "if [ -d {lq} ]; then printf 'DIR'; \
         elif [ -L {lq} ]; then readlink {lq}; \
         elif [ -e {lq} ]; then printf 'FILE'; fi",
        lq = shell_quote(&link_path),
    ))?;
    let link_status = link_status.trim();

    if link_status == "DIR" {
        anyhow::bail!(
            "driver plugin '{}': expected a symlink at '{}' but found a directory; \
             remove it manually to allow the symlink to be created",
            driver_name,
            link_path
        );
    }

    if link_status != binary {
        // Log why we're (re)creating the link.
        if link_status == "FILE" {
            log::info!(
                "driver plugin '{}': replacing regular file at {} with symlink → {}",
                driver_name,
                link_path,
                binary
            );
        } else if !link_status.is_empty() {
            // Wrong symlink target.
            log::info!(
                "driver plugin '{}': correcting symlink {} (was → {}, now → {})",
                driver_name,
                link_path,
                link_status,
                binary
            );
        } else if !changed {
            // Link absent on first converge (apt was already satisfied).
            log::info!(
                "driver plugin '{}': created symlink {} → {}",
                driver_name,
                link_path,
                binary
            );
        }

        let symlink_cmd = format!(
            "mkdir -p {plugin_dir_q} && ln -sf {binary_q} {link_q}",
            plugin_dir_q = shell_quote(plugin_dir),
            binary_q = shell_quote(binary),
            link_q = shell_quote(&link_path),
        );
        host.remote().run_privileged_checked(&symlink_cmd)?;
        changed = true;
    }

    Ok(changed)
}

/// Returns the final path component of a path string (the filename).
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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
            plugins: Default::default(),
            plugin_dir: "/opt/nomad/plugins".to_string(),
            plugin_installs: Default::default(),
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
            plugins: Default::default(),
            plugin_dir: "/opt/nomad/plugins".to_string(),
            plugin_installs: Default::default(),
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

    // ── Driver plugins ────────────────────────────────────────────────────────

    /// Helper: Nomad latest-current + CNI already converged response sequence.
    fn nomad_and_cni_current_responses() -> Vec<RemoteOutput> {
        let mut r = nomad_latest_current_responses();
        r.extend([
            // uname -m for CNI
            RemoteOutput {
                status: 0,
                stdout: "x86_64\n".to_string(),
                stderr: String::new(),
            },
            // read CNI sentinel → matches
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
        r
    }

    #[test]
    fn test_driver_plugins_skipped_when_empty() {
        // Client with no plugin_installs: no extra commands.
        let transport = RecordingTransport::new(nomad_and_cni_current_responses());
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &client_node_config(), &mut ctx)
            .expect("install succeeds");

        assert!(!result.changes_made);
    }

    #[test]
    fn test_tarball_plugin_installs_when_sentinel_missing() {
        let mut config = client_node_config();
        config.plugin_installs.insert(
            "containerd-driver".to_string(),
            PluginInstallConfig::Tarball {
                url: "https://example.com/containerd_{arch}.tar.gz".to_string(),
                binary: "nomad-driver-containerd".to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // uname -m for driver plugin arch
            RemoteOutput {
                status: 0,
                stdout: "x86_64\n".to_string(),
                stderr: String::new(),
            },
            // read sentinel → absent (None)
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // id -u (privilege check)
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
            .execute(&host, &config, &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        assert!(result.message.contains("driver plugins"));

        let commands = transport.commands.lock().expect("commands lock");
        let install_cmd = commands.last().expect("install command present");
        assert!(install_cmd.contains("containerd_amd64.tar.gz"));
        assert!(install_cmd.contains(".installed-containerd-driver"));
        assert!(install_cmd.contains("/opt/nomad/plugins"));
        // New flow: extract to temp dir inside plugin_dir, mv binary, then write sentinel.
        assert!(install_cmd.contains("mktemp -d"));
        assert!(install_cmd.contains("mv "));
    }

    #[test]
    fn test_tarball_plugin_skipped_when_already_converged() {
        let mut config = client_node_config();
        let url = "https://example.com/containerd_amd64.tar.gz";
        let binary = "nomad-driver-containerd";
        config.plugin_installs.insert(
            "containerd-driver".to_string(),
            PluginInstallConfig::Tarball {
                url: url.to_string(),
                binary: binary.to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // No uname -m needed: URL has no {arch} placeholder.
            // read sentinel → matches (2-line: URL\nbinary)
            RemoteOutput {
                status: 0,
                stdout: format!("{}\n{}", url, binary),
                stderr: String::new(),
            },
            // file_exists binary → present
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // [ -x binary ] → executable
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
            .execute(&host, &config, &mut ctx)
            .expect("install succeeds");

        assert!(!result.changes_made);
        let commands = transport.commands.lock().expect("commands lock");
        assert!(!commands.iter().any(|c| c.contains("curl")));
    }

    #[test]
    fn test_apt_plugin_installs_when_version_not_satisfied() {
        let mut config = client_node_config();
        config.plugin_installs.insert(
            "lxc".to_string(),
            PluginInstallConfig::Apt {
                package: "nomad-driver-lxc".to_string(),
                version: Some("1.0.0".to_string()),
                binary: "/usr/sbin/nomad-driver-lxc".to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // package_version_satisfies → not satisfied (package missing)
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // id -u for apt-install
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
            // [ -f binary ] && [ -x binary ] → ok
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // link_status check → empty (link absent)
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // id -u for symlink
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // symlink command
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
            .execute(&host, &config, &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        let commands = transport.commands.lock().expect("commands lock");
        let apt_cmd = commands
            .iter()
            .find(|c| c.contains("apt-get install") && c.contains("nomad-driver-lxc"))
            .expect("apt-get install command present");
        assert!(apt_cmd.contains("nomad-driver-lxc=1.0.0"));

        let ln_cmd = commands.last().expect("symlink command present");
        assert!(ln_cmd.contains("ln -sf"));
        assert!(ln_cmd.contains("/usr/sbin/nomad-driver-lxc"));
        assert!(ln_cmd.contains("/opt/nomad/plugins/nomad-driver-lxc"));
    }

    #[test]
    fn test_apt_plugin_skipped_when_already_converged() {
        let mut config = client_node_config();
        config.plugin_installs.insert(
            "lxc".to_string(),
            PluginInstallConfig::Apt {
                package: "nomad-driver-lxc".to_string(),
                version: Some("1.0.0".to_string()),
                binary: "/usr/sbin/nomad-driver-lxc".to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // package_version_satisfies → satisfied
            RemoteOutput {
                status: 0,
                stdout: "1.0.0-1\n".to_string(),
                stderr: String::new(),
            },
            // link_status check → readlink returns correct target
            RemoteOutput {
                status: 0,
                stdout: "/usr/sbin/nomad-driver-lxc".to_string(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let result = Install
            .execute(&host, &config, &mut ctx)
            .expect("install succeeds");

        assert!(!result.changes_made);
        let commands = transport.commands.lock().expect("commands lock");
        assert!(!commands
            .iter()
            .any(|c| c.contains("apt-get install") && c.contains("lxc")));
        assert!(!commands.iter().any(|c| c.contains("ln -sf")));
    }

    #[test]
    fn test_tarball_sentinel_malformed_triggers_reinstall() {
        // Malformed sentinels (empty, one-line, garbage) must all trigger reinstall.
        let cases = [
            ("", "empty"),
            ("https://example.com/foo.tgz", "one-line"),
            ("garbage\n", "garbage with trailing newline"),
            ("\n", "blank lines only"),
        ];
        for (sentinel_content, label) in cases {
            assert!(
                parse_tarball_sentinel(sentinel_content).is_none(),
                "expected None for '{label}'"
            );
        }
    }

    #[test]
    fn test_tarball_sentinel_valid_parses_correctly() {
        let url = "https://example.com/foo.tgz";
        let binary = "linux-amd64/nomad-driver-foo";
        let content = format!("{}\n{}", url, binary);
        let result = parse_tarball_sentinel(&content);
        assert_eq!(result, Some((url, binary)));
    }

    #[test]
    fn test_tarball_plugin_reinstalls_when_binary_not_executable() {
        // Sentinel matches (2-line) and binary exists, but binary is not executable → reinstall.
        let mut config = client_node_config();
        let url = "https://example.com/containerd_amd64.tar.gz";
        let binary = "nomad-driver-containerd";
        config.plugin_installs.insert(
            "containerd-driver".to_string(),
            PluginInstallConfig::Tarball {
                url: url.to_string(),
                binary: binary.to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // read sentinel → valid 2-line match
            RemoteOutput {
                status: 0,
                stdout: format!("{}\n{}", url, binary),
                stderr: String::new(),
            },
            // file_exists binary → present
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            // [ -x binary ] → NOT executable (exit 1)
            RemoteOutput {
                status: 1,
                stdout: String::new(),
                stderr: String::new(),
            },
            // id -u for install command
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
            .execute(&host, &config, &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        let commands = transport.commands.lock().expect("commands lock");
        assert!(commands.iter().any(|c| c.contains("curl")));
    }

    #[test]
    fn test_apt_symlink_drift_repairs_wrong_target() {
        // Symlink exists but points to wrong path → should be corrected.
        let mut config = client_node_config();
        config.plugin_installs.insert(
            "lxc".to_string(),
            PluginInstallConfig::Apt {
                package: "nomad-driver-lxc".to_string(),
                version: Some("1.0.0".to_string()),
                binary: "/usr/sbin/nomad-driver-lxc".to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // package_version_satisfies → satisfied
            RemoteOutput {
                status: 0,
                stdout: "1.0.0-1\n".to_string(),
                stderr: String::new(),
            },
            // link_status check → wrong symlink target
            RemoteOutput {
                status: 0,
                stdout: "/usr/bin/nomad-driver-lxc".to_string(),
                stderr: String::new(),
            },
            // id -u for symlink repair
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // symlink repair command
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
            .execute(&host, &config, &mut ctx)
            .expect("install succeeds");

        assert!(result.changes_made);
        let commands = transport.commands.lock().expect("commands lock");
        let ln_cmd = commands.last().expect("symlink command present");
        assert!(ln_cmd.contains("ln -sf"));
        assert!(ln_cmd.contains("/usr/sbin/nomad-driver-lxc"));
    }

    #[test]
    fn test_apt_symlink_directory_collision_errors() {
        // A directory at the expected link path must produce a hard error.
        let mut config = client_node_config();
        config.plugin_installs.insert(
            "lxc".to_string(),
            PluginInstallConfig::Apt {
                package: "nomad-driver-lxc".to_string(),
                version: None,
                binary: "/usr/sbin/nomad-driver-lxc".to_string(),
            },
        );

        let mut responses = nomad_and_cni_current_responses();
        responses.extend([
            // installed_package_version → present
            RemoteOutput {
                status: 0,
                stdout: "1.0.0-1\n".to_string(),
                stderr: String::new(),
            },
            // link_status check → "DIR"
            RemoteOutput {
                status: 0,
                stdout: "DIR".to_string(),
                stderr: String::new(),
            },
        ]);

        let transport = RecordingTransport::new(responses);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let err = Install
            .execute(&host, &config, &mut ctx)
            .expect_err("should fail on directory collision");
        assert!(err.to_string().contains("directory"));
    }

    #[test]
    fn test_driver_plugins_duplicate_basename_errors() {
        // Two plugins resolving to the same basename in plugin_dir must be rejected.
        let mut config = client_node_config();
        config.plugin_installs.insert(
            "plugin-a".to_string(),
            PluginInstallConfig::Apt {
                package: "pkg-a".to_string(),
                version: None,
                binary: "/usr/sbin/nomad-driver-foo".to_string(),
            },
        );
        config.plugin_installs.insert(
            "plugin-b".to_string(),
            PluginInstallConfig::Tarball {
                url: "https://example.com/foo.tar.gz".to_string(),
                binary: "subdir/nomad-driver-foo".to_string(), // same basename
            },
        );

        // Only need the Nomad + CNI current responses; preflight should fail before any installs.
        let transport = RecordingTransport::new(nomad_and_cni_current_responses());
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        let mut ctx = ExecutionContext::default();

        let err = Install
            .execute(&host, &config, &mut ctx)
            .expect_err("should fail on duplicate basename");
        assert!(err.to_string().contains("nomad-driver-foo"));
    }

    #[test]
    fn test_basename_helper() {
        assert_eq!(basename("/usr/sbin/nomad-driver-lxc"), "nomad-driver-lxc");
        assert_eq!(
            basename("nomad-driver-containerd"),
            "nomad-driver-containerd"
        );
        assert_eq!(basename("linux-amd64/nomad-driver-foo"), "nomad-driver-foo");
    }
}
