use chrono::Utc;
use gt_core::identity::{verify_signature, Identity, PeerId};
use gt_core::types::{MergeRequestStatus, StoredMergeRequest};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A signed merge request: "I forked your repo, here's my branch, please merge."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequest {
    pub id: String,
    pub author: PeerId,
    /// The author's repo (fork) name.
    pub source_repo: String,
    /// Branch in the fork to merge from.
    pub source_branch: String,
    /// Target repo owner.
    pub target_owner: PeerId,
    /// Target repo name.
    pub target_repo: String,
    /// Target branch to merge into.
    pub target_branch: String,
    pub title: String,
    pub description: String,
    pub signature: Vec<u8>,
}

impl MergeRequest {
    /// Create and sign a new merge request.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: &Identity,
        source_repo: String,
        source_branch: String,
        target_owner: PeerId,
        target_repo: String,
        target_branch: String,
        title: String,
        description: String,
    ) -> Self {
        let id = Uuid::new_v4().to_string();
        let mut mr = MergeRequest {
            id,
            author: identity.peer_id(),
            source_repo,
            source_branch,
            target_owner,
            target_repo,
            target_branch,
            title,
            description,
            signature: Vec::new(),
        };
        let payload = mr.signable_payload();
        mr.signature = identity.sign(&payload);
        mr
    }

    /// Verify the merge request's signature.
    pub fn verify(&self) -> bool {
        let payload = self.signable_payload();
        verify_signature(&self.author, &payload, &self.signature).is_ok()
    }

    /// Convert to stored format.
    pub fn to_stored(&self) -> StoredMergeRequest {
        StoredMergeRequest {
            id: self.id.clone(),
            author: self.author.clone(),
            source_repo: self.source_repo.clone(),
            source_branch: self.source_branch.clone(),
            target_repo: self.target_repo.clone(),
            target_branch: self.target_branch.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            status: MergeRequestStatus::Open,
            created_at: Utc::now(),
        }
    }

    fn signable_payload(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(self.id.as_bytes());
        data.extend_from_slice(&self.author.0);
        data.extend_from_slice(self.source_repo.as_bytes());
        data.extend_from_slice(self.source_branch.as_bytes());
        data.extend_from_slice(&self.target_owner.0);
        data.extend_from_slice(self.target_repo.as_bytes());
        data.extend_from_slice(self.target_branch.as_bytes());
        data.extend_from_slice(self.title.as_bytes());
        data.extend_from_slice(self.description.as_bytes());
        data
    }
}

/// Store for received merge requests.
pub struct MergeRequestStore {
    requests: Vec<MergeRequest>,
    status: std::collections::HashMap<String, MergeRequestStatus>,
}

impl MergeRequestStore {
    pub fn new() -> Self {
        MergeRequestStore {
            requests: Vec::new(),
            status: std::collections::HashMap::new(),
        }
    }

    /// Add a merge request after verifying its signature.
    pub fn add(&mut self, mr: MergeRequest) -> bool {
        if !mr.verify() {
            return false;
        }
        self.requests.push(mr);
        true
    }

    /// Get all merge requests for a given repo.
    pub fn for_repo(&self, repo_name: &str) -> Vec<&MergeRequest> {
        self.requests
            .iter()
            .filter(|mr| mr.target_repo == repo_name)
            .collect()
    }

    /// Get a merge request by ID.
    pub fn get(&self, id: &str) -> Option<&MergeRequest> {
        self.requests.iter().find(|mr| mr.id == id)
    }

    pub fn status_of(&self, id: &str) -> MergeRequestStatus {
        self.status
            .get(id)
            .cloned()
            .unwrap_or(MergeRequestStatus::Open)
    }

    pub fn set_status(&mut self, id: &str, status: MergeRequestStatus) -> bool {
        if self.get(id).is_none() {
            return false;
        }
        self.status.insert(id.to_string(), status);
        true
    }
}

impl Default for MergeRequestStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_defaults_open_and_updates() {
        let author = gt_core::identity::Identity::generate();
        let owner = gt_core::identity::Identity::generate().peer_id();
        let mr = MergeRequest::new(
            &author,
            "fork".into(),
            "feature".into(),
            owner,
            "demo".into(),
            "main".into(),
            "T".into(),
            "D".into(),
        );
        let id = mr.id.clone();
        let mut store = MergeRequestStore::new();
        assert!(store.add(mr));
        assert_eq!(store.status_of(&id), MergeRequestStatus::Open);
        assert!(!store.set_status("missing", MergeRequestStatus::Closed));
        assert!(store.set_status(&id, MergeRequestStatus::Merged));
        assert_eq!(store.status_of(&id), MergeRequestStatus::Merged);
    }
}
