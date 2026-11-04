//! git-remote-gitforest — Git remote helper for gitforest:// URLs
//!
//! Git invokes this binary automatically when it encounters a remote URL
//! starting with "gitforest://". It speaks the git remote helper protocol
//! on stdin/stdout and communicates with the local gitforest daemon via HTTP.
//!
//! Usage by git (automatic):
//!   git clone gitforest://<peer_id>/<repo_name>
//!   git push gitforest://<peer_id>/<repo_name>
//!   git fetch gitforest://<peer_id>/<repo_name>

use anyhow::{Context, Result};
use base64::Engine;
use std::io::{self, BufRead, Write};

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7878";

/// Return the daemon URL from the environment or the default.
fn daemon_url() -> String {
    std::env::var("GITFOREST_DAEMON_URL").unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string())
}

/// Read the local API token from the well-known path.
fn api_token() -> Option<String> {
    let path = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".local/share/gitforest/api.token"))
        .unwrap_or_else(|| std::env::temp_dir().join("gitforest-api.token"));
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Build an HTTP client with the local API token attached.
fn http_client() -> reqwest::blocking::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = api_token() {
        let value = format!("Bearer {token}");
        if let Ok(header) = reqwest::header::HeaderValue::from_str(&value) {
            headers.insert("Authorization", header);
        }
    }
    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

fn http_get(url: &str) -> Result<reqwest::blocking::Response, reqwest::Error> {
    http_client().get(url).send()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Git calls us with: git-remote-gitforest <remote-name> <url>
    if args.len() < 3 {
        eprintln!("Usage: git-remote-gitforest <remote> <url>");
        eprintln!("This is a git remote helper — it's called by git automatically.");
        std::process::exit(1);
    }

    let _remote_name = &args[1];
    let url = &args[2];

    // Parse gitforest://<peer_id_or_name>/<repo_name>
    let (peer_identifier, repo_name) = parse_url(url).context("invalid gitforest URL")?;

    // Resolve name tags (e.g., "laptop#cd26") to full peer IDs via the daemon
    let peer_id = resolve_identifier(&peer_identifier);

    eprintln!("gitforest: connecting to {peer_id}/{repo_name}");

    // Git remote helper protocol: read commands from stdin, respond on stdout
    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf)?;
        if n == 0 {
            break;
        } // EOF

        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }

        match line {
            "capabilities" => {
                writeln!(out, "fetch")?;
                writeln!(out, "push")?;
                writeln!(out, "option")?;
                writeln!(out)?;
                out.flush()?;
            }

            cmd if cmd.starts_with("option ") => {
                writeln!(out, "unsupported")?;
                out.flush()?;
            }

            "list" | "list for-push" => {
                let refs = list_remote_refs(&peer_id, &repo_name)?;
                for (sha, refname) in &refs {
                    writeln!(out, "{sha} {refname}")?;
                }
                writeln!(out)?;
                out.flush()?;
            }

            cmd if cmd.starts_with("fetch ") => {
                let mut fetch_refs = vec![cmd.to_string()];
                // Read remaining fetch lines from the same reader
                loop {
                    line_buf.clear();
                    let n = reader.read_line(&mut line_buf)?;
                    if n == 0 {
                        break;
                    }
                    let l = line_buf.trim();
                    if l.is_empty() {
                        break;
                    }
                    fetch_refs.push(l.to_string());
                }

                do_fetch(&peer_id, &repo_name, &fetch_refs)?;
                writeln!(out)?;
                out.flush()?;
            }

            cmd if cmd.starts_with("push ") => {
                // push <src>:<dst>
                let mut push_refs = vec![cmd.to_string()];
                loop {
                    line_buf.clear();
                    let n = reader.read_line(&mut line_buf)?;
                    if n == 0 {
                        break;
                    }
                    let l = line_buf.trim();
                    if l.is_empty() {
                        break;
                    }
                    push_refs.push(l.to_string());
                }

                do_push(&peer_id, &repo_name, &push_refs, &mut out)?;
                writeln!(out)?;
                out.flush()?;
            }

            "" => {
                break;
            }

            other => {
                eprintln!("gitforest: unknown command: {other}");
            }
        }
    }

    Ok(())
}

