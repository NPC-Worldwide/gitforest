use chrono::{DateTime, Utc};
use gt_core::identity::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supported blockchain/payment networks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChainType {
    BitcoinLightning,
    EthereumL2(L2Network),
    Solana,
}

impl std::fmt::Display for ChainType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainType::BitcoinLightning => write!(f, "lightning"),
            ChainType::EthereumL2(net) => write!(f, "eth-{}", net),
            ChainType::Solana => write!(f, "solana"),
        }
    }
}

impl std::str::FromStr for ChainType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "lightning" | "btc-lightning" => Ok(ChainType::BitcoinLightning),
            "eth-arbitrum" | "arbitrum" => Ok(ChainType::EthereumL2(L2Network::Arbitrum)),
            "eth-optimism" | "optimism" => Ok(ChainType::EthereumL2(L2Network::Optimism)),
            "eth-base" | "base" => Ok(ChainType::EthereumL2(L2Network::Base)),
            "eth-polygon" | "polygon" => Ok(ChainType::EthereumL2(L2Network::Polygon)),
            "solana" | "sol" => Ok(ChainType::Solana),
            _ => Err(format!("unknown chain: {s}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum L2Network {
    Arbitrum,
    Optimism,
    Base,
    Polygon,
}

impl std::fmt::Display for L2Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            L2Network::Arbitrum => write!(f, "arbitrum"),
            L2Network::Optimism => write!(f, "optimism"),
            L2Network::Base => write!(f, "base"),
            L2Network::Polygon => write!(f, "polygon"),
        }
    }
}

/// A payment amount on a specific chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentAmount {
    pub chain: ChainType,
    pub amount: u64,
    pub currency: String,
}

impl PaymentAmount {
    pub fn lightning(sats: u64) -> Self {
        PaymentAmount {
            chain: ChainType::BitcoinLightning,
            amount: sats,
            currency: "BTC".into(),
        }
    }

    pub fn eth_l2(network: L2Network, wei: u64) -> Self {
        PaymentAmount {
            chain: ChainType::EthereumL2(network),
            amount: wei,
            currency: "ETH".into(),
        }
    }
}

/// A payment channel between a job submitter and a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentChannel {
    pub id: String,
    pub job_id: String,
    pub payer: PeerId,
    pub payee: PeerId,
    pub chain: ChainType,
    pub escrow_amount: u64,
    pub released_amount: u64,
    pub status: PaymentStatus,
    pub payer_address: String,
    pub payee_address: String,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub escrow_ref: Option<String>,
    #[serde(default)]
    pub payment_details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PaymentStatus {
    Open,
    Funded,
    Released,
    Disputed,
    Refunded,
    Expired,
}

impl PaymentChannel {
    pub fn new(
        job_id: String,
        payer: PeerId,
        payee: PeerId,
        chain: ChainType,
        escrow_amount: u64,
        payer_address: String,
        payee_address: String,
    ) -> Self {
        PaymentChannel {
            id: uuid::Uuid::new_v4().to_string(),
            job_id,
            payer,
            payee,
            chain,
            escrow_amount,
            released_amount: 0,
            status: PaymentStatus::Open,
            payer_address,
            payee_address,
            created_at: Utc::now(),
            settled_at: None,
            escrow_ref: None,
            payment_details: None,
        }
    }

    pub fn fund(&mut self) {
        self.status = PaymentStatus::Funded;
    }

    pub fn release(&mut self) {
        self.released_amount = self.escrow_amount;
        self.status = PaymentStatus::Released;
        self.settled_at = Some(Utc::now());
    }

    pub fn dispute(&mut self) {
        self.status = PaymentStatus::Disputed;
    }

    pub fn refund(&mut self) {
        self.status = PaymentStatus::Refunded;
        self.settled_at = Some(Utc::now());
    }
}

#[derive(Debug, Clone)]
pub struct EscrowInfo {
    pub escrow_id: String,
    pub payment_details: String,
}

