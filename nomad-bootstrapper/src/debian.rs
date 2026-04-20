use anyhow::Result;

use crate::transport::{shell_quote, RemoteHost};

/// Debian-only remote host wrapper.
///
/// This layer owns generic Debian/APT operations such as package queries,
/// upgradable checks, repository file writes, keyring fetches, and generic
/// privileged file/service operations. Higher-level phases provide
/// Nomad-specific package names, service names, repository paths, and content.
pub struct DebianHost<'a> {
    remote: RemoteHost<'a>,
}

impl<'a> DebianHost<'a> {
    pub fn new(remote: RemoteHost<'a>) -> Self {
        Self { remote }
    }

    pub fn remote(&self) -> &RemoteHost<'a> {
        &self.remote
    }

    pub fn ensure_supported_platform(&self) -> Result<()> {
        if self.remote.is_dry_run() {
            return Ok(());
        }

        let os_release = self.remote.run_checked("cat /etc/os-release")?.stdout;
        let parsed = parse_os_release(&os_release);
        match parsed.get("ID").map(String::as_str) {
            Some("debian") => Ok(()),
            Some(id) => anyhow::bail!(
                "host {} is not supported: expected Debian, found {}",
                self.remote.label(),
                id
            ),
            None => anyhow::bail!(
                "host {} is missing ID in /etc/os-release",
                self.remote.label()
            ),
        }
    }

    pub fn package_installed(&self, package: &str) -> Result<bool> {
        if self.remote.is_dry_run() {
            return Ok(false);
        }

        let output = self
            .remote
            .run(&format!("dpkg -s {} >/dev/null 2>&1", shell_quote(package)))?;
        Ok(output.success())
    }

    pub fn get_codename(&self) -> Result<String> {
        if self.remote.is_dry_run() {
            return Ok("bookworm".to_string());
        }

        let os_release = self.remote.run_checked("cat /etc/os-release")?.stdout;
        parse_codename(&os_release)
    }

    pub fn read_privileged_file(&self, path: &str) -> Result<Option<String>> {
        if self.remote.is_dry_run() {
            return Ok(None);
        }
        self.remote.read_file_privileged(path)
    }

    pub fn read_apt_source_file(&self, path: &str) -> Result<Option<String>> {
        if self.remote.is_dry_run() {
            return Ok(None);
        }
        self.remote.read_file(path)
    }

    pub fn apt_keyring_exists(&self, path: &str) -> Result<bool> {
        self.remote.file_exists(path)
    }

    /// Returns the installed version of a package, or `None` if not installed.
    pub fn installed_package_version(&self, package: &str) -> Result<Option<String>> {
        if self.remote.is_dry_run() {
            return Ok(None);
        }
        let package = shell_quote(package);
        let output = self.remote.run(&format!(
            "if dpkg -s {package} >/dev/null 2>&1; then dpkg-query -W -f='${{Version}}' {package}; fi",
        ))?;
        if !output.success() {
            anyhow::bail!(
                "host {} could not query installed package version: {}",
                self.remote.label(),
                output.stderr.trim()
            );
        }
        let version = output.stdout.trim();
        if version.is_empty() {
            Ok(None)
        } else {
            Ok(Some(version.to_string()))
        }
    }

    /// Returns `true` if the installed version satisfies `desired`.
    /// `"latest"` always returns `false` — use [`package_is_upgradable`] instead.
    /// Strips Debian epoch and revision suffixes before comparing bare upstream versions.
    pub fn package_version_satisfies(&self, package: &str, desired: &str) -> Result<bool> {
        if desired == "latest" {
            return Ok(false);
        }

        let installed = match self.installed_package_version(package)? {
            Some(value) => value,
            None => return Ok(false),
        };
        if installed == desired {
            return Ok(true);
        }

        // Debian versions: [epoch:]upstream[-revision]. Strip both to compare bare upstream.
        let bare = installed
            .split(':')
            .next_back()
            .unwrap_or(&installed)
            .split('-')
            .next()
            .unwrap_or(&installed);
        Ok(bare == desired)
    }

    /// Returns `true` if a newer candidate version is available for the package.
    /// Uses `apt-cache policy` for reliable, stable output. Returns `true` in dry-run mode.
    pub fn package_is_upgradable(&self, package: &str) -> Result<bool> {
        if self.remote.is_dry_run() {
            return Ok(true);
        }

        let output = self
            .remote
            .run(&format!("apt-cache policy {}", shell_quote(package)))?;
        if !output.success() {
            anyhow::bail!(
                "host {} could not determine whether package {} is upgradable: {}",
                self.remote.label(),
                package,
                output.stderr.trim()
            );
        }

        // Parse "Installed: X" and "Candidate: Y" from apt-cache policy output.
        // A package is upgradable when both fields are present, non-empty, not "(none)",
        // and the candidate version differs from the installed version.
        let mut installed = None;
        let mut candidate = None;
        for line in output.stdout.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("Installed:") {
                installed = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Candidate:") {
                candidate = Some(v.trim().to_string());
            }
        }
        match (installed, candidate) {
            (Some(i), Some(c)) if i != "(none)" && c != "(none)" && i != c => Ok(true),
            _ => Ok(false),
        }
    }

    /// Writes an APT source list file at `path` (mode 0o644, root-owned).
    pub fn write_apt_source_file(&self, path: &str, content: &str) -> Result<()> {
        self.remote
            .write_file_atomic_privileged(path, content, 0o644)
    }

    /// Fetches a GPG key from `url`, dearmors it, and writes the keyring to `keyring_path`.
    pub fn fetch_gpg_keyring(&self, url: &str, keyring_path: &str) -> Result<()> {
        let parent_dir = std::path::Path::new(keyring_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/"))
            .to_string_lossy()
            .into_owned();
        let command = format!(
            "set -eu; mkdir -p {parent}; tmp=$(mktemp /tmp/apt-key.XXXXXX); curl -fsSL {url} -o \"$tmp\"; gpg --dearmor -o {keyring} \"$tmp\"; rm -f \"$tmp\"",
            parent = shell_quote(&parent_dir),
            url = shell_quote(url),
            keyring = shell_quote(keyring_path)
        );
        self.remote.run_privileged_checked(&command)?;
        Ok(())
    }

    pub fn apt_update(&self) -> Result<()> {
        self.remote.run_privileged_checked("apt-get update -qq")?;
        Ok(())
    }

    pub fn apt_install(&self, packages: &[String]) -> Result<()> {
        let joined = packages
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ");
        self.remote
            .run_privileged_checked(&format!("apt-get install -y -qq {}", joined))?;
        Ok(())
    }

    /// Lists all `.hcl` files directly inside `dir`.
    ///
    /// Returns a sorted list of absolute file paths.
    pub fn list_hcl_files(&self, dir: &str) -> Result<Vec<String>> {
        self.remote.list_files_privileged(dir, "*.hcl")
    }

    /// Removes a privileged file at `path`.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.remote
            .run_privileged_checked(&format!("rm -f {}", shell_quote(path)))?;
        Ok(())
    }

    /// Writes `content` atomically to `path` (mode 0o640).
    ///
    /// Used for `nomad.env` which must always exist to satisfy the systemd
    /// `EnvironmentFile=` directive in the Nomad service unit on Debian.
    /// Callers are responsible for rendering and validating content; use
    /// `render_env_content` in the configure module.
    pub fn write_env_file(&self, path: &str, content: &str) -> Result<()> {
        self.remote.write_file_atomic_privileged(path, content, 0o640)
    }

    /// Writes a privileged config file at `path` (mode 0o640, root-owned).
    /// For configs that support validation, prefer [`write_config_validated`].
    #[allow(dead_code)]
    pub fn write_config(&self, path: &str, content: &str) -> Result<()> {
        self.remote
            .write_file_atomic_privileged(path, content, 0o640)
    }

    /// Writes a privileged config file at `path` (mode 0o640, root-owned), running
    /// `validate_cmd` against the staged temp file before committing. `validate_cmd`
    /// is a shell fragment where `$tmp` refers to the staged file path. If validation
    /// fails, the staged file is removed and the destination is left untouched.
    pub fn write_config_validated(
        &self,
        path: &str,
        content: &str,
        validate_cmd: &str,
    ) -> Result<()> {
        self.remote
            .write_file_atomic_privileged_validated(path, content, 0o640, validate_cmd)
    }

    /// Restarts a systemd service by name.
    pub fn restart_service(&self, service: &str) -> Result<()> {
        self.remote
            .run_privileged_checked(&format!("systemctl restart {}", shell_quote(service)))?;
        Ok(())
    }

    /// Runs `command` on the remote host and returns its stdout.
    pub fn command_output(&self, command: &str) -> Result<String> {
        Ok(self.remote.run_checked(command)?.stdout)
    }
}

