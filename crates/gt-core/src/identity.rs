use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum IdentityError {
    #[error("failed to read key file: {0}")]
    ReadError(#[from] std::io::Error),
    #[error("invalid key data: {0}")]
    InvalidKey(String),
    #[error("signature verification failed")]
    VerificationFailed,
    #[error("serialization error: {0}")]
    SerializationError(String),
}

/// A user's identity on the GitForest network.
/// The public key IS the identity — no usernames, no accounts.
#[derive(Clone, Debug)]
pub struct Identity {
    signing_key: SigningKey,
}

/// Public half of an identity, safe to share.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl std::str::FromStr for PeerId {
    type Err = IdentityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes =
            hex::decode(s).map_err(|e| IdentityError::InvalidKey(format!("invalid hex: {e}")))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| IdentityError::InvalidKey("expected 32 bytes".into()))?;
        Ok(PeerId(arr))
    }
}

/// A signed message from a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedMessage {
    pub peer_id: PeerId,
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
}

impl Identity {
    /// Generate a new random identity.
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Identity { signing_key }
    }

    /// Load identity from a key file (raw 32-byte seed).
    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            return Err(IdentityError::InvalidKey(format!(
                "key file should be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let seed: [u8; 32] = bytes.try_into().unwrap();
        let signing_key = SigningKey::from_bytes(&seed);
        Ok(Identity { signing_key })
    }

    /// Save identity to a key file.
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = opts.open(path)?;
        std::io::Write::write_all(&mut file, &self.signing_key.to_bytes())?;
        #[cfg(unix)]
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Get the public peer ID for this identity.
    pub fn peer_id(&self) -> PeerId {
        PeerId(self.signing_key.verifying_key().to_bytes())
    }

    /// Sign arbitrary data.
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let sig: Signature = self.signing_key.sign(data);
        sig.to_bytes().to_vec()
    }

    /// Create a signed message.
    pub fn sign_message(&self, payload: Vec<u8>) -> SignedMessage {
        let signature = self.sign(&payload);
        SignedMessage {
            peer_id: self.peer_id(),
            payload,
            signature,
        }
    }

    /// Get the verifying (public) key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Get the raw 32-byte secret seed.
    /// Used to derive a libp2p keypair so the DHT identity matches our network identity.
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

pub const PEER_CALL_SKEW_SECS: i64 = 300;

pub fn peer_call_payload(
    peer_id: &PeerId,
    method: &str,
    path: &str,
    timestamp: i64,
    body: &[u8],
) -> Vec<u8> {
    let mut data = b"gitforest:peer-call:v2:".to_vec();
    data.extend_from_slice(&peer_id.0);
    data.extend_from_slice(method.as_bytes());
    data.push(0);
    data.extend_from_slice(path.as_bytes());
    data.push(0);
    data.extend_from_slice(&timestamp.to_be_bytes());
    data.extend_from_slice(&Sha256::digest(body));
    data
}

pub fn verify_peer_call(
    peer_id: &PeerId,
    method: &str,
    path: &str,
    timestamp: i64,
    body: &[u8],
    signature: &[u8],
    now: i64,
) -> bool {
    if (now - timestamp).abs() > PEER_CALL_SKEW_SECS {
        return false;
    }
    let payload = peer_call_payload(peer_id, method, path, timestamp, body);
    verify_signature(peer_id, &payload, signature).is_ok()
}

pub fn peer_call_headers(
    identity: &Identity,
    method: &str,
    path: &str,
    body: &[u8],
) -> [(String, String); 3] {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let peer_id = identity.peer_id();
    let signature = identity.sign(&peer_call_payload(&peer_id, method, path, now, body));
    [
        ("X-GF-Peer".to_string(), peer_id.to_string()),
        ("X-GF-Timestamp".to_string(), now.to_string()),
        ("X-GF-Signature".to_string(), hex::encode(signature)),
    ]
}

/// Verify a signed message from a peer.
pub fn verify_signature(
    peer_id: &PeerId,
    data: &[u8],
    signature: &[u8],
) -> Result<(), IdentityError> {
    let vk = VerifyingKey::from_bytes(&peer_id.0)
        .map_err(|e| IdentityError::InvalidKey(format!("invalid public key: {e}")))?;
    let sig = Signature::from_slice(signature)
        .map_err(|e| IdentityError::InvalidKey(format!("invalid signature: {e}")))?;
    vk.verify(data, &sig)
        .map_err(|_| IdentityError::VerificationFailed)
}

/// Verify a signed message.
pub fn verify_signed_message(msg: &SignedMessage) -> Result<(), IdentityError> {
    verify_signature(&msg.peer_id, &msg.payload, &msg.signature)
}

/// Compute a repo address from owner + repo name.
/// This is what gets announced/looked up on the DHT.
pub fn repo_address(owner: &PeerId, repo_name: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:repo:");
    hasher.update(owner.0);
    hasher.update(b":");
    hasher.update(repo_name.as_bytes());
    hasher.finalize().into()
}

/// Default path for the identity key file.
pub fn default_key_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gitforest")
        .join("identity.key")
}

