//! Integration tests for nomad-bootstrapper
//!
//! These tests validate the bootstrapper behavior in realistic scenarios.
//! Many are designed to run in Docker containers for isolated testing.

#[cfg(test)]
mod tests {
    use std::process::Command;

    fn bin_path() -> &'static str {
        env!("CARGO_BIN_EXE_nomad-bootstrapper")
    }

    #[test]
    fn test_help_flag_succeeds() {
        let output = Command::new(bin_path())
            .arg("--help")
            .output()
            .expect("run --help");
        assert!(output.status.success());

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Bootstrap and configure Nomad"));
        assert!(stdout.contains("--phase"));
    }

    #[test]
    fn test_version_flag_succeeds() {
        let output = Command::new(bin_path())
            .arg("--version")
            .output()
            .expect("run --version");
        assert!(output.status.success());

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("nomad-bootstrapper"));
    }

    #[test]
    fn test_phase_and_up_to_conflict_is_rejected() {
        let output = Command::new(bin_path())
            .args([
                "--phase",
                "ensure-deps",
                "--up-to",
                "verify",
                "--log-level",
                "info",
            ])
            .output()
            .expect("run conflict flags");
        assert!(!output.status.success());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("cannot be used together"));
    }

    #[test]
    fn test_configure_phase_requires_role_details() {
        let output = Command::new(bin_path())
            .args(["--phase", "configure"])
            .output()
            .expect("run configure without role");
        assert!(!output.status.success());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--role must be specified"));
    }
}
