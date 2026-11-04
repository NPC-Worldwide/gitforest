use crate::payment::{ChainType, EscrowInfo, L2Network, PaymentBackend};
use gt_core::sanitize::sanitize_log;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_GAS_LIMIT: u64 = 21_000;
const RECEIPT_MAX_ATTEMPTS: u32 = 45;
const RECEIPT_POLL_MS: u64 = 2_000;

pub fn chain_id_for(network: &L2Network) -> u64 {
    match network {
        L2Network::Arbitrum => 42161,
        L2Network::Optimism => 10,
        L2Network::Base => 8453,
        L2Network::Polygon => 137,
    }
}

pub struct EvmTransfer {
    pub to: String,
    pub value_wei: u64,
    pub nonce: u64,
    pub gas_price_wei: u64,
    pub gas_limit: u64,
    pub chain_id: u64,
}

pub trait EvmSigner: Send + Sync {
    fn sign_transfer(&self, tx: &EvmTransfer) -> anyhow::Result<Vec<u8>>;
    fn address(&self) -> String;
}

pub struct CommandSigner {
    command: String,
    from: String,
}

impl CommandSigner {
    pub fn new(command: String, from: String) -> Self {
        CommandSigner { command, from }
    }
}

impl EvmSigner for CommandSigner {
    fn sign_transfer(&self, tx: &EvmTransfer) -> anyhow::Result<Vec<u8>> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .env("TX_TO", &tx.to)
            .env("TX_VALUE_WEI", tx.value_wei.to_string())
            .env("TX_NONCE", tx.nonce.to_string())
            .env("TX_GAS_PRICE_WEI", tx.gas_price_wei.to_string())
            .env("TX_GAS_LIMIT", tx.gas_limit.to_string())
            .env("TX_CHAIN_ID", tx.chain_id.to_string())
            .output()?;
        if !output.status.success() {
            anyhow::bail!("signer command failed with {}", output.status);
        }
        let hex_out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let hex_out = hex_out.strip_prefix("0x").unwrap_or(&hex_out).to_string();
        Ok(hex::decode(&hex_out)?)
    }

    fn address(&self) -> String {
        self.from.clone()
    }
}

fn keccak256(data: &[u8]) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut hasher = Keccak::v256();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

fn address_of_secret(secret: &[u8; 32]) -> anyhow::Result<String> {
    crate::keystore::eth_address_of_secret(secret)
}

pub struct EmbeddedSigner {
    secret: zeroize::Zeroizing<[u8; 32]>,
    address: String,
}

impl EmbeddedSigner {
    pub fn from_keystore(path: &std::path::Path, password: &[u8]) -> anyhow::Result<Self> {
        let secret = crate::keystore::unlock_keystore(path, password)?;
        let address = address_of_secret(&secret)?;
        let filed = crate::keystore::keystore_address(path)?;
        if filed.to_lowercase() != address {
            anyhow::bail!("keystore address does not match its key");
        }
        Ok(EmbeddedSigner { secret, address })
    }

    fn sign_hash(&self, hash: &[u8; 32]) -> (u8, [u8; 64]) {
        let key = secp256k1::SecretKey::from_secret_bytes(*self.secret)
            .expect("secret validated at load");
        let msg = secp256k1::Message::from_digest(*hash);
        let (recid, compact) =
            secp256k1::ecdsa::RecoverableSignature::sign_ecdsa_recoverable(msg, &key)
                .serialize_compact();
        (u8::from(recid), compact)
    }
}

fn rlp_int_bytes(value: &[u8]) -> &[u8] {
    value
        .iter()
        .position(|b| *b != 0)
        .map(|i| &value[i..])
        .unwrap_or(&[])
}

