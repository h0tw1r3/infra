use anyhow::Result;

use crate::transport::{shell_quote, RemoteHost};

pub const NOMAD_CONFIG_PATH: &str = "/etc/nomad.d/nomad.hcl";
pub const HASHICORP_KEYRING_PATH: &str = "/usr/share/keyrings/hashicorp-archive-keyring.gpg";
pub const HASHICORP_SOURCE_LIST_PATH: &str = "/etc/apt/sources.list.d/hashicorp.list";

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

    pub fn check_pkg(&self, pkg: &str) -> Result<bool> {
        if self.remote.is_dry_run() {
            return Ok(false);
        }

        let output = self
            .remote
            .run(&format!("dpkg -s {} >/dev/null 2>&1", shell_quote(pkg)))?;
        Ok(output.success())
    }

    pub fn get_codename(&self) -> Result<String> {
        if self.remote.is_dry_run() {
            return Ok("bookworm".to_string());
        }

        let os_release = self.remote.run_checked("cat /etc/os-release")?.stdout;
        parse_codename(&os_release)
    }

    pub fn read_nomad_config(&self) -> Result<Option<String>> {
        if self.remote.is_dry_run() {
            return Ok(None);
        }
        self.remote.read_file(NOMAD_CONFIG_PATH)
    }

    pub fn read_repo_file(&self) -> Result<Option<String>> {
        if self.remote.is_dry_run() {
            return Ok(None);
        }
        self.remote.read_file(HASHICORP_SOURCE_LIST_PATH)
    }

    pub fn keyring_exists(&self) -> Result<bool> {
        self.remote.file_exists(HASHICORP_KEYRING_PATH)
    }

    pub fn installed_nomad_version(&self) -> Result<Option<String>> {
        if self.remote.is_dry_run() {
            return Ok(None);
        }
        let output = self.remote.run(
            "if dpkg -s nomad >/dev/null 2>&1; then dpkg-query -W -f='${Version}' nomad; fi",
        )?;
        if !output.success() {
            anyhow::bail!(
                "host {} could not query installed nomad version: {}",
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

    pub fn nomad_version_satisfies(&self, desired: &str) -> Result<bool> {
        if desired == "latest" {
            return Ok(false);
        }

        let installed = match self.installed_nomad_version()? {
            Some(value) => value,
            None => return Ok(false),
        };
        if installed == desired {
            return Ok(true);
        }

        Ok(installed.split('-').next().unwrap_or(&installed) == desired)
    }

    pub fn write_repo_file(&self, content: &str) -> Result<()> {
        self.remote
            .write_file_atomic(HASHICORP_SOURCE_LIST_PATH, content, 0o644)
    }

    pub fn fetch_hashicorp_keyring(&self) -> Result<()> {
        let command = format!(
            "set -eu; mkdir -p /usr/share/keyrings; tmp=$(mktemp /tmp/hashicorp-key.XXXXXX); curl -fsSL https://apt.releases.hashicorp.com/gpg -o \"$tmp\"; gpg --dearmor -o {keyring} \"$tmp\"; rm -f \"$tmp\"",
            keyring = shell_quote(HASHICORP_KEYRING_PATH)
        );
        self.remote.run_checked(&command)?;
        Ok(())
    }

    pub fn apt_update(&self) -> Result<()> {
        self.remote.run_checked("apt-get update -qq")?;
        Ok(())
    }

    pub fn apt_install(&self, packages: &[String]) -> Result<()> {
        let joined = packages
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ");
        self.remote
            .run_checked(&format!("apt-get install -y -qq {}", joined))?;
        Ok(())
    }

    pub fn write_nomad_config(&self, config: &str) -> Result<()> {
        let command = format!(
            "set -eu; mkdir -p /etc/nomad.d; tmp=$(mktemp /etc/nomad.d/nomad.hcl.XXXXXX); cat > \"$tmp\"; chmod 640 \"$tmp\"; if nomad agent -h 2>/dev/null | grep -q -- -validate; then nomad agent -validate -config=\"$tmp\" >/dev/null; fi; mv \"$tmp\" {path}",
            path = shell_quote(NOMAD_CONFIG_PATH)
        );
        self.remote
            .run_with_input_checked(&command, config.as_bytes())?;
        Ok(())
    }

    pub fn restart_nomad(&self) -> Result<()> {
        self.remote.run_checked("systemctl restart nomad")?;
        Ok(())
    }

    pub fn nomad_version_output(&self) -> Result<String> {
        if self.remote.is_dry_run() {
            return Ok("Nomad vDRY-RUN".to_string());
        }
        Ok(self.remote.run_checked("nomad version")?.stdout)
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

pub fn desired_repo_contents(codename: &str) -> String {
    format!(
        "deb [signed-by={}] https://apt.releases.hashicorp.com {} main\n",
        HASHICORP_KEYRING_PATH, codename
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
    fn test_desired_repo_contents() {
        let repo = desired_repo_contents("bookworm");
        assert!(repo.contains("bookworm"));
        assert!(repo.contains(HASHICORP_KEYRING_PATH));
    }

    #[test]
    fn test_normalize_config() {
        let config = "server {\n  enabled = true\n}\n\n";
        assert_eq!(normalize_config(config), "server {\n  enabled = true\n}");
    }
}