pub fn parse_os_release(content: &str) -> std::collections::HashMap<String, String> {
    let mut parsed = std::collections::HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            parsed.insert(
                key.to_string(),
                value.trim_matches('"').trim_matches('\'').to_string(),
            );
        }
    }
    parsed
}

pub fn parse_codename(content: &str) -> Result<String> {
    let parsed = parse_os_release(content);
    parsed
        .get("VERSION_CODENAME")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("could not determine VERSION_CODENAME from /etc/os-release"))
}

pub fn apt_repo_contents(
    signed_by_path: &str,
    base_url: &str,
    suite: &str,
    component: &str,
) -> String {
    format!(
        "deb [signed-by={}] {} {} {}\n",
        signed_by_path, base_url, suite, component
    )
}

pub fn normalize_config(config: &str) -> String {
    config
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{recording_target, RecordingTransport};
    use crate::transport::{RemoteHost, RemoteOutput};

    #[test]
    fn test_parse_os_release() {
        let parsed = parse_os_release(
            r#"
            PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
            VERSION_CODENAME=bookworm
            ID=debian
            "#,
        );
        assert_eq!(parsed.get("ID"), Some(&"debian".to_string()));
        assert_eq!(
            parsed.get("VERSION_CODENAME"),
            Some(&"bookworm".to_string())
        );
    }

    #[test]
    fn test_parse_codename() {
        let codename = parse_codename("VERSION_CODENAME=bookworm\n").expect("codename");
        assert_eq!(codename, "bookworm");
    }

    #[test]
    fn test_apt_repo_contents() {
        let repo = apt_repo_contents(
            "/usr/share/keyrings/hashicorp-archive-keyring.gpg",
            "https://apt.releases.hashicorp.com",
            "bookworm",
            "main",
        );
        assert!(repo.contains("bookworm"));
        assert!(repo.contains("/usr/share/keyrings/hashicorp-archive-keyring.gpg"));
        assert!(repo.contains("https://apt.releases.hashicorp.com"));
    }

    #[test]
    fn test_normalize_config() {
        let config = "server {\n  enabled = true\n}\n\n";
        assert_eq!(normalize_config(config), "server {\n  enabled = true\n}");
    }

    #[test]
    fn test_package_is_upgradable_detects_upgradable_output() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "nomad:\n  Installed: 1.7.9\n  Candidate: 1.8.0\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        assert!(host.package_is_upgradable("nomad").expect("package query"));
        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(commands[0], "apt-cache policy nomad");
    }

    #[test]
    fn test_package_is_upgradable_detects_current_package() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "nomad:\n  Installed: 1.8.0\n  Candidate: 1.8.0\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        assert!(!host.package_is_upgradable("nomad").expect("package query"));
    }

    #[test]
    fn test_package_version_satisfies_exact_match() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "1.6.0-1\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        assert!(host
            .package_version_satisfies("nomad", "1.6.0-1")
            .expect("version check"));
    }

    #[test]
    fn test_package_version_satisfies_strips_revision() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "1.6.0-1\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        assert!(host
            .package_version_satisfies("nomad", "1.6.0")
            .expect("version check"));
    }

    #[test]
    fn test_package_version_satisfies_strips_epoch() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "1:1.6.0-1\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        assert!(host
            .package_version_satisfies("nomad", "1.6.0")
            .expect("version check"));
    }

    #[test]
    fn test_package_version_satisfies_no_match() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "1.5.0-1\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        assert!(!host
            .package_version_satisfies("nomad", "1.6.0")
            .expect("version check"));
    }

    #[test]
    fn test_package_version_satisfies_package_missing() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        assert!(!host
            .package_version_satisfies("nomad", "1.6.0")
            .expect("version check"));
    }

    #[test]
    fn test_package_version_satisfies_latest_always_false() {
        // "latest" is handled by a separate upgrade-check path; satisfies is always false
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "1.6.0-1\n".to_string(),
            stderr: String::new(),
        }]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));
        assert!(!host
            .package_version_satisfies("nomad", "latest")
            .expect("version check"));
    }

    #[test]
    fn test_write_config_validated_includes_validate_cmd_before_mv() {
        let transport = RecordingTransport::new(vec![
            // id -u → 0 (root, bypasses escalation wrapper)
            RemoteOutput {
                status: 0,
                stdout: "0\n".to_string(),
                stderr: String::new(),
            },
            // the atomic write+validate+mv command
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let target = recording_target();
        let host = DebianHost::new(RemoteHost::new(&transport, &target));

        host.write_config_validated(
            "/etc/nomad.d/nomad.hcl",
            "name = \"test\"\n",
            "nomad agent -validate -config \"$tmp\"",
        )
        .expect("write succeeds");

        let commands = transport.commands.lock().expect("commands lock");
        // commands[0] = "id -u", commands[1] = the atomic shell script
        assert_eq!(commands[0], "id -u");
        let script = &commands[1];
        assert!(
            script.contains("nomad agent -validate -config \"$tmp\""),
            "validate_cmd must appear in the shell script: {}",
            script
        );
        assert!(
            script.contains("mv \"$tmp\""),
            "mv must follow validation: {}",
            script
        );
        let validate_pos = script
            .find("nomad agent -validate")
            .expect("validate present");
        let mv_pos = script.find("mv \"$tmp\"").expect("mv present");
        assert!(validate_pos < mv_pos, "validate must precede mv");
    }

    #[test]
    fn test_write_env_file_pipes_content_to_privileged_write() {
        let transport = RecordingTransport::new(vec![
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

        host.write_env_file("/etc/nomad.d/nomad.env", "KEY=\"value\"\n")
            .expect("env file write");

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(commands[0], "id -u");
        assert!(commands[1].contains("cat >"), "command should pipe content");
    }

    #[test]
    fn test_write_env_file_accepts_empty_content() {
        let transport = RecordingTransport::new(vec![
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

        host.write_env_file("/etc/nomad.d/nomad.env", "")
            .expect("empty env file write succeeds");

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(commands[0], "id -u");
        assert!(commands[1].contains("cat >"), "command should pipe content");
    }
}
