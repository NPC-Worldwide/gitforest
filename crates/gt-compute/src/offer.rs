use chrono::{DateTime, Utc};
use gt_core::identity::{verify_signature, Identity, PeerId};
use gt_core::protocol::ComputeOfferMessage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::str::FromStr;

use crate::payment::ChainType;

/// A compute offer advertised by a worker on the DHT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOffer {
    pub worker_id: PeerId,
    pub cpu_cores: u32,
    pub ram_mb: u64,
    pub disk_mb: u64,
    pub has_gpu: bool,
    pub hourly_rate_sats: u64,
    pub accepted_chains: Vec<ChainType>,
    pub wallet_addresses: HashMap<String, String>,
    pub max_concurrent_jobs: u32,
    pub current_load: u32,
    pub available: bool,
    pub completed_jobs: u64,
    pub expires_at: DateTime<Utc>,
    pub signature: Vec<u8>,
}

impl ComputeOffer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: &Identity,
        cpu_cores: u32,
        ram_mb: u64,
        disk_mb: u64,
        has_gpu: bool,
        hourly_rate_sats: u64,
        accepted_chains: Vec<ChainType>,
        wallet_addresses: HashMap<String, String>,
        max_concurrent_jobs: u32,
    ) -> Self {
        let expires_at = Utc::now() + chrono::Duration::hours(1);

        let mut offer = ComputeOffer {
            worker_id: identity.peer_id(),
            cpu_cores,
            ram_mb,
            disk_mb,
            has_gpu,
            hourly_rate_sats,
            accepted_chains,
            wallet_addresses,
            max_concurrent_jobs,
            current_load: 0,
            available: true,
            completed_jobs: 0,
            expires_at,
            signature: vec![],
        };

        let payload = offer.signable_payload();
        offer.signature = identity.sign(&payload);
        offer
    }

    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:compute-offer:");
        hasher.update(self.worker_id.0);
        hasher.update(self.cpu_cores.to_le_bytes());
        hasher.update(self.ram_mb.to_le_bytes());
        hasher.update(self.disk_mb.to_le_bytes());
        hasher.update(self.hourly_rate_sats.to_le_bytes());
        hasher.update(self.expires_at.timestamp().to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.worker_id, &self.signable_payload(), &self.signature).is_ok()
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// Check if this offer meets the given requirements.
    pub fn meets_requirements(&self, req: &crate::job::ComputeRequirements) -> bool {
        self.available
            && !self.is_expired()
            && self.cpu_cores >= req.min_cpu_cores
            && self.ram_mb >= req.min_ram_mb
            && self.disk_mb >= req.min_disk_mb
            && (!req.gpu_required || self.has_gpu)
            && self.current_load < self.max_concurrent_jobs
    }

    /// Compute the DHT address for this offer.
    pub fn dht_address(&self) -> [u8; 32] {
        compute_offer_address(&self.worker_id)
    }

    pub fn from_message(msg: &ComputeOfferMessage) -> Option<Self> {
        Self::from_message_with_bits(msg, gt_core::identity::MIN_POW_BITS)
    }

    pub fn from_message_with_bits(msg: &ComputeOfferMessage, min_bits: u8) -> Option<Self> {
        if !msg.verify_with_bits(min_bits) {
            return None;
        }
        let accepted_chains: Vec<ChainType> = msg
            .accepted_chains
            .iter()
            .filter_map(|c| ChainType::from_str(c).ok())
            .collect();
        Some(ComputeOffer {
            worker_id: msg.worker_id.clone(),
            cpu_cores: msg.cpu_cores,
            ram_mb: msg.ram_mb,
            disk_mb: msg.disk_mb,
            has_gpu: msg.has_gpu,
            hourly_rate_sats: msg.hourly_rate_sats,
            accepted_chains,
            wallet_addresses: HashMap::new(),
            max_concurrent_jobs: msg.max_concurrent_jobs,
            current_load: 0,
            available: true,
            completed_jobs: 0,
            expires_at: Utc::now() + chrono::Duration::hours(1),
            signature: msg.signature.clone(),
        })
    }
}