/// Parse gitforest://<peer_id>/<repo_name>
fn parse_url(url: &str) -> Result<(String, String)> {
    let stripped = url
        .strip_prefix("gitforest://")
        .context("URL must start with gitforest://")?;

    let parts: Vec<&str> = stripped.splitn(2, '/').collect();
    if parts.len() != 2 {
        anyhow::bail!("URL must be gitforest://<peer_id>/<repo_name>");
    }

    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Resolve a peer identifier (name tag, alias, or full hex) to a full peer ID.
///
/// Tries the daemon's `/api/resolve/{identifier}` endpoint first.
/// Falls back to using the identifier as-is (for direct hex peer IDs).
fn resolve_identifier(identifier: &str) -> String {
    // If it's already a 64-char hex string, use directly
    if identifier.len() == 64 && identifier.chars().all(|c| c.is_ascii_hexdigit()) {
        return identifier.to_string();
    }

    // Try resolving via daemon
    let daemon = daemon_url();
    let url = format!("{daemon}/api/resolve/{identifier}");
    if let Ok(resp) = http_get(&url) {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>() {
                if let Some(peer_id) = json["peer_id"].as_str() {
                    eprintln!("gitforest: resolved {} -> {}", identifier, peer_id);
                    return peer_id.to_string();
                }
            }
        }
    }

    // Fall back to using identifier as-is
    eprintln!("gitforest: could not resolve '{}', using as-is", identifier);
    identifier.to_string()
}

/// Ask the daemon for the refs on a remote repo.
fn list_remote_refs(peer_id: &str, repo_name: &str) -> Result<Vec<(String, String)>> {
    // Try the daemon API
    let daemon = daemon_url();
    let url = format!("{daemon}/api/remote/{peer_id}/{repo_name}/refs");

    let response = http_get(&url);
    match response {
        Ok(resp) if resp.status().is_success() => {
            let refs: Vec<(String, String)> = resp.json()?;
            Ok(refs)
        }
        _ => {
            // Daemon might not be running — check if this is a local repo
            eprintln!("gitforest: daemon not reachable, checking local config");
            Ok(vec![])
        }
    }
}

/// Fetch objects from the remote via the daemon.
/// Uses `git fetch <bundle>` to import objects without overwriting local HEAD.
fn do_fetch(peer_id: &str, repo_name: &str, _refs: &[String]) -> Result<()> {
    eprintln!("gitforest: fetching from {peer_id}/{repo_name} via daemon");

    let daemon = daemon_url();
    let url = format!("{daemon}/api/remote/{peer_id}/{repo_name}/fetch");
    let resp = http_client().post(&url).send()?;

    if !resp.status().is_success() {
        anyhow::bail!("fetch failed: HTTP {}", resp.status());
    }

    let bytes = resp.bytes()?;
    let tmp = std::env::temp_dir().join(format!("gf_recv_{}.bundle", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &bytes)?;
    eprintln!(
        "gitforest: received {} bytes, fetching from bundle",
        bytes.len()
    );

    // Use `git fetch <bundle>` which updates FETCH_HEAD / remote-tracking refs
    // without touching the local HEAD or checked-out branch.
    let output = std::process::Command::new("git")
        .args(["fetch", &tmp.display().to_string()])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("git fetch from bundle failed: {stderr}");
    }

    let _ = std::fs::remove_file(&tmp);
    eprintln!("gitforest: fetch complete");
    Ok(())
}

/// A ref update sent with a push request.
#[derive(serde::Serialize)]
struct PushRefUpdate {
    pusher: String,
    ref_name: String,
    new_sha: String,
    old_sha: String,
}

