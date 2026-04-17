//! Integration tests for nomad-bootstrapper.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::{tempdir, TempDir};

    fn bin_path() -> &'static str {
        env!("CARGO_BIN_EXE_nomad-bootstrapper")
    }

    fn write_inventory(contents: &str) -> (TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("inventory.toml");
        fs::write(&path, contents).expect("write inventory");
        (dir, path)
    }

    #[test]
    fn test_help_flag_succeeds() {
        let output = Command::new(bin_path())
            .arg("--help")
            .output()
            .expect("run --help");
        assert!(output.status.success());

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Bootstrap Nomad on Debian hosts over SSH"));
        assert!(stdout.contains("--inventory"));
        assert!(stdout.contains("--phase"));
        assert!(stdout.contains("--preflight-only"));
        assert!(stdout.contains("--concurrency"));
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
        let (_dir, inventory) = write_inventory(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
            bootstrap_expect = 1
        "#,
        );

        let output = Command::new(bin_path())
            .args([
                "--inventory",
                inventory.to_str().expect("inventory path"),
                "--phase",
                "ensure-deps",
                "--up-to",
                "verify",
            ])
            .output()
            .expect("run conflict flags");
        assert!(!output.status.success());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("cannot be used together"));
    }

    #[test]
    fn test_preflight_only_and_phase_conflict_is_rejected() {
        let (_dir, inventory) = write_inventory(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
            bootstrap_expect = 1
        "#,
        );

        let output = Command::new(bin_path())
            .args([
                "--inventory",
                inventory.to_str().expect("inventory path"),
                "--preflight-only",
                "--phase",
                "ensure-deps",
            ])
            .output()
            .expect("run preflight-only conflict");
        assert!(!output.status.success());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--preflight-only cannot be used together"));
    }

    #[test]
    fn test_invalid_inventory_is_rejected() {
        let (_dir, inventory) = write_inventory(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
        "#,
        );

        let output = Command::new(bin_path())
            .args(["--inventory", inventory.to_str().expect("inventory path")])
            .output()
            .expect("run invalid inventory");
        assert!(!output.status.success());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("bootstrap_expect"));
    }

    #[test]
    fn test_dry_run_with_valid_inventory_succeeds() {
        let (_dir, inventory) = write_inventory(
            r#"
            [controller]
            concurrency = 4

            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
            bootstrap_expect = 1
            nomad_version = "latest"
        "#,
        );

        let output = Command::new(bin_path())
            .args([
                "--inventory",
                inventory.to_str().expect("inventory path"),
                "--dry-run",
                "--concurrency",
                "1",
            ])
            .output()
            .expect("run dry-run");
        assert!(output.status.success(), "{:?}", output);
    }

    #[test]
    fn test_success_summary_is_visible_at_warn_log_level() {
        let (_dir, inventory) = write_inventory(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
            bootstrap_expect = 1
            nomad_version = "latest"
        "#,
        );

        let output = Command::new(bin_path())
            .args([
                "--inventory",
                inventory.to_str().expect("inventory path"),
                "--dry-run",
                "--log-level",
                "warn",
            ])
            .output()
            .expect("run dry-run with warn logging");
        assert!(output.status.success(), "{:?}", output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Run succeeded"));
        assert!(stdout.contains("queued_for_provisioning"));
        assert!(stdout.contains("running_phase(ensure-deps)"));
    }

    #[test]
    fn test_preflight_only_dry_run_reports_preflight_summary() {
        let (_dir, inventory) = write_inventory(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
            bootstrap_expect = 1
        "#,
        );

        let output = Command::new(bin_path())
            .args([
                "--inventory",
                inventory.to_str().expect("inventory path"),
                "--dry-run",
                "--preflight-only",
                "--log-level",
                "warn",
            ])
            .output()
            .expect("run preflight-only dry-run");
        assert!(output.status.success(), "{:?}", output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Run succeeded"));
        assert!(stdout.contains("preflight_passed"));
        assert!(!stdout.contains("queued_for_provisioning"));
        assert!(!stdout.contains("running_phase("));
    }

    #[test]
    fn test_zero_concurrency_is_rejected() {
        let (_dir, inventory) = write_inventory(
            r#"
            [[nodes]]
            name = "server-1"
            host = "server-1.example.com"
            role = "server"
            bootstrap_expect = 1
        "#,
        );

        let output = Command::new(bin_path())
            .args([
                "--inventory",
                inventory.to_str().expect("inventory path"),
                "--concurrency",
                "0",
            ])
            .output()
            .expect("run invalid concurrency");
        assert!(!output.status.success());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid value"));
    }
}
