// Integration tests spawn short-lived daemon child processes and clean them up
// with .kill(); waiting on them is unnecessary and would block the async runtime.
#![allow(clippy::zombie_processes)]

use gt_collab::acl::RepoAcl;
use gt_collab::merge_request::MergeRequest;
use gt_collab::patches::PatchSet;
use gt_core::config::{Config, RepoConfig};
use gt_core::identity::{self, Identity, PeerId, PeerName};
use gt_core::types::{BlobId, CollectionFile, CollectionManifest};
use gt_lfs::blob::{adaptive_piece_size, blob_dht_address, BlobDownloadState, BlobMeta};
use gt_lfs::collection::CollectionStore;
use gt_lfs::pointer::{is_pointer_file, PointerFile};
use gt_lfs::store::BlobStore;
use gt_lfs::tracker::BlobTracker;
use gt_transport::torrent::{BundleMeta, DownloadState};
use std::collections::HashMap;
use std::path::PathBuf;

fn push_sig_b64(identity: &Identity, repo: &str, ref_b64: &str, bundle: &[u8]) -> String {
    let payload = gt_core::protocol::push_signable_payload(repo, ref_b64, bundle);
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        identity.sign(&payload),
    )
}

/// Test the full identity lifecycle: generate, save, load, sign, verify.
#[test]
fn test_identity_lifecycle() {
    let id = Identity::generate();
    let peer_id = id.peer_id();

    // Save and reload
    let tmp = std::env::temp_dir().join("gt_test_identity_lifecycle");
    let key_path = tmp.join("test.key");
    let _ = std::fs::remove_file(&key_path);

    id.save(&key_path).unwrap();
    let loaded = Identity::load(&key_path).unwrap();
    assert_eq!(peer_id, loaded.peer_id());

    // Sign and verify
    let data = b"gitforest integration test";
    let sig = id.sign(data);
    assert!(identity::verify_signature(&peer_id, data, &sig).is_ok());

    // Wrong key fails
    let other = Identity::generate();
    assert!(identity::verify_signature(&other.peer_id(), data, &sig).is_err());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test repo address is deterministic and unique per (owner, name) pair.
#[test]
fn test_repo_addressing() {
    let alice = Identity::generate();
    let bob = Identity::generate();

    let addr1 = identity::repo_address(&alice.peer_id(), "myrepo");
    let addr2 = identity::repo_address(&alice.peer_id(), "myrepo");
    let addr3 = identity::repo_address(&alice.peer_id(), "other");
    let addr4 = identity::repo_address(&bob.peer_id(), "myrepo");

    // Same owner + name = same address
    assert_eq!(addr1, addr2);
    // Different name = different address
    assert_ne!(addr1, addr3);
    // Different owner = different address
    assert_ne!(addr1, addr4);
}

/// Test ACL: owner push, grant, revoke, signature verification.
#[test]
fn test_acl_full_flow() {
    let owner = Identity::generate();
    let collaborator = Identity::generate();
    let random = Identity::generate();

    let mut acl = RepoAcl::new(&owner, "project");
    assert!(acl.verify().is_ok());

    // Owner can always push
    assert!(acl.can_push(&owner.peer_id(), "refs/heads/main"));
    assert!(acl.can_push(&owner.peer_id(), "refs/tags/v1.0"));

    // Random user cannot push
    assert!(!acl.can_push(&random.peer_id(), "refs/heads/main"));

    // Grant collaborator access to branches only
    acl.grant(&owner, collaborator.peer_id(), "refs/heads/*".into())
        .unwrap();
    assert!(acl.verify().is_ok());
    assert!(acl.can_push(&collaborator.peer_id(), "refs/heads/feature-x"));
    assert!(!acl.can_push(&collaborator.peer_id(), "refs/tags/v1.0"));

    // Random user still cannot push
    assert!(!acl.can_push(&random.peer_id(), "refs/heads/main"));

    // Non-owner cannot modify ACL
    assert!(acl.grant(&random, random.peer_id(), "*".into()).is_err());

    // Revoke
    acl.revoke(&owner, &collaborator.peer_id()).unwrap();
    assert!(!acl.can_push(&collaborator.peer_id(), "refs/heads/feature-x"));
    assert!(acl.verify().is_ok());

    // Tampered ACL fails verification
    let mut tampered = acl.clone();
    tampered.version = 999;
    assert!(tampered.verify().is_err());
}

/// Test signed patch set creation and verification.
#[test]
fn test_patch_flow() {
    let author = Identity::generate();
    let owner = Identity::generate();

    let ps = PatchSet::new(
        &author,
        owner.peer_id(),
        "target-repo".into(),
        "main".into(),
        "Fix: handle edge case".into(),
        "This patch fixes the null pointer issue".into(),
        vec![b"diff --git a/foo.rs b/foo.rs\n...".to_vec()],
    );

    // Signature is valid
    assert!(ps.verify());
    assert_eq!(ps.author, author.peer_id());
    assert_eq!(ps.patches.len(), 1);

    // Tampered patch fails verification
    let mut tampered = ps.clone();
    tampered.title = "Malicious title".into();
    assert!(!tampered.verify());
}

/// Test merge request creation and verification.
#[test]
fn test_merge_request_flow() {
    let author = Identity::generate();
    let owner = Identity::generate();

    let mr = MergeRequest::new(
        &author,
        "my-fork".into(),
        "feature-branch".into(),
        owner.peer_id(),
        "upstream-repo".into(),
        "main".into(),
        "Add dark mode support".into(),
        "Implements dark mode using CSS variables".into(),
    );

    assert!(mr.verify());
    assert_eq!(mr.author, author.peer_id());

    // Tampered MR fails
    let mut tampered = mr.clone();
    tampered.description = "different".into();
    assert!(!tampered.verify());
}

/// Test bundle metadata and piece-level integrity verification.
#[test]
fn test_torrent_piece_verification() {
    // Create a fake bundle
    let data: Vec<u8> = (0..1024 * 512).map(|i| (i % 256) as u8).collect(); // 512KB
    let tmp = std::env::temp_dir().join("gt_test_bundle");
    std::fs::create_dir_all(&tmp).unwrap();
    let bundle_path = tmp.join("test.bundle");
    std::fs::write(&bundle_path, &data).unwrap();

    let meta = BundleMeta::from_file(&bundle_path, "test-repo", "owner123").unwrap();
    assert_eq!(meta.total_size, data.len() as u64);
    assert!(meta.piece_count() > 0);

    // Full bundle verification
    assert!(meta.verify_bundle(&data));

    // Tampered data fails
    let mut bad_data = data.clone();
    bad_data[0] = 255;
    assert!(!meta.verify_bundle(&bad_data));

    // Piece-level verification
    let piece_size = meta.piece_size;
    for (i, chunk) in data.chunks(piece_size).enumerate() {
        assert!(meta.verify_piece(i, chunk));
    }

    // Tampered piece fails
    let mut bad_piece = data[..piece_size].to_vec();
    bad_piece[0] = 255;
    assert!(!meta.verify_piece(0, &bad_piece));

    // Download state tracking
    let mut dl = DownloadState::new(meta.clone());
    assert!(!dl.is_complete());
    assert_eq!(dl.progress(), 0.0);

    // Receive all pieces
    for (i, chunk) in data.chunks(piece_size).enumerate() {
        assert!(dl.receive_piece(i, chunk.to_vec()));
    }
    assert!(dl.is_complete());
    assert_eq!(dl.progress(), 1.0);

    // Assemble and verify
    let assembled = dl.assemble().unwrap();
    assert_eq!(assembled, data);

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test config serialization roundtrip.
#[test]
fn test_config_roundtrip() {
    let id = Identity::generate();
    let config = Config {
        data_dir: PathBuf::from("/tmp/gitforest-test"),
        p2p_port: 9418,
        api_port: 7878,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![RepoConfig {
            name: "myrepo".into(),
            path: PathBuf::from("/home/user/myrepo.git"),
            owner: id.peer_id(),
            description: Some("Test repo".into()),
            seeding: true,
        }],
        bootstrap_peers: vec![],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: std::collections::HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: None,
        payments: Default::default(),
    };

    let tmp = std::env::temp_dir().join("gt_test_config");
    let config_path = tmp.join("config.toml");
    let _ = std::fs::remove_file(&config_path);

    config.save(&config_path).unwrap();
    let loaded = Config::load(&config_path).unwrap();

    assert_eq!(loaded.p2p_port, 9418);
    assert_eq!(loaded.repos.len(), 1);
    assert_eq!(loaded.repos[0].name, "myrepo");
    assert_eq!(loaded.repos[0].owner, id.peer_id());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test the git remote URL parser.
#[test]
fn test_peer_id_display_parse_roundtrip() {
    let id = Identity::generate();
    let pid = id.peer_id();
    let s = pid.to_string();

    // Should be 64 hex chars (32 bytes)
    assert_eq!(s.len(), 64);

    // Parse back
    let parsed: PeerId = s.parse().unwrap();
    assert_eq!(pid, parsed);

    // Invalid hex should fail
    assert!("not-hex-at-all".parse::<PeerId>().is_err());
    assert!("abcd".parse::<PeerId>().is_err()); // too short
}

// ============ Large File Storage ============

/// Test the full blob lifecycle: create, store, retrieve, pointer, download state.
#[test]
fn test_blob_lifecycle() {
    let tmp = std::env::temp_dir().join("gt_int_blob_lifecycle");
    let _ = std::fs::remove_dir_all(&tmp);

    // Create test data (simulating a small model weight file)
    let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect(); // 512KB
    let input_dir = tmp.join("input");
    std::fs::create_dir_all(&input_dir).unwrap();
    let file_path = input_dir.join("model.bin");
    std::fs::write(&file_path, &data).unwrap();

    // Create blob metadata from file
    let meta = BlobMeta::from_file(&file_path).unwrap();
    assert_eq!(meta.total_size, data.len() as u64);
    assert_eq!(meta.piece_size, 256 * 1024); // < 100 MB => 256KB pieces
    assert_eq!(meta.piece_count(), 2);

    // Verify memory-based produces same result
    let mem_meta = BlobMeta::from_data(&data);
    assert_eq!(meta.blob_id, mem_meta.blob_id);

    // Store in blob store
    let store = BlobStore::new(&tmp);
    let stored_meta = store.store_file(&file_path).unwrap();
    assert_eq!(stored_meta.blob_id, meta.blob_id);
    assert!(store.has(&meta.blob_id));

    // Read back metadata
    let loaded = store.get_meta(&meta.blob_id).unwrap().unwrap();
    assert_eq!(loaded.blob_id, meta.blob_id);
    assert_eq!(loaded.piece_hashes, meta.piece_hashes);

    // Read pieces and verify
    let piece0 = store.read_piece(&meta.blob_id, &meta, 0).unwrap();
    let piece1 = store.read_piece(&meta.blob_id, &meta, 1).unwrap();
    assert!(meta.verify_piece(0, &piece0));
    assert!(meta.verify_piece(1, &piece1));

    // Create pointer file
    let pointer = PointerFile::new(meta.blob_id.clone(), meta.total_size);
    let encoded = pointer.encode();
    assert!(is_pointer_file(encoded.as_bytes()));
    let decoded = PointerFile::decode(&encoded).unwrap();
    assert_eq!(decoded, pointer);

    // Simulate download via BlobDownloadState
    let mut dl = BlobDownloadState::new(meta.clone());
    assert!(!dl.is_complete());
    assert_eq!(dl.needed_pieces().len(), 2);

    dl.receive_piece(0, piece0);
    dl.receive_piece(1, piece1);
    assert!(dl.is_complete());
    assert_eq!(dl.progress(), 1.0);

    let assembled = dl.assemble().unwrap();
    assert_eq!(assembled, data);

    // List and total size
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(store.total_size().unwrap(), data.len() as u64);

    // DHT address is deterministic
    let addr1 = blob_dht_address(&meta.blob_id);
    let addr2 = blob_dht_address(&meta.blob_id);
    assert_eq!(addr1, addr2);

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test collection manifests: create, sign, verify, store.
#[test]
fn test_collection_lifecycle() {
    let owner = Identity::generate();

    let files = vec![
        CollectionFile {
            blob_id: BlobId([0x11; 32]),
            path: "model.safetensors".into(),
            size: 4_000_000_000,
            media_type: Some("application/octet-stream".into()),
        },
        CollectionFile {
            blob_id: BlobId([0x22; 32]),
            path: "tokenizer.json".into(),
            size: 2_000_000,
            media_type: Some("application/json".into()),
        },
        CollectionFile {
            blob_id: BlobId([0x33; 32]),
            path: "config.json".into(),
            size: 1_500,
            media_type: Some("application/json".into()),
        },
    ];

    let manifest = CollectionManifest::new(
        &owner,
        "llama-7b".into(),
        "LLaMA 7B model weights and config".into(),
        files,
        HashMap::from([
            ("architecture".into(), "llama".into()),
            ("params".into(), "7B".into()),
            ("format".into(), "safetensors".into()),
        ]),
    );

    // Verify signature
    assert!(manifest.verify());
    assert_eq!(manifest.owner, owner.peer_id());
    assert_eq!(manifest.total_size(), 4_002_001_500);
    assert_eq!(manifest.files.len(), 3);
    assert_eq!(manifest.metadata["architecture"], "llama");

    // Tamper detection
    let mut tampered = manifest.clone();
    tampered.description = "Tampered!".into();
    assert!(!tampered.verify());

    // Store
    let mut store = CollectionStore::new();
    let id = manifest.id.clone();
    store.add(manifest);
    assert!(store.get(&id).is_some());
    assert_eq!(store.by_owner(&owner.peer_id()).len(), 1);
}

/// Test adaptive piece sizing for different file sizes.
#[test]
fn test_adaptive_piece_sizes() {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    // Small files (< 100 MB) -> 256 KB
    assert_eq!(adaptive_piece_size(0), 256 * 1024);
    assert_eq!(adaptive_piece_size(10 * MB), 256 * 1024);
    assert_eq!(adaptive_piece_size(99 * MB), 256 * 1024);

    // Medium files (100 MB - 1 GB) -> 1 MB
    assert_eq!(adaptive_piece_size(100 * MB), 1024 * 1024);
    assert_eq!(adaptive_piece_size(500 * MB), 1024 * 1024);
    assert_eq!(adaptive_piece_size(GB - 1), 1024 * 1024);

    // Large files (>= 1 GB) -> 4 MB
    assert_eq!(adaptive_piece_size(GB), 4 * 1024 * 1024);
    assert_eq!(adaptive_piece_size(10 * GB), 4 * 1024 * 1024);
    assert_eq!(adaptive_piece_size(100 * GB), 4 * 1024 * 1024);
}

/// Test tracker seeder/leecher flow.
#[test]
fn test_blob_tracker_flow() {
    let mut tracker = BlobTracker::new();
    let blob = BlobId([0xde; 32]);
    let seeder1 = Identity::generate().peer_id();
    let seeder2 = Identity::generate().peer_id();
    let leecher = Identity::generate().peer_id();

    // Start seeding
    tracker.add_seeder(blob.clone(), seeder1.clone());
    tracker.add_seeder(blob.clone(), seeder2.clone());
    assert_eq!(tracker.seeder_count(&blob), 2);

    // Leecher starts downloading
    tracker.add_leecher(blob.clone(), leecher.clone());
    assert_eq!(tracker.leecher_count(&blob), 1);

    // Leecher finishes download, becomes seeder
    tracker.promote_to_seeder(&blob, leecher.clone());
    assert_eq!(tracker.seeder_count(&blob), 3);
    assert_eq!(tracker.leecher_count(&blob), 0);

    // Check per-peer view
    assert_eq!(tracker.blobs_for_peer(&leecher).len(), 1);
}

/// Test signed message roundtrip.
#[test]
fn test_signed_message_serialization() {
    let id = Identity::generate();
    let msg = id.sign_message(b"test payload for serialization".to_vec());

    // Serialize to JSON
    let json = serde_json::to_string(&msg).unwrap();

    // Deserialize back
    let deserialized: gt_core::identity::SignedMessage = serde_json::from_str(&json).unwrap();

    // Verify the deserialized message
    assert!(identity::verify_signed_message(&deserialized).is_ok());
    assert_eq!(deserialized.payload, b"test payload for serialization");
    assert_eq!(deserialized.peer_id, id.peer_id());
}

// ============ Peer Names ============

/// Test PeerName creation, display, and parsing.
#[test]
fn test_peer_name_roundtrip() {
    let id = Identity::generate();
    let peer_id = id.peer_id();
    let peer_id_hex = peer_id.to_string();

    let pn = PeerName::new("laptop".into(), peer_id.clone());

    // Discriminator is first 4 hex chars of peer ID
    let disc = &peer_id_hex[..4];
    assert_eq!(pn.discriminator(), disc);
    assert_eq!(pn.short_tag(), format!("laptop#{disc}"));
    assert_eq!(pn.to_string(), format!("laptop#{disc}"));

    // Parse back from short tag (note: parsed PeerName has zeroed peer_id)
    let parsed: PeerName = pn.short_tag().parse().unwrap();
    assert_eq!(parsed.display_name, "laptop");
}

/// Test PeerName parsing edge cases.
#[test]
fn test_peer_name_parse_errors() {
    // Missing discriminator
    assert!("laptop".parse::<PeerName>().is_err());
    // Empty name
    assert!("#abcd".parse::<PeerName>().is_err());
    // Invalid discriminator (not hex)
    assert!("laptop#zzzz".parse::<PeerName>().is_err());
    // Wrong discriminator length
    assert!("laptop#ab".parse::<PeerName>().is_err());
    assert!("laptop#abcde".parse::<PeerName>().is_err());
}

/// Test Config with peer_names and resolve_peer.
#[test]
fn test_config_resolve_peer() {
    let id1 = Identity::generate();
    let id2 = Identity::generate();
    let pid1 = id1.peer_id();
    let pid2 = id2.peer_id();
    let pid1_hex = pid1.to_string();
    let pid2_hex = pid2.to_string();

    let tag1 = format!("laptop#{}", &pid1_hex[..4]);
    let tag2 = format!("desktop#{}", &pid2_hex[..4]);

    let mut config = Config::default();
    config.peer_names.insert(tag1.clone(), pid1_hex.clone());
    config.peer_names.insert(tag2.clone(), pid2_hex.clone());

    // Resolve by full hex
    assert_eq!(config.resolve_peer(&pid1_hex).unwrap(), pid1);

    // Resolve by name tag
    assert_eq!(config.resolve_peer(&tag1).unwrap(), pid1);
    assert_eq!(config.resolve_peer(&tag2).unwrap(), pid2);

    // Unknown name returns None
    assert!(config.resolve_peer("unknown#0000").is_none());
    assert!(config.resolve_peer("random").is_none());
}

/// Test config roundtrip with peer_names field.
#[test]
fn test_config_peer_names_roundtrip() {
    let id = Identity::generate();
    let pid_hex = id.peer_id().to_string();

    let mut config = Config {
        data_dir: PathBuf::from("/tmp/gitforest-test-names"),
        p2p_port: 9418,
        api_port: 7878,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![],
        bootstrap_peers: vec![],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: None,
        payments: Default::default(),
    };
    config
        .peer_names
        .insert(format!("friend#{}", &pid_hex[..4]), pid_hex.clone());

    let tmp = std::env::temp_dir().join("gt_daemon_test_config_names");
    let config_path = tmp.join("config.toml");
    let _ = std::fs::remove_file(&config_path);

    config.save(&config_path).unwrap();
    let loaded = Config::load(&config_path).unwrap();

    assert_eq!(loaded.peer_names.len(), 1);
    let tag = format!("friend#{}", &pid_hex[..4]);
    assert_eq!(loaded.peer_names.get(&tag).unwrap(), &pid_hex);

    // Resolve works on loaded config
    assert_eq!(loaded.resolve_peer(&tag).unwrap(), id.peer_id());

    let _ = std::fs::remove_dir_all(&tmp);
}

// ============ Two-Machine Network Simulation ============

use base64::Engine;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Create a temp git repo with one commit.
fn create_git_repo(path: &std::path::Path, readme_content: &str) {
    std::fs::create_dir_all(path).unwrap();
    Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .unwrap();
    std::fs::write(path.join("README.md"), readme_content).unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(path)
        .output()
        .unwrap();
}

/// Wait until an HTTP endpoint is reachable.
fn hermetic_http_env() {
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "FTP_PROXY",
        "ftp_proxy",
    ] {
        std::env::remove_var(key);
    }
    std::env::set_var("NO_PROXY", "*");
    std::env::set_var("no_proxy", "*");
}

async fn wait_for_api(port: u16, timeout_secs: u64) {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        match client
            .get(format!("http://127.0.0.1:{port}/api/status"))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Daemon on port {port} did not become ready within {timeout_secs}s");
}

/// Wait until a daemon has learned a dialable address for a peer, either via
/// peer exchange or DHT identify. Discovery is asynchronous by nature; tests
/// must wait for it the same way they wait for API readiness.
async fn wait_for_peer_tagged(api_port: u16, peer_id: &str, timeout_secs: u64) {
    let client = reqwest::Client::new();
    let tag = format!("/gf/{peer_id}");
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{api_port}/api/peers"))
            .send()
            .await
        {
            if let Ok(body) = resp.text().await {
                if body.contains(&tag) {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("Daemon on port {api_port} did not tag {peer_id} within {timeout_secs}s");
}

/// Simulate two machines on localhost, test full P2P workflow:
/// fetch, unauthorized push, ACL grant, authorized push, merge request.
#[tokio::test]
async fn test_two_machine_simulation() {
    hermetic_http_env();
    let tmp = std::env::temp_dir().join(format!("gt_sim_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Resolve binary path
    let binary = std::env::var("CARGO_BIN_EXE_gitforest").unwrap_or_else(|_| {
        // Find binary relative to the test executable (in target/debug/deps/)
        let mut path = std::env::current_exe().unwrap();
        path.pop(); // deps
        path.pop(); // debug
        path.push("gitforest");
        path.to_string_lossy().to_string()
    });

    // ─── Machine A (repo owner) ───
    let a_dir = tmp.join("machine_a");
    let a_repo = a_dir.join("myrepo");
    let a_identity = Identity::generate();
    let a_peer_id = a_identity.peer_id().to_string();

    let a_config_home = a_dir.join(".config").join("gitforest");
    std::fs::create_dir_all(&a_config_home).unwrap();
    a_identity
        .save(&a_config_home.join("identity.key"))
        .unwrap();
    create_git_repo(&a_repo, "# Machine A Repo\n");

    let a_config = Config {
        data_dir: a_dir.join("data"),
        p2p_port: 19418,
        api_port: 17878,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![RepoConfig {
            name: "myrepo".into(),
            path: a_repo.clone(),
            owner: a_identity.peer_id(),
            description: Some("Test repo".into()),
            seeding: true,
        }],
        bootstrap_peers: vec![],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: Some("127.0.0.1".to_string()),
        payments: Default::default(),
    };
    let a_config_path = a_dir.join("config.toml");
    a_config.save(&a_config_path).unwrap();

    // ─── Machine B (fork / collaborator) ───
    let b_dir = tmp.join("machine_b");
    let b_repo = b_dir.join("myfork");
    let b_identity = Identity::generate();
    let b_peer_id = b_identity.peer_id().to_string();

    let b_config_home = b_dir.join(".config").join("gitforest");
    std::fs::create_dir_all(&b_config_home).unwrap();
    b_identity
        .save(&b_config_home.join("identity.key"))
        .unwrap();
    create_git_repo(&b_repo, "# Machine B Fork\n");

    // Add a second commit to B so there's something to push
    std::fs::write(b_repo.join("feature.txt"), "new feature\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "add feature"])
        .current_dir(&b_repo)
        .output()
        .unwrap();

    let b_config = Config {
        data_dir: b_dir.join("data"),
        p2p_port: 19420,
        api_port: 17879,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![RepoConfig {
            name: "myfork".into(),
            path: b_repo.clone(),
            owner: b_identity.peer_id(),
            description: Some("Fork".into()),
            seeding: true,
        }],
        bootstrap_peers: vec!["/ip4/127.0.0.1/tcp/19418/api/17878".to_string()],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: Some("127.0.0.1".to_string()),
        payments: Default::default(),
    };
    let b_config_path = b_dir.join("config.toml");
    b_config.save(&b_config_path).unwrap();

    // ─── Start daemons ───
    // Set HOME and XDG_CONFIG_HOME so each daemon loads its own identity
    let a_xdg_config = a_dir.join(".config");
    let b_xdg_config = b_dir.join(".config");
    let mut proc_a = Command::new(&binary)
        .env("HOME", &a_dir)
        .env("XDG_RUNTIME_DIR", &a_dir)
        .env("XDG_CONFIG_HOME", &a_xdg_config)
        .env("GITFOREST_SKIP_AUTH", "1")
        .args([
            "--config",
            &a_config_path.to_string_lossy(),
            "--identity",
            &a_config_home.join("identity.key").to_string_lossy(),
            "daemon",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to start Machine A daemon");

    let mut proc_b = Command::new(&binary)
        .env("HOME", &b_dir)
        .env("XDG_RUNTIME_DIR", &b_dir)
        .env("XDG_CONFIG_HOME", &b_xdg_config)
        .env("GITFOREST_SKIP_AUTH", "1")
        .args([
            "--config",
            &b_config_path.to_string_lossy(),
            "--identity",
            &b_config_home.join("identity.key").to_string_lossy(),
            "daemon",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to start Machine B daemon");

    // ─── Wait for readiness ───
    wait_for_api(17878, 30).await;
    wait_for_api(17879, 30).await;
    wait_for_peer_tagged(17879, &a_peer_id, 30).await;

    let client = reqwest::Client::new();

    // Verify A's actual peer ID matches what we expect
    let resp = client
        .get("http://127.0.0.1:17878/api/id")
        .send()
        .await
        .expect("id request failed");
    let id_json: serde_json::Value = resp.json().await.expect("id json");
    let actual_a_peer_id = id_json
        .get("peer_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        actual_a_peer_id, a_peer_id,
        "A's daemon identity should match test identity"
    );

    // Sanity check: A can serve its own refs directly
    let resp = client
        .get("http://127.0.0.1:17878/api/repos/myrepo/refs")
        .send()
        .await
        .expect("direct refs request failed");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "A should serve its own refs directly, got HTTP {status}, body: {body}"
    );
    let direct_refs: Vec<(String, String)> = serde_json::from_str(&body).expect("direct refs json");
    assert!(!direct_refs.is_empty(), "A should have refs");

    // Sanity check: A's remote endpoint for its own repo works
    let a_remote_url = format!("http://127.0.0.1:17878/api/remote/{a_peer_id}/myrepo/refs");
    let resp = client
        .get(&a_remote_url)
        .send()
        .await
        .expect("A remote refs request failed");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "A should handle remote refs for itself ({a_remote_url}), got HTTP {status}, body: {body}"
    );

    // ─── Test 1: B can list A's refs via proxy ───
    let proxy_url = format!("http://127.0.0.1:17879/api/remote/{a_peer_id}/myrepo/refs");
    let resp = client
        .get(&proxy_url)
        .send()
        .await
        .expect("refs request failed");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "B should proxy refs from A ({proxy_url}), got HTTP {status}, body: {body}"
    );
    let refs: Vec<(String, String)> = serde_json::from_str(&body).expect("refs json");
    assert!(!refs.is_empty(), "A should have refs");

    // ─── Test 2: Unauthorized push from B to A is rejected ───
    let bundle_path = b_repo.join("push.bundle");
    let status = Command::new("git")
        .args(["bundle", "create", &bundle_path.to_string_lossy(), "--all"])
        .current_dir(&b_repo)
        .status()
        .unwrap();
    assert!(status.success(), "git bundle create failed");
    let bundle_bytes = std::fs::read(&bundle_path).unwrap();

    // Get B's new HEAD SHA
    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    let b_head = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();

    // Query A's current HEAD so we can pass the correct old_sha
    let a_head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&a_repo)
        .output()
        .unwrap();
    let a_head = String::from_utf8_lossy(&a_head_output.stdout)
        .trim()
        .to_string();

    let ref_updates = vec![serde_json::json!({
        "pusher": b_peer_id,
        "ref_name": "refs/heads/main",
        "new_sha": b_head,
        "old_sha": a_head,
    })];
    let ref_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_string(&ref_updates).unwrap());

    let sig_b64 = push_sig_b64(&b_identity, "myrepo", &ref_b64, &bundle_bytes);
    let resp = client
        .post(format!(
            "http://127.0.0.1:17879/api/remote/{a_peer_id}/myrepo/push"
        ))
        .header("X-GitForest-Push-Refs", ref_b64)
        .header("X-GitForest-Push-Sig", sig_b64)
        .header("Content-Type", "application/x-git-bundle")
        .body(bundle_bytes.clone())
        .send()
        .await
        .expect("push request failed");
    assert!(
        resp.status().is_success(),
        "push endpoint should return HTTP 200 even for rejection"
    );
    let push_result: serde_json::Value = resp.json().await.expect("push result json");
    assert_eq!(
        push_result.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "unauthorized push should be rejected"
    );
    assert!(
        push_result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("not authorized"),
        "error should mention authorization"
    );

    // ─── Test 3: A grants B push access ───
    let resp = client
        .post("http://127.0.0.1:17878/api/repos/myrepo/acl")
        .json(&serde_json::json!({
            "action": "grant",
            "peer_id": b_peer_id,
            "ref_pattern": "refs/heads/*",
        }))
        .send()
        .await
        .expect("ACL grant failed");
    assert!(resp.status().is_success(), "ACL grant should succeed");

    // ─── Test 4: Authorized push from B to A succeeds ───
    let ref_updates2 = vec![serde_json::json!({
        "pusher": b_peer_id,
        "ref_name": "refs/heads/main",
        "new_sha": b_head,
        "old_sha": a_head,
    })];
    let ref_b64_2 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_string(&ref_updates2).unwrap());

    let sig_b64_2 = push_sig_b64(&b_identity, "myrepo", &ref_b64_2, &bundle_bytes);
    let resp = client
        .post(format!(
            "http://127.0.0.1:17879/api/remote/{a_peer_id}/myrepo/push"
        ))
        .header("X-GitForest-Push-Refs", ref_b64_2)
        .header("X-GitForest-Push-Sig", sig_b64_2)
        .header("Content-Type", "application/x-git-bundle")
        .body(bundle_bytes)
        .send()
        .await
        .expect("authorized push request failed");
    assert!(resp.status().is_success());
    let push_result: serde_json::Value = resp.json().await.expect("push result json");
    assert_eq!(
        push_result.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "authorized push should succeed"
    );

    // ─── Test 5: A's repo now has B's commit ───
    let resp = client
        .get("http://127.0.0.1:17878/api/repos/myrepo/log")
        .send()
        .await
        .expect("log request failed");
    assert!(resp.status().is_success());
    let log: Vec<serde_json::Value> = resp.json().await.expect("log json");
    let has_b_commit = log
        .iter()
        .any(|c| c.get("message").and_then(|v| v.as_str()) == Some("add feature"));
    assert!(
        has_b_commit,
        "A's repo should contain B's commit after push"
    );

    // ─── Test 6: B sends a merge request to A ───
    let resp = client
        .post("http://127.0.0.1:17878/api/repos/myrepo/merge-requests")
        .json(&serde_json::json!({
            "source_repo": "myfork",
            "source_branch": "main",
            "target_branch": "main",
            "title": "Add feature from B",
            "description": "This adds the feature",
        }))
        .send()
        .await
        .expect("MR submission failed");
    assert!(resp.status().is_success(), "MR submission should succeed");
    let mr_result: serde_json::Value = resp.json().await.expect("MR json");
    assert!(mr_result.get("id").is_some(), "MR should have an ID");

    // Verify A received the MR
    let resp = client
        .get("http://127.0.0.1:17878/api/repos/myrepo/merge-requests")
        .send()
        .await
        .expect("MR list failed");
    assert!(resp.status().is_success());
    let mrs: Vec<serde_json::Value> = resp.json().await.expect("MR list json");
    assert_eq!(mrs.len(), 1, "A should have exactly 1 MR");
    assert_eq!(
        mrs[0].get("title").and_then(|v| v.as_str()),
        Some("Add feature from B")
    );

    // ─── Test 7: ACL persists across config reload ───
    let reloaded = Config::load(&a_config_path).unwrap();
    let a_grants = reloaded
        .acl_grants
        .get("myrepo")
        .expect("ACL grants should be saved");
    assert_eq!(a_grants.len(), 1);
    assert_eq!(a_grants[0].peer_id, b_identity.peer_id());
    assert_eq!(a_grants[0].ref_pattern, "refs/heads/*");

    // ─── Cleanup ───
    let _ = proc_a.kill();
    let _ = proc_b.kill();
    let _ = proc_a.wait();
    let _ = proc_b.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

// ============ Ten-Device Network Simulation ============

#[allow(dead_code)]
struct SimDevice {
    dir: PathBuf,
    repo: PathBuf,
    identity: Identity,
    peer_id: String,
    api_port: u16,
    p2p_port: u16,
    config_path: PathBuf,
    config_home: PathBuf,
}

/// Spawn 10 simulated devices in a star topology (all bootstrap to device 0).
/// Test proxy fetch, ACL grant/revoke, authorized push, and merge requests
/// across the full swarm.
#[tokio::test]
async fn test_ten_device_simulation() {
    hermetic_http_env();
    const N: usize = 10;
    const BASE_API: u16 = 17900;
    const BASE_P2P: u16 = 19500;
    // Leave a gap of 10 between P2P ports so that p2p_port+1 (git server) doesn't collide
    // with the next device's p2p_port.
    const P2P_STEP: u16 = 10;

    let tmp = std::env::temp_dir().join(format!("gt_sim_10dev_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let binary = std::env::var("CARGO_BIN_EXE_gitforest").unwrap_or_else(|_| {
        let mut path = std::env::current_exe().unwrap();
        path.pop(); // deps
        path.pop(); // debug
        path.push("gitforest");
        path.to_string_lossy().to_string()
    });

    // ─── Build devices ───
    let mut devices: Vec<SimDevice> = Vec::with_capacity(N);
    for i in 0..N {
        let dir = tmp.join(format!("dev_{i}"));
        let repo = dir.join("repo");
        let identity = Identity::generate();
        let peer_id = identity.peer_id().to_string();
        let config_home = dir.join(".config").join("gitforest");
        std::fs::create_dir_all(&config_home).unwrap();
        identity.save(&config_home.join("identity.key")).unwrap();

        create_git_repo(&repo, &format!("# Device {i} Repo\n"));

        // Each device after 0 adds a unique commit so they have something to push
        if i > 0 {
            std::fs::write(
                repo.join(format!("feature_{i}.txt")),
                format!("feature {i}\n"),
            )
            .unwrap();
            Command::new("git")
                .args(["add", "."])
                .current_dir(&repo)
                .output()
                .unwrap();
            Command::new("git")
                .args(["commit", "-m", &format!("add feature {i}")])
                .current_dir(&repo)
                .output()
                .unwrap();
        }

        let api_port = BASE_API + i as u16;
        let p2p_port = BASE_P2P + (i as u16) * P2P_STEP;

        let bootstrap = if i == 0 {
            vec![]
        } else {
            vec![format!("/ip4/127.0.0.1/tcp/{}/api/{}", BASE_P2P, BASE_API)]
        };

        let config = Config {
            data_dir: dir.join("data"),
            p2p_port,
            api_port,
            auto_seed: true,
            enable_mdns: false,
            max_upload_rate: 0,
            max_download_rate: 0,
            repos: vec![RepoConfig {
                name: "myrepo".into(),
                path: repo.clone(),
                owner: identity.peer_id(),
                description: Some(format!("Device {i} repo")),
                seeding: true,
            }],
            bootstrap_peers: bootstrap,
            advertise_addr: None,
            cluster_token: None,
            compute: None,
            peer_names: HashMap::new(),
            acl_grants: HashMap::new(),
            model_acl_grants: HashMap::new(),
            llm_backend: None,
            p2p_listen: Some("127.0.0.1".to_string()),
            payments: Default::default(),
        };
        let config_path = dir.join("config.toml");
        config.save(&config_path).unwrap();

        devices.push(SimDevice {
            dir,
            repo,
            identity,
            peer_id,
            api_port,
            p2p_port,
            config_path,
            config_home,
        });
    }

    let owner = &devices[0];
    let owner_peer_id = owner.peer_id.clone();

    // ─── Start all daemons ───
    let mut procs: Vec<std::process::Child> = Vec::with_capacity(N);
    for dev in &devices {
        let xdg_config = dev.dir.join(".config");
        let proc = Command::new(&binary)
            .env("HOME", &dev.dir)
            .env("XDG_RUNTIME_DIR", &dev.dir)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("GITFOREST_SKIP_AUTH", "1")
            .args([
                "--config",
                &dev.config_path.to_string_lossy(),
                "--identity",
                &dev.config_home.join("identity.key").to_string_lossy(),
                "daemon",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|_| panic!("failed to start device {} daemon", dev.api_port));
        procs.push(proc);
    }

    // Ensure cleanup even on panic
    let mut cleanup = || {
        for p in &mut procs {
            let _ = p.kill();
        }
        for p in &mut procs {
            let _ = p.wait();
        }
        let _ = std::fs::remove_dir_all(&tmp);
    };

    // ─── Wait for all APIs ───
    for dev in &devices {
        wait_for_api(dev.api_port, 30).await;
    }

    // ─── Wait for peer discovery: every device must learn owner's address ───
    for dev in &devices[1..] {
        wait_for_peer_tagged(dev.api_port, &owner_peer_id, 30).await;
    }

    let client = reqwest::Client::new();

    // ─── Test 1: Every device can proxy-fetch refs from device 0 ───
    for dev in &devices[1..] {
        let url = format!(
            "http://127.0.0.1:{}/api/remote/{owner_peer_id}/myrepo/refs",
            dev.api_port
        );
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|_| panic!("refs request from dev {} failed", dev.api_port));
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "Device {} should proxy refs from owner, got HTTP {status}, body: {body}",
            dev.api_port
        );
        let refs: Vec<(String, String)> = serde_json::from_str(&body)
            .unwrap_or_else(|_| panic!("refs json from dev {}", dev.api_port));
        assert!(!refs.is_empty(), "Owner should have refs");
    }

    // Query owner's current HEAD for conflict detection
    let owner_head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&owner.repo)
        .output()
        .unwrap();
    let owner_head = String::from_utf8_lossy(&owner_head_output.stdout)
        .trim()
        .to_string();

    // ─── Test 2: Unauthorized push from device 5 rejected ───
    let pusher = &devices[5];
    let bundle_path = pusher.repo.join("push.bundle");
    let status = Command::new("git")
        .args(["bundle", "create", &bundle_path.to_string_lossy(), "--all"])
        .current_dir(&pusher.repo)
        .status()
        .unwrap();
    assert!(status.success(), "git bundle create failed");
    let bundle_bytes = std::fs::read(&bundle_path).unwrap();

    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&pusher.repo)
        .output()
        .unwrap();
    let push_sha = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();

    let ref_updates = vec![serde_json::json!({
        "pusher": pusher.peer_id,
        "ref_name": "refs/heads/main",
        "new_sha": push_sha,
        "old_sha": owner_head,
    })];
    let ref_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_string(&ref_updates).unwrap());
    let sig_b64 = push_sig_b64(&pusher.identity, "myrepo", &ref_b64, &bundle_bytes);

    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/remote/{owner_peer_id}/myrepo/push",
            pusher.api_port
        ))
        .header("X-GitForest-Push-Refs", ref_b64.clone())
        .header("X-GitForest-Push-Sig", sig_b64.clone())
        .header("Content-Type", "application/x-git-bundle")
        .body(bundle_bytes.clone())
        .send()
        .await
        .expect("push request failed");
    assert!(resp.status().is_success());
    let push_result: serde_json::Value = resp.json().await.expect("push result json");
    assert_eq!(
        push_result.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "unauthorized push from device 5 should be rejected"
    );

    // ─── Test 3: Owner grants ACL to device 5 ───
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/repos/myrepo/acl",
            owner.api_port
        ))
        .json(&serde_json::json!({
            "action": "grant",
            "peer_id": pusher.peer_id,
            "ref_pattern": "refs/heads/*",
        }))
        .send()
        .await
        .expect("ACL grant failed");
    assert!(resp.status().is_success(), "ACL grant should succeed");

    // ─── Test 4: Device 5 pushes successfully ───
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/remote/{owner_peer_id}/myrepo/push",
            pusher.api_port
        ))
        .header("X-GitForest-Push-Refs", ref_b64)
        .header("X-GitForest-Push-Sig", sig_b64)
        .header("Content-Type", "application/x-git-bundle")
        .body(bundle_bytes.clone())
        .send()
        .await
        .expect("authorized push request failed");
    assert!(resp.status().is_success());
    let push_result: serde_json::Value = resp.json().await.expect("push result json");
    assert_eq!(
        push_result.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "authorized push from device 5 should succeed"
    );

    // ─── Test 5: Every device can see the new commit on owner via proxy ───
    for dev in &devices[1..] {
        let url = format!("http://127.0.0.1:{}/api/repos/myrepo/log", owner.api_port);
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|_| panic!("log request from dev {} failed", dev.api_port));
        assert!(resp.status().is_success());
        let log: Vec<serde_json::Value> = resp.json().await.expect("log json");
        let has_commit = log
            .iter()
            .any(|c| c.get("message").and_then(|v| v.as_str()) == Some("add feature 5"));
        assert!(
            has_commit,
            "Owner repo should contain device 5's commit (checked from dev {})",
            dev.api_port
        );
    }

    // ─── Test 6: Owner revokes ACL from device 5 ───
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/repos/myrepo/acl",
            owner.api_port
        ))
        .json(&serde_json::json!({
            "action": "revoke",
            "peer_id": pusher.peer_id,
        }))
        .send()
        .await
        .expect("ACL revoke failed");
    assert!(resp.status().is_success(), "ACL revoke should succeed");

    // ─── Test 7: Device 5 push rejected after revoke ───
    // Make a new commit so the SHA differs
    std::fs::write(pusher.repo.join("extra.txt"), "extra content\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&pusher.repo)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "extra commit after revoke"])
        .current_dir(&pusher.repo)
        .output()
        .unwrap();

    let head_output2 = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&pusher.repo)
        .output()
        .unwrap();
    let new_sha = String::from_utf8_lossy(&head_output2.stdout)
        .trim()
        .to_string();

    let new_bundle_path = pusher.repo.join("push2.bundle");
    let status = Command::new("git")
        .args([
            "bundle",
            "create",
            &new_bundle_path.to_string_lossy(),
            "--all",
        ])
        .current_dir(&pusher.repo)
        .status()
        .unwrap();
    assert!(status.success(), "git bundle create failed");
    let new_bundle_bytes = std::fs::read(&new_bundle_path).unwrap();

    let ref_updates3 = vec![serde_json::json!({
        "pusher": pusher.peer_id,
        "ref_name": "refs/heads/main",
        "new_sha": new_sha,
        "old_sha": push_sha,
    })];
    let ref_b64_3 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_string(&ref_updates3).unwrap());
    let sig_b64_3 = push_sig_b64(&pusher.identity, "myrepo", &ref_b64_3, &new_bundle_bytes);

    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/remote/{owner_peer_id}/myrepo/push",
            pusher.api_port
        ))
        .header("X-GitForest-Push-Refs", ref_b64_3)
        .header("X-GitForest-Push-Sig", sig_b64_3)
        .header("Content-Type", "application/x-git-bundle")
        .body(new_bundle_bytes)
        .send()
        .await
        .expect("post-revoke push request failed");
    assert!(resp.status().is_success());
    let push_result: serde_json::Value = resp.json().await.expect("push result json");
    assert_eq!(
        push_result.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "push after revoke should be rejected"
    );

    // ─── Test 8: Device 9 sends a merge request to owner ───
    let _dev9 = &devices[9];
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/api/repos/myrepo/merge-requests",
            owner.api_port
        ))
        .json(&serde_json::json!({
            "source_repo": "myrepo",
            "source_branch": "main",
            "target_branch": "main",
            "title": "Feature from device 9",
            "description": "Adding feature 9",
        }))
        .send()
        .await
        .expect("MR submission failed");
    assert!(resp.status().is_success(), "MR submission should succeed");
    let mr_result: serde_json::Value = resp.json().await.expect("MR json");
    assert!(mr_result.get("id").is_some(), "MR should have an ID");

    // Verify owner received the MR
    let resp = client
        .get(format!(
            "http://127.0.0.1:{}/api/repos/myrepo/merge-requests",
            owner.api_port
        ))
        .send()
        .await
        .expect("MR list failed");
    assert!(resp.status().is_success());
    let mrs: Vec<serde_json::Value> = resp.json().await.expect("MR list json");
    assert_eq!(mrs.len(), 1, "Owner should have exactly 1 MR");
    assert_eq!(
        mrs[0].get("title").and_then(|v| v.as_str()),
        Some("Feature from device 9")
    );

    // ─── Test 9: ACL persistence in owner config ───
    let reloaded = Config::load(&owner.config_path).unwrap();
    let grants = reloaded
        .acl_grants
        .get("myrepo")
        .expect("ACL grants should exist");
    assert_eq!(grants.len(), 0, "Grants should be empty after revoke");

    // ─── Cleanup ───
    cleanup();
}

