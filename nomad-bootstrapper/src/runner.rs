use anyhow::Result;
use log::{debug, info};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};

/// CommandRunner wraps system command execution with logging and error handling
pub struct CommandRunner {
    dry_run: bool,
}

impl CommandRunner {
    pub fn new(dry_run: bool) -> Self {
        CommandRunner { dry_run }
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Execute a shell command and return output
    pub fn run(&self, cmd: &str, args: &[&str]) -> Result<Output> {
        if self.dry_run {
            info!("[DRY RUN] Would execute: {} {}", cmd, args.join(" "));
            return Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: vec![],
                stderr: vec![],
            });
        }

        debug!("Executing: {} {}", cmd, args.join(" "));
        let output = Command::new(cmd).args(args).output().map_err(|e| {
            anyhow::anyhow!("Failed to execute '{}' with args {:?}: {}", cmd, args, e)
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}: {}",
                cmd,
                output.status.code(),
                stderr
            );
        }

        Ok(output)
    }

    /// Run a command and return stdout as string
    pub fn run_output(&self, cmd: &str, args: &[&str]) -> Result<String> {
        let output = self.run(cmd, args)?;
        let stdout = String::from_utf8(output.stdout)?;
        Ok(stdout.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_runner_dry_run() {
        let runner = CommandRunner::new(true);
        let result = runner.run("echo", &["hello"]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_command_runner_echo() {
        let runner = CommandRunner::new(false);
        let result = runner.run("echo", &["hello"]);
        assert!(result.is_ok());
    }
}
