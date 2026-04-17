/// Persistent provisioning state tracking
use anyhow::Result;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const PROVISIONED_STATE_PATH: &str = "/etc/nomad.d/.provisioned.toml";

/// Tracks the state of a previously provisioned Nomad node
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProvisionedState {
    /// Version of Nomad that was installed
    pub provisioned_version: Option<String>,
    /// Timestamp (ISO8601) of last successful provision
    pub provisioned_timestamp: Option<String>,
    /// Hash of the last applied configuration
    pub last_configure_hash: Option<String>,
}

impl ProvisionedState {
    /// Load state from disk if it exists, otherwise return default empty state
    pub fn load() -> Result<Self> {
        if Path::new(PROVISIONED_STATE_PATH).exists() {
            let content = fs::read_to_string(PROVISIONED_STATE_PATH)?;
            let state: ProvisionedState = toml::from_str(&content)?;
            debug!("Loaded provisioned state: {:?}", state);
            Ok(state)
        } else {
            debug!("No provisioned state file found; starting fresh");
            Ok(Self::default())
        }
    }

    /// Save state to disk
    pub fn save(&self) -> Result<()> {
        let parent = Path::new(PROVISIONED_STATE_PATH).parent();
        if let Some(parent_path) = parent {
            fs::create_dir_all(parent_path)?;
        }

        let content = toml::to_string_pretty(self)?;
        fs::write(PROVISIONED_STATE_PATH, content)?;
        info!("Saved provisioned state to {}", PROVISIONED_STATE_PATH);
        Ok(())
    }

    /// Check if the provisioned version matches the desired version
    #[allow(dead_code)]
    pub fn version_matches(&self, desired: &str) -> bool {
        if desired == "latest" {
            // For "latest", we can't compare directly; always re-check
            false
        } else {
            self.provisioned_version.as_deref() == Some(desired)
        }
    }

    /// Check if configuration hash has changed
    #[allow(dead_code)]
    pub fn config_hash_changed(&self, new_hash: &str) -> bool {
        self.last_configure_hash.as_deref() != Some(new_hash)
    }

    /// Update version and timestamp
    pub fn update_provision(&mut self, version: &str) {
        self.provisioned_version = Some(version.to_string());
        self.provisioned_timestamp = Some(chrono::Local::now().to_rfc3339());
    }

    /// Update configuration hash
    pub fn update_config_hash(&mut self, hash: &str) {
        self.last_configure_hash = Some(hash.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_matches() {
        let state = ProvisionedState {
            provisioned_version: Some("1.6.0".to_string()),
            ..Default::default()
        };
        assert!(state.version_matches("1.6.0"));
        assert!(!state.version_matches("1.5.0"));
        assert!(!state.version_matches("latest"));
    }

    #[test]
    fn test_latest_version_never_matches() {
        let state = ProvisionedState {
            provisioned_version: Some("1.6.0".to_string()),
            ..Default::default()
        };
        assert!(!state.version_matches("latest"));
    }

    #[test]
    fn test_config_hash_changed() {
        let state = ProvisionedState::default();
        assert!(state.config_hash_changed("abc123"));

        let state = ProvisionedState {
            last_configure_hash: Some("abc123".to_string()),
            ..Default::default()
        };
        assert!(!state.config_hash_changed("abc123"));
        assert!(state.config_hash_changed("def456"));
    }

    #[test]
    fn test_update_provision() {
        let mut state = ProvisionedState::default();
        state.update_provision("1.6.0");

        assert_eq!(state.provisioned_version, Some("1.6.0".to_string()));
        assert!(state.provisioned_timestamp.is_some());
    }

    #[test]
    fn test_update_config_hash() {
        let mut state = ProvisionedState::default();
        state.update_config_hash("newHash");

        assert_eq!(state.last_configure_hash, Some("newHash".to_string()));
    }
}