/// Identity key path honoring an explicit override.
///
/// `GITFOREST_IDENTITY` wins when set and non-empty so scripts, tests,
/// and non-default layouts can point at a key without depending on
/// platform config-dir conventions.
pub fn identity_key_path() -> PathBuf {
    if let Ok(path) = std::env::var("GITFOREST_IDENTITY") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    default_key_path()
}

/// Genesis proof-of-work difficulty for worker offers, in leading zero bits.
/// 16 bits costs ~65k hashes to mine (milliseconds) and 1 hash to verify —
/// cheap for one real worker, expensive for ten thousand Sybils.
///
/// This is a floor, not a fixed value: verifiers never accept fewer bits
/// than this no matter how they are configured (see
/// `ComputeOfferMessage::verify_with_bits`), while miners aim at a
/// separately configured target so difficulty can rise over time without
/// a flag day. Raising the network's difficulty is a two-release
/// procedure: first ship a higher default mining target (new offers
/// carry more work; old verifiers still accept them since their check
/// is a lower bound), then, once upgraded offers dominate, raise the
/// default verification minimum. Operators in a hurry can set
/// `compute.min_pow_bits` early; laggard workers simply stop matching
/// on strict nodes instead of breaking the network.
pub const MIN_POW_BITS: u8 = 16;

pub fn pow_hash(peer_id: &PeerId, nonce: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:pow:");
    hasher.update(peer_id.0);
    hasher.update(nonce.to_le_bytes());
    hasher.finalize().into()
}

fn leading_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut count = 0u32;
    for byte in hash {
        if *byte == 0 {
            count += 8;
        } else {
            count += byte.leading_zeros();
            break;
        }
    }
    count
}

/// Measured strength of a PoW proof, in leading zero bits. Intake paths
/// stamp this on registry entries so a later difficulty raise can evict
/// entries that no longer qualify.
pub fn pow_bits(peer_id: &PeerId, nonce: u64) -> u32 {
    leading_zero_bits(&pow_hash(peer_id, nonce))
}

pub fn verify_pow(peer_id: &PeerId, nonce: u64, bits: u8) -> bool {
    leading_zero_bits(&pow_hash(peer_id, nonce)) >= bits as u32
}