/// Trait for chain-specific payment operations.
/// Implementations will be added incrementally for each chain.
pub trait PaymentBackend: Send + Sync {
    fn create_escrow(&self, amount: u64, payer: &str, payee: &str) -> anyhow::Result<EscrowInfo>;
    fn release_escrow(&self, channel_id: &str) -> anyhow::Result<()>;
    fn refund_escrow(&self, channel_id: &str) -> anyhow::Result<()>;
    fn check_balance(&self, address: &str) -> anyhow::Result<u64>;
    fn chain_type(&self) -> ChainType;
    fn escrow_funded(&self, escrow_id: &str) -> anyhow::Result<Option<bool>> {
        let _ = escrow_id;
        Ok(None)
    }
    fn pay_invoice(&self, payment_details: &str, amount: u64) -> anyhow::Result<()> {
        let _ = (payment_details, amount);
        anyhow::bail!(
            "direct invoice payment not supported on {}",
            self.chain_type()
        )
    }
}

/// Stub payment backend for development/testing.
pub struct StubPaymentBackend {
    pub chain: ChainType,
}

impl PaymentBackend for StubPaymentBackend {
    fn create_escrow(&self, amount: u64, _payer: &str, _payee: &str) -> anyhow::Result<EscrowInfo> {
        tracing::info!("STUB: Creating escrow for {amount} on {}", self.chain);
        Ok(EscrowInfo {
            escrow_id: uuid::Uuid::new_v4().to_string(),
            payment_details: "stub: no payment needed".into(),
        })
    }

    fn release_escrow(&self, channel_id: &str) -> anyhow::Result<()> {
        tracing::info!("STUB: Releasing escrow {channel_id} on {}", self.chain);
        Ok(())
    }

    fn refund_escrow(&self, channel_id: &str) -> anyhow::Result<()> {
        tracing::info!("STUB: Refunding escrow {channel_id} on {}", self.chain);
        Ok(())
    }

    fn check_balance(&self, address: &str) -> anyhow::Result<u64> {
        tracing::info!("STUB: Checking balance for {address} on {}", self.chain);
        Ok(100_000_000) // 1 BTC / 1 ETH in smallest units
    }

    fn chain_type(&self) -> ChainType {
        self.chain.clone()
    }
}

/// Manages payment channels across multiple chains.
pub struct PaymentManager {
    channels: HashMap<String, PaymentChannel>,
    backends: HashMap<String, std::sync::Arc<dyn PaymentBackend>>,
}

impl Default for PaymentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PaymentManager {
    pub fn new() -> Self {
        PaymentManager {
            channels: HashMap::new(),
            backends: HashMap::new(),
        }
    }

    pub fn register_backend(&mut self, backend: Box<dyn PaymentBackend>) {
        let key = backend.chain_type().to_string();
        self.backends.insert(key, std::sync::Arc::from(backend));
    }

    pub fn has_backend(&self, chain: &ChainType) -> bool {
        self.backends.contains_key(&chain.to_string())
    }

    pub fn backend_for(&self, chain: &ChainType) -> Option<std::sync::Arc<dyn PaymentBackend>> {
        self.backends.get(&chain.to_string()).cloned()
    }

    pub fn open_channel(&mut self, channel: PaymentChannel) -> anyhow::Result<String> {
        let chain_key = channel.chain.to_string();
        let backend = self
            .backends
            .get(&chain_key)
            .ok_or_else(|| anyhow::anyhow!("no payment backend for chain: {chain_key}"))?;

        let info = backend.create_escrow(
            channel.escrow_amount,
            &channel.payer_address,
            &channel.payee_address,
        )?;

        let mut channel = channel;
        channel.escrow_ref = Some(info.escrow_id);
        channel.payment_details = Some(info.payment_details);
        channel.fund();
        let id = channel.id.clone();
        self.channels.insert(id.clone(), channel);
        Ok(id)
    }

    pub fn insert_funded(&mut self, channel: PaymentChannel, info: EscrowInfo) -> String {
        let mut channel = channel;
        channel.escrow_ref = Some(info.escrow_id);
        channel.payment_details = Some(info.payment_details);
        channel.fund();
        let id = channel.id.clone();
        self.channels.insert(id.clone(), channel);
        id
    }

    pub fn job_channels(&self, job_id: &str) -> Vec<PaymentChannel> {
        self.channels
            .values()
            .filter(|c| c.job_id == job_id)
            .cloned()
            .collect()
    }

