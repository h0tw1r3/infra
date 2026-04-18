use anyhow::Result;

use crate::debian::{apt_repo_contents, normalize_config, DebianHost};
use crate::executor::PhaseExecutor;
use crate::models::{ExecutionContext, NodeConfig, PhaseResult};

pub struct SetupRepo;

const HASHICORP_GPG_URL: &str = "https://apt.releases.hashicorp.com/gpg";
const HASHICORP_REPO_URL: &str = "https://apt.releases.hashicorp.com";
const HASHICORP_KEYRING_PATH: &str = "/usr/share/keyrings/hashicorp-archive-keyring.gpg";
const HASHICORP_SOURCE_LIST_PATH: &str = "/etc/apt/sources.list.d/hashicorp.list";

impl PhaseExecutor for SetupRepo {
    fn execute(
        &self,
        host: &DebianHost<'_>,
        _config: &NodeConfig,
        _ctx: &mut ExecutionContext,
    ) -> Result<PhaseResult> {
        let codename = host.get_codename()?;
        let desired_repo = apt_repo_contents(
            HASHICORP_KEYRING_PATH,
            HASHICORP_REPO_URL,
            &codename,
            "main",
        );
        let mut changed = false;

        if !host.apt_keyring_exists(HASHICORP_KEYRING_PATH)? {
            host.fetch_gpg_keyring(HASHICORP_GPG_URL, HASHICORP_KEYRING_PATH)?;
            changed = true;
        }

        let existing_repo = host.read_apt_source_file(HASHICORP_SOURCE_LIST_PATH)?;
        let repo_matches = existing_repo
            .as_deref()
            .map(normalize_config)
            .map(|current| current == normalize_config(&desired_repo))
            .unwrap_or(false);

        if !repo_matches {
            host.write_apt_source_file(HASHICORP_SOURCE_LIST_PATH, &desired_repo)?;
            changed = true;
        }

        if changed {
            host.apt_update()?;
            return Ok(PhaseResult::changed(
                self.name(),
                format!("configured HashiCorp repository for {}", codename),
            ));
        }

        Ok(PhaseResult::unchanged(
            self.name(),
            format!("HashiCorp repository already configured for {}", codename),
        ))
    }

    fn name(&self) -> &'static str {
        "setup-repo"
    }
}
