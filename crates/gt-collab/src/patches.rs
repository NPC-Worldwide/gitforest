use chrono::Utc;
use gt_core::identity::{verify_signature, Identity, PeerId};
use gt_core::types::{PatchStatus, StoredPatch};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A signed patch set submitted to a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSet {
    pub id: String,
    pub author: PeerId,
    pub target_owner: PeerId,
    pub target_repo: String,
    pub target_branch: String,
    pub title: String,
    pub description: String,
    /// Raw patch data (output of `git format-patch`).
    pub patches: Vec<Vec<u8>>,
    pub signature: Vec<u8>,
}

impl PatchSet {
    /// Create and sign a new patch set.
    pub fn new(
        identity: &Identity,
        target_owner: PeerId,
        target_repo: String,
        target_branch: String,
        title: String,
        description: String,
        patches: Vec<Vec<u8>>,
    ) -> Self {
        let id = Uuid::new_v4().to_string();
        let mut ps = PatchSet {
            id,
            author: identity.peer_id(),
            target_owner,
            target_repo,
            target_branch,
            title,
            description,
            patches,
            signature: Vec::new(),
        };
        let payload = ps.signable_payload();
        ps.signature = identity.sign(&payload);
        ps
    }

    /// Verify the patch set's signature.
    pub fn verify(&self) -> bool {
        let payload = self.signable_payload();
        verify_signature(&self.author, &payload, &self.signature).is_ok()
    }

    /// Convert to stored format for persistence.
    pub fn to_stored(&self) -> StoredPatch {
        StoredPatch {
            id: self.id.clone(),
            author: self.author.clone(),
            target_repo: self.target_repo.clone(),
            target_branch: self.target_branch.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            status: PatchStatus::Pending,
            created_at: Utc::now(),
        }
    }

    fn signable_payload(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(self.id.as_bytes());
        data.extend_from_slice(&self.author.0);
        data.extend_from_slice(&self.target_owner.0);
        data.extend_from_slice(self.target_repo.as_bytes());
        data.extend_from_slice(self.target_branch.as_bytes());
        data.extend_from_slice(self.title.as_bytes());
        data.extend_from_slice(self.description.as_bytes());
        for patch in &self.patches {
            data.extend_from_slice(patch);
        }
        data
    }
}

/// Store for received patch sets.
pub struct PatchStore {
    patches: Vec<PatchSet>,
}

impl PatchStore {
    pub fn new() -> Self {
        PatchStore {
            patches: Vec::new(),
        }
    }

    /// Add a patch set after verifying its signature.
    pub fn add(&mut self, ps: PatchSet) -> bool {
        if !ps.verify() {
            return false;
        }
        self.patches.push(ps);
        true
    }

    /// Get all patches for a given repo.
    pub fn for_repo(&self, repo_name: &str) -> Vec<&PatchSet> {
        self.patches
            .iter()
            .filter(|p| p.target_repo == repo_name)
            .collect()
    }

    /// Get a patch by ID.
    pub fn get(&self, id: &str) -> Option<&PatchSet> {
        self.patches.iter().find(|p| p.id == id)
    }
}

impl Default for PatchStore {
    fn default() -> Self {
        Self::new()
    }
}