pub fn build_signer(
    from_address: &str,
    signer_command: &str,
    keystore_path: Option<&std::path::Path>,
    key_password_file: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn EvmSigner>> {
    if let Some(path) = keystore_path {
        let password = read_key_password(key_password_file)?;
        let signer = EmbeddedSigner::from_keystore(path, &password)?;
        if signer.address() != from_address.to_lowercase() {
            anyhow::bail!(
                "keystore address {} does not match from_address {from_address}",
                signer.address()
            );
        }
        return Ok(Box::new(signer));
    }
    if signer_command.is_empty() {
        anyhow::bail!("no signer configured: set keystore_path or signer_command");
    }
    Ok(Box::new(CommandSigner::new(
        signer_command.to_string(),
        from_address.to_string(),
    )))
}

fn read_key_password(
    key_password_file: Option<&std::path::Path>,
) -> anyhow::Result<zeroize::Zeroizing<Vec<u8>>> {
    if let Some(path) = key_password_file {
        let mut raw = std::fs::read(path)?;
        while raw.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            raw.pop();
        }
        return Ok(zeroize::Zeroizing::new(raw));
    }
    std::env::var("GITFOREST_EVM_KEY_PASSWORD")
        .map(|s| zeroize::Zeroizing::new(s.into_bytes()))
        .map_err(|_| {
            anyhow::anyhow!("keystore needs key_password_file or GITFOREST_EVM_KEY_PASSWORD")
        })
}

impl EvmSigner for EmbeddedSigner {
    fn sign_transfer(&self, tx: &EvmTransfer) -> anyhow::Result<Vec<u8>> {
        let to_hex = tx.to.strip_prefix("0x").unwrap_or(&tx.to);
        let to = hex::decode(to_hex)?;
        if to.len() != 20 {
            anyhow::bail!("destination must be 20 bytes");
        }
        let mut signing = rlp::RlpStream::new_list(9);
        signing.append(&tx.nonce);
        signing.append(&tx.gas_price_wei);
        signing.append(&tx.gas_limit);
        signing.append(&to);
        signing.append(&tx.value_wei);
        signing.append_empty_data();
        signing.append(&tx.chain_id);
        signing.append_empty_data();
        signing.append_empty_data();
        let hash = keccak256(&signing.out());
        let (recid, compact) = self.sign_hash(&hash);
        let v = tx.chain_id * 2 + 35 + u64::from(recid);
        let mut raw = rlp::RlpStream::new_list(9);
        raw.append(&tx.nonce);
        raw.append(&tx.gas_price_wei);
        raw.append(&tx.gas_limit);
        raw.append(&to);
        raw.append(&tx.value_wei);
        raw.append_empty_data();
        raw.append(&v);
        raw.append(&rlp_int_bytes(&compact[0..32]));
        raw.append(&rlp_int_bytes(&compact[32..64]));
        Ok(raw.out().to_vec())
    }