pub fn mine_pow(peer_id: &PeerId, bits: u8) -> u64 {
    let mut nonce = 0u64;
    loop {
        if verify_pow(peer_id, nonce, bits) {
            return nonce;
        }
        nonce = nonce.wrapping_add(1);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PowCache {
    peer_id: PeerId,
    nonce: u64,
    bits: u8,
}

pub fn pow_path_for(key_path: &Path) -> PathBuf {
    let mut path = key_path.to_path_buf();
    path.set_extension("pow");
    path
}

pub fn load_pow(path: &Path) -> Option<(PeerId, u64, u8)> {
    let bytes = std::fs::read(path).ok()?;
    let cache: PowCache = serde_json::from_slice(&bytes).ok()?;
    Some((cache.peer_id, cache.nonce, cache.bits))
}

pub fn save_pow(path: &Path, peer_id: &PeerId, nonce: u64, bits: u8) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cache = PowCache {
        peer_id: peer_id.clone(),
        nonce,
        bits,
    };
    std::fs::write(path, serde_json::to_vec(&cache)?).map_err(std::io::Error::other)
}

/// Return a valid PoW nonce for this identity, mining and caching one if
/// needed. The cache is keyed by peer id, so identities sharing a cache
/// path can never use each other's proofs.
pub fn offer_pow_nonce(identity: &Identity) -> u64 {
    offer_pow_nonce_with_target(identity, MIN_POW_BITS)
}

/// Mine (or reuse a cached proof) toward `target_bits` of work. Targets
/// below the genesis floor are clamped up: miners never produce proofs
/// weaker than the network's minimum. A cached proof is reused only when
/// it meets the target, so raising the target re-mines exactly once and
/// then reuses the stronger proof.
pub fn offer_pow_nonce_with_target(identity: &Identity, target_bits: u8) -> u64 {
    let target_bits = target_bits.max(MIN_POW_BITS);
    let path = std::env::var("GITFOREST_POW_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| pow_path_for(&default_key_path()));
    if let Some((peer_id, nonce, bits)) = load_pow(&path) {
        if peer_id == identity.peer_id() && bits >= target_bits && verify_pow(&peer_id, nonce, bits)
        {
            return nonce;
        }
    }
    let nonce = mine_pow(&identity.peer_id(), target_bits);
    let _ = save_pow(&path, &identity.peer_id(), nonce, target_bits);
    nonce
}

/// Default path for the display name file.
pub fn default_name_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gitforest")
        .join("display_name")
}

/// Save a display name to the default path.
pub fn save_display_name(name: &str) -> Result<(), IdentityError> {
    let path = default_name_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, name.trim())?;
    Ok(())
}

/// Load the display name from the default path.
pub fn load_display_name() -> Option<String> {
    let path = default_name_path();
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A human-readable peer name in Discord-style format: `name#disc`.
///
/// The discriminator is the first 4 hex characters of the peer ID,
/// making names like `laptop#cd26` instead of raw 64-char hex keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerName {
    pub display_name: String,
    pub peer_id: PeerId,
}

impl PeerName {
    /// Create a new PeerName from a display name and peer ID.
    pub fn new(display_name: String, peer_id: PeerId) -> Self {
        PeerName {
            display_name,
            peer_id,
        }
    }

    /// Get the 4-char hex discriminator (first 4 hex chars of peer ID).
    pub fn discriminator(&self) -> String {
        hex::encode(self.peer_id.0)[..4].to_string()
    }

    /// Get the short tag: `name#disc` (e.g., `laptop#cd26`).
    pub fn short_tag(&self) -> String {
        format!("{}#{}", self.display_name, self.discriminator())
    }
}

impl std::fmt::Display for PeerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short_tag())
    }
}

impl std::str::FromStr for PeerName {
    type Err = IdentityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, disc) = s
            .rsplit_once('#')
            .ok_or_else(|| IdentityError::InvalidKey("expected format: name#disc".into()))?;
        if disc.len() != 4 || hex::decode(disc).is_err() {
            return Err(IdentityError::InvalidKey(
                "discriminator must be exactly 4 hex characters".into(),
            ));
        }
        if name.is_empty() {
            return Err(IdentityError::InvalidKey(
                "display name cannot be empty".into(),
            ));
        }
        // We can't reconstruct the full peer ID from just the discriminator,
        // so we store a zeroed peer ID — callers must resolve via config.
        Ok(PeerName {
            display_name: name.to_string(),
            peer_id: PeerId([0u8; 32]),
        })
    }
}

mod hex_bytes {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_key_path_honors_env_override() {
        let prior = std::env::var("GITFOREST_IDENTITY").ok();
        std::env::set_var("GITFOREST_IDENTITY", "/tmp/custom-identity.key");
        assert_eq!(
            identity_key_path(),
            PathBuf::from("/tmp/custom-identity.key")
        );
        std::env::set_var("GITFOREST_IDENTITY", "   ");
        assert_eq!(identity_key_path(), default_key_path());
        std::env::remove_var("GITFOREST_IDENTITY");
        assert_eq!(identity_key_path(), default_key_path());
        if let Some(value) = prior {
            std::env::set_var("GITFOREST_IDENTITY", value);
        }
    }

    #[test]
    fn test_generate_and_sign() {
        let id = Identity::generate();
        let data = b"hello gitforest";
        let sig = id.sign(data);
        assert!(verify_signature(&id.peer_id(), data, &sig).is_ok());
    }

    #[test]
    fn test_signed_message() {
        let id = Identity::generate();
        let msg = id.sign_message(b"test payload".to_vec());
        assert!(verify_signed_message(&msg).is_ok());
    }

