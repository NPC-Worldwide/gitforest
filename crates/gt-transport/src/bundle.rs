use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Create a git bundle from a bare repo.
/// Bundles are the payload shared via the P2P network.
pub fn create_bundle(repo_path: &Path, output_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["bundle", "create"])
        .arg(output_path)
        .arg("--all")
        .current_dir(repo_path)
        .status()
        .context("failed to run git bundle create")?;

    if !status.success() {
        anyhow::bail!("git bundle create failed with status {status}");
    }
    Ok(())
}

/// Create an incremental bundle (only commits after a given ref).
pub fn create_incremental_bundle(
    repo_path: &Path,
    output_path: &Path,
    since_ref: &str,
) -> Result<()> {
    let status = Command::new("git")
        .args(["bundle", "create"])
        .arg(output_path)
        .arg(format!("{since_ref}..HEAD"))
        .current_dir(repo_path)
        .status()
        .context("failed to run git bundle create (incremental)")?;

    if !status.success() {
        anyhow::bail!("git bundle create (incremental) failed with status {status}");
    }
    Ok(())
}

/// Verify a git bundle is valid.
pub fn verify_bundle(bundle_path: &Path) -> Result<bool> {
    let status = Command::new("git")
        .args(["bundle", "verify"])
        .arg(bundle_path)
        .status()
        .context("failed to run git bundle verify")?;

    Ok(status.success())
}

/// Clone from a bundle file into a new repo.
pub fn clone_from_bundle(bundle_path: &Path, target_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone"])
        .arg(bundle_path)
        .arg(target_path)
        .status()
        .context("failed to clone from bundle")?;

    if !status.success() {
        anyhow::bail!("git clone from bundle failed with status {status}");
    }
    Ok(())
}

/// Fetch from a bundle into an existing repo.
pub fn fetch_from_bundle(repo_path: &Path, bundle_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["fetch"])
        .arg(bundle_path)
        .current_dir(repo_path)
        .status()
        .context("failed to fetch from bundle")?;

    if !status.success() {
        anyhow::bail!("git fetch from bundle failed with status {status}");
    }
    Ok(())
}

/// List refs in a bundle.
pub fn list_bundle_refs(bundle_path: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["bundle", "list-heads"])
        .arg(bundle_path)
        .output()
        .context("failed to run git bundle list-heads")?;

    if !output.status.success() {
        anyhow::bail!("git bundle list-heads failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let refs: Vec<(String, String)> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect();
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Create a temp git repo with one commit, return its path.
    fn setup_test_repo() -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("gt_test_bundle_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        Command::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        std::fs::write(tmp.join("README.md"), "# Test\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        tmp
    }

    #[test]
    fn test_create_and_verify_bundle() {
        let repo = setup_test_repo();
        let bundle_path = repo.join("test.bundle");

        create_bundle(&repo, &bundle_path).unwrap();
        assert!(bundle_path.exists());
        assert!(verify_bundle(&bundle_path).unwrap());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_clone_from_bundle() {
        let repo = setup_test_repo();
        let bundle_path = repo.join("test.bundle");
        create_bundle(&repo, &bundle_path).unwrap();

        let clone_dir =
            std::env::temp_dir().join(format!("gt_test_clone_{}", uuid::Uuid::new_v4()));
        clone_from_bundle(&bundle_path, &clone_dir).unwrap();

        // Cloned repo should have HEAD
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&clone_dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(sha.len(), 40);

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    #[test]
    fn test_list_bundle_refs() {
        let repo = setup_test_repo();
        let bundle_path = repo.join("test.bundle");
        create_bundle(&repo, &bundle_path).unwrap();

        let refs = list_bundle_refs(&bundle_path).unwrap();
        assert!(!refs.is_empty());
        // Should have at least HEAD or refs/heads/main/master
        let has_known_ref = refs
            .iter()
            .any(|(_, name)| name.contains("HEAD") || name.contains("refs/heads/"));
        assert!(
            has_known_ref,
            "expected at least one HEAD or branch ref, got: {:?}",
            refs
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_verify_invalid_bundle() {
        let tmp =
            std::env::temp_dir().join(format!("gt_test_invalid_bundle_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("garbage.bundle");
        std::fs::write(&path, b"this is not a git bundle").unwrap();

        let result = verify_bundle(&path).unwrap();
        assert!(!result);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_fetch_from_bundle() {
        let repo = setup_test_repo();
        let bundle_path = repo.join("test.bundle");
        create_bundle(&repo, &bundle_path).unwrap();

        // Create a second empty repo to fetch into
        let repo2 = std::env::temp_dir().join(format!("gt_test_fetch_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&repo2).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(&repo2)
            .output()
            .unwrap();

        fetch_from_bundle(&repo2, &bundle_path).unwrap();

        // Should now have FETCH_HEAD
        let output = Command::new("git")
            .args(["rev-parse", "FETCH_HEAD"])
            .current_dir(&repo2)
            .output()
            .unwrap();
        assert!(output.status.success());

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&repo2);
    }
}
