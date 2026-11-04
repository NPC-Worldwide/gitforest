use anyhow::{Context, Result};
use gt_core::config::RepoConfig;
use gt_core::identity::{Identity, PeerId};
use gt_core::protocol::RepoAnnouncement;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

/// Manages local git repos for the daemon.
#[allow(dead_code)]
pub struct RepoManager {
    data_dir: PathBuf,
}

#[allow(dead_code)]
impl RepoManager {
    pub fn new(data_dir: PathBuf) -> Self {
        RepoManager { data_dir }
    }

    /// Initialize a bare git repo in the data directory.
    pub fn init_bare(&self, name: &str) -> Result<PathBuf> {
        let repo_path = self.data_dir.join("repos").join(format!("{name}.git"));
        std::fs::create_dir_all(&repo_path)?;

        let status = Command::new("git")
            .args(["init", "--bare"])
            .arg(&repo_path)
            .status()
            .context("failed to run git init --bare")?;

        if !status.success() {
            anyhow::bail!("git init --bare failed");
        }

        info!("Initialized bare repo at {}", repo_path.display());
        Ok(repo_path)
    }

    /// Create a repo announcement for the network.
    pub fn make_announcement(
        &self,
        identity: &Identity,
        config: &RepoConfig,
    ) -> Result<RepoAnnouncement> {
        let head = get_head_commit(&config.path).unwrap_or_else(|_| "0000000".to_string());
        let default_branch =
            get_default_branch(&config.path).unwrap_or_else(|_| "main".to_string());

        let mut announcement = RepoAnnouncement {
            owner: identity.peer_id(),
            name: config.name.clone(),
            description: config.description.clone().unwrap_or_default(),
            default_branch,
            head,
            signature: Vec::new(),
            display_name: gt_core::identity::load_display_name(),
        };

        // Sign the announcement
        let payload = announcement_payload(&announcement);
        announcement.signature = identity.sign(&payload);

        Ok(announcement)
    }

    /// Path where we store cloned repos.
    pub fn cloned_repo_path(&self, owner: &PeerId, name: &str) -> PathBuf {
        self.data_dir
            .join("cloned")
            .join(owner.to_string())
            .join(format!("{name}.git"))
    }
}

fn get_head_commit(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("failed to get HEAD")?;

    if !output.status.success() {
        anyhow::bail!("no HEAD commit");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_default_branch(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("failed to get default branch")?;

    if !output.status.success() {
        anyhow::bail!("no default branch");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn announcement_payload(a: &RepoAnnouncement) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&a.owner.0);
    data.extend_from_slice(a.name.as_bytes());
    data.extend_from_slice(a.description.as_bytes());
    data.extend_from_slice(a.default_branch.as_bytes());
    data.extend_from_slice(a.head.as_bytes());
    data
}