    #[test]
    fn test_tampered_message_fails() {
        let id = Identity::generate();
        let mut msg = id.sign_message(b"original".to_vec());
        msg.payload = b"tampered".to_vec();
        assert!(verify_signed_message(&msg).is_err());
    }

    #[test]
    fn test_wrong_signer_fails() {
        let id1 = Identity::generate();
        let id2 = Identity::generate();
        let data = b"test";
        let sig = id1.sign(data);
        assert!(verify_signature(&id2.peer_id(), data, &sig).is_err());
    }

    #[test]
    fn test_peer_id_roundtrip() {
        let id = Identity::generate();
        let pid = id.peer_id();
        let s = pid.to_string();
        let parsed: PeerId = s.parse().unwrap();
        assert_eq!(pid, parsed);
    }

    #[test]
    fn test_repo_address_deterministic() {
        let id = Identity::generate();
        let addr1 = repo_address(&id.peer_id(), "myrepo");
        let addr2 = repo_address(&id.peer_id(), "myrepo");
        assert_eq!(addr1, addr2);
    }

    #[test]
    fn test_repo_address_differs_by_name() {
        let id = Identity::generate();
        let addr1 = repo_address(&id.peer_id(), "repo1");
        let addr2 = repo_address(&id.peer_id(), "repo2");
        assert_ne!(addr1, addr2);
    }

