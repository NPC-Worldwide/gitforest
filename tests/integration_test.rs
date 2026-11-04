use gt_core::identity::{self, Identity, PeerId, PeerName};
use gt_core::config::{Config, RepoConfig};
use gt_core::types::{BlobId, CollectionFile, CollectionManifest};
use gt_collab::acl::RepoAcl;
use gt_collab::patches::PatchSet;
use gt_collab::merge_request::MergeRequest;
use gt_lfs::blob::{adaptive_piece_size, blob_dht_address, BlobMeta, BlobDownloadState};
use gt_lfs::pointer::{PointerFile, is_pointer_file};
use gt_lfs::store::BlobStore;
use gt_lfs::tracker::BlobTracker;
use gt_lfs::collection::CollectionStore;
use gt_transport::torrent::{BundleMeta, DownloadState};
use std::collections::HashMap;
use std::path::PathBuf;

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
    acl.grant(&owner, collaborator.peer_id(), "refs/heads/*".into()).unwrap();
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
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![
            RepoConfig {
                name: "myrepo".into(),
                path: PathBuf::from("/home/user/myrepo.git"),
                owner: id.peer_id(),
                description: Some("Test repo".into()),
                seeding: true,
            },
        ],
        bootstrap_peers: vec![],
        compute: None,
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
        max_upload_rate: 0,
        max_download_rate: 0,
        repos: vec![],
        bootstrap_peers: vec![],
        compute: None,
        peer_names: HashMap::new(),
    };
    config.peer_names.insert(
        format!("friend#{}", &pid_hex[..4]),
        pid_hex.clone(),
    );

    let tmp = std::env::temp_dir().join("gt_test_config_names");
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
