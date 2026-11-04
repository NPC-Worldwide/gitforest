use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::info;

/// Serves a git repo over a TCP stream using git-upload-pack.
/// This allows a remote peer to fetch/clone from us.
pub async fn serve_upload_pack(repo_path: &Path, stream: TcpStream) -> Result<()> {
    let repo_path = repo_path.to_path_buf();
    info!("Serving git-upload-pack for {:?}", repo_path);

    let mut child = tokio::process::Command::new("git-upload-pack")
        .arg("--strict")
        .arg(&repo_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn git-upload-pack")?;

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();

    // Use into_split for owned halves that can be moved into spawned tasks
    let (mut stream_read, mut stream_write) = stream.into_split();

    let to_git = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match stream_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if child_stdin.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        drop(child_stdin);
    });

    let from_git = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match child_stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if stream_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::join!(to_git, from_git);
    let _ = child.wait().await;

    Ok(())
}

/// Serves a git repo for push via git-receive-pack.
/// Only called after we verify the pusher is authorized.
pub async fn serve_receive_pack(repo_path: &Path, stream: TcpStream) -> Result<()> {
    let repo_path = repo_path.to_path_buf();
    info!("Serving git-receive-pack for {:?}", repo_path);

    let mut child = tokio::process::Command::new("git-receive-pack")
        .arg(&repo_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn git-receive-pack")?;

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = child.stdout.take().unwrap();

    let (mut stream_read, mut stream_write) = stream.into_split();

    let to_git = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match stream_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if child_stdin.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        drop(child_stdin);
    });

    let from_git = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match child_stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if stream_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::join!(to_git, from_git);
    let _ = child.wait().await;

    Ok(())
}

/// List refs in a local git repo.
pub fn list_refs(repo_path: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["show-ref", "--head"])
        .current_dir(repo_path)
        .output()
        .context("failed to run git show-ref")?;

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

/// Get HEAD commit SHA for a repo.
pub fn get_head(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("failed to run git rev-parse HEAD")?;

    if !output.status.success() {
        anyhow::bail!("git rev-parse HEAD failed");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn setup_test_repo() -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("gt_test_bridge_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        StdCommand::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        std::fs::write(tmp.join("file.txt"), "hello").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&tmp)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        tmp
    }

    #[test]
    fn test_list_refs() {
        let repo = setup_test_repo();
        let refs = list_refs(&repo).unwrap();
        assert!(!refs.is_empty());
        // At least one ref should be a branch
        assert!(refs.iter().any(|(_, name)| name.starts_with("refs/")));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_list_refs_empty_repo() {
        let tmp =
            std::env::temp_dir().join(format!("gt_test_bridge_empty_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        StdCommand::new("git")
            .args(["init", "--bare"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        let refs = list_refs(&tmp).unwrap();
        assert!(refs.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_get_head() {
        let repo = setup_test_repo();
        let head = get_head(&repo).unwrap();
        assert_eq!(head.len(), 40);
        assert!(head.chars().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_get_head_no_commits() {
        let tmp =
            std::env::temp_dir().join(format!("gt_test_bridge_nohead_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        StdCommand::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        assert!(get_head(&tmp).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