    #[test]
    fn test_save_load_identity() {
        let dir = std::env::temp_dir().join("gitforest_test_identity");
        let path = dir.join("test.key");
        let _ = std::fs::remove_file(&path);

        let id = Identity::generate();
        id.save(&path).unwrap();

        let loaded = Identity::load(&path).unwrap();
        assert_eq!(id.peer_id(), loaded.peer_id());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_peer_name_short_tag() {
        let id = Identity::generate();
        let pid = id.peer_id();
        let name = PeerName::new("laptop".into(), pid.clone());
        let tag = name.short_tag();

        // Should be "laptop#" + first 4 hex chars of peer ID
        let expected_disc = &pid.to_string()[..4];
        assert_eq!(tag, format!("laptop#{expected_disc}"));
        assert_eq!(name.to_string(), tag);
    }

    #[test]
    fn test_peer_name_from_str() {
        let parsed: PeerName = "desktop#a1b2".parse().unwrap();
        assert_eq!(parsed.display_name, "desktop");

        // Invalid formats
        assert!("nodisc".parse::<PeerName>().is_err());
        assert!("#a1b2".parse::<PeerName>().is_err());
        assert!("name#zzzz".parse::<PeerName>().is_err());
        assert!("name#ab".parse::<PeerName>().is_err());
    }

    #[test]
    fn test_pow_mine_and_verify() {
        let id = Identity::generate();
        let nonce = mine_pow(&id.peer_id(), 8);
        assert!(verify_pow(&id.peer_id(), nonce, 8));
    }

    #[test]
    fn test_leading_zero_bits() {
        assert_eq!(leading_zero_bits(&[0u8; 32]), 256);
        let mut hash = [0u8; 32];
        hash[0] = 0x80;
        assert_eq!(leading_zero_bits(&hash), 0);
        hash[0] = 0x00;
        hash[1] = 0x0f;
        assert_eq!(leading_zero_bits(&hash), 12);
        hash[0] = 0x01;
        assert_eq!(leading_zero_bits(&hash), 7);
    }

    #[test]
    fn test_pow_difficulty_is_monotonic() {
        let id = Identity::generate();
        let nonce = mine_pow(&id.peer_id(), 10);
        assert!(verify_pow(&id.peer_id(), nonce, 10));
        assert!(verify_pow(&id.peer_id(), nonce, 4));
    }

    #[test]
    fn test_pow_cache_roundtrip_and_peer_binding() {
        let id = Identity::generate();
        let other = Identity::generate();
        let dir = std::env::temp_dir().join(format!("gf_pow_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = pow_path_for(&dir.join("identity.key"));
        assert!(load_pow(&path).is_none());
        let nonce = mine_pow(&id.peer_id(), MIN_POW_BITS);
        save_pow(&path, &id.peer_id(), nonce, MIN_POW_BITS).unwrap();
        let (peer_id, loaded_nonce, bits) = load_pow(&path).unwrap();
        assert_eq!(peer_id, id.peer_id());
        assert_eq!(loaded_nonce, nonce);
        assert_eq!(bits, MIN_POW_BITS);
        assert!(verify_pow(&peer_id, loaded_nonce, bits));
        assert!(!verify_pow(&other.peer_id(), loaded_nonce, bits));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pow_target_raise_remines_and_floor_clamps() {
        let id = Identity::generate();
        let dir = std::env::temp_dir().join(format!("gf_pow_raise_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.pow");
        std::env::set_var("GITFOREST_POW_PATH", &path);
        let n16 = offer_pow_nonce_with_target(&id, MIN_POW_BITS);
        assert!(verify_pow(&id.peer_id(), n16, MIN_POW_BITS));
        assert_eq!(load_pow(&path).unwrap().2, MIN_POW_BITS);
        let n17 = offer_pow_nonce_with_target(&id, MIN_POW_BITS + 1);
        assert!(verify_pow(&id.peer_id(), n17, MIN_POW_BITS + 1));
        assert_eq!(load_pow(&path).unwrap().2, MIN_POW_BITS + 1);
        assert_eq!(offer_pow_nonce_with_target(&id, MIN_POW_BITS + 1), n17);
        let _ = std::fs::remove_dir_all(&dir);

        let id2 = Identity::generate();
        let dir2 = std::env::temp_dir().join(format!("gf_pow_floor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir2);
        std::fs::create_dir_all(&dir2).unwrap();
        std::env::set_var("GITFOREST_POW_PATH", dir2.join("test.pow"));
        let nonce = offer_pow_nonce_with_target(&id2, 1);
        assert!(verify_pow(&id2.peer_id(), nonce, MIN_POW_BITS));
        assert_eq!(load_pow(&dir2.join("test.pow")).unwrap().2, MIN_POW_BITS);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn test_peer_call_roundtrip_and_tamper() {
        let id = Identity::generate();
        let peer = id.peer_id();
        let now = 1_800_000_000i64;
        let payload = peer_call_payload(&peer, "POST", "/api/models/m/infer", now, b"{}");
        let sig = id.sign(&payload);
        assert!(verify_peer_call(
            &peer,
            "POST",
            "/api/models/m/infer",
            now,
            b"{}",
            &sig,
            now
        ));
        assert!(!verify_peer_call(
            &peer,
            "GET",
            "/api/models/m/infer",
            now,
            b"{}",
            &sig,
            now
        ));
        assert!(!verify_peer_call(
            &peer,
            "POST",
            "/api/models/other/infer",
            now,
            b"{}",
            &sig,
            now
        ));
        assert!(!verify_peer_call(
            &Identity::generate().peer_id(),
            "POST",
            "/api/models/m/infer",
            now,
            b"{}",
            &sig,
            now
        ));
        let mut bad = sig.clone();
        bad[0] ^= 0xff;
        assert!(!verify_peer_call(
            &peer,
            "POST",
            "/api/models/m/infer",
            now,
            b"{}",
            &bad,
            now
        ));
        assert!(!verify_peer_call(
            &peer,
            "POST",
            "/api/models/m/infer",
            now,
            b"{\"prompt\":\"evil\"}",
            &sig,
            now
        ));
        assert!(!verify_peer_call(
            &peer,
            "POST",
            "/api/models/m/infer",
            now,
            b"{}",
            &sig,
            now + PEER_CALL_SKEW_SECS + 1
        ));
        assert!(!verify_peer_call(
            &peer,
            "POST",
            "/api/models/m/infer",
            now,
            b"{}",
            &sig,
            now - PEER_CALL_SKEW_SECS - 1
        ));
    }

    #[test]
    fn test_peer_call_headers_verify_live() {
        let id = Identity::generate();
        let headers = peer_call_headers(&id, "GET", "/api/blobs/x/meta", b"");
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let peer: PeerId = get("X-GF-Peer").parse().unwrap();
        let ts: i64 = get("X-GF-Timestamp").parse().unwrap();
        let sig = hex::decode(get("X-GF-Signature")).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(verify_peer_call(
            &peer,
            "GET",
            "/api/blobs/x/meta",
            ts,
            b"",
            &sig,
            now
        ));
    }
}