/// Sign a push with our identity key so the receiving daemon can bind the
/// claimed pusher id to a key holder (stops cluster-member impersonation).
fn sign_push(
    identity: &gt_core::identity::Identity,
    repo_name: &str,
    ref_b64: &str,
    bundle: &[u8],
) -> String {
    let payload = gt_core::protocol::push_signable_payload(repo_name, ref_b64, bundle);
    base64::engine::general_purpose::STANDARD.encode(identity.sign(&payload))
}

/// Push refs to the remote via the daemon.
fn do_push(peer_id: &str, repo_name: &str, refs: &[String], out: &mut impl Write) -> Result<()> {
    // Load our identity: the pusher id we claim must be the key we sign with.
    let identity = gt_core::identity::Identity::load(&gt_core::identity::identity_key_path())
        .context("no local identity key — run `gitforest init` first")?;
    let pusher = identity.peer_id().to_string();

    // Query remote refs so we can build incremental bundles
    let remote_refs = list_remote_refs(peer_id, repo_name).unwrap_or_default();
    // list_remote_refs returns (sha, refname) but we need refname -> sha
    let remote_ref_map: std::collections::HashMap<String, String> = remote_refs
        .into_iter()
        .map(|(sha, name)| (name, sha))
        .collect();

    let mut bundle_specs: Vec<String> = Vec::new();
    let mut ref_updates: Vec<PushRefUpdate> = Vec::new();

    for push_line in refs {
        let push_line = push_line.strip_prefix("push ").unwrap_or(push_line);
        let parts: Vec<&str> = push_line.splitn(2, ':').collect();
        if parts.len() != 2 {
            continue;
        }
        let (src, dst) = (parts[0], parts[1]);

        if src.is_empty() {
            // Deletion
            let old_sha = remote_ref_map.get(dst).cloned().unwrap_or_default();
            ref_updates.push(PushRefUpdate {
                pusher: pusher.clone(),
                ref_name: dst.to_string(),
                new_sha: String::new(),
                old_sha,
            });
            continue;
        }

        // Resolve local SHA
        let local_sha_output = std::process::Command::new("git")
            .args(["rev-parse", src])
            .output();
        let local_sha = match local_sha_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => {
                writeln!(out, "error {dst} failed to resolve local ref {src}")?;
                continue;
            }
        };

        // Build bundle spec: incremental if we know the remote SHA
        let remote_sha = remote_ref_map.get(dst).cloned();
        // Use the original refname (e.g. HEAD) for bundle specs — git bundle create
        // resolves refnames correctly but rejects bare SHAs in some cases.
        let bundle_src = if src.starts_with("refs/") || src == "HEAD" {
            src.to_string()
        } else {
            local_sha.clone()
        };
        match remote_sha {
            Some(ref remote)
                if !remote.is_empty() && remote != "0000000000000000000000000000000000000000" =>
            {
                // Exclude old, include new — two separate args for git bundle create
                bundle_specs.push(format!("^{}", remote));
                bundle_specs.push(bundle_src);
            }
            _ => {
                bundle_specs.push(bundle_src);
            }
        }

        ref_updates.push(PushRefUpdate {
            pusher: pusher.clone(),
            ref_name: dst.to_string(),
            new_sha: local_sha,
            old_sha: remote_sha.unwrap_or_default(),
        });
    }

    if ref_updates.is_empty() {
        return Ok(());
    }

    // If every update is a deletion, no bundle needed
    let needs_bundle = !bundle_specs.is_empty();

    let tmp_bundle = std::env::temp_dir().join(format!("gf_push_{}.bundle", std::process::id()));

    if needs_bundle {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["bundle", "create"]);
        cmd.arg(&tmp_bundle);
        for spec in &bundle_specs {
            cmd.arg(spec);
        }
        let status = cmd.status().context("git bundle create failed")?;
        if !status.success() {
            for update in &ref_updates {
                writeln!(out, "error {} bundle creation failed", update.ref_name)?;
            }
            return Ok(());
        }
    }

    // Encode ref updates as base64 JSON header
    let ref_json = serde_json::to_string(&ref_updates).context("serialize ref updates")?;
    let ref_b64 = base64::engine::general_purpose::STANDARD.encode(ref_json);

    let bundle_bytes = if needs_bundle && tmp_bundle.exists() {
        std::fs::read(&tmp_bundle).context("read bundle")?
    } else {
        Vec::new()
    };
    let _ = std::fs::remove_file(&tmp_bundle);

    eprintln!(
        "gitforest: pushing {} ref(s) to {peer_id}/{repo_name} ({} bytes)",
        ref_updates.len(),
        bundle_bytes.len()
    );

    let daemon = daemon_url();
    let url = format!("{daemon}/api/remote/{peer_id}/{repo_name}/push");
    let sig_b64 = sign_push(&identity, repo_name, &ref_b64, &bundle_bytes);
    let response = http_client()
        .post(&url)
        .header("X-gitforest-Push-Refs", ref_b64)
        .header("X-gitforest-Push-Sig", sig_b64)
        .header("Content-Type", "application/x-git-bundle")
        .body(bundle_bytes)
        .send();

    match response {
        Ok(resp) if resp.status().is_success() => {
            let result: serde_json::Value = resp.json().unwrap_or_default();
            let ok_all = result.get("ok").and_then(|v| v.as_bool()) == Some(true);
            let error_msg = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("push rejected");
            let suggestion = result.get("suggestion").and_then(|v| v.as_str());

            if !ok_all {
                if let Some("merge_request") = suggestion {
                    eprintln!("\ngitforest: push rejected — you don't have direct write access.");
                    eprintln!("           The repo owner must grant you access first,");
                    eprintln!("           or you can propose changes as a merge request.");
                    eprintln!("\n           To create a merge request, visit:");
                    eprintln!(
                        "             http://127.0.0.1:7878/repos/{repo_name}/merge-requests"
                    );
                    eprintln!("\n           Or use the CLI (coming soon):");
                    eprintln!("             gitforest mr create {repo_name} --branch <branch>");
                    eprintln!();
                }
            }

            for update in &ref_updates {
                if ok_all {
                    writeln!(out, "ok {}", update.ref_name)?;
                } else {
                    writeln!(out, "error {} {}", update.ref_name, error_msg)?;
                }
            }
        }
        Ok(resp) => {
            for update in &ref_updates {
                writeln!(out, "error {} HTTP {}", update.ref_name, resp.status())?;
            }
        }
        Err(e) => {
            for update in &ref_updates {
                writeln!(out, "error {} {}", update.ref_name, e)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_url() {
        let (peer, repo) = parse_url("gitforest://abc123/myrepo").unwrap();
        assert_eq!(peer, "abc123");
        assert_eq!(repo, "myrepo");
    }

    #[test]
    fn test_parse_url_invalid() {
        assert!(parse_url("https://gitforest.com/foo/bar").is_err());
        assert!(parse_url("gitforest://no-slash").is_err());
    }

    #[test]
    fn test_parse_url_with_name_tag() {
        let (peer, repo) = parse_url("gitforest://laptop#cd26/myrepo").unwrap();
        assert_eq!(peer, "laptop#cd26");
        assert_eq!(repo, "myrepo");
    }

    #[test]
    fn test_sign_push_verifies_and_binds() {
        use base64::Engine;
        let alice = gt_core::identity::Identity::generate();
        let bob = gt_core::identity::Identity::generate();
        let sig_b64 = sign_push(&alice, "myrepo", "refs64", b"bundle-bytes");
        let sig = base64::engine::general_purpose::STANDARD
            .decode(&sig_b64)
            .unwrap();
        let payload = gt_core::protocol::push_signable_payload("myrepo", "refs64", b"bundle-bytes");
        assert!(gt_core::identity::verify_signature(&alice.peer_id(), &payload, &sig).is_ok());
        assert!(gt_core::identity::verify_signature(&bob.peer_id(), &payload, &sig).is_err());
        let tampered = gt_core::protocol::push_signable_payload("myrepo", "refs64", b"other-bytes");
        assert!(gt_core::identity::verify_signature(&alice.peer_id(), &tampered, &sig).is_err());
    }
}
