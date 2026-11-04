use gt_core::identity::{verify_signature, Identity, PeerId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AclError {
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    #[error("invalid ACL signature")]
    InvalidSignature,
    #[error("only the repo owner can modify the ACL")]
    NotOwner,
}

/// A single access grant for a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessGrant {
    pub peer_id: PeerId,
    /// Glob pattern for refs they can push to (e.g. "refs/heads/*", "refs/heads/feature-*").
    pub ref_pattern: String,
}

/// Access Control List for a repository.
/// Signed by the repo owner. Peers can verify it independently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoAcl {
    pub owner: PeerId,
    pub repo_name: String,
    /// Version counter — monotonically increasing. Peers should reject older versions.
    pub version: u64,
    pub grants: Vec<AccessGrant>,
    /// Owner's signature over the serialized (owner, repo_name, version, grants).
    pub signature: Vec<u8>,
}

impl RepoAcl {
    /// Create a new ACL (owner-only, no additional grants).
    pub fn new(identity: &Identity, repo_name: &str) -> Self {
        let mut acl = RepoAcl {
            owner: identity.peer_id(),
            repo_name: repo_name.to_string(),
            version: 1,
            grants: Vec::new(),
            signature: Vec::new(),
        };
        acl.sign(identity);
        acl
    }

    /// Grant push access to a peer for matching refs.
    pub fn grant(
        &mut self,
        identity: &Identity,
        peer_id: PeerId,
        ref_pattern: String,
    ) -> Result<(), AclError> {
        if identity.peer_id() != self.owner {
            return Err(AclError::NotOwner);
        }
        // Remove existing grant for this peer if any
        self.grants.retain(|g| g.peer_id != peer_id);
        self.grants.push(AccessGrant {
            peer_id,
            ref_pattern,
        });
        self.version += 1;
        self.sign(identity);
        Ok(())
    }

    /// Revoke all access for a peer.
    pub fn revoke(&mut self, identity: &Identity, peer_id: &PeerId) -> Result<(), AclError> {
        if identity.peer_id() != self.owner {
            return Err(AclError::NotOwner);
        }
        self.grants.retain(|g| &g.peer_id != peer_id);
        self.version += 1;
        self.sign(identity);
        Ok(())
    }

    /// Check if a peer can push to a specific ref.
    pub fn can_push(&self, peer_id: &PeerId, ref_name: &str) -> bool {
        // Owner can always push
        if peer_id == &self.owner {
            return true;
        }
        self.grants
            .iter()
            .any(|g| &g.peer_id == peer_id && ref_matches_pattern(ref_name, &g.ref_pattern))
    }

    /// Verify the ACL signature.
    pub fn verify(&self) -> Result<(), AclError> {
        let payload = self.signable_payload();
        verify_signature(&self.owner, &payload, &self.signature)
            .map_err(|_| AclError::InvalidSignature)
    }

    fn sign(&mut self, identity: &Identity) {
        let payload = self.signable_payload();
        self.signature = identity.sign(&payload);
    }

    fn signable_payload(&self) -> Vec<u8> {
        // Deterministic serialization of the ACL fields (excluding signature)
        let mut data = Vec::new();
        data.extend_from_slice(&self.owner.0);
        data.extend_from_slice(self.repo_name.as_bytes());
        data.extend_from_slice(&self.version.to_le_bytes());
        for grant in &self.grants {
            data.extend_from_slice(&grant.peer_id.0);
            data.extend_from_slice(grant.ref_pattern.as_bytes());
        }
        data
    }
}

/// Access Control List for a model (collection).
/// Signed by the list owner. Only consulted when present: models without
/// an ACL record stay open to the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAcl {
    pub owner: PeerId,
    pub model_id: String,
    pub version: u64,
    pub grants: Vec<PeerId>,
    pub signature: Vec<u8>,
}

impl ModelAcl {
    pub fn new(identity: &Identity, model_id: &str) -> Self {
        let mut acl = ModelAcl {
            owner: identity.peer_id(),
            model_id: model_id.to_string(),
            version: 1,
            grants: Vec::new(),
            signature: Vec::new(),
        };
        acl.sign(identity);
        acl
    }

    pub fn grant(&mut self, identity: &Identity, peer_id: PeerId) -> Result<(), AclError> {
        if identity.peer_id() != self.owner {
            return Err(AclError::NotOwner);
        }
        if !self.grants.contains(&peer_id) {
            self.grants.push(peer_id);
        }
        self.version += 1;
        self.sign(identity);
        Ok(())
    }