    fn address(&self) -> String {
        self.address.clone()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingTransfer {
    payer: String,
    payee: String,
    amount_wei: u64,
    /// Hash of the broadcast transaction, if release has broadcast.
    /// Retries check this hash first and never rebroadcast while it
    /// is unconfirmed, so a slow chain cannot cause a double pay.
    tx_hash: Option<String>,
}

#[derive(Clone)]
pub struct EvmL2Backend {
    network: L2Network,
    rpc_url: String,
    client: reqwest::blocking::Client,
    signer: Arc<dyn EvmSigner>,
    pending: Arc<Mutex<HashMap<String, PendingTransfer>>>,
    store_path: Option<PathBuf>,
    receipt_max_attempts: u32,
    receipt_poll_ms: u64,
}

/// Outcome of one transaction-receipt lookup.
#[derive(Debug, PartialEq, Eq)]
enum ReceiptOutcome {
    /// Mined with status success.
    Mined,
    /// Mined with status failure: funds never moved, safe to retry fresh.
    Failed,
    /// Not mined yet, or the lookup itself failed: keep waiting.
    Pending,
}

impl EvmL2Backend {
    pub fn new(
        network: L2Network,
        rpc_url: String,
        signer: Box<dyn EvmSigner>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(RPC_TIMEOUT)
            .build()?;
        Ok(EvmL2Backend {
            network,
            rpc_url,
            client,
            signer: Arc::from(signer),
            pending: Arc::new(Mutex::new(HashMap::new())),
            store_path: None,
            receipt_max_attempts: RECEIPT_MAX_ATTEMPTS,
            receipt_poll_ms: RECEIPT_POLL_MS,
        })
    }

    /// Persist pending escrows to `path`, restoring and reconciling any
    /// file already there (mirrors the LND escrow store).
    pub fn with_store_path(mut self, path: PathBuf) -> Self {
        if let Ok(contents) = std::fs::read(&path) {
            if let Ok(saved) = serde_json::from_slice::<HashMap<String, PendingTransfer>>(&contents)
            {
                tracing::info!("restored {} EVM escrows from disk", saved.len());
                *self.pending.lock().expect("evm lock") = saved;
            } else {
                tracing::warn!("ignoring unreadable EVM escrow store");
            }
        }
        self.store_path = Some(path);
        self.reconcile_restored();
        self
    }

    fn persist(&self) {
        let Some(path) = &self.store_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let pending = self.pending.lock().expect("evm lock");
        if let Ok(contents) = serde_json::to_vec(&*pending) {
            let _ = std::fs::write(path, contents);
        }
    }

    /// Re-check restored broadcast transactions once at startup: drop
    /// mined ones, clear failed ones for a fresh retry, keep pending
    /// ones. Any RPC failure keeps everything and returns, like LND.
    fn reconcile_restored(&self) {
        let restored: Vec<(String, Option<String>)> = self
            .pending
            .lock()
            .expect("evm lock")
            .iter()
            .map(|(id, t)| (id.clone(), t.tx_hash.clone()))
            .collect();
        if restored.is_empty() {
            return;
        }
        let mut mined = Vec::new();
        let mut failed = Vec::new();
        for (id, tx_hash) in &restored {
            let Some(hash) = tx_hash else {
                continue;
            };
            match self.check_receipt(hash) {
                Ok(ReceiptOutcome::Mined) => mined.push(id.clone()),
                Ok(ReceiptOutcome::Failed) => failed.push(id.clone()),
                Ok(ReceiptOutcome::Pending) => {}
                Err(e) => {
                    tracing::warn!(
                        "EVM node unreachable during escrow reconcile ({e}); keeping {} restored escrows",
                        restored.len()
                    );
                    return;
                }
            }
        }
        if mined.is_empty() && failed.is_empty() {
            return;
        }
        {
            let mut pending = self.pending.lock().expect("evm lock");
            for id in &mined {
                pending.remove(id);
            }
            for id in &failed {
                if let Some(t) = pending.get_mut(id) {
                    t.tx_hash = None;
                }
            }
        }
        tracing::info!(
            "reconciled {} mined and {} failed EVM escrows off disk",
            mined.len(),
            failed.len()
        );
        self.persist();
    }

    fn rpc(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let res: serde_json::Value = self.client.post(&self.rpc_url).json(&body).send()?.json()?;
        if let Some(err) = res.get("error") {
            anyhow::bail!("evm rpc {method} failed: {err}");
        }
        res.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("evm rpc {method} missing result"))
    }

    fn quantity(&self, method: &str, params: serde_json::Value) -> anyhow::Result<u64> {
        let result = self.rpc(method, params)?;
        let hex_str = result
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("evm rpc {method} returned non-string"))?;
        parse_quantity(hex_str)
    }

