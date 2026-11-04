use gt_core::identity::PeerId;
use gt_core::types::BlobId;
use std::collections::{HashMap, HashSet};

/// Tracks which peers are seeding/leeching which blobs.
pub struct BlobTracker {
    seeders: HashMap<BlobId, HashSet<PeerId>>,
    leechers: HashMap<BlobId, HashSet<PeerId>>,
}

impl BlobTracker {
    pub fn new() -> Self {
        BlobTracker {
            seeders: HashMap::new(),
            leechers: HashMap::new(),
        }
    }

    pub fn add_seeder(&mut self, blob_id: BlobId, peer_id: PeerId) {
        self.seeders
            .entry(blob_id.clone())
            .or_default()
            .insert(peer_id.clone());
        // If they were leeching, remove from leechers
        if let Some(set) = self.leechers.get_mut(&blob_id) {
            set.remove(&peer_id);
        }
    }

    pub fn remove_seeder(&mut self, blob_id: &BlobId, peer_id: &PeerId) {
        if let Some(set) = self.seeders.get_mut(blob_id) {
            set.remove(peer_id);
        }
    }

    pub fn add_leecher(&mut self, blob_id: BlobId, peer_id: PeerId) {
        self.leechers.entry(blob_id).or_default().insert(peer_id);
    }

    pub fn remove_leecher(&mut self, blob_id: &BlobId, peer_id: &PeerId) {
        if let Some(set) = self.leechers.get_mut(blob_id) {
            set.remove(peer_id);
        }
    }

    /// Promote a leecher to seeder (download complete).
    pub fn promote_to_seeder(&mut self, blob_id: &BlobId, peer_id: PeerId) {
        self.remove_leecher(blob_id, &peer_id);
        self.add_seeder(blob_id.clone(), peer_id);
    }

    pub fn seeder_count(&self, blob_id: &BlobId) -> u32 {
        self.seeders.get(blob_id).map_or(0, |s| s.len() as u32)
    }

    pub fn leecher_count(&self, blob_id: &BlobId) -> u32 {
        self.leechers.get(blob_id).map_or(0, |s| s.len() as u32)
    }

    pub fn seeders_for(&self, blob_id: &BlobId) -> Vec<PeerId> {
        self.seeders
            .get(blob_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn leechers_for(&self, blob_id: &BlobId) -> Vec<PeerId> {
        self.leechers
            .get(blob_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// All blobs a peer is seeding.
    pub fn blobs_for_peer(&self, peer_id: &PeerId) -> Vec<BlobId> {
        self.seeders
            .iter()
            .filter(|(_, peers)| peers.contains(peer_id))
            .map(|(blob_id, _)| blob_id.clone())
            .collect()
    }

    /// Total number of tracked blobs (with at least one seeder or leecher).
    pub fn tracked_blob_count(&self) -> usize {
        let mut all: HashSet<&BlobId> = HashSet::new();
        for k in self.seeders.keys() {
            all.insert(k);
        }
        for k in self.leechers.keys() {
            all.insert(k);
        }
        all.len()
    }
}

impl Default for BlobTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    #[test]
    fn test_seeder_tracking() {
        let mut tracker = BlobTracker::new();
        let blob = BlobId([0x42; 32]);
        let alice = Identity::generate().peer_id();
        let bob = Identity::generate().peer_id();

        tracker.add_seeder(blob.clone(), alice.clone());
        assert_eq!(tracker.seeder_count(&blob), 1);

        tracker.add_seeder(blob.clone(), bob.clone());
        assert_eq!(tracker.seeder_count(&blob), 2);

        tracker.remove_seeder(&blob, &alice);
        assert_eq!(tracker.seeder_count(&blob), 1);
        assert_eq!(tracker.seeders_for(&blob), vec![bob.clone()]);
    }

    #[test]
    fn test_leecher_tracking() {
        let mut tracker = BlobTracker::new();
        let blob = BlobId([0x99; 32]);
        let peer = Identity::generate().peer_id();

        tracker.add_leecher(blob.clone(), peer.clone());
        assert_eq!(tracker.leecher_count(&blob), 1);
        assert_eq!(tracker.seeder_count(&blob), 0);
        assert_eq!(tracker.leechers_for(&blob), vec![peer.clone()]);

        tracker.remove_leecher(&blob, &peer);
        assert_eq!(tracker.leecher_count(&blob), 0);
        assert!(tracker.leechers_for(&blob).is_empty());
    }

    #[test]
    fn test_promote_to_seeder() {
        let mut tracker = BlobTracker::new();
        let blob = BlobId([0xaa; 32]);
        let peer = Identity::generate().peer_id();

        tracker.add_leecher(blob.clone(), peer.clone());
        assert_eq!(tracker.leecher_count(&blob), 1);
        assert_eq!(tracker.seeder_count(&blob), 0);

        tracker.promote_to_seeder(&blob, peer.clone());
        assert_eq!(tracker.leecher_count(&blob), 0);
        assert_eq!(tracker.seeder_count(&blob), 1);
    }

    #[test]
    fn test_blobs_for_peer() {
        let mut tracker = BlobTracker::new();
        let alice = Identity::generate().peer_id();
        let blob1 = BlobId([0x01; 32]);
        let blob2 = BlobId([0x02; 32]);
        let blob3 = BlobId([0x03; 32]);

        tracker.add_seeder(blob1.clone(), alice.clone());
        tracker.add_seeder(blob2.clone(), alice.clone());
        // blob3: alice is only leeching
        tracker.add_leecher(blob3.clone(), alice.clone());

        let seeding = tracker.blobs_for_peer(&alice);
        assert_eq!(seeding.len(), 2);
        assert!(seeding.contains(&blob1));
        assert!(seeding.contains(&blob2));
    }

    #[test]
    fn test_tracked_blob_count() {
        let mut tracker = BlobTracker::new();
        assert_eq!(tracker.tracked_blob_count(), 0);

        let peer = Identity::generate().peer_id();
        tracker.add_seeder(BlobId([0x01; 32]), peer.clone());
        tracker.add_leecher(BlobId([0x02; 32]), peer.clone());
        // Same blob, different role
        tracker.add_leecher(BlobId([0x01; 32]), Identity::generate().peer_id());

        assert_eq!(tracker.tracked_blob_count(), 2);
    }

    #[test]
    fn test_add_seeder_removes_from_leechers() {
        let mut tracker = BlobTracker::new();
        let blob = BlobId([0x55; 32]);
        let peer = Identity::generate().peer_id();

        tracker.add_leecher(blob.clone(), peer.clone());
        assert_eq!(tracker.leecher_count(&blob), 1);

        tracker.add_seeder(blob.clone(), peer.clone());
        assert_eq!(tracker.leecher_count(&blob), 0);
        assert_eq!(tracker.seeder_count(&blob), 1);
    }
}