    pub fn revoke(&mut self, identity: &Identity, peer_id: &PeerId) -> Result<(), AclError> {
        if identity.peer_id() != self.owner {
            return Err(AclError::NotOwner);
        }
        self.grants.retain(|g| g != peer_id);
        self.version += 1;
        self.sign(identity);
        Ok(())
    }

    pub fn can_access(&self, peer_id: &PeerId) -> bool {
        peer_id == &self.owner || self.grants.contains(peer_id)
    }

    pub fn verify(&self) -> Result<(), AclError> {
        let payload = self.signable_payload();
        verify_signature(&self.owner, &payload, &self.signature)
            .map_err(|_| AclError::InvalidSignature)
    }

    fn sign(&mut self, identity: &Identity) {
        let payload = self.signable_payload();
        self.signature = identity.sign(&payload);
    }

    fn signable_payload(&self) -> Vec<u8> {
        let mut data = b"gitforest:model-acl:v1:".to_vec();
        data.extend_from_slice(&self.owner.0);
        data.extend_from_slice(self.model_id.as_bytes());
        data.extend_from_slice(&self.version.to_le_bytes());
        for grant in &self.grants {
            data.extend_from_slice(&grant.0);
        }
        data
    }
}

/// Simple glob matching for ref patterns.
fn ref_matches_pattern(ref_name: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        ref_name.starts_with(prefix)
    } else {
        ref_name == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_owner_can_always_push() {
        let id = Identity::generate();
        let acl = RepoAcl::new(&id, "myrepo");
        assert!(acl.can_push(&id.peer_id(), "refs/heads/main"));
    }

    #[test]
    fn test_grant_and_check() {
        let owner = Identity::generate();
        let collab = Identity::generate();
        let mut acl = RepoAcl::new(&owner, "myrepo");
        acl.grant(&owner, collab.peer_id(), "refs/heads/*".into())
            .unwrap();
        assert!(acl.can_push(&collab.peer_id(), "refs/heads/feature-x"));
        assert!(!acl.can_push(&collab.peer_id(), "refs/tags/v1.0"));
    }

    #[test]
    fn test_revoke() {
        let owner = Identity::generate();
        let collab = Identity::generate();
        let mut acl = RepoAcl::new(&owner, "myrepo");
        acl.grant(&owner, collab.peer_id(), "refs/heads/*".into())
            .unwrap();
        acl.revoke(&owner, &collab.peer_id()).unwrap();
        assert!(!acl.can_push(&collab.peer_id(), "refs/heads/main"));
    }

    #[test]
    fn test_non_owner_cannot_modify() {
        let owner = Identity::generate();
        let other = Identity::generate();
        let mut acl = RepoAcl::new(&owner, "myrepo");
        assert!(acl.grant(&other, other.peer_id(), "*".into()).is_err());
    }

    #[test]
    fn test_acl_signature_verification() {
        let owner = Identity::generate();
        let acl = RepoAcl::new(&owner, "myrepo");
        assert!(acl.verify().is_ok());
    }

    #[test]
    fn test_tampered_acl_fails_verification() {
        let owner = Identity::generate();
        let mut acl = RepoAcl::new(&owner, "myrepo");
        // Tamper with the version without re-signing
        acl.version = 999;
        assert!(acl.verify().is_err());
    }

    #[test]
    fn test_model_acl_grant_revoke_verify() {
        let owner = Identity::generate();
        let friend = Identity::generate().peer_id();
        let stranger = Identity::generate().peer_id();
        let mut acl = ModelAcl::new(&owner, "model-1");
        assert!(acl.can_access(&owner.peer_id()));
        assert!(!acl.can_access(&friend));
        acl.grant(&owner, friend.clone()).unwrap();
        assert!(acl.can_access(&friend));
        assert!(!acl.can_access(&stranger));
        assert!(acl.verify().is_ok());
        acl.grant(&owner, friend.clone()).unwrap();
        assert_eq!(acl.grants.len(), 1);
        acl.revoke(&owner, &friend).unwrap();
        assert!(!acl.can_access(&friend));
        assert!(acl.verify().is_ok());
    }

    #[test]
    fn test_model_acl_non_owner_cannot_modify() {
        let owner = Identity::generate();
        let other = Identity::generate();
        let mut acl = ModelAcl::new(&owner, "model-1");
        assert!(acl.grant(&other, other.peer_id()).is_err());
        assert!(acl.revoke(&other, &other.peer_id()).is_err());
    }

    #[test]
    fn test_model_acl_tamper_fails() {
        let owner = Identity::generate();
        let mut acl = ModelAcl::new(&owner, "model-1");
        acl.version = 999;
        assert!(acl.verify().is_err());
    }
}
