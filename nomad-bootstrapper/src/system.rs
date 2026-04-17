use anyhow::Result;
use log::debug;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub const NOMAD_CONFIG_PATH: &str = "/etc/nomad.d/nomad.hcl";
pub const HASHICORP_KEYRING_PATH: &str = "/usr/share/keyrings/hashicorp-archive-keyring.gpg";
pub const HASHICORP_SOURCE_LIST_PATH: &str = "/etc/apt/sources.list.d/hashicorp.list";

/// Check if a package is installed via dpkg
pub fn check_pkg(pkg: &str) -> bool {
    std::process::Command::new("dpkg")
        .args(["-s", pkg])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Get the Debian/Ubuntu codename from /etc/os-release
pub fn get_codename() -> Result<String> {
    let content = fs::read_to_string("/etc/os-release")?;
    parse_codename(&content)
}

pub fn parse_codename(content: &str) -> Result<String> {
    for line in content.lines() {
        if line.starts_with("VERSION_CODENAME=") {
            let codename = line
                .strip_prefix("VERSION_CODENAME=")
                .unwrap_or_default()
                .trim_matches('"');
            debug!("Detected codename: {}", codename);
            return Ok(codename.to_string());
        }
    }

    anyhow::bail!("Could not determine Debian codename from /etc/os-release")
}

/// Check if the Nomad package is present on the system
pub fn is_nomad_present() -> bool {
    check_pkg("nomad")
}

/// Return the installed Nomad package version string, or None if not installed
pub fn installed_nomad_version() -> Option<String> {
    if !is_nomad_present() {
        return None;
    }

    std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "nomad"])
        .output()
        .ok()
        .and_then(|output| {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if version.is_empty() {
                None
            } else {
                Some(version)
            }
        })
}

/// Check if the installed Nomad version satisfies the desired version using
/// Debian-native version comparison. Handles epoch prefixes and revision
/// suffixes (e.g., desired "1.7.0" matches installed "1.7.0-1").
///
/// Returns false if Nomad is not installed or the comparison fails.
pub fn nomad_version_satisfies(desired: &str) -> bool {
    let installed = match installed_nomad_version() {
        Some(v) => v,
        None => return false,
    };

    // Try exact match first (fast path)
    if installed == desired {
        return true;
    }

    // Use dpkg --compare-versions for Debian-aware comparison.
    // Strip any Debian revision suffix from the installed version for comparison
    // (e.g., "1.7.0-1" → compare "1.7.0" eq "1.7.0").
    let installed_upstream = installed.split('-').next().unwrap_or(&installed);
    if installed_upstream == desired {
        return true;
    }

    // Fall back to dpkg --compare-versions for edge cases
    std::process::Command::new("dpkg")
        .args(["--compare-versions", &installed, "eq", desired])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read and parse existing Nomad HCL configuration
pub fn read_nomad_config() -> Result<Option<String>> {
    match fs::read_to_string(NOMAD_CONFIG_PATH) {
        Ok(config) => Ok(Some(config)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Failed to read nomad.hcl: {}", e)),
    }
}

pub fn file_exists(path: &str) -> bool {
    Path::new(path).exists()
}

pub fn read_file_if_exists(path: &str) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Failed to read {}: {}", path, e)),
    }
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

pub fn desired_repo_contents(codename: &str) -> String {
    format!(
        "deb [signed-by={}] https://apt.releases.hashicorp.com {} main\n",
        HASHICORP_KEYRING_PATH, codename
    )
}

/// Compute a hash of the configuration for change detection
pub fn config_hash(config: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let normalized = normalize_config(config);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Set restrictive permissions on a config file (owner read/write, group read).
/// Uses mode 0640 to prevent world-readable access to potentially sensitive
/// Nomad configuration (join addresses, TLS paths, tokens).
pub fn set_config_permissions(path: &str) -> Result<()> {
    let permissions = fs::Permissions::from_mode(0o640);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_codename_parsing() {
        let content = "NAME=Debian\nVERSION_CODENAME=bookworm\n";
        assert_eq!(parse_codename(content).expect("codename"), "bookworm");
    }

    #[test]
    fn test_normalize_config_trims_line_endings() {
        let config = "server {\n  enabled = true\n}\n\n";
        assert_eq!(normalize_config(config), "server {\n  enabled = true\n}");
    }

    #[test]
    fn test_desired_repo_contents() {
        let repo = desired_repo_contents("bookworm");
        assert!(repo.contains("bookworm"));
        assert!(repo.contains(HASHICORP_KEYRING_PATH));
    }

    #[test]
    fn test_config_hash_consistency() {
        let config = "server {\n  enabled = true\n}\n";
        let hash1 = config_hash(config);
        let hash2 = config_hash(config);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_config_hash_different_for_different_configs() {
        let config1 = "server {\n  enabled = true\n}\n";
        let config2 = "server {\n  enabled = false\n}\n";
        let hash1 = config_hash(config1);
        let hash2 = config_hash(config2);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_is_nomad_present_returns_bool() {
        // On dev machines nomad is likely not installed; just verify it returns without panic
        let _ = is_nomad_present();
    }

    #[test]
    fn test_installed_nomad_version_returns_option() {
        // Verify the function works regardless of whether nomad is installed
        let result = installed_nomad_version();
        // If nomad is not installed, should be None
        if !is_nomad_present() {
            assert!(result.is_none());
        }
    }
}
