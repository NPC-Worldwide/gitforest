use chrono::{DateTime, TimeZone, Utc};
use gt_core::identity::PeerId;
use gt_core::protocol::CacheAnnouncement;
use gt_core::types::BlobId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: String,
    pub blob_id: BlobId,
    pub size: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct CacheStore {
    path: PathBuf,
    entries: HashMap<String, CacheEntry>,
}

impl CacheStore {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("caches.json");
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<CacheEntry>>(&bytes).ok())
            .map(|list| list.into_iter().map(|e| (e.key.clone(), e)).collect())
            .unwrap_or_default();
        CacheStore { path, entries }
    }

    pub fn get(&self, key: &str) -> Option<&CacheEntry> {
        self.entries.get(key)
    }

    pub fn entries(&self) -> impl Iterator<Item = &CacheEntry> {
        self.entries.values()
    }

    pub fn newest_with_prefix(&self, prefix: &str) -> Option<&CacheEntry> {
        self.entries
            .values()
            .filter(|e| e.key.starts_with(prefix))
            .max_by_key(|e| e.created_at)
    }

    pub fn insert(&mut self, entry: CacheEntry) -> bool {
        if self.entries.contains_key(&entry.key) {
            return false;
        }
        self.entries.insert(entry.key.clone(), entry);
        self.save();
        true
    }

    pub fn remove(&mut self, key: &str) -> Option<CacheEntry> {
        let entry = self.entries.remove(key)?;
        self.save();
        Some(entry)
    }

    pub fn blob_referenced_elsewhere(&self, blob_id: &BlobId, except_key: &str) -> bool {
        self.entries
            .iter()
            .any(|(k, e)| k != except_key && e.blob_id == *blob_id)
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.entries.values().collect::<Vec<_>>()) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}

pub const REMOTE_CACHE_TTL: Duration = Duration::from_secs(300);
pub const MAX_REMOTE_CACHE_KEYS: usize = 4096;

#[derive(Debug, Clone)]
pub struct RemoteCacheEntry {
    pub key: String,
    pub blob_id: BlobId,
    pub size: u64,
    pub updated_at: DateTime<Utc>,
    pub seeder: PeerId,
    pub seen_at: Instant,
}

#[derive(Debug, Default)]
pub struct RemoteCacheDir {
    entries: HashMap<String, Vec<RemoteCacheEntry>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteCacheSummary {
    pub key: String,
    pub seeders: usize,
    pub size: u64,
    pub updated_at: DateTime<Utc>,
}

impl RemoteCacheDir {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.values().all(Vec::is_empty)
    }

    pub fn merge_announcement(&mut self, ann: &CacheAnnouncement, now: Instant) -> bool {
        if !ann.verify() {
            return false;
        }
        if ann.key.is_empty() || ann.key.len() > 128 {
            return false;
        }
        let Some(updated_at) = Utc.timestamp_opt(ann.updated_at, 0).single() else {
            return false;
        };
        if self.entries.len() >= MAX_REMOTE_CACHE_KEYS && !self.entries.contains_key(&ann.key) {
            return false;
        }
        let seeders = self.entries.entry(ann.key.clone()).or_default();
        if let Some(existing) = seeders.iter_mut().find(|e| e.seeder == ann.owner) {
            if updated_at > existing.updated_at {
                existing.blob_id = ann.blob_id.clone();
                existing.size = ann.size;
                existing.updated_at = updated_at;
            }
            existing.seen_at = now;
            return true;
        }
        seeders.push(RemoteCacheEntry {
            key: ann.key.clone(),
            blob_id: ann.blob_id.clone(),
            size: ann.size,
            updated_at,
            seeder: ann.owner.clone(),
            seen_at: now,
        });
        true
    }

    pub fn get_exact(&self, key: &str) -> Option<&RemoteCacheEntry> {
        self.entries.get(key).and_then(|seeders| {
            seeders.iter().max_by(|a, b| {
                a.updated_at
                    .cmp(&b.updated_at)
                    .then(a.seeder.0.cmp(&b.seeder.0))
            })
        })
    }