    pub fn settle_info(&self, job_id: &str) -> Option<(String, ChainType, String)> {
        self.for_job(job_id).and_then(|c| {
            let escrow = c.escrow_ref.clone()?;
            if matches!(c.status, PaymentStatus::Released | PaymentStatus::Refunded) {
                return None;
            }
            Some((c.id.clone(), c.chain.clone(), escrow))
        })
    }

    pub fn mark_settled(&mut self, channel_id: &str, success: bool) {
        if let Some(channel) = self.channels.get_mut(channel_id) {
            if success {
                channel.release();
            } else {
                channel.refund();
            }
        }
    }

    pub fn release(&mut self, channel_id: &str) -> anyhow::Result<()> {
        let channel = self
            .channels
            .get_mut(channel_id)
            .ok_or_else(|| anyhow::anyhow!("channel not found: {channel_id}"))?;

        let chain_key = channel.chain.to_string();
        let escrow_ref = channel
            .escrow_ref
            .clone()
            .unwrap_or_else(|| channel_id.to_string());
        if let Some(backend) = self.backends.get(&chain_key) {
            backend.release_escrow(&escrow_ref)?;
        }

        channel.release();
        Ok(())
    }

    pub fn refund(&mut self, channel_id: &str) -> anyhow::Result<()> {
        let channel = self
            .channels
            .get_mut(channel_id)
            .ok_or_else(|| anyhow::anyhow!("channel not found: {channel_id}"))?;

        let chain_key = channel.chain.to_string();
        let escrow_ref = channel
            .escrow_ref
            .clone()
            .unwrap_or_else(|| channel_id.to_string());
        if let Some(backend) = self.backends.get(&chain_key) {
            backend.refund_escrow(&escrow_ref)?;
        }

        channel.refund();
        Ok(())
    }

    pub fn get_channel(&self, id: &str) -> Option<&PaymentChannel> {
        self.channels.get(id)
    }

    pub fn list_channels(&self) -> Vec<&PaymentChannel> {
        self.channels.values().collect()
    }

    pub fn for_job(&self, job_id: &str) -> Option<&PaymentChannel> {
        self.channels.values().find(|c| c.job_id == job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    #[test]
    fn test_chain_type_roundtrip() {
        let chains = vec![
            ("lightning", ChainType::BitcoinLightning),
            ("arbitrum", ChainType::EthereumL2(L2Network::Arbitrum)),
            ("base", ChainType::EthereumL2(L2Network::Base)),
            ("solana", ChainType::Solana),
        ];

        for (s, expected) in chains {
            let parsed: ChainType = s.parse().unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn test_payment_channel_lifecycle() {
        let payer = Identity::generate();
        let payee = Identity::generate();

        let mut channel = PaymentChannel::new(
            "job-123".into(),
            payer.peer_id(),
            payee.peer_id(),
            ChainType::BitcoinLightning,
            50000,
            "lnbc1payer...".into(),
            "lnbc1payee...".into(),
        );

        assert_eq!(channel.status, PaymentStatus::Open);

        channel.fund();
        assert_eq!(channel.status, PaymentStatus::Funded);

        channel.release();
        assert_eq!(channel.status, PaymentStatus::Released);
        assert_eq!(channel.released_amount, 50000);
        assert!(channel.settled_at.is_some());
    }

    #[test]
    fn test_payment_manager_with_stub() {
        let payer = Identity::generate();
        let payee = Identity::generate();

        let mut manager = PaymentManager::new();
        manager.register_backend(Box::new(StubPaymentBackend {
            chain: ChainType::BitcoinLightning,
        }));

        let channel = PaymentChannel::new(
            "job-456".into(),
            payer.peer_id(),
            payee.peer_id(),
            ChainType::BitcoinLightning,
            25000,
            "payer-addr".into(),
            "payee-addr".into(),
        );

        let id = manager.open_channel(channel).unwrap();
        assert!(manager.get_channel(&id).is_some());
        assert!(manager.for_job("job-456").is_some());

        manager.release(&id).unwrap();
        let ch = manager.get_channel(&id).unwrap();
        assert_eq!(ch.status, PaymentStatus::Released);
    }
}