    fn send_transfer(&self, to: &str, amount_wei: u64) -> anyhow::Result<String> {
        let from = self.signer.address();
        let nonce = self.quantity(
            "eth_getTransactionCount",
            serde_json::json!([from, "latest"]),
        )?;
        let gas_price = self.quantity("eth_gasPrice", serde_json::json!([]))?;
        let tx = EvmTransfer {
            to: to.to_string(),
            value_wei: amount_wei,
            nonce,
            gas_price_wei: gas_price,
            gas_limit: DEFAULT_GAS_LIMIT,
            chain_id: chain_id_for(&self.network),
        };
        let raw = self.signer.sign_transfer(&tx)?;
        let result = self.rpc(
            "eth_sendRawTransaction",
            serde_json::json!([format!("0x{}", hex::encode(&raw))]),
        )?;
        result
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("eth_sendRawTransaction returned non-string"))
    }

    /// One receipt lookup. Transport failures are Err (node down);
    /// anything else short of an explicit mined status is Pending,
    /// including null results, error payloads, and malformed bodies.
    fn check_receipt(&self, tx_hash: &str) -> anyhow::Result<ReceiptOutcome> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionReceipt",
            "params": [tx_hash],
        });
        let res: serde_json::Value = self.client.post(&self.rpc_url).json(&body).send()?.json()?;
        if res.get("error").is_some() {
            return Ok(ReceiptOutcome::Pending);
        }
        let Some(receipt) = res.get("result") else {
            return Ok(ReceiptOutcome::Pending);
        };
        if receipt.is_null() {
            return Ok(ReceiptOutcome::Pending);
        }
        let status = receipt.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let digits = status
            .strip_prefix("0x")
            .or_else(|| status.strip_prefix("0X"))
            .unwrap_or(status);
        match u128::from_str_radix(digits, 16) {
            Ok(1) => Ok(ReceiptOutcome::Mined),
            Ok(0) => Ok(ReceiptOutcome::Failed),
            _ => Ok(ReceiptOutcome::Pending),
        }
    }

    /// Poll a broadcast transaction until it mines, fails, or exhausts
    /// attempts (then Pending). Transport blips ride through: the node
    /// answered seconds ago, so a miss is worth one more poll.
    fn wait_receipt(&self, tx_hash: &str) -> ReceiptOutcome {
        for attempt in 0..self.receipt_max_attempts {
            match self.check_receipt(tx_hash) {
                Ok(ReceiptOutcome::Mined) => return ReceiptOutcome::Mined,
                Ok(ReceiptOutcome::Failed) => return ReceiptOutcome::Failed,
                Ok(ReceiptOutcome::Pending) | Err(_) => {
                    if attempt + 1 < self.receipt_max_attempts {
                        std::thread::sleep(Duration::from_millis(self.receipt_poll_ms));
                    }
                }
            }
        }
        ReceiptOutcome::Pending
    }

    /// Settle an already-broadcast transaction: drop it when mined,
    /// clear it for a fresh retry when it reverted (a failed tx moves
    /// no funds, so rebroadcasting cannot double-pay), keep it when
    /// still unconfirmed.
    fn settle_known(&self, channel_id: &str, tx_hash: &str) -> anyhow::Result<()> {
        match self.wait_receipt(tx_hash) {
            ReceiptOutcome::Mined => {
                self.pending.lock().expect("evm lock").remove(channel_id);
                self.persist();
                tracing::info!("EVM escrow {channel_id} released in {tx_hash}");
                Ok(())
            }
            ReceiptOutcome::Failed => {
                let mut pending = self.pending.lock().expect("evm lock");
                if let Some(t) = pending.get_mut(channel_id) {
                    t.tx_hash = None;
                }
                drop(pending);
                self.persist();
                anyhow::bail!(
                    "EVM transfer {tx_hash} mined but failed; escrow kept for a fresh retry"
                )
            }
            ReceiptOutcome::Pending => {
                anyhow::bail!(
                    "EVM transfer {tx_hash} unconfirmed after {} attempts; escrow kept",
                    self.receipt_max_attempts
                )
            }
        }
    }
}

fn parse_quantity(hex_str: &str) -> anyhow::Result<u64> {
    let digits = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    if digits.is_empty() {
        anyhow::bail!("empty quantity");
    }
    Ok(u128::from_str_radix(digits, 16).map(|v| v.min(u128::from(u64::MAX)) as u64)?)
}