    pub fn seeders_for(&self, key: &str) -> Vec<&RemoteCacheEntry> {
        let mut out: Vec<&RemoteCacheEntry> = self
            .entries
            .get(key)
            .map(|seeders| seeders.iter().collect())
            .unwrap_or_default();
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then(a.seeder.0.cmp(&b.seeder.0))
        });
        out
    }

    pub fn best_with_prefix(&self, prefix: &str) -> Option<&RemoteCacheEntry> {
        self.entries
            .values()
            .flatten()
            .filter(|e| e.key.starts_with(prefix))
            .max_by(|a, b| {
                a.key
                    .len()
                    .cmp(&b.key.len())
                    .then(a.updated_at.cmp(&b.updated_at))
                    .then(a.key.cmp(&b.key))
            })
    }

    pub fn snapshot(&self) -> Vec<RemoteCacheSummary> {
        let mut out: Vec<RemoteCacheSummary> = self
            .entries
            .iter()
            .filter_map(|(key, seeders)| {
                let newest = seeders.iter().max_by_key(|e| e.updated_at)?;
                Some(RemoteCacheSummary {
                    key: key.clone(),
                    seeders: seeders.len(),
                    size: newest.size,
                    updated_at: newest.updated_at,
                })
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    pub fn prune_expired_at(&mut self, now: Instant) {
        for seeders in self.entries.values_mut() {
            seeders.retain(|e| {
                now.checked_duration_since(e.seen_at).unwrap_or_default() < REMOTE_CACHE_TTL
            });
        }
        self.entries.retain(|_, seeders| !seeders.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_rejects_duplicate_keys_and_matches_prefixes() {
        let dir = std::env::temp_dir().join(format!("gf_cache_{}", uuid::Uuid::new_v4()));
        let mut store = CacheStore::load(&dir);
        let entry = |key: &str| CacheEntry {
            key: key.to_string(),
            blob_id: BlobId([1u8; 32]),
            size: 10,
            created_at: Utc::now(),
        };
        assert!(store.insert(entry("linux-cargo-abc")));
        assert!(!store.insert(entry("linux-cargo-abc")));
        assert!(store.get("linux-cargo-abc").is_some());
        assert!(store.get("missing").is_none());
        assert_eq!(
            store
                .newest_with_prefix("linux-cargo-")
                .map(|e| e.key.as_str()),
            Some("linux-cargo-abc")
        );
        assert!(store.newest_with_prefix("macos-").is_none());
        let reloaded = CacheStore::load(&dir);
        assert!(reloaded.get("linux-cargo-abc").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn signed_ann(
        owner: &gt_core::identity::Identity,
        key: &str,
        byte: u8,
        updated_at: i64,
    ) -> CacheAnnouncement {
        let mut ann = CacheAnnouncement {
            owner: owner.peer_id(),
            key: key.to_string(),
            blob_id: BlobId([byte; 32]),
            size: 100,
            updated_at,
            signature: vec![],
        };
        ann.signature = owner.sign(&ann.signable_payload());
        ann
    }

    #[test]
    fn merge_rejects_bad_signatures_and_stale_updates() {
        let owner = gt_core::identity::Identity::generate();
        let other = gt_core::identity::Identity::generate();
        let mut dir = RemoteCacheDir::default();
        let now = Instant::now();
        let mut bad = signed_ann(&owner, "k", 1, 100);
        bad.size = 999;
        assert!(!dir.merge_announcement(&bad, now));
        assert!(dir.is_empty());
        let mut wrong_signer = signed_ann(&owner, "k", 1, 100);
        wrong_signer.owner = other.peer_id();
        assert!(!dir.merge_announcement(&wrong_signer, now));
        assert!(dir.merge_announcement(&signed_ann(&owner, "k", 1, 100), now));
        assert!(dir.merge_announcement(&signed_ann(&owner, "k", 2, 90), now));
        let entry = dir.get_exact("k").unwrap();
        assert_eq!(entry.blob_id, BlobId([1u8; 32]));
        assert!(dir.merge_announcement(&signed_ann(&owner, "k", 3, 110), now));
        let entry = dir.get_exact("k").unwrap();
        assert_eq!(entry.blob_id, BlobId([3u8; 32]));
        assert_eq!(dir.len(), 1);
    }

    #[test]
    fn exact_prefers_newest_and_seeders_sort_newest_first() {
        let a = gt_core::identity::Identity::generate();
        let b = gt_core::identity::Identity::generate();
        let mut dir = RemoteCacheDir::default();
        let now = Instant::now();
        assert!(dir.merge_announcement(&signed_ann(&a, "k", 1, 100), now));
        assert!(dir.merge_announcement(&signed_ann(&b, "k", 2, 200), now));
        assert_eq!(dir.get_exact("k").unwrap().seeder, b.peer_id());
        let seeders = dir.seeders_for("k");
        assert_eq!(seeders.len(), 2);
        assert_eq!(seeders[0].seeder, b.peer_id());
        assert_eq!(seeders[1].seeder, a.peer_id());
        assert!(dir.get_exact("missing").is_none());
        assert!(dir.seeders_for("missing").is_empty());
    }

    #[test]
    fn prefix_match_prefers_longest_key_then_newest() {
        let a = gt_core::identity::Identity::generate();
        let mut dir = RemoteCacheDir::default();
        let now = Instant::now();
        assert!(dir.merge_announcement(&signed_ann(&a, "linux-cargo-aaa", 1, 300), now));
        assert!(dir.merge_announcement(&signed_ann(&a, "linux-cargo-aaa-bbb", 2, 100), now));
        assert_eq!(
            dir.best_with_prefix("linux-").unwrap().key,
            "linux-cargo-aaa-bbb"
        );
        assert!(dir.merge_announcement(&signed_ann(&a, "linux-cargo-aaa-ccc", 3, 200), now));
        assert_eq!(
            dir.best_with_prefix("linux-").unwrap().key,
            "linux-cargo-aaa-ccc"
        );
        assert!(dir.best_with_prefix("macos-").is_none());
    }

    #[test]
    fn ties_break_deterministically() {
        let a = gt_core::identity::Identity::generate();
        let b = gt_core::identity::Identity::generate();
        let mut dir = RemoteCacheDir::default();
        let now = Instant::now();
        assert!(dir.merge_announcement(&signed_ann(&a, "same-len-aa", 1, 100), now));
        assert!(dir.merge_announcement(&signed_ann(&a, "same-len-bb", 2, 100), now));
        assert_eq!(dir.best_with_prefix("same-").unwrap().key, "same-len-bb");
        assert!(dir.merge_announcement(&signed_ann(&b, "tied", 3, 50), now));
        assert!(dir.merge_announcement(&signed_ann(&a, "tied", 4, 50), now));
        let first = dir.get_exact("tied").unwrap().seeder.clone();
        let second = dir.get_exact("tied").unwrap().seeder.clone();
        assert_eq!(first, second);
        let seeders = dir.seeders_for("tied");
        assert_eq!(seeders.len(), 2);
        assert!(seeders[0].seeder.0 <= seeders[1].seeder.0);
    }

    #[test]
    fn expired_entries_are_pruned() {
        let a = gt_core::identity::Identity::generate();
        let mut dir = RemoteCacheDir::default();
        let base = Instant::now();
        assert!(dir.merge_announcement(&signed_ann(&a, "fresh", 1, 100), base));
        assert!(dir.merge_announcement(&signed_ann(&a, "stale", 2, 100), base));
        let later = base + REMOTE_CACHE_TTL + Duration::from_secs(1);
        assert!(dir.merge_announcement(&signed_ann(&a, "fresh", 1, 100), later));
        dir.prune_expired_at(later);
        assert!(dir.get_exact("fresh").is_some());
        assert!(dir.get_exact("stale").is_none());
        dir.prune_expired_at(later + REMOTE_CACHE_TTL + Duration::from_secs(1));
        assert!(dir.is_empty());
        assert_eq!(dir.len(), 0);
    }
}