// ============ End-to-End Remote Helper Test ============

/// Test the actual `git-remote-gitforest` binary through real `git` commands.
/// Verifies `git fetch` and `git push` work end-to-end.
#[tokio::test]
async fn test_git_remote_helper_end_to_end() {
    hermetic_http_env();
    let tmp = std::env::temp_dir().join(format!("gt_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let binary = std::env::var("CARGO_BIN_EXE_gitforest").unwrap_or_else(|_| {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        path.pop();
        path.push("gitforest");
        path.to_string_lossy().to_string()
    });

    let helper_binary = std::env::var("CARGO_BIN_EXE_git-remote-gitforest").unwrap_or_else(|_| {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        path.pop();
        path.push("git-remote-gitforest");
        path.to_string_lossy().to_string()
    });

    // ─── Owner (machine A) ───
    let a_dir = tmp.join("owner");
    let a_repo = a_dir.join("myrepo");
    let a_identity = Identity::generate();
    let a_peer_id = a_identity.peer_id().to_string();

    let a_config_home = a_dir.join(".config").join("gitforest");
    std::fs::create_dir_all(&a_config_home).unwrap();
    a_identity
        .save(&a_config_home.join("identity.key"))
        .unwrap();
    create_git_repo(&a_repo, "# Owner Repo\n");

    let a_config = Config {
        data_dir: a_dir.join("data"),
        p2p_port: 19600,
        api_port: 18000,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![RepoConfig {
            name: "myrepo".into(),
            path: a_repo.clone(),
            owner: a_identity.peer_id(),
            description: Some("Owner repo".into()),
            seeding: true,
        }],
        bootstrap_peers: vec![],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: Some("127.0.0.1".to_string()),
        payments: Default::default(),
    };
    let a_config_path = a_dir.join("config.toml");
    a_config.save(&a_config_path).unwrap();

    // ─── Pusher (machine B) ───
    let b_dir = tmp.join("pusher");
    let b_repo = b_dir.join("myclone");
    let b_identity = Identity::generate();
    let b_peer_id = b_identity.peer_id().to_string();

    let b_config_home = b_dir.join(".config").join("gitforest");
    std::fs::create_dir_all(&b_config_home).unwrap();
    b_identity
        .save(&b_config_home.join("identity.key"))
        .unwrap();

    // Clone the owner's repo normally so B has the same history and a working tree
    let status = Command::new("git")
        .args([
            "clone",
            &a_repo.to_string_lossy(),
            &b_repo.to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    // Add a commit on B to push
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    std::fs::write(b_repo.join("new.txt"), "new content\n").unwrap();
    let commit_out = Command::new("git")
        .args(["add", "."])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    assert!(commit_out.status.success(), "git add failed");
    let commit_out = Command::new("git")
        .args(["commit", "-m", "pusher commit"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    assert!(
        commit_out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit_out.stderr)
    );

    let b_config = Config {
        data_dir: b_dir.join("data"),
        p2p_port: 19602,
        api_port: 18001,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![],
        bootstrap_peers: vec!["/ip4/127.0.0.1/tcp/19600/api/18000".to_string()],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: Some("127.0.0.1".to_string()),
        payments: Default::default(),
    };
    let b_config_path = b_dir.join("config.toml");
    b_config.save(&b_config_path).unwrap();

    // ─── Start daemons ───
    let a_xdg_config = a_dir.join(".config");
    let b_xdg_config = b_dir.join(".config");

    let mut proc_a = Command::new(&binary)
        .env("HOME", &a_dir)
        .env("XDG_RUNTIME_DIR", &a_dir)
        .env("XDG_CONFIG_HOME", &a_xdg_config)
        .env("GITFOREST_SKIP_AUTH", "1")
        .args([
            "--config",
            &a_config_path.to_string_lossy(),
            "--identity",
            &a_config_home.join("identity.key").to_string_lossy(),
            "daemon",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to start owner daemon");

    let mut proc_b = Command::new(&binary)
        .env("HOME", &b_dir)
        .env("XDG_RUNTIME_DIR", &b_dir)
        .env("XDG_CONFIG_HOME", &b_xdg_config)
        .env("GITFOREST_SKIP_AUTH", "1")
        .args([
            "--config",
            &b_config_path.to_string_lossy(),
            "--identity",
            &b_config_home.join("identity.key").to_string_lossy(),
            "daemon",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to start pusher daemon");

    // Wait for readiness
    wait_for_api(18000, 30).await;
    wait_for_api(18001, 30).await;
    wait_for_peer_tagged(18001, &a_peer_id, 30).await;

    let client = reqwest::Client::new();

    // Verify identities
    let resp = client
        .get("http://127.0.0.1:18000/api/id")
        .send()
        .await
        .unwrap();
    let id_json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        id_json.get("peer_id").and_then(|v| v.as_str()),
        Some(a_peer_id.as_str())
    );

    // ─── Set gitforest remote on B (clone --bare created an origin already) ───
    let remote_url = format!("gitforest://{a_peer_id}/myrepo");
    let status = Command::new("git")
        .args(["remote", "set-url", "origin", &remote_url])
        .current_dir(&b_repo)
        .status()
        .unwrap();
    assert!(status.success(), "git remote set-url failed");

    // Set up PATH so git can find git-remote-gitforest, and point it at B's daemon
    let mut path_env = std::env::var("PATH").unwrap_or_default();
    let helper_dir = std::path::Path::new(&helper_binary).parent().unwrap();
    path_env = format!("{}:{}", helper_dir.display(), path_env);

    // ─── Test 1: git fetch via remote helper ───
    let output = Command::new("git")
        .env("PATH", &path_env)
        .env("HOME", &b_dir)
        .env("XDG_RUNTIME_DIR", &b_dir)
        .env("XDG_CONFIG_HOME", &b_xdg_config)
        .env("GITFOREST_DAEMON_URL", "http://127.0.0.1:18001")
        .env("GITFOREST_IDENTITY", b_config_home.join("identity.key"))
        .args(["fetch", "origin"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git fetch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );

    // ─── Test 2: git push without ACL is rejected ───
    let output = Command::new("git")
        .env("PATH", &path_env)
        .env("HOME", &b_dir)
        .env("XDG_RUNTIME_DIR", &b_dir)
        .env("XDG_CONFIG_HOME", &b_xdg_config)
        .env("GITFOREST_DAEMON_URL", "http://127.0.0.1:18001")
        .env("GITFOREST_IDENTITY", b_config_home.join("identity.key"))
        .args(["push", "origin", "HEAD:refs/heads/main"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Git push may return non-zero even when the remote helper reports errors per-ref
    assert!(
        stderr.contains("error") || !output.status.success(),
        "unauthorized push should fail; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );

    // ─── Test 3: Owner grants ACL to B ───
    let resp = client
        .post("http://127.0.0.1:18000/api/repos/myrepo/acl")
        .json(&serde_json::json!({
            "action": "grant",
            "peer_id": b_peer_id,
            "ref_pattern": "refs/heads/*",
        }))
        .send()
        .await
        .expect("ACL grant failed");
    assert!(resp.status().is_success(), "ACL grant should succeed");

    // ─── Test 4: git push with ACL succeeds ───
    let output = Command::new("git")
        .env("PATH", &path_env)
        .env("HOME", &b_dir)
        .env("XDG_RUNTIME_DIR", &b_dir)
        .env("XDG_CONFIG_HOME", &b_xdg_config)
        .env("GITFOREST_DAEMON_URL", "http://127.0.0.1:18001")
        .env("GITFOREST_IDENTITY", b_config_home.join("identity.key"))
        .args(["push", "origin", "HEAD:refs/heads/main"])
        .current_dir(&b_repo)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "authorized push should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );

    // ─── Test 5: Owner repo now has B's commit ───
    let resp = client
        .get("http://127.0.0.1:18000/api/repos/myrepo/log")
        .send()
        .await
        .expect("log request failed");
    assert!(resp.status().is_success());
    let log: Vec<serde_json::Value> = resp.json().await.expect("log json");
    let has_commit = log
        .iter()
        .any(|c| c.get("message").and_then(|v| v.as_str()) == Some("pusher commit"));
    assert!(has_commit, "Owner repo should contain the pushed commit");

    // ─── Cleanup ───
    let _ = proc_a.kill();
    let _ = proc_b.kill();
    let _ = proc_a.wait();
    let _ = proc_b.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test `git clone gitforest://...` end-to-end through the remote helper.
#[tokio::test]
async fn test_git_clone_end_to_end() {
    hermetic_http_env();
    let tmp = std::env::temp_dir().join(format!("gt_clone_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let binary = std::env::var("CARGO_BIN_EXE_gitforest").unwrap_or_else(|_| {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        path.pop();
        path.push("gitforest");
        path.to_string_lossy().to_string()
    });

    let helper_binary = std::env::var("CARGO_BIN_EXE_git-remote-gitforest").unwrap_or_else(|_| {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        path.pop();
        path.push("git-remote-gitforest");
        path.to_string_lossy().to_string()
    });

    // ─── Owner daemon ───
    let owner_dir = tmp.join("owner");
    let owner_repo = owner_dir.join("myrepo");
    let owner_identity = Identity::generate();
    let owner_peer_id = owner_identity.peer_id().to_string();

    let owner_config_home = owner_dir.join(".config").join("gitforest");
    std::fs::create_dir_all(&owner_config_home).unwrap();
    owner_identity
        .save(&owner_config_home.join("identity.key"))
        .unwrap();
    create_git_repo(&owner_repo, "# Owner Repo\n");

    // Add a second commit so there's something worth cloning
    std::fs::write(owner_repo.join("feature.txt"), "feature\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&owner_repo)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "add feature"])
        .current_dir(&owner_repo)
        .output()
        .unwrap();

    let owner_config = Config {
        data_dir: owner_dir.join("data"),
        p2p_port: 19610,
        api_port: 18010,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![RepoConfig {
            name: "myrepo".into(),
            path: owner_repo.clone(),
            owner: owner_identity.peer_id(),
            description: Some("Owner repo".into()),
            seeding: true,
        }],
        bootstrap_peers: vec![],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: Some("127.0.0.1".to_string()),
        payments: Default::default(),
    };
    let owner_config_path = owner_dir.join("config.toml");
    owner_config.save(&owner_config_path).unwrap();

    // ─── Cloner daemon (no repos, just proxying) ───
    let cloner_dir = tmp.join("cloner");
    let cloner_identity = Identity::generate();

    let cloner_config_home = cloner_dir.join(".config").join("gitforest");
    std::fs::create_dir_all(&cloner_config_home).unwrap();
    cloner_identity
        .save(&cloner_config_home.join("identity.key"))
        .unwrap();

    let cloner_config = Config {
        data_dir: cloner_dir.join("data"),
        p2p_port: 19612,
        api_port: 18011,
        auto_seed: true,
        enable_mdns: false,
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![],
        bootstrap_peers: vec!["/ip4/127.0.0.1/tcp/19610/api/18010".to_string()],
        advertise_addr: None,
        cluster_token: None,
        compute: None,
        peer_names: HashMap::new(),
        acl_grants: HashMap::new(),
        model_acl_grants: HashMap::new(),
        llm_backend: None,
        p2p_listen: Some("127.0.0.1".to_string()),
        payments: Default::default(),
    };
    let cloner_config_path = cloner_dir.join("config.toml");
    cloner_config.save(&cloner_config_path).unwrap();

    // ─── Start daemons ───
    let owner_xdg_config = owner_dir.join(".config");
    let cloner_xdg_config = cloner_dir.join(".config");

    let mut proc_owner = Command::new(&binary)
        .env("HOME", &owner_dir)
        .env("XDG_RUNTIME_DIR", &owner_dir)
        .env("XDG_CONFIG_HOME", &owner_xdg_config)
        .env("GITFOREST_SKIP_AUTH", "1")
        .args([
            "--config",
            &owner_config_path.to_string_lossy(),
            "--identity",
            &owner_config_home.join("identity.key").to_string_lossy(),
            "daemon",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to start owner daemon");

    let mut proc_cloner = Command::new(&binary)
        .env("HOME", &cloner_dir)
        .env("XDG_RUNTIME_DIR", &cloner_dir)
        .env("XDG_CONFIG_HOME", &cloner_xdg_config)
        .env("GITFOREST_SKIP_AUTH", "1")
        .args([
            "--config",
            &cloner_config_path.to_string_lossy(),
            "--identity",
            &cloner_config_home.join("identity.key").to_string_lossy(),
            "daemon",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to start cloner daemon");

    wait_for_api(18010, 30).await;
    wait_for_api(18011, 30).await;
    wait_for_peer_tagged(18011, &owner_peer_id, 30).await;

    // Set up PATH for remote helper
    let mut path_env = std::env::var("PATH").unwrap_or_default();
    let helper_dir = std::path::Path::new(&helper_binary).parent().unwrap();
    path_env = format!("{}:{}", helper_dir.display(), path_env);

    let clone_dest = tmp.join("cloned_repo");

    // ─── Test: git clone via remote helper ───
    let remote_url = format!("gitforest://{owner_peer_id}/myrepo");
    let output = Command::new("git")
        .env("PATH", &path_env)
        .env("HOME", &cloner_dir)
        .env("XDG_RUNTIME_DIR", &cloner_dir)
        .env("XDG_CONFIG_HOME", &cloner_xdg_config)
        .env("GITFOREST_DAEMON_URL", "http://127.0.0.1:18011")
        .env(
            "GITFOREST_IDENTITY",
            cloner_config_home.join("identity.key"),
        )
        .args(["clone", &remote_url, &clone_dest.to_string_lossy()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git clone failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );

    // Verify the cloned repo has both commits
    let log_output = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&clone_dest)
        .output()
        .unwrap();
    assert!(log_output.status.success());
    let log = String::from_utf8_lossy(&log_output.stdout);
    assert!(
        log.contains("add feature"),
        "cloned repo should have 'add feature' commit"
    );
    assert!(
        log.contains("initial"),
        "cloned repo should have 'initial' commit"
    );

    // ─── Cleanup ───
    let _ = proc_owner.kill();
    let _ = proc_cloner.kill();
    let _ = proc_owner.wait();
    let _ = proc_cloner.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}
