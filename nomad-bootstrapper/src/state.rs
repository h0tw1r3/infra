use chrono::Utc;
use log::warn;
use serde::{Deserialize, Serialize};

use crate::transport::RemoteHost;

pub const PROVISIONED_STATE_PATH: &str = "/etc/nomad.d/.provisioned.toml";

/// The state file is advisory only.
/// Live remote probes remain authoritative for idempotency decisions.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvisionedState {
    pub provisioned_version: Option<String>,
    pub provisioned_timestamp: Option<String>,
    pub last_configure_hash: Option<String>,
}

impl ProvisionedState {
    pub fn load_optional(host: &RemoteHost<'_>) -> Self {
        if host.is_dry_run() {
            return Self::default();
        }

        let raw = match host.read_file(PROVISIONED_STATE_PATH) {
            Ok(raw) => raw,
            Err(err) => {
                warn!(
                    "host {}: ignoring unreadable state file {}: {}",
                    host.label(),
                    PROVISIONED_STATE_PATH,
                    err
                );
                return Self::default();
            }
        };

        let Some(raw) = raw else {
            return Self::default();
        };

        match toml::from_str::<Self>(&raw) {
            Ok(state) => state,
            Err(err) => {
                warn!(
                    "host {}: ignoring invalid state file {}: {}",
                    host.label(),
                    PROVISIONED_STATE_PATH,
                    err
                );
                Self::default()
            }
        }
    }

    pub fn save_optional(&self, host: &RemoteHost<'_>) {
        if host.is_dry_run() {
            return;
        }

        let raw = match toml::to_string_pretty(self) {
            Ok(raw) => raw,
            Err(err) => {
                warn!(
                    "host {}: failed to serialize state file: {}",
                    host.label(),
                    err
                );
                return;
            }
        };

        if let Err(err) = host.write_file_atomic(PROVISIONED_STATE_PATH, &raw, 0o640) {
            warn!(
                "host {}: failed to write optional state file {}: {}",
                host.label(),
                PROVISIONED_STATE_PATH,
                err
            );
        }
    }

    pub fn update_provision(&mut self, version: &str) {
        self.provisioned_version = Some(version.to_string());
        self.provisioned_timestamp = Some(Utc::now().to_rfc3339());
    }

    pub fn update_config_hash(&mut self, hash: &str) {
        self.last_configure_hash = Some(hash.to_string());
    }
}

pub fn config_hash(config: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let normalized = crate::debian::normalize_config(config);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;
    use crate::models::ResolvedTarget;
    use crate::transport::{RemoteHost, RemoteOutput, Transport};

    struct FakeTransport {
        dry_run: bool,
        stdout: String,
        status: i32,
    }

    impl Transport for FakeTransport {
        fn is_dry_run(&self) -> bool {
            self.dry_run
        }

        fn exec(
            &self,
            _target: &ResolvedTarget,
            _command: &str,
            _input: Option<&[u8]>,
        ) -> Result<RemoteOutput> {
            Ok(RemoteOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: String::new(),
            })
        }
    }

    fn fake_host<'a>(transport: &'a dyn Transport) -> RemoteHost<'a> {
        let target = Box::leak(Box::new(ResolvedTarget {
            name: "node-1".to_string(),
            host: "node-1.example.com".to_string(),
            user: None,
            identity_file: None,
            port: None,
            options: Vec::new(),
        }));
        RemoteHost::new(transport, target)
    }

    #[test]
    fn test_update_provision() {
        let mut state = ProvisionedState::default();
        state.update_provision("1.7.6");
        assert_eq!(state.provisioned_version.as_deref(), Some("1.7.6"));
        assert!(state.provisioned_timestamp.is_some());
    }

    #[test]
    fn test_update_config_hash() {
        let mut state = ProvisionedState::default();
        state.update_config_hash("abc123");
        assert_eq!(state.last_configure_hash.as_deref(), Some("abc123"));
    }

    #[test]
    fn test_config_hash_stable() {
        let hash1 = config_hash("server {\n  enabled = true\n}\n");
        let hash2 = config_hash("server {\n  enabled = true\n}\n");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_load_optional_ignores_invalid_state_file() {
        let transport = FakeTransport {
            dry_run: false,
            stdout: "not valid toml".to_string(),
            status: 0,
        };
        let host = fake_host(&transport);

        let state = ProvisionedState::load_optional(&host);
        assert_eq!(state, ProvisionedState::default());
    }

    #[test]
    fn test_load_optional_treats_missing_state_as_default() {
        let transport = FakeTransport {
            dry_run: false,
            stdout: String::new(),
            status: 0,
        };
        let host = fake_host(&transport);

        let state = ProvisionedState::load_optional(&host);
        assert_eq!(state, ProvisionedState::default());
    }
}
