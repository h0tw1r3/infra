use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use log::{debug, info};

use crate::models::ResolvedTarget;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl RemoteOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

pub trait Transport: Send + Sync {
    fn is_dry_run(&self) -> bool;
    fn exec(
        &self,
        target: &ResolvedTarget,
        command: &str,
        input: Option<&[u8]>,
    ) -> Result<RemoteOutput>;
}

pub struct SshTransport {
    dry_run: bool,
}

impl SshTransport {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }
}

impl Transport for SshTransport {
    fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    fn exec(
        &self,
        target: &ResolvedTarget,
        command: &str,
        input: Option<&[u8]>,
    ) -> Result<RemoteOutput> {
        let mut args = Vec::new();
        if let Some(port) = target.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }
        if let Some(user) = &target.user {
            args.push("-l".to_string());
            args.push(user.clone());
        }
        if let Some(identity_file) = &target.identity_file {
            args.push("-i".to_string());
            args.push(identity_file.clone());
        }
        for option in &target.options {
            args.push("-o".to_string());
            args.push(option.clone());
        }
        args.push(target.host.clone());
        args.push(format!("sh -lc {}", shell_quote(command)));

        if self.dry_run {
            info!(
                "[DRY RUN:{}] ssh {}",
                target.label(),
                args.iter()
                    .map(|value| shell_quote(value))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            return Ok(RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        debug!("Executing over SSH on {}: {}", target.label(), command);
        let mut child = Command::new("ssh")
            .args(&args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                anyhow::anyhow!("failed to start ssh for {}: {}", target.label(), err)
            })?;

        if let Some(input) = input {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                anyhow::anyhow!("failed to open ssh stdin for {}", target.label())
            })?;
            stdin.write_all(input)?;
        }

        let output = child.wait_with_output()?;
        Ok(RemoteOutput {
            status: output.status.code().unwrap_or(255),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }
}

pub struct RemoteHost<'a> {
    transport: &'a dyn Transport,
    target: &'a ResolvedTarget,
}

impl<'a> RemoteHost<'a> {
    pub fn new(transport: &'a dyn Transport, target: &'a ResolvedTarget) -> Self {
        Self { transport, target }
    }

    pub fn label(&self) -> &str {
        self.target.label()
    }

    pub fn is_dry_run(&self) -> bool {
        self.transport.is_dry_run()
    }

    pub fn run(&self, command: &str) -> Result<RemoteOutput> {
        self.transport.exec(self.target, command, None)
    }

    pub fn run_checked(&self, command: &str) -> Result<RemoteOutput> {
        let output = self.run(command)?;
        ensure_success(self.label(), command, &output)?;
        Ok(output)
    }

    pub fn run_with_input_checked(&self, command: &str, input: &[u8]) -> Result<RemoteOutput> {
        let output = self.transport.exec(self.target, command, Some(input))?;
        ensure_success(self.label(), command, &output)?;
        Ok(output)
    }

    pub fn file_exists(&self, path: &str) -> Result<bool> {
        let command = format!("[ -e {} ]", shell_quote(path));
        Ok(self.run(&command)?.success())
    }

    pub fn read_file(&self, path: &str) -> Result<Option<String>> {
        let command = format!(
            "if [ -f {} ]; then cat {}; fi",
            shell_quote(path),
            shell_quote(path)
        );
        let output = self.run_checked(&command)?;
        if output.stdout.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output.stdout))
        }
    }

    pub fn write_file_atomic(&self, path: &str, content: &str, mode: u32) -> Result<()> {
        let parent = Path::new(path)
            .parent()
            .and_then(|value| value.to_str())
            .unwrap_or("/");
        let command = format!(
            "set -eu; mkdir -p {parent}; tmp=$(mktemp {parent}/.nomad-bootstrapper.XXXXXX); cat > \"$tmp\"; chmod {mode:o} \"$tmp\"; mv \"$tmp\" {path}",
            parent = shell_quote(parent),
            mode = mode,
            path = shell_quote(path),
        );
        self.run_with_input_checked(&command, content.as_bytes())?;
        Ok(())
    }
}

fn ensure_success(host: &str, command: &str, output: &RemoteOutput) -> Result<()> {
    if output.success() {
        return Ok(());
    }

    anyhow::bail!(
        "host {} command failed (exit {}): {}\nstdout: {}\nstderr: {}",
        host,
        output.status,
        command,
        output.stdout.trim(),
        output.stderr.trim()
    )
}

pub fn shell_quote(value: &str) -> String {
    shell_escape::unix::escape(value.into()).to_string()
}
