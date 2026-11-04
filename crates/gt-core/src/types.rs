use crate::identity::PeerId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// A repository on the GitForest network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetadata {
    pub owner: PeerId,
    pub name: String,
    pub description: String,
    pub default_branch: String,
    pub head_commit: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Local repo state tracked by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRepo {
    pub metadata: RepoMetadata,
    pub path: PathBuf,
    pub seeding: bool,
    pub seeder_count: u32,
    pub leecher_count: u32,
}

/// Stats for the network/daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStats {
    pub peer_id: PeerId,
    pub connected_peers: u32,
    pub repos_hosted: u32,
    pub repos_seeding: u32,
    pub total_uploaded: u64,
    pub total_downloaded: u64,
    pub uptime_secs: u64,
}

/// A merge request stored locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMergeRequest {
    pub id: String,
    pub author: PeerId,
    pub source_repo: String,
    pub source_branch: String,
    pub target_repo: String,
    pub target_branch: String,
    pub title: String,
    pub description: String,
    pub status: MergeRequestStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MergeRequestStatus {
    Open,
    Merged,
    Closed,
}

/// A patch submission stored locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPatch {
    pub id: String,
    pub author: PeerId,
    pub target_repo: String,
    pub target_branch: String,
    pub title: String,
    pub description: String,
    pub status: PatchStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PatchStatus {
    Pending,
    Applied,
    Rejected,
}

// ============ Large File Storage ============

/// Content-addressed blob identifier (SHA-256 hash of file content).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobId(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl std::str::FromStr for BlobId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|e| format!("invalid hex: {e}"))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "expected 32 bytes".to_string())?;
        Ok(BlobId(arr))
    }
}

/// A build artifact pinned to blob-store bytes by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactBlob {
    pub name: String,
    pub blob_id: BlobId,
    pub size: u64,
}

/// A file entry within a collection (model or dataset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionFile {
    /// Content hash of this file.
    pub blob_id: BlobId,
    /// Path within the collection (e.g., "weights/layer1.bin").
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// Optional MIME type.
    pub media_type: Option<String>,
}

/// A collection manifest describing a model or dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionManifest {
    /// Unique identifier.
    pub id: String,
    /// Human-readable name (e.g., "llama-7b-q4").
    pub name: String,
    /// Description.
    pub description: String,
    /// Owner peer ID.
    pub owner: PeerId,
    /// Files in this collection.
    pub files: Vec<CollectionFile>,
    /// Arbitrary metadata (e.g., model architecture, license, training data).
    pub metadata: HashMap<String, String>,
    /// When this manifest was created.
    pub created_at: DateTime<Utc>,
    /// Signature over the manifest by the owner.
    pub signature: Vec<u8>,
}

impl CollectionManifest {
    /// Create and sign a new collection manifest.
    pub fn new(
        identity: &crate::identity::Identity,
        name: String,
        description: String,
        files: Vec<CollectionFile>,
        metadata: HashMap<String, String>,
    ) -> Self {
        let mut manifest = CollectionManifest {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            description,
            owner: identity.peer_id(),
            files,
            metadata,
            created_at: chrono::Utc::now(),
            signature: vec![],
        };

        let payload = manifest.signable_payload();
        manifest.signature = identity.sign(&payload);
        manifest
    }

    /// Compute the signable payload (domain-separated SHA-256 hash of fields).
    pub fn signable_payload(&self) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:collection:");
        hasher.update(self.id.as_bytes());
        hasher.update(self.name.as_bytes());
        hasher.update(self.description.as_bytes());
        hasher.update(self.owner.0);
        for file in &self.files {
            hasher.update(file.blob_id.0);
            hasher.update(file.path.as_bytes());
            hasher.update(file.size.to_le_bytes());
        }
        hasher.update(self.created_at.timestamp().to_le_bytes());
        hasher.finalize().to_vec()
    }

    /// Verify the signature on this manifest.
    pub fn verify(&self) -> bool {
        crate::identity::verify_signature(&self.owner, &self.signable_payload(), &self.signature)
            .is_ok()
    }

    /// Total size of all files in this collection.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

mod hex_bytes {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}