/// Compute the DHT address for a worker's compute offer.
pub fn compute_offer_address(worker_id: &PeerId) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:compute:");
    hasher.update(worker_id.0);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::ComputeRequirements;

    #[test]
    fn test_create_and_verify_offer() {
        let worker = Identity::generate();
        let offer = ComputeOffer::new(
            &worker,
            4,
            8192,
            50000,
            false,
            1000,
            vec![ChainType::BitcoinLightning],
            HashMap::from([("lightning".into(), "lnbc1...".into())]),
            2,
        );

        assert!(offer.verify());
        assert!(offer.available);
        assert!(!offer.is_expired());
    }

    #[test]
    fn test_tampered_offer_fails() {
        let worker = Identity::generate();
        let mut offer = ComputeOffer::new(
            &worker,
            4,
            8192,
            50000,
            false,
            1000,
            vec![ChainType::BitcoinLightning],
            HashMap::new(),
            2,
        );

        offer.hourly_rate_sats = 1; // tamper with price
        assert!(!offer.verify());
    }

    #[test]
    fn test_meets_requirements() {
        let worker = Identity::generate();
        let offer = ComputeOffer::new(
            &worker,
            4,
            8192,
            50000,
            false,
            1000,
            vec![ChainType::BitcoinLightning],
            HashMap::new(),
            2,
        );

        let basic_req = ComputeRequirements {
            min_cpu_cores: 2,
            min_ram_mb: 4096,
            min_disk_mb: 10000,
            gpu_required: false,
            network_access: false,
        };
        assert!(offer.meets_requirements(&basic_req));

        let gpu_req = ComputeRequirements {
            min_cpu_cores: 1,
            min_ram_mb: 1024,
            min_disk_mb: 1024,
            gpu_required: true,
            network_access: false,
        };
        assert!(!offer.meets_requirements(&gpu_req)); // no GPU

        let too_much_cpu = ComputeRequirements {
            min_cpu_cores: 8,
            min_ram_mb: 1024,
            min_disk_mb: 1024,
            gpu_required: false,
            network_access: false,
        };
        assert!(!offer.meets_requirements(&too_much_cpu)); // not enough CPU
    }

    #[test]
    fn test_dht_address_deterministic() {
        let worker = Identity::generate();
        let offer = ComputeOffer::new(
            &worker,
            1,
            1024,
            1024,
            false,
            100,
            vec![],
            HashMap::new(),
            1,
        );

        let addr1 = offer.dht_address();
        let addr2 = compute_offer_address(&worker.peer_id());
        assert_eq!(addr1, addr2);
    }

    fn signed_offer(worker: &Identity, signer: &Identity) -> ComputeOfferMessage {
        let mut msg = ComputeOfferMessage {
            worker_id: worker.peer_id(),
            cpu_cores: 4,
            ram_mb: 8192,
            disk_mb: 50000,
            has_gpu: false,
            hourly_rate_sats: 1000,
            accepted_chains: vec!["lightning".into(), "nope-chain".into()],
            max_concurrent_jobs: 2,
            pow_nonce: gt_core::identity::mine_pow(
                &worker.peer_id(),
                gt_core::identity::MIN_POW_BITS,
            ),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = signer.sign(&payload);
        msg
    }

    #[test]
    fn test_from_message_roundtrip() {
        let worker = Identity::generate();
        let msg = signed_offer(&worker, &worker);
        let offer = ComputeOffer::from_message(&msg).expect("valid message converts");
        assert_eq!(offer.worker_id, worker.peer_id());
        assert_eq!(offer.cpu_cores, 4);
        assert_eq!(offer.ram_mb, 8192);
        assert_eq!(offer.hourly_rate_sats, 1000);
        assert_eq!(offer.accepted_chains, vec![ChainType::BitcoinLightning]);
        assert!(offer.available);
        assert!(!offer.is_expired());
    }

    #[test]
    fn test_from_message_rejects_bad_signature() {
        let worker = Identity::generate();
        let other = Identity::generate();
        let msg = signed_offer(&worker, &other);
        assert!(ComputeOffer::from_message(&msg).is_none());
    }

    #[test]
    fn test_from_message_rejects_weak_pow() {
        let worker = Identity::generate();
        let mut msg = signed_offer(&worker, &worker);
        msg.pow_nonce = 0;
        if gt_core::identity::verify_pow(&worker.peer_id(), 0, gt_core::identity::MIN_POW_BITS) {
            msg.pow_nonce = 1;
        }
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        assert!(ComputeOffer::from_message(&msg).is_none());
    }

    #[test]
    fn test_from_message_rejects_tampered_fields() {
        let worker = Identity::generate();
        let mut msg = signed_offer(&worker, &worker);
        msg.hourly_rate_sats = 1;
        assert!(ComputeOffer::from_message(&msg).is_none());
    }

    #[test]
    fn test_from_message_with_bits_levels() {
        use gt_core::identity::{mine_pow, verify_pow, MIN_POW_BITS};
        let worker = Identity::generate();
        let mut msg = signed_offer(&worker, &worker);
        if verify_pow(&worker.peer_id(), msg.pow_nonce, MIN_POW_BITS + 1) {
            msg.pow_nonce = (0u64..)
                .find(|n| {
                    verify_pow(&worker.peer_id(), *n, MIN_POW_BITS)
                        && !verify_pow(&worker.peer_id(), *n, MIN_POW_BITS + 1)
                })
                .unwrap();
            let payload = msg.signable_payload();
            msg.signature = worker.sign(&payload);
        }
        assert!(ComputeOffer::from_message_with_bits(&msg, MIN_POW_BITS).is_some());
        assert!(ComputeOffer::from_message_with_bits(&msg, MIN_POW_BITS + 1).is_none());
        assert!(ComputeOffer::from_message_with_bits(&msg, 0).is_some());

        let mut strong = signed_offer(&worker, &worker);
        strong.pow_nonce = mine_pow(&worker.peer_id(), MIN_POW_BITS + 1);
        let payload = strong.signable_payload();
        strong.signature = worker.sign(&payload);
        assert!(ComputeOffer::from_message_with_bits(&strong, MIN_POW_BITS).is_some());
        assert!(ComputeOffer::from_message_with_bits(&strong, MIN_POW_BITS + 1).is_some());

        let mut weak = signed_offer(&worker, &worker);
        weak.pow_nonce = 0;
        if verify_pow(&worker.peer_id(), 0, MIN_POW_BITS) {
            weak.pow_nonce = 1;
        }
        let payload = weak.signable_payload();
        weak.signature = worker.sign(&payload);
        assert!(ComputeOffer::from_message_with_bits(&weak, 0).is_none());
    }
}
