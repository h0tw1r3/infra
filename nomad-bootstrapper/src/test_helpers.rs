use anyhow::Result;
use std::sync::Mutex;

use crate::models::ResolvedTarget;
use crate::transport::{RemoteOutput, Transport};

pub struct RecordingTransport {
    pub outputs: Mutex<Vec<RemoteOutput>>,
    pub commands: Mutex<Vec<String>>,
}

impl RecordingTransport {
    pub fn new(outputs: Vec<RemoteOutput>) -> Self {
        Self {
            outputs: Mutex::new(outputs.into_iter().rev().collect()),
            commands: Mutex::new(Vec::new()),
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

pub fn recording_target() -> ResolvedTarget {
    ResolvedTarget {
        name: "node-1".to_string(),
        host: "node-1.example.com".to_string(),
        user: None,
        identity_file: None,
        port: None,
        options: Vec::new(),
        privilege_escalation: None,
    }
}