impl PaymentBackend for EvmL2Backend {
    fn create_escrow(&self, amount: u64, payer: &str, payee: &str) -> anyhow::Result<EscrowInfo> {
        let reported = self.quantity("eth_chainId", serde_json::json!([]))?;
        let expected = chain_id_for(&self.network);
        if reported != expected {
            anyhow::bail!("wrong chain: rpc reports {reported}, expected {expected}");
        }
        let escrow_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(
            "EVM escrow {escrow_id} for {amount} wei ({} -> {})",
            sanitize_log(payer),
            sanitize_log(payee)
        );
        self.pending.lock().expect("evm lock").insert(
            escrow_id.clone(),
            PendingTransfer {
                payer: payer.to_string(),
                payee: payee.to_string(),
                amount_wei: amount,
                tx_hash: None,
            },
        );
        self.persist();
        Ok(EscrowInfo {
            escrow_id,
            payment_details: format!(
                "send {amount} wei to {payee} on {} (chain {expected})",
                self.network
            ),
        })
    }

    fn release_escrow(&self, channel_id: &str) -> anyhow::Result<()> {
        let transfer = {
            let pending = self.pending.lock().expect("evm lock");
            pending
                .get(channel_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown evm escrow: {channel_id}"))?
        };
        if let Some(tx_hash) = transfer.tx_hash.clone() {
            return self.settle_known(channel_id, &tx_hash);
        }
        let tx_hash = self.send_transfer(&transfer.payee, transfer.amount_wei)?;
        {
            let mut pending = self.pending.lock().expect("evm lock");
            if let Some(t) = pending.get_mut(channel_id) {
                t.tx_hash = Some(tx_hash.clone());
            }
        }
        self.persist();
        self.settle_known(channel_id, &tx_hash)
    }

    fn refund_escrow(&self, channel_id: &str) -> anyhow::Result<()> {
        let transfer = {
            let pending = self.pending.lock().expect("evm lock");
            pending
                .get(channel_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown evm escrow: {channel_id}"))?
        };
        if let Some(tx_hash) = transfer.tx_hash {
            anyhow::bail!(
                "EVM escrow {channel_id} already broadcast {tx_hash}; cannot refund on-chain, needs manual recovery"
            );
        }
        self.pending.lock().expect("evm lock").remove(channel_id);
        self.persist();
        tracing::info!(
            "EVM escrow {channel_id} dropped without broadcast ({} wei never left {})",
            transfer.amount_wei,
            sanitize_log(&transfer.payer)
        );
        Ok(())
    }

    fn check_balance(&self, address: &str) -> anyhow::Result<u64> {
        self.quantity("eth_getBalance", serde_json::json!([address, "latest"]))
    }

    fn chain_type(&self) -> ChainType {
        ChainType::EthereumL2(self.network.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::MockServer;
    use std::sync::{Arc, Mutex};

    type SeenTransfers = Arc<Mutex<Vec<(String, u64, u64, u64)>>>;

    struct MockSigner {
        address: String,
        signed: Vec<u8>,
        seen: SeenTransfers,
    }

    impl EvmSigner for MockSigner {
        fn sign_transfer(&self, tx: &EvmTransfer) -> anyhow::Result<Vec<u8>> {
            self.seen
                .lock()
                .unwrap()
                .push((tx.to.clone(), tx.value_wei, tx.nonce, tx.chain_id));
            Ok(self.signed.clone())
        }

        fn address(&self) -> String {
            self.address.clone()
        }
    }

    fn backend_for(server: &MockServer, seen: SeenTransfers) -> EvmL2Backend {
        let signer = MockSigner {
            address: "0x1111111111111111111111111111111111111111".into(),
            signed: vec![0xde, 0xad, 0xbe, 0xef],
            seen,
        };
        EvmL2Backend::new(L2Network::Base, server.base_url.clone(), Box::new(signer)).unwrap()
    }

    fn rpc_result(value: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{value}"}}"#)
    }

    fn mined_receipt() -> String {
        r#"{"jsonrpc":"2.0","id":1,"result":{"status":"0x1"}}"#.to_string()
    }

    fn failed_receipt() -> String {
        r#"{"jsonrpc":"2.0","id":1,"result":{"status":"0x0"}}"#.to_string()
    }

    fn count_rpc_calls(server: &MockServer, rpc_method: &str) -> usize {
        server
            .requests()
            .iter()
            .filter(|r| r.body_str().contains(rpc_method))
            .count()
    }

    #[test]
    fn release_pays_the_worker_on_the_right_chain() {
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x3")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xabc123")),
            ("eth_getTransactionReceipt".into(), mined_receipt()),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = backend_for(&server, seen.clone());
        let info = backend
            .create_escrow(1_000_000, "0xpayer", "0xworker")
            .unwrap();
        assert!(info.payment_details.contains("0xworker"));
        assert!(info.payment_details.contains("8453"));
        backend.release_escrow(&info.escrow_id).unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "0xworker");
        assert_eq!(seen[0].1, 1_000_000);
        assert_eq!(seen[0].2, 3);
        assert_eq!(seen[0].3, 8453);

        let requests = server.requests();
        let broadcast = requests
            .iter()
            .find(|r| r.body_str().contains("eth_sendRawTransaction"))
            .unwrap();
        assert!(broadcast.body_str().contains("0xdeadbeef"));
        server.stop();
    }

    #[test]
    fn refund_drops_without_signing_or_broadcast() {
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x9")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xdef456")),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = backend_for(&server, seen.clone());
        let info = backend.create_escrow(500, "0xpayer", "0xworker").unwrap();
        backend.refund_escrow(&info.escrow_id).unwrap();
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(count_rpc_calls(&server, "eth_sendRawTransaction"), 0);
        assert!(backend.refund_escrow(&info.escrow_id).is_err());
        server.stop();
    }

    #[test]
    fn release_removes_escrow_only_after_mining() {
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x3")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xabc123")),
            ("eth_getTransactionReceipt".into(), mined_receipt()),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = backend_for(&server, seen.clone());
        let info = backend
            .create_escrow(1_000_000, "0xpayer", "0xworker")
            .unwrap();
        backend.release_escrow(&info.escrow_id).unwrap();
        assert!(backend.release_escrow(&info.escrow_id).is_err());
        assert_eq!(count_rpc_calls(&server, "eth_sendRawTransaction"), 1);
        server.stop();
    }

    #[test]
    fn release_timeout_broadcasts_once_across_retries() {
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x3")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xabc123")),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut backend = backend_for(&server, seen.clone());
        backend.receipt_max_attempts = 2;
        backend.receipt_poll_ms = 5;
        let info = backend
            .create_escrow(1_000_000, "0xpayer", "0xworker")
            .unwrap();
        let err = backend.release_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("unconfirmed"), "{err}");
        let err = backend.release_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("unconfirmed"), "{err}");
        assert_eq!(count_rpc_calls(&server, "eth_sendRawTransaction"), 1);
        server.stop();
    }

    #[test]
    fn release_failed_receipt_retries_fresh() {
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x3")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xabc123")),
            ("eth_getTransactionReceipt".into(), failed_receipt()),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = backend_for(&server, seen.clone());
        let info = backend
            .create_escrow(1_000_000, "0xpayer", "0xworker")
            .unwrap();
        let err = backend.release_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("mined but failed"), "{err}");
        let err = backend.release_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("mined but failed"), "{err}");
        assert_eq!(count_rpc_calls(&server, "eth_sendRawTransaction"), 2);
        server.stop();
    }

    #[test]
    fn refund_after_broadcast_is_refused_with_hash() {
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x3")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xabc123")),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut backend = backend_for(&server, seen.clone());
        backend.receipt_max_attempts = 2;
        backend.receipt_poll_ms = 5;
        let info = backend
            .create_escrow(1_000_000, "0xpayer", "0xworker")
            .unwrap();
        assert!(backend.release_escrow(&info.escrow_id).is_err());
        let err = backend.refund_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("0xabc123"), "{err}");
        assert_eq!(count_rpc_calls(&server, "eth_sendRawTransaction"), 1);
        server.stop();
    }

    #[test]
    fn reconcile_restored_drops_mined_and_keeps_unconfirmed() {
        let dir = std::env::temp_dir().join(format!("gf_evmrec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("evm_escrows_base.json");
        let server = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionCount".into(), rpc_result("0x3")),
            ("eth_gasPrice".into(), rpc_result("0x64")),
            ("eth_sendRawTransaction".into(), rpc_result("0xabc123")),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut backend = backend_for(&server, seen.clone()).with_store_path(store.clone());
        backend.receipt_max_attempts = 2;
        backend.receipt_poll_ms = 5;
        let info = backend
            .create_escrow(1_000_000, "0xpayer", "0xworker")
            .unwrap();
        assert!(backend.release_escrow(&info.escrow_id).is_err());
        assert!(store.exists());
        drop(backend);

        let server2 = MockServer::start(vec![
            ("eth_chainId".into(), rpc_result("0x2105")),
            ("eth_getTransactionReceipt".into(), mined_receipt()),
        ]);
        let seen2 = Arc::new(Mutex::new(Vec::new()));
        let backend2 = backend_for(&server2, seen2).with_store_path(store.clone());
        assert!(backend2.release_escrow(&info.escrow_id).is_err());
        server.stop();
        server2.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_chain_aborts_before_any_signing() {
        let server = MockServer::start(vec![("eth_chainId".into(), rpc_result("0x1"))]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = backend_for(&server, seen.clone());
        let err = backend
            .create_escrow(500, "0xpayer", "0xworker")
            .unwrap_err();
        assert!(err.to_string().contains("wrong chain"), "{err}");
        assert!(seen.lock().unwrap().is_empty());
        server.stop();
    }

    #[test]
    fn balance_parses_hex_quantity() {
        let server = MockServer::start(vec![(
            "eth_getBalance".into(),
            rpc_result("0x38d7ea4c68000"),
        )]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = backend_for(&server, seen);
        assert_eq!(
            backend
                .check_balance("0x1111111111111111111111111111111111111111")
                .unwrap(),
            1_000_000_000_000_000
        );
        server.stop();
    }

    #[test]
    fn command_signer_passes_tx_fields_and_decodes_hex() {
        let signer = CommandSigner::new(
            "echo $TX_TO:$TX_VALUE_WEI:$TX_NONCE:$TX_CHAIN_ID >&2; echo 0x0102".into(),
            "0xfrom".into(),
        );
        let tx = EvmTransfer {
            to: "0xto".into(),
            value_wei: 42,
            nonce: 7,
            gas_price_wei: 100,
            gas_limit: 21_000,
            chain_id: 8453,
        };
        assert_eq!(signer.sign_transfer(&tx).unwrap(), vec![0x01, 0x02]);
        assert_eq!(signer.address(), "0xfrom");
    }

    #[test]
    fn command_signer_failure_is_an_error() {
        let signer = CommandSigner::new("exit 3".into(), "0xfrom".into());
        let tx = EvmTransfer {
            to: "0xto".into(),
            value_wei: 1,
            nonce: 0,
            gas_price_wei: 1,
            gas_limit: 21_000,
            chain_id: 1,
        };
        assert!(signer.sign_transfer(&tx).is_err());
    }

    fn embedded_test_signer() -> EmbeddedSigner {
        let path = std::env::temp_dir().join(format!(
            "gf_emb_{}_{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_file(&path);
        let secret =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .unwrap();
        let mut key = [0u8; 32];
        key.copy_from_slice(&secret);
        crate::keystore::create_keystore(
            &path,
            &key,
            b"test-password",
            "0xFCAd0B19bB29D4674531d6f115237E16AfCE377c",
        )
        .unwrap();
        let signer = EmbeddedSigner::from_keystore(&path, b"test-password").unwrap();
        let _ = std::fs::remove_file(&path);
        signer
    }

    #[test]
    fn embedded_signer_derives_address() {
        let signer = embedded_test_signer();
        assert_eq!(
            signer.address(),
            "0xfcad0b19bb29d4674531d6f115237e16afce377c"
        );
    }

    #[test]
    fn embedded_signer_matches_eth_account_vector() {
        let signer = embedded_test_signer();
        let tx = EvmTransfer {
            to: "0x000000000000000000000000000000000000dEaD".into(),
            value_wei: 1_000_000,
            nonce: 0,
            gas_price_wei: 1_000_000_000,
            gas_limit: 21_000,
            chain_id: 8453,
        };
        let raw = signer.sign_transfer(&tx).unwrap();
        let expected = hex::decode(
            "f86880843b9aca0082520894000000000000000000000000000000000000dead830f42408082422da0e3b1bab15227d59c8978be994ce9d48660c14b72596e38114e465cbb02204eeca00aa1f14971e54e4bf8b33c10b213b22e9aa7743f28d3fbb6e451e4fc38c078b1",
        )
        .unwrap();
        assert_eq!(hex::encode(&raw), hex::encode(&expected));
    }

    #[test]
    fn embedded_signer_rejects_bad_destination() {
        let signer = embedded_test_signer();
        let tx = EvmTransfer {
            to: "0x1234".into(),
            value_wei: 1,
            nonce: 0,
            gas_price_wei: 1,
            gas_limit: 21_000,
            chain_id: 8453,
        };
        assert!(signer.sign_transfer(&tx).is_err());
    }

    #[test]
    fn embedded_signer_rejects_wrong_password() {
        let path = std::env::temp_dir().join(format!(
            "gf_emb_pw_{}_{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_file(&path);
        crate::keystore::create_keystore(&path, &[9u8; 32], b"right", "0x0").unwrap();
        assert!(EmbeddedSigner::from_keystore(&path, b"wrong").is_err());
        let _ = std::fs::remove_file(&path);
    }

    fn keystore_for_build_test(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gf_emb_build_{tag}_{}_{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_file(&path);
        let secret =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .unwrap();
        let mut key = [0u8; 32];
        key.copy_from_slice(&secret);
        crate::keystore::create_keystore(
            &path,
            &key,
            b"build-pw",
            "0xFCAd0B19bB29D4674531d6f115237E16AfCE377c",
        )
        .unwrap();
        path
    }

    #[test]
    fn build_signer_prefers_keystore_over_command() {
        let path = keystore_for_build_test("prefers");
        let pw_path = path.with_extension("pw");
        std::fs::write(&pw_path, b"build-pw\n").unwrap();
        let signer = match build_signer(
            "0xfcad0b19bb29d4674531d6f115237e16afce377c",
            "should-not-be-used",
            Some(&path),
            Some(&pw_path),
        ) {
            Ok(signer) => signer,
            Err(e) => panic!("build_signer failed: {e}"),
        };
        assert_eq!(
            signer.address(),
            "0xfcad0b19bb29d4674531d6f115237e16afce377c"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&pw_path);
    }

    #[test]
    fn build_signer_rejects_address_mismatch() {
        let path = keystore_for_build_test("mismatch");
        let pw_path = path.with_extension("pw");
        std::fs::write(&pw_path, b"build-pw").unwrap();
        let err = match build_signer(
            "0x0000000000000000000000000000000000000001",
            "",
            Some(&path),
            Some(&pw_path),
        ) {
            Ok(_) => panic!("expected address mismatch error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("does not match"), "{err}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&pw_path);
    }

    #[test]
    fn build_signer_needs_some_signer() {
        let err = match build_signer("0xabc", "", None, None) {
            Ok(_) => panic!("expected no-signer error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no signer"), "{err}");
        let signer = match build_signer("0xabc", "true", None, None) {
            Ok(signer) => signer,
            Err(e) => panic!("build_signer failed: {e}"),
        };
        assert_eq!(signer.address(), "0xabc");
    }
}
