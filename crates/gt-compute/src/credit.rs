use gt_core::identity::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const DEFAULT_CREDIT_ALLOWANCE_SECS: u64 = 3600;

pub fn compute_units(execution_time_ms: u64) -> u64 {
    (execution_time_ms.div_ceil(1000)).max(1)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreditEntry {
    pub given_secs: u64,
    pub taken_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreditLedger {
    entries: HashMap<PeerId, CreditEntry>,
    paid: HashSet<(String, PeerId)>,
}

impl CreditLedger {
    pub fn new() -> Self {
        CreditLedger {
            entries: HashMap::new(),
            paid: HashSet::new(),
        }
    }

    pub fn record_given(&mut self, peer: &PeerId, secs: u64) {
        let entry = self.entries.entry(peer.clone()).or_default();
        entry.given_secs = entry.given_secs.saturating_add(secs);
    }

    pub fn record_taken(&mut self, peer: &PeerId, secs: u64) {
        let entry = self.entries.entry(peer.clone()).or_default();
        entry.taken_secs = entry.taken_secs.saturating_add(secs);
    }

    pub fn mark_paid(&mut self, job_id: &str, worker: &PeerId) {
        self.paid.insert((job_id.to_string(), worker.clone()));
    }

    pub fn paid_for(&self, job_id: &str, worker: &PeerId) -> bool {
        self.paid.contains(&(job_id.to_string(), worker.clone()))
    }

    pub fn entry(&self, peer: &PeerId) -> CreditEntry {
        self.entries.get(peer).cloned().unwrap_or_default()
    }

    pub fn peer_deficit(&self, peer: &PeerId) -> u64 {
        let e = self.entry(peer);
        e.given_secs.saturating_sub(e.taken_secs)
    }

    pub fn totals(&self) -> (u64, u64) {
        self.entries.values().fold((0u64, 0u64), |(g, t), e| {
            (
                g.saturating_add(e.given_secs),
                t.saturating_add(e.taken_secs),
            )
        })
    }

    pub fn own_deficit(&self) -> u64 {
        let (given, taken) = self.totals();
        taken.saturating_sub(given)
    }

    pub fn worker_should_serve(&self, peer: &PeerId, allowance_secs: u64) -> bool {
        self.peer_deficit(peer) <= allowance_secs
    }

    pub fn may_spend(&self, allowance_secs: u64) -> bool {
        self.own_deficit() <= allowance_secs
    }

    pub fn peers(&self) -> Vec<(PeerId, CreditEntry)> {
        self.entries
            .iter()
            .map(|(p, e)| (p.clone(), e.clone()))
            .collect()
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
    use gt_core::identity::Identity;

    #[test]
    fn test_compute_units_rounds_up_to_one_second() {
        assert_eq!(compute_units(0), 1);
        assert_eq!(compute_units(1), 1);
        assert_eq!(compute_units(1000), 1);
        assert_eq!(compute_units(1001), 2);
        assert_eq!(compute_units(60_000), 60);
    }

    #[test]
    fn test_deficit_and_allowance() {
        let mut ledger = CreditLedger::new();
        let peer = Identity::generate().peer_id();
        assert!(ledger.worker_should_serve(&peer, 100));
        assert!(ledger.may_spend(100));
        ledger.record_given(&peer, 90);
        assert!(ledger.worker_should_serve(&peer, 100));
        ledger.record_given(&peer, 20);
        assert_eq!(ledger.peer_deficit(&peer), 110);
        assert!(!ledger.worker_should_serve(&peer, 100));
        ledger.record_taken(&peer, 50);
        assert_eq!(ledger.peer_deficit(&peer), 60);
        assert!(ledger.worker_should_serve(&peer, 100));
    }

    #[test]
    fn test_own_deficit_nets_across_peers() {
        let mut ledger = CreditLedger::new();
        let a = Identity::generate().peer_id();
        let b = Identity::generate().peer_id();
        ledger.record_given(&a, 30);
        ledger.record_taken(&b, 100);
        assert_eq!(ledger.own_deficit(), 70);
        assert!(!ledger.may_spend(60));
        assert!(ledger.may_spend(70));
    }

    #[test]
    fn test_paid_marks_survive_save_load() {
        let mut ledger = CreditLedger::new();
        let peer = Identity::generate().peer_id();
        ledger.record_given(&peer, 10);
        ledger.mark_paid("job-1", &peer);
        let tmp = std::env::temp_dir().join(format!("gf_credit_{}.json", std::process::id()));
        ledger.save(&tmp).unwrap();
        let loaded = CreditLedger::load(&tmp);
        assert_eq!(loaded.peer_deficit(&peer), 10);
        assert!(loaded.paid_for("job-1", &peer));
        assert!(!loaded.paid_for("job-2", &peer));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_counters_saturate() {
        let mut ledger = CreditLedger::new();
        let peer = Identity::generate().peer_id();
        ledger.record_given(&peer, u64::MAX);
        ledger.record_given(&peer, 1);
        assert_eq!(ledger.peer_deficit(&peer), u64::MAX);
    }
}
