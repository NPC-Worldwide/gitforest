use gt_core::identity::{repo_address, PeerId};
use gt_core::protocol::RepoAnnouncement;
use std::collections::HashMap;

/// Local index of known repos on the network.
/// Caches repo announcements we've seen.
pub struct RepoIndex {
    /// Map from repo address hash → announcement.
    by_address: HashMap<[u8; 32], Vec<RepoAnnouncement>>,
    /// Map from owner → list of repo names.
    by_owner: HashMap<PeerId, Vec<String>>,
}

impl RepoIndex {
    pub fn new() -> Self {
        RepoIndex {
            by_address: HashMap::new(),
            by_owner: HashMap::new(),
        }
    }

    /// Add or update a repo announcement in the index.
    pub fn upsert(&mut self, announcement: RepoAnnouncement) {
        let addr = repo_address(&announcement.owner, &announcement.name);

        // Update by_address
        let entries = self.by_address.entry(addr).or_default();
        // Replace existing announcement from same owner, or add new
        if let Some(existing) = entries.iter_mut().find(|a| a.owner == announcement.owner) {
            *existing = announcement.clone();
        } else {
            entries.push(announcement.clone());
        }

        // Update by_owner
        let names = self.by_owner.entry(announcement.owner.clone()).or_default();
        if !names.contains(&announcement.name) {
            names.push(announcement.name.clone());
        }
    }

    /// Look up a repo by address hash.
    pub fn lookup(&self, address: &[u8; 32]) -> Option<&Vec<RepoAnnouncement>> {
        self.by_address.get(address)
    }

    /// Look up repos by owner.
    pub fn by_owner(&self, owner: &PeerId) -> Option<&Vec<String>> {
        self.by_owner.get(owner)
    }

    /// Get all known repo announcements.
    pub fn all(&self) -> Vec<&RepoAnnouncement> {
        self.by_address.values().flatten().collect()
    }

    /// Number of unique repos in the index.
    pub fn count(&self) -> usize {
        self.by_address.len()
    }
}

impl Default for RepoIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    fn make_announcement(owner: &Identity, name: &str) -> RepoAnnouncement {
        RepoAnnouncement {
            owner: owner.peer_id(),
            name: name.to_string(),
            description: format!("Repo {name}"),
            default_branch: "main".into(),
            head: "abc123".into(),
            signature: owner.sign(name.as_bytes()),
            display_name: None,
        }
    }

    #[test]
    fn test_new_empty() {
        let idx = RepoIndex::new();
        assert_eq!(idx.count(), 0);
        assert!(idx.all().is_empty());
    }

    #[test]
    fn test_default_same_as_new() {
        let idx = RepoIndex::default();
        assert_eq!(idx.count(), 0);
        assert!(idx.all().is_empty());
    }

    #[test]
    fn test_upsert_and_lookup() {
        let mut idx = RepoIndex::new();
        let alice = Identity::generate();
        let ann = make_announcement(&alice, "myrepo");
        let addr = repo_address(&alice.peer_id(), "myrepo");

        idx.upsert(ann.clone());

        let result = idx.lookup(&addr).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "myrepo");
        assert_eq!(result[0].owner, alice.peer_id());
    }

    #[test]
    fn test_upsert_updates_existing() {
        let mut idx = RepoIndex::new();
        let alice = Identity::generate();

        let mut ann = make_announcement(&alice, "repo");
        ann.head = "aaa".into();
        idx.upsert(ann);

        let mut ann2 = make_announcement(&alice, "repo");
        ann2.head = "bbb".into();
        idx.upsert(ann2);

        let addr = repo_address(&alice.peer_id(), "repo");
        let result = idx.lookup(&addr).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].head, "bbb");
        // count should still be 1
        assert_eq!(idx.count(), 1);
    }

    #[test]
    fn test_different_owners_same_name() {
        let mut idx = RepoIndex::new();
        let alice = Identity::generate();
        let bob = Identity::generate();

        idx.upsert(make_announcement(&alice, "project"));
        idx.upsert(make_announcement(&bob, "project"));

        // Different owners produce different addresses
        let addr_a = repo_address(&alice.peer_id(), "project");
        let addr_b = repo_address(&bob.peer_id(), "project");
        assert_ne!(addr_a, addr_b);

        assert!(idx.lookup(&addr_a).is_some());
        assert!(idx.lookup(&addr_b).is_some());
        assert_eq!(idx.count(), 2);
    }

    #[test]
    fn test_by_owner() {
        let mut idx = RepoIndex::new();
        let alice = Identity::generate();
        let bob = Identity::generate();

        idx.upsert(make_announcement(&alice, "repo-a"));
        idx.upsert(make_announcement(&alice, "repo-b"));
        idx.upsert(make_announcement(&bob, "repo-c"));

        let alice_repos = idx.by_owner(&alice.peer_id()).unwrap();
        assert_eq!(alice_repos.len(), 2);
        assert!(alice_repos.contains(&"repo-a".to_string()));
        assert!(alice_repos.contains(&"repo-b".to_string()));

        let bob_repos = idx.by_owner(&bob.peer_id()).unwrap();
        assert_eq!(bob_repos.len(), 1);
        assert!(bob_repos.contains(&"repo-c".to_string()));
    }

    #[test]
    fn test_by_owner_no_duplicates() {
        let mut idx = RepoIndex::new();
        let alice = Identity::generate();

        idx.upsert(make_announcement(&alice, "repo"));
        idx.upsert(make_announcement(&alice, "repo")); // same again

        let names = idx.by_owner(&alice.peer_id()).unwrap();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn test_lookup_missing() {
        let idx = RepoIndex::new();
        assert!(idx.lookup(&[0u8; 32]).is_none());
    }

    #[test]
    fn test_count() {
        let mut idx = RepoIndex::new();
        let a = Identity::generate();
        let b = Identity::generate();
        let c = Identity::generate();

        idx.upsert(make_announcement(&a, "r1"));
        idx.upsert(make_announcement(&b, "r2"));
        idx.upsert(make_announcement(&c, "r3"));

        assert_eq!(idx.count(), 3);
    }

    #[test]
    fn test_all() {
        let mut idx = RepoIndex::new();
        let a = Identity::generate();
        let b = Identity::generate();

        idx.upsert(make_announcement(&a, "x"));
        idx.upsert(make_announcement(&b, "y"));

        let all = idx.all();
        assert_eq!(all.len(), 2);
        let names: Vec<&str> = all.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"x"));
        assert!(names.contains(&"y"));
    }

    #[test]
    fn test_by_owner_missing() {
        let idx = RepoIndex::new();
        let unknown = Identity::generate();
        assert!(idx.by_owner(&unknown.peer_id()).is_none());
    }
}
