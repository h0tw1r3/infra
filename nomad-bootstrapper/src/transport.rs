use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio as ProcessStdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use openssh::{Session, Stdio};
use tempfile::{Builder as TempDirBuilder, TempDir};
use tokio::io::AsyncWriteExt;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

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
    fn check_session(&self, _target: &ResolvedTarget) -> Result<()> {
        Ok(())
    }
    fn exec(
        &self,
        target: &ResolvedTarget,
        command: &str,
        input: Option<&[u8]>,
    ) -> Result<RemoteOutput>;
}

struct SessionHandle {
    session: Arc<Session>,
    control_dir: TempDir,
    control_path: PathBuf,
    host: String,
}

pub struct SshTransport {
    dry_run: bool,
    runtime: Mutex<Runtime>,
    sessions: Mutex<HashMap<String, SessionHandle>>,
}

impl SshTransport {
    pub fn new(dry_run: bool) -> Result<Self> {
        let runtime = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create SSH runtime")?;

        Ok(Self {
            dry_run,
            runtime: Mutex::new(runtime),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn get_or_connect_session(&self, target: &ResolvedTarget) -> Result<Arc<Session>> {
        let key = session_key(target);
        if let Some(handle) = self.sessions.lock().expect("SSH sessions lock").get(&key) {
            return Ok(Arc::clone(&handle.session));
        }

        let handle = self.connect_session(target)?;
        let session = Arc::clone(&handle.session);

        let mut sessions = self.sessions.lock().expect("SSH sessions lock");
        if let Some(existing) = sessions.get(&key) {
            return Ok(Arc::clone(&existing.session));
        }

        sessions.insert(key, handle);
        Ok(session)
    }

    fn connect_session(&self, target: &ResolvedTarget) -> Result<SessionHandle> {
        let control_dir = TempDirBuilder::new()
            .prefix("nomad-bootstrapper-ssh.")
            .tempdir()
            .context("failed to create SSH control directory")?;
        let control_path = control_dir.path().join("control");
        let launch_args = build_master_args(target, &control_path);

        debug!("Establishing SSH session for {}", target.label());
        let output = Command::new("ssh")
            .args(&launch_args)
            .stdin(ProcessStdio::null())
            .stdout(ProcessStdio::piped())
            .stderr(ProcessStdio::piped())
            .output()
            .map_err(|err| {
                anyhow::anyhow!(
                    "failed to start ssh session for {}: {}",
                    target.label(),
                    err
                )
            })?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to establish ssh session for {} (exit {}): stdout: {} stderr: {}",
                target.label(),
                output.status.code().unwrap_or(255),
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        if !control_path.exists() {
            anyhow::bail!(
                "failed to establish ssh session for {}: control socket was not created",
                target.label()
            );
        }

        Ok(SessionHandle {
            session: Arc::new(Session::resume(
                control_path.clone().into_boxed_path(),
                None,
            )),
            control_dir,
            control_path,
            host: target.host.clone(),
        })
    }

    fn close_all_sessions(&self) {
        let handles = {
            let mut sessions = self.sessions.lock().expect("SSH sessions lock");
            sessions
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>()
        };

        if handles.is_empty() {
            return;
        }

        let runtime = self.runtime.lock().expect("SSH runtime lock");
        for handle in handles {
            let SessionHandle {
                session,
                control_dir,
                control_path,
                host,
            } = handle;

            let close_result = match Arc::try_unwrap(session) {
                Ok(session) => runtime.block_on(session.close()).map_err(|err| err.into()),
                Err(_) => close_master_process(&control_path, &host),
            };

            if let Err(err) = close_result {
                warn!(
                    "failed to close SSH session for {} using {}: {}",
                    host,
                    control_path.display(),
                    err
                );
            }

            drop(control_dir);
        }
    }
}

impl Transport for SshTransport {
    fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    fn check_session(&self, target: &ResolvedTarget) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        let key = session_key(target);
        let session = self
            .sessions
            .lock()
            .expect("SSH sessions lock")
            .get(&key)
            .map(|handle| Arc::clone(&handle.session))
            .ok_or_else(|| {
                anyhow::anyhow!("no retained SSH session exists for {}", target.label())
            })?;

        let runtime = self.runtime.lock().expect("SSH runtime lock");
        runtime.block_on(async move {
            session.check().await.map_err(|err| {
                anyhow::anyhow!(
                    "retained SSH session is unhealthy for {}: {}",
                    target.label(),
                    err
                )
            })
        })
    }

    fn exec(
        &self,
        target: &ResolvedTarget,
        command: &str,
        input: Option<&[u8]>,
    ) -> Result<RemoteOutput> {
        let exec_args = build_exec_args(target, command);
        if self.dry_run {
            info!(
                "[DRY RUN:{}] ssh {}",
                target.label(),
                exec_args
                    .iter()
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

        let session = self.get_or_connect_session(target)?;
        debug!(
            "Executing over retained SSH session on {}: {}",
            target.label(),
            command
        );

        let runtime = self.runtime.lock().expect("SSH runtime lock");
        runtime.block_on(async move {
            let mut remote_command = session.shell(command);
            let output = if let Some(input) = input {
                remote_command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                let mut child = remote_command.spawn().await.map_err(|err| {
                    anyhow::anyhow!(
                        "failed to start remote command for {}: {}",
                        target.label(),
                        err
                    )
                })?;
                let mut stdin = child.stdin().take().ok_or_else(|| {
                    anyhow::anyhow!("failed to open remote stdin for {}", target.label())
                })?;
                stdin.write_all(input).await.with_context(|| {
                    format!("failed to write remote stdin for {}", target.label())
                })?;
                drop(stdin);
                child.wait_with_output().await.map_err(|err| {
                    anyhow::anyhow!(
                        "failed to collect remote command output for {}: {}",
                        target.label(),
                        err
                    )
                })?
            } else {
                remote_command.output().await.map_err(|err| {
                    anyhow::anyhow!(
                        "failed to run remote command for {}: {}",
                        target.label(),
                        err
                    )
                })?
            };

            Ok(RemoteOutput {
                status: output.status.code().unwrap_or(255),
                stdout: String::from_utf8(output.stdout)
                    .context("remote stdout was not valid UTF-8")?,
                stderr: String::from_utf8(output.stderr)
                    .context("remote stderr was not valid UTF-8")?,
            })
        })
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        self.close_all_sessions();
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

    pub fn current_uid(&self) -> Result<u32> {
        let output = self.run_checked("id -u")?;
        output
            .stdout
            .trim()
            .parse::<u32>()
            .map_err(|err| anyhow::anyhow!("host {} returned invalid uid: {}", self.label(), err))
    }

    pub fn run_privileged_checked(&self, command: &str) -> Result<RemoteOutput> {
        let command = self.privileged_command(command)?;
        let output = self.run(&command)?;
        ensure_success(self.label(), &command, &output)?;
        Ok(output)
    }

    pub fn run_privileged_with_input_checked(
        &self,
        command: &str,
        input: &[u8],
    ) -> Result<RemoteOutput> {
        let command = self.privileged_command(command)?;
        let output = self.transport.exec(self.target, &command, Some(input))?;
        ensure_success(self.label(), &command, &output)?;
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

    /// List files matching `name_pattern` directly inside `dir`.
    ///
    /// Returns a sorted list of absolute file paths. Requires privilege escalation
    /// since `/etc/nomad.d` may not be world-readable.
    pub fn list_files_privileged(&self, dir: &str, name_pattern: &str) -> Result<Vec<String>> {
        let command = format!(
            "find {} -maxdepth 1 -name {} -type f",
            shell_quote(dir),
            shell_quote(name_pattern),
        );
        let output = self.run_privileged_checked(&command)?;
        let mut paths: Vec<String> = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        paths.sort();
        Ok(paths)
    }

    pub fn read_file_privileged(&self, path: &str) -> Result<Option<String>> {
        let command = format!(
            "if [ -f {} ]; then cat {}; fi",
            shell_quote(path),
            shell_quote(path)
        );
        let output = self.run_privileged_checked(&command)?;
        if output.stdout.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output.stdout))
        }
    }

    pub fn write_file_atomic_privileged(&self, path: &str, content: &str, mode: u32) -> Result<()> {
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
        self.run_privileged_with_input_checked(&command, content.as_bytes())?;
        Ok(())
    }

    /// Like [`write_file_atomic_privileged`] but runs `validate_cmd` against the staged
    /// temp file before committing. `validate_cmd` is a shell fragment where `$tmp` holds
    /// the path to the staged file. If validation fails, the temp file is removed and an
    /// error is returned, leaving the destination file untouched.
    pub fn write_file_atomic_privileged_validated(
        &self,
        path: &str,
        content: &str,
        mode: u32,
        validate_cmd: &str,
    ) -> Result<()> {
        let parent = Path::new(path)
            .parent()
            .and_then(|value| value.to_str())
            .unwrap_or("/");
        let command = format!(
            "set -eu; mkdir -p {parent}; tmp=$(mktemp {parent}/.nomad-bootstrapper.XXXXXX); cat > \"$tmp\"; chmod {mode:o} \"$tmp\"; if ! {validate_cmd}; then rm -f \"$tmp\"; exit 1; fi; mv \"$tmp\" {path}",
            parent = shell_quote(parent),
            mode = mode,
            validate_cmd = validate_cmd,
            path = shell_quote(path),
        );
        self.run_privileged_with_input_checked(&command, content.as_bytes())?;
        Ok(())
    }

    fn privileged_command(&self, command: &str) -> Result<String> {
        if self.is_dry_run() {
            return Ok(command.to_string());
        }

        if self.current_uid()? == 0 {
            return Ok(command.to_string());
        }

        let escalation = self.target.privilege_escalation.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "host {} requires configured privilege escalation for privileged command",
                self.label()
            )
        })?;

        let prefix = escalation
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!("{} sh -lc {}", prefix, shell_quote(command)))
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

fn build_exec_args(target: &ResolvedTarget, command: &str) -> Vec<String> {
    let mut args = ssh_target_args(target);
    args.push(target.host.clone());
    args.push(format!("sh -lc {}", shell_quote(command)));
    args
}

fn build_master_args(target: &ResolvedTarget, control_path: &Path) -> Vec<String> {
    let mut args = ssh_target_args(target);
    args.push("-o".to_string());
    args.push("BatchMode=yes".to_string());
    args.push("-o".to_string());
    args.push("ControlMaster=yes".to_string());
    args.push("-o".to_string());
    args.push(format!("ControlPath={}", control_path.display()));
    args.push("-o".to_string());
    args.push("ControlPersist=yes".to_string());
    args.push("-f".to_string());
    args.push("-N".to_string());
    args.push(target.host.clone());
    args
}

fn ssh_target_args(target: &ResolvedTarget) -> Vec<String> {
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
    args
}

fn close_master_process(control_path: &Path, host: &str) -> Result<()> {
    let output = Command::new("ssh")
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("exit")
        .arg(host)
        .stdin(ProcessStdio::null())
        .stdout(ProcessStdio::piped())
        .stderr(ProcessStdio::piped())
        .output()
        .context("failed to execute ssh control exit")?;

    if !output.status.success() {
        anyhow::bail!(
            "ssh control exit failed (exit {}): stdout: {} stderr: {}",
            output.status.code().unwrap_or(255),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

fn session_key(target: &ResolvedTarget) -> String {
    format!(
        "{}\u{1f}|{}\u{1f}|{}\u{1f}|{}\u{1f}|{}",
        target.host,
        target.user.as_deref().unwrap_or_default(),
        target
            .port
            .map(|value| value.to_string())
            .unwrap_or_default(),
        target.identity_file.as_deref().unwrap_or_default(),
        target.options.join("\u{1e}")
    )
}

pub fn shell_quote(value: &str) -> String {
    shell_escape::unix::escape(value.into()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ResolvedTarget {
        ResolvedTarget {
            name: "node-1".to_string(),
            host: "node-1.example.com".to_string(),
            user: Some("root".to_string()),
            identity_file: Some("~/.ssh/id_ed25519".to_string()),
            port: Some(2222),
            options: vec!["StrictHostKeyChecking=accept-new".to_string()],
            privilege_escalation: None,
        }
    }

    #[test]
    fn test_build_exec_args_preserves_legacy_ssh_shape() {
        let args = build_exec_args(&target(), "nomad version");

        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "-l".to_string(),
                "root".to_string(),
                "-i".to_string(),
                "~/.ssh/id_ed25519".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
                "node-1.example.com".to_string(),
                "sh -lc 'nomad version'".to_string(),
            ]
        );
    }

    #[test]
    fn test_build_master_args_adds_control_master_options() {
        let args = build_master_args(&target(), Path::new("/tmp/control"));

        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"ControlMaster=yes".to_string()));
        assert!(args.contains(&"ControlPersist=yes".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ControlPath=/tmp/control".to_string()));
    }

    #[test]
    fn test_session_key_changes_with_connection_settings() {
        let baseline = session_key(&target());

        let mut changed_port = target();
        changed_port.port = Some(4647);
        assert_ne!(baseline, session_key(&changed_port));

        let mut changed_option = target();
        changed_option.options.push("Compression=yes".to_string());
        assert_ne!(baseline, session_key(&changed_option));
    }

    struct RecordingTransport {
        outputs: std::sync::Mutex<Vec<RemoteOutput>>,
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingTransport {
        fn new(outputs: Vec<RemoteOutput>) -> Self {
            Self {
                outputs: std::sync::Mutex::new(outputs.into_iter().rev().collect()),
                commands: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Transport for RecordingTransport {
        fn is_dry_run(&self) -> bool {
            false
        }

        fn exec(
            &self,
            _target: &ResolvedTarget,
            command: &str,
            _input: Option<&[u8]>,
        ) -> Result<RemoteOutput> {
            self.commands
                .lock()
                .expect("commands lock")
                .push(command.to_string());
            Ok(self
                .outputs
                .lock()
                .expect("outputs lock")
                .pop()
                .expect("output"))
        }
    }

    #[test]
    fn test_run_privileged_checked_bypasses_escalation_for_root() {
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
        let mut target = target();
        target.privilege_escalation = Some(vec!["sudo".to_string(), "-n".to_string()]);
        let remote = RemoteHost::new(&transport, &target);

        remote
            .run_privileged_checked("apt-get update -qq")
            .expect("privileged command succeeds");

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(commands[0], "id -u");
        assert_eq!(commands[1], "apt-get update -qq");
    }

    #[test]
    fn test_run_privileged_checked_uses_configured_escalation_for_non_root() {
        let transport = RecordingTransport::new(vec![
            RemoteOutput {
                status: 0,
                stdout: "1000\n".to_string(),
                stderr: String::new(),
            },
            RemoteOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let mut target = target();
        target.user = Some("admin".to_string());
        target.privilege_escalation = Some(vec!["sudo".to_string(), "-n".to_string()]);
        let remote = RemoteHost::new(&transport, &target);

        remote
            .run_privileged_checked("apt-get update -qq")
            .expect("privileged command succeeds");

        let commands = transport.commands.lock().expect("commands lock");
        assert_eq!(commands[0], "id -u");
        assert_eq!(commands[1], "sudo -n sh -lc 'apt-get update -qq'");
    }

    #[test]
    fn test_run_privileged_checked_rejects_non_root_without_escalation() {
        let transport = RecordingTransport::new(vec![RemoteOutput {
            status: 0,
            stdout: "1000\n".to_string(),
            stderr: String::new(),
        }]);
        let mut target = target();
        target.user = Some("admin".to_string());
        target.privilege_escalation = None;
        let remote = RemoteHost::new(&transport, &target);

        let err = remote
            .run_privileged_checked("apt-get update -qq")
            .expect_err("missing escalation should fail");
        assert!(err
            .to_string()
            .contains("requires configured privilege escalation"));
    }
}
