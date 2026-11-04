use chrono::{DateTime, Utc};
use gt_core::identity::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::offer::ComputeOffer;

pub const MIN_MATCH_SCORE: f64 = 0.15;
pub const MIN_SUBMITTER_SCORE: f64 = 0.2;

/// Tracks known workers and their reputation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerRegistry {
    workers: HashMap<PeerId, WorkerInfo>,
}

fn default_pow_bits() -> u8 {
    gt_core::identity::MIN_POW_BITS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub offer: ComputeOffer,
    pub reputation: ReputationScore,
    pub last_seen: DateTime<Utc>,
    /// Measured PoW strength of the accepted offer. Entries written
    /// before this field existed load as the genesis floor — the only
    /// strength their acceptance proves.
    #[serde(default = "default_pow_bits")]
    pub pow_bits: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReputationScore {
    pub total_jobs: u64,
    pub successful_jobs: u64,
    pub failed_jobs: u64,
    pub disputed_jobs: u64,
    #[serde(default)]
    pub ghosted_jobs: u64,
    pub avg_execution_time_ratio: f64,
    pub score: f64,
}

impl Default for ReputationScore {
    fn default() -> Self {
        ReputationScore {
            total_jobs: 0,
            successful_jobs: 0,
            failed_jobs: 0,
            disputed_jobs: 0,
            ghosted_jobs: 0,
            avg_execution_time_ratio: 1.0,
            score: 0.5, // neutral starting score
        }
    }
}

impl ReputationScore {
    /// Recalculate the score based on history.
    pub fn recalculate(&mut self) {
        if self.total_jobs == 0 {
            self.score = 0.5;
            return;
        }

        let success_rate = self.successful_jobs as f64 / self.total_jobs as f64;
        let dispute_penalty = self.disputed_jobs as f64 / self.total_jobs as f64;
        let ghost_penalty = self.ghosted_jobs as f64 / self.total_jobs as f64;

        // Score: weighted combination of success rate and dispute history
        // Higher is better, capped at 1.0
        self.score = (success_rate * 0.8 - dispute_penalty * 0.5 - ghost_penalty * 0.4 + 0.2)
            .clamp(0.0, 1.0);
    }

    pub fn record_success(&mut self) {
        self.total_jobs += 1;
        self.successful_jobs += 1;
        self.recalculate();
    }

    pub fn record_failure(&mut self) {
        self.total_jobs += 1;
        self.failed_jobs += 1;
        self.recalculate();
    }

    pub fn record_dispute(&mut self) {
        self.total_jobs += 1;
        self.disputed_jobs += 1;
        self.recalculate();
    }

    pub fn record_ghost(&mut self) {
        self.total_jobs += 1;
        self.ghosted_jobs += 1;
        self.recalculate();
    }
}

/// Tracks submitter validity: do their jobs run, or are they junk?
/// Workers use this to shed load from griefers and broken clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmitterRegistry {
    submitters: HashMap<PeerId, SubmitterScore>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitterScore {
    pub submitted: u64,
    pub runnable: u64,
    pub junk: u64,
    pub score: f64,
}

impl Default for SubmitterScore {
    fn default() -> Self {
        SubmitterScore {
            submitted: 0,
            runnable: 0,
            junk: 0,
            score: 0.5,
        }
    }
}

impl SubmitterScore {
    pub fn recalculate(&mut self) {
        let resolved = self.runnable + self.junk;
        if resolved == 0 {
            self.score = 0.5;
            return;
        }
        let runnable_rate = self.runnable as f64 / resolved as f64;
        let junk_rate = self.junk as f64 / resolved as f64;
        self.score = (runnable_rate * 0.9 - junk_rate * 0.6 + 0.15).clamp(0.0, 1.0);
    }

    pub fn record_submitted(&mut self) {
        self.submitted += 1;
    }

    pub fn record_runnable(&mut self) {
        self.runnable += 1;
        self.recalculate();
    }

    pub fn record_junk(&mut self) {
        self.junk += 1;
        self.recalculate();
    }
}

impl SubmitterRegistry {
    pub fn new() -> Self {
        SubmitterRegistry {
            submitters: HashMap::new(),
        }
    }

    pub fn score_for(&self, submitter: &PeerId) -> SubmitterScore {
        self.submitters.get(submitter).cloned().unwrap_or_default()
    }

    pub fn record_submitted(&mut self, submitter: &PeerId) {
        self.submitters
            .entry(submitter.clone())
            .or_default()
            .record_submitted();
    }

    pub fn record_runnable(&mut self, submitter: &PeerId) {
        self.submitters
            .entry(submitter.clone())
            .or_default()
            .record_runnable();
    }

    pub fn record_junk(&mut self, submitter: &PeerId) {
        self.submitters
            .entry(submitter.clone())
            .or_default()
            .record_junk();
    }

    pub fn is_allowed(&self, submitter: &PeerId) -> bool {
        self.score_for(submitter).score >= MIN_SUBMITTER_SCORE
    }

    pub fn len(&self) -> usize {
        self.submitters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.submitters.is_empty()
    }

    pub fn entries(&self) -> Vec<(&PeerId, &SubmitterScore)> {
        self.submitters.iter().collect()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }

    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

impl WorkerRegistry {
    pub fn new() -> Self {
        WorkerRegistry {
            workers: HashMap::new(),
        }
    }

    fn reputation_mut(&mut self, worker: &PeerId) -> Option<&mut ReputationScore> {
        self.workers.get_mut(worker).map(|w| &mut w.reputation)
    }

    pub fn record_success(&mut self, worker: &PeerId) {
        if let Some(rep) = self.reputation_mut(worker) {
            rep.record_success();
        }
    }

    pub fn record_failure(&mut self, worker: &PeerId) {
        if let Some(rep) = self.reputation_mut(worker) {
            rep.record_failure();
        }
    }

    pub fn record_dispute(&mut self, worker: &PeerId) {
        if let Some(rep) = self.reputation_mut(worker) {
            rep.record_dispute();
        }
    }

    pub fn record_ghost(&mut self, worker: &PeerId) {
        if let Some(rep) = self.reputation_mut(worker) {
            rep.record_ghost();
        }
    }

    /// Register or update a worker's offer.
    pub fn register(&mut self, offer: ComputeOffer) {
        self.register_with_proof(offer, default_pow_bits());
    }

    /// Register or update a worker's offer, stamping the measured PoW
    /// strength of the accepted proof. Intake paths must use this so a
    /// later difficulty raise can evict entries that no longer qualify.
    pub fn register_with_proof(&mut self, offer: ComputeOffer, pow_bits: u8) {
        let peer_id = offer.worker_id.clone();
        let entry = self.workers.entry(peer_id).or_insert_with(|| WorkerInfo {
            offer: offer.clone(),
            reputation: ReputationScore::default(),
            last_seen: Utc::now(),
            pow_bits,
        });
        entry.offer = offer;
        entry.last_seen = Utc::now();
        entry.pow_bits = pow_bits;
    }

    /// Drop entries whose stamped proof is weaker than `min_bits`
    /// (floored at genesis difficulty). Applied to persisted entries on
    /// startup so raising the minimum takes effect immediately instead
    /// of lingering until offer expiry.
    pub fn retain_pow(&mut self, min_bits: u8) {
        let min_bits = min_bits.max(gt_core::identity::MIN_POW_BITS);
        self.workers.retain(|_, w| w.pow_bits >= min_bits);
    }

    /// Get a worker's info.
    pub fn get(&self, peer_id: &PeerId) -> Option<&WorkerInfo> {
        self.workers.get(peer_id)
    }

    /// Get mutable worker info.
    pub fn get_mut(&mut self, peer_id: &PeerId) -> Option<&mut WorkerInfo> {
        self.workers.get_mut(peer_id)
    }

    /// List all available workers.
    pub fn available(&self) -> Vec<&WorkerInfo> {
        self.workers
            .values()
            .filter(|w| w.offer.available && !w.offer.is_expired())
            .collect()
    }

    /// Find workers matching requirements, sorted by reputation (best first).
    /// Workers below the reputation floor are excluded.
    pub fn find_matching(
        &self,
        requirements: &crate::job::ComputeRequirements,
    ) -> Vec<&WorkerInfo> {
        let mut matching: Vec<&WorkerInfo> = self
            .workers
            .values()
            .filter(|w| {
                w.offer.meets_requirements(requirements) && w.reputation.score >= MIN_MATCH_SCORE
            })
            .collect();
        matching.sort_by(|a, b| b.reputation.score.partial_cmp(&a.reputation.score).unwrap());
        matching
    }

    /// Remove expired workers, keeping reputation for workers with history.
    pub fn prune_expired(&mut self) {
        self.workers
            .retain(|_, w| !w.offer.is_expired() || w.reputation.total_jobs > 0);
    }

    /// Total number of known workers.
    pub fn count(&self) -> usize {
        self.workers.len()
    }

    pub fn entries(&self) -> Vec<(&PeerId, &WorkerInfo)> {
        self.workers.iter().collect()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }

    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payment::ChainType;
    use gt_core::identity::Identity;

    #[test]
    fn test_reputation_scoring() {
        let mut rep = ReputationScore::default();
        assert_eq!(rep.score, 0.5);

        rep.record_success();
        rep.record_success();
        rep.record_success();
        assert!(rep.score > 0.8);

        rep.record_dispute();
        assert!(rep.score < 0.8); // penalty
    }

    #[test]
    fn test_worker_registry() {
        let w1 = Identity::generate();
        let w2 = Identity::generate();

        let mut registry = WorkerRegistry::new();

        let offer1 = ComputeOffer::new(
            &w1,
            4,
            8192,
            50000,
            false,
            1000,
            vec![ChainType::BitcoinLightning],
            HashMap::new(),
            2,
        );
        let offer2 = ComputeOffer::new(
            &w2,
            8,
            16384,
            100000,
            true,
            2000,
            vec![ChainType::BitcoinLightning],
            HashMap::new(),
            4,
        );

        registry.register(offer1);
        registry.register(offer2);

        assert_eq!(registry.count(), 2);
        assert_eq!(registry.available().len(), 2);

        // Find matching for basic requirements
        let req = crate::job::ComputeRequirements {
            min_cpu_cores: 2,
            min_ram_mb: 4096,
            min_disk_mb: 10000,
            gpu_required: false,
            network_access: false,
        };
        let matching = registry.find_matching(&req);
        assert_eq!(matching.len(), 2);

        // GPU requirement filters
        let gpu_req = crate::job::ComputeRequirements {
            min_cpu_cores: 1,
            min_ram_mb: 1024,
            min_disk_mb: 1024,
            gpu_required: true,
            network_access: false,
        };
        let gpu_matching = registry.find_matching(&gpu_req);
        assert_eq!(gpu_matching.len(), 1); // only w2 has GPU
    }

    #[test]
    fn test_ghost_penalty_and_match_floor() {
        let w = Identity::generate();
        let mut registry = WorkerRegistry::new();
        registry.register(ComputeOffer::new(
            &w,
            4,
            8192,
            50000,
            false,
            1000,
            vec![ChainType::BitcoinLightning],
            HashMap::new(),
            2,
        ));
        let req = crate::job::ComputeRequirements {
            min_cpu_cores: 1,
            min_ram_mb: 1024,
            min_disk_mb: 1024,
            gpu_required: false,
            network_access: false,
        };
        assert_eq!(registry.find_matching(&req).len(), 1);
        for _ in 0..6 {
            registry.record_ghost(&w.peer_id());
        }
        let rep = &registry.get(&w.peer_id()).unwrap().reputation;
        assert_eq!(rep.ghosted_jobs, 6);
        assert!(rep.score < MIN_MATCH_SCORE);
        assert!(registry.find_matching(&req).is_empty());
    }

    #[test]
    fn test_registry_save_load_roundtrip() {
        let w = Identity::generate();
        let mut registry = WorkerRegistry::new();
        registry.register(ComputeOffer::new(
            &w,
            4,
            8192,
            50000,
            false,
            1000,
            vec![ChainType::BitcoinLightning],
            HashMap::new(),
            2,
        ));
        registry.record_success(&w.peer_id());
        registry.record_dispute(&w.peer_id());
        let dir = std::env::temp_dir().join(format!("gf_repreg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("workers.json");
        registry.save(&path).unwrap();
        let loaded = WorkerRegistry::load(&path);
        let rep = &loaded.get(&w.peer_id()).unwrap().reputation;
        assert_eq!(rep.successful_jobs, 1);
        assert_eq!(rep.disputed_jobs, 1);
        assert_eq!(rep.total_jobs, 2);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(WorkerRegistry::load(&dir.join("missing.json")).count(), 0);
    }

    #[test]
    fn test_retain_pow_evicts_weak_and_keeps_strong() {
        use gt_core::identity::MIN_POW_BITS;
        let weak = Identity::generate();
        let strong = Identity::generate();
        let mut registry = WorkerRegistry::new();
        let mk = |w: &Identity| {
            ComputeOffer::new(
                w,
                4,
                8192,
                50000,
                false,
                1000,
                vec![ChainType::BitcoinLightning],
                HashMap::new(),
                2,
            )
        };
        registry.register_with_proof(mk(&weak), MIN_POW_BITS);
        registry.register_with_proof(mk(&strong), MIN_POW_BITS + 4);
        registry.retain_pow(MIN_POW_BITS + 1);
        assert!(registry.get(&weak.peer_id()).is_none());
        assert!(registry.get(&strong.peer_id()).is_some());
        registry.retain_pow(0);
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn test_legacy_registry_without_pow_bits_loads_at_floor() {
        use gt_core::identity::MIN_POW_BITS;
        let w = Identity::generate();
        let mut registry = WorkerRegistry::new();
        registry.register_with_proof(
            ComputeOffer::new(
                &w,
                4,
                8192,
                50000,
                false,
                1000,
                vec![ChainType::BitcoinLightning],
                HashMap::new(),
                2,
            ),
            MIN_POW_BITS + 3,
        );
        let dir = std::env::temp_dir().join(format!("gf_powreg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("workers.json");
        registry.save(&path).unwrap();
        assert_eq!(
            WorkerRegistry::load(&path)
                .get(&w.peer_id())
                .unwrap()
                .pow_bits,
            MIN_POW_BITS + 3
        );
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for worker in raw
            .get_mut("workers")
            .and_then(|v| v.as_object_mut())
            .unwrap()
            .values_mut()
        {
            worker.as_object_mut().unwrap().remove("pow_bits");
        }
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(
            WorkerRegistry::load(&path)
                .get(&w.peer_id())
                .unwrap()
                .pow_bits,
            MIN_POW_BITS
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_submitter_validity_floor() {
        let good = Identity::generate();
        let bad = Identity::generate();
        let mut registry = SubmitterRegistry::new();
        assert!(registry.is_allowed(&good.peer_id()));
        for _ in 0..3 {
            registry.record_submitted(&good.peer_id());
            registry.record_runnable(&good.peer_id());
        }
        assert!(registry.score_for(&good.peer_id()).score > 0.5);
        for _ in 0..4 {
            registry.record_submitted(&bad.peer_id());
            registry.record_junk(&bad.peer_id());
        }
        assert!(!registry.is_allowed(&bad.peer_id()));
        let dir = std::env::temp_dir().join(format!("gf_subreg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("submitters.json");
        registry.save(&path).unwrap();
        let loaded = SubmitterRegistry::load(&path);
        assert!(!loaded.is_allowed(&bad.peer_id()));
        assert!(loaded.is_allowed(&good.peer_id()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
