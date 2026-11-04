use anyhow::{bail, Context, Result};
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use gt_core::identity::Identity;
use rand::RngCore as _;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const PASSPHRASE_ENV_VAR: &str = "GF_SECRETS_PASSPHRASE";

const ENVELOPE_VERSION: u32 = 1;
const ENVELOPE_VERSION_V2: u32 = 2;
const ENVELOPE_ALG: &str = "xchacha20poly1305";
const ENVELOPE_KDF_V2: &str = "pbkdf2-sha256";
const NONCE_BYTES: usize = 24;
const SALT_BYTES: usize = 32;

#[derive(serde::Serialize, serde::Deserialize)]
struct SecretsEnvelope {
    v: u32,
    alg: String,
    nonce: String,
    ciphertext: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SecretsEnvelopeV2 {
    v: u32,
    alg: String,
    kdf: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

fn derive_store_key(secret: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest-repo-secrets-store-v1");
    hasher.update([0u8]);
    hasher.update(secret);
    hasher.finalize().into()
}

pub struct RepoSecretsStore {
    dir: PathBuf,
    key: [u8; 32],
    passphrase: Option<Vec<u8>>,
}

pub fn secrets_passphrase_from_env() -> Option<Vec<u8>> {
    std::env::var(PASSPHRASE_ENV_VAR)
        .ok()
        .map(|s| s.into_bytes())
        .filter(|b| !b.is_empty())
}

impl RepoSecretsStore {
    pub fn new(data_dir: &Path, identity: &Identity) -> Self {
        RepoSecretsStore {
            dir: data_dir.join("secrets"),
            key: derive_store_key(&identity.secret_bytes()),
            passphrase: None,
        }
    }

    pub fn with_passphrase(mut self, passphrase: Option<&[u8]>) -> Self {
        self.passphrase = passphrase.map(|b| b.to_vec()).filter(|b| !b.is_empty());
        self
    }

    pub fn valid_repo_name(repo: &str) -> bool {
        !repo.is_empty()
            && repo.len() <= 128
            && repo
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    }

    fn path(&self, repo: &str) -> Result<PathBuf> {
        if !Self::valid_repo_name(repo) {
            bail!("bad repo name");
        }
        Ok(self.dir.join(format!("{repo}.json")))
    }

    fn open_sealed(
        key: &[u8; 32],
        repo: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Option<HashMap<String, String>> {
        let nonce = XNonce::try_from(nonce).ok()?;
        let cipher = XChaCha20Poly1305::new(&Key::from(*key));
        let plaintext = cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: repo.as_bytes(),
                },
            )
            .ok()?;
        serde_json::from_slice(&plaintext).ok()
    }

    fn seal_map(
        key: &[u8; 32],
        repo: &str,
        map: &HashMap<String, String>,
    ) -> Result<(XNonce, Vec<u8>)> {
        let plaintext = serde_json::to_vec_pretty(map)?;
        let mut nonce = [0u8; NONCE_BYTES];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new(&Key::from(*key));
        let nonce = XNonce::from(nonce);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad: repo.as_bytes(),
                },
            )
            .map_err(|e| anyhow::anyhow!("seal secrets for {repo}: {e}"))?;
        Ok((nonce, ciphertext))
    }

    fn decrypt_v1(&self, repo: &str, text: &str) -> Option<HashMap<String, String>> {
        let envelope: SecretsEnvelope = serde_json::from_str(text).ok()?;
        if envelope.v != ENVELOPE_VERSION || envelope.alg != ENVELOPE_ALG {
            return None;
        }
        let engine = base64::engine::general_purpose::STANDARD;
        let nonce = engine.decode(envelope.nonce.as_bytes()).ok()?;
        let ciphertext = engine.decode(envelope.ciphertext.as_bytes()).ok()?;
        Self::open_sealed(&self.key, repo, &nonce, &ciphertext)
    }

    fn decrypt_v2(&self, repo: &str, text: &str) -> Option<HashMap<String, String>> {
        let passphrase = self.passphrase.as_ref()?;
        let envelope: SecretsEnvelopeV2 = serde_json::from_str(text).ok()?;
        if envelope.v != ENVELOPE_VERSION_V2
            || envelope.alg != ENVELOPE_ALG
            || envelope.kdf != ENVELOPE_KDF_V2
        {
            return None;
        }
        let engine = base64::engine::general_purpose::STANDARD;
        let salt = engine.decode(envelope.salt.as_bytes()).ok()?;
        let nonce = engine.decode(envelope.nonce.as_bytes()).ok()?;
        let ciphertext = engine.decode(envelope.ciphertext.as_bytes()).ok()?;
        let key = gt_compute::keystore::derive_passphrase_key(passphrase, &salt);
        Self::open_sealed(&key, repo, &nonce, &ciphertext)
    }

    fn write_map(&self, repo: &str, map: &HashMap<String, String>) -> Result<()> {
        let path = self.path(repo)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let engine = base64::engine::general_purpose::STANDARD;
        let text = match self.passphrase.as_ref() {
            Some(passphrase) => {
                let mut salt = [0u8; SALT_BYTES];
                rand::thread_rng().fill_bytes(&mut salt);
                let key = gt_compute::keystore::derive_passphrase_key(passphrase, &salt);
                let (nonce, ciphertext) = Self::seal_map(&key, repo, map)?;
                let envelope = SecretsEnvelopeV2 {
                    v: ENVELOPE_VERSION_V2,
                    alg: ENVELOPE_ALG.to_string(),
                    kdf: ENVELOPE_KDF_V2.to_string(),
                    salt: engine.encode(salt),
                    nonce: engine.encode(nonce),
                    ciphertext: engine.encode(ciphertext),
                };
                serde_json::to_string_pretty(&envelope)?
            }
            None => {
                let (nonce, ciphertext) = Self::seal_map(&self.key, repo, map)?;
                let envelope = SecretsEnvelope {
                    v: ENVELOPE_VERSION,
                    alg: ENVELOPE_ALG.to_string(),
                    nonce: engine.encode(nonce),
                    ciphertext: engine.encode(ciphertext),
                };
                serde_json::to_string_pretty(&envelope)?
            }
        };
        write_private(&path, text.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn load(&self, repo: &str) -> HashMap<String, String> {
        let Ok(path) = self.path(repo) else {
            return HashMap::new();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return HashMap::new();
        };
        let version = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("v")?.as_u64());
        match version {
            Some(v) if v == ENVELOPE_VERSION_V2 as u64 => {
                self.decrypt_v2(repo, &text).unwrap_or_default()
            }
            Some(v) if v == ENVELOPE_VERSION as u64 => match self.decrypt_v1(repo, &text) {
                Some(map) => {
                    if self.passphrase.is_some() {
                        let _ = self.write_map(repo, &map);
                    }
                    map
                }
                None => HashMap::new(),
            },
            _ => {
                let legacy: HashMap<String, String> = match serde_json::from_str(&text) {
                    Ok(map) => map,
                    Err(_) => return HashMap::new(),
                };
                let _ = self.write_map(repo, &legacy);
                legacy
            }
        }
    }

    pub fn names(&self, repo: &str) -> Vec<String> {
        let mut names: Vec<String> = self.load(repo).keys().cloned().collect();
        names.sort();
        names
    }

    pub fn set(&self, repo: &str, key: &str, value: &str) -> Result<()> {
        if !gt_compute::runner::valid_env_key(key) {
            bail!("bad secret name");
        }
        if value.is_empty() {
            bail!("secret value is empty");
        }
        if value.len() > gt_compute::local::MAX_ENV_VALUE_BYTES {
            bail!("secret value too large");
        }
        let mut map = self.load(repo);
        map.insert(key.to_string(), value.to_string());
        self.write_map(repo, &map)
    }

    pub fn delete(&self, repo: &str, key: &str) -> Result<bool> {
        self.path(repo)?;
        let mut map = self.load(repo);
        if map.remove(key).is_none() {
            return Ok(false);
        }
        self.write_map(repo, &map)?;
        Ok(true)
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    let mut file = opts.open(path)?;
    use std::io::Write;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (RepoSecretsStore, Identity, PathBuf) {
        let dir = std::env::temp_dir().join(format!("gf_secrets_{}", uuid::Uuid::new_v4()));
        let identity = Identity::generate();
        (RepoSecretsStore::new(&dir, &identity), identity, dir)
    }

    #[test]
    fn roundtrip_and_names_sorted() {
        let (store, _identity, dir) = store();
        assert!(store.names("demo").is_empty());
        store.set("demo", "TOKEN", "abc").unwrap();
        store.set("demo", "API_KEY", "xyz").unwrap();
        assert_eq!(
            store.names("demo"),
            vec!["API_KEY".to_string(), "TOKEN".to_string()]
        );
        let loaded = store.load("demo");
        assert_eq!(loaded.get("TOKEN").unwrap(), "abc");
        assert!(store.load("other").is_empty());
        assert!(store.delete("demo", "TOKEN").unwrap());
        assert!(!store.delete("demo", "TOKEN").unwrap());
        assert_eq!(store.names("demo"), vec!["API_KEY".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_names_values_and_repos() {
        let (store, _identity, dir) = store();
        assert!(store.set("demo", "1BAD", "x").is_err());
        assert!(store.set("demo", "OK", "").is_err());
        assert!(store
            .set(
                "demo",
                "OK",
                &"x".repeat(gt_compute::local::MAX_ENV_VALUE_BYTES + 1)
            )
            .is_err());
        assert!(store.set("../escape", "OK", "x").is_err());
        assert!(store.set("a/b", "OK", "x").is_err());
        assert!(store.delete("../escape", "OK").is_err());
        assert!(store.load("../escape").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn store_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (store, _identity, dir) = store();
        store.set("demo", "TOKEN", "abc").unwrap();
        let meta = std::fs::metadata(dir.join("secrets").join("demo.json")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_file_holds_no_plaintext() {
        let (store, _identity, dir) = store();
        store.set("demo", "TOKEN", "super-secret-value").unwrap();
        let raw = std::fs::read_to_string(dir.join("secrets").join("demo.json")).unwrap();
        assert!(!raw.contains("super-secret-value"), "{raw}");
        assert!(!raw.contains("TOKEN"), "{raw}");
        let envelope: SecretsEnvelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(envelope.v, ENVELOPE_VERSION);
        assert_eq!(envelope.alg, ENVELOPE_ALG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_survives_restart_but_not_identity_change() {
        let dir = std::env::temp_dir().join(format!("gf_secrets_{}", uuid::Uuid::new_v4()));
        let identity = Identity::generate();
        RepoSecretsStore::new(&dir, &identity)
            .set("demo", "TOKEN", "abc")
            .unwrap();
        let reopened = RepoSecretsStore::new(&dir, &identity);
        assert_eq!(reopened.load("demo").get("TOKEN").unwrap(), "abc");
        let stranger = RepoSecretsStore::new(&dir, &Identity::generate());
        assert!(stranger.load("demo").is_empty());
        assert!(stranger.names("demo").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_plaintext_migrates_to_envelope_on_load() {
        let (store, _identity, dir) = store();
        let path = dir.join("secrets").join("demo.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"TOKEN\": \"legacy-value\"}").unwrap();
        let loaded = store.load("demo");
        assert_eq!(loaded.get("TOKEN").unwrap(), "legacy-value");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("legacy-value"), "{raw}");
        let envelope: SecretsEnvelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(envelope.alg, ENVELOPE_ALG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_fails_closed() {
        let (store, _identity, dir) = store();
        store.set("demo", "TOKEN", "abc").unwrap();
        let path = dir.join("secrets").join("demo.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut envelope: SecretsEnvelope = serde_json::from_str(&raw).unwrap();
        let engine = base64::engine::general_purpose::STANDARD;
        let mut ciphertext = engine.decode(envelope.ciphertext.as_bytes()).unwrap();
        ciphertext[0] ^= 0x01;
        envelope.ciphertext = engine.encode(ciphertext);
        std::fs::write(&path, serde_json::to_string(&envelope).unwrap()).unwrap();
        assert!(store.load("demo").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ciphertext_cannot_move_between_repos() {
        let (store, _identity, dir) = store();
        store.set("alpha", "TOKEN", "abc").unwrap();
        let raw = std::fs::read_to_string(dir.join("secrets").join("alpha.json")).unwrap();
        std::fs::write(dir.join("secrets").join("beta.json"), &raw).unwrap();
        assert!(store.load("beta").is_empty());
        assert_eq!(store.load("alpha").get("TOKEN").unwrap(), "abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn passphrase_store(passphrase: &[u8]) -> (RepoSecretsStore, Identity, PathBuf) {
        let dir = std::env::temp_dir().join(format!("gf_secrets_{}", uuid::Uuid::new_v4()));
        let identity = Identity::generate();
        (
            RepoSecretsStore::new(&dir, &identity).with_passphrase(Some(passphrase)),
            identity,
            dir,
        )
    }

    #[test]
    fn passphrase_seals_v2_and_roundtrips() {
        let (store, _identity, dir) = passphrase_store(b"correct horse");
        store.set("demo", "TOKEN", "v2-secret-value").unwrap();
        let raw = std::fs::read_to_string(dir.join("secrets").join("demo.json")).unwrap();
        assert!(!raw.contains("v2-secret-value"), "{raw}");
        let envelope: SecretsEnvelopeV2 = serde_json::from_str(&raw).unwrap();
        assert_eq!(envelope.v, ENVELOPE_VERSION_V2);
        assert_eq!(envelope.kdf, ENVELOPE_KDF_V2);
        assert_eq!(store.load("demo").get("TOKEN").unwrap(), "v2-secret-value");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_without_matching_passphrase_fails_closed() {
        let dir = std::env::temp_dir().join(format!("gf_secrets_{}", uuid::Uuid::new_v4()));
        let identity = Identity::generate();
        RepoSecretsStore::new(&dir, &identity)
            .with_passphrase(Some(b"right"))
            .set("demo", "TOKEN", "abc")
            .unwrap();
        let wrong = RepoSecretsStore::new(&dir, &identity).with_passphrase(Some(b"wrong"));
        assert!(wrong.load("demo").is_empty());
        let none = RepoSecretsStore::new(&dir, &identity).with_passphrase(None);
        assert!(none.load("demo").is_empty());
        let right = RepoSecretsStore::new(&dir, &identity).with_passphrase(Some(b"right"));
        assert_eq!(right.load("demo").get("TOKEN").unwrap(), "abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_upgrades_to_v2_when_passphrase_appears() {
        let dir = std::env::temp_dir().join(format!("gf_secrets_{}", uuid::Uuid::new_v4()));
        let identity = Identity::generate();
        RepoSecretsStore::new(&dir, &identity)
            .with_passphrase(None)
            .set("demo", "TOKEN", "abc")
            .unwrap();
        let path = dir.join("secrets").join("demo.json");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("\"v\": 1"), "{before}");
        let upgraded = RepoSecretsStore::new(&dir, &identity).with_passphrase(Some(b"new"));
        assert_eq!(upgraded.load("demo").get("TOKEN").unwrap(), "abc");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("\"v\": 2"), "{after}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_writes_use_fresh_salt_each_time() {
        let (store, _identity, dir) = passphrase_store(b"pepper");
        store.set("demo", "A", "1").unwrap();
        let first = std::fs::read_to_string(dir.join("secrets").join("demo.json")).unwrap();
        store.set("demo", "B", "2").unwrap();
        let second = std::fs::read_to_string(dir.join("secrets").join("demo.json")).unwrap();
        let a: SecretsEnvelopeV2 = serde_json::from_str(&first).unwrap();
        let b: SecretsEnvelopeV2 = serde_json::from_str(&second).unwrap();
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.nonce, b.nonce);
        assert_eq!(store.load("demo").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_var_configures_passphrase() {
        let key = PASSPHRASE_ENV_VAR;
        let prior = std::env::var(key).ok();
        std::env::set_var(key, "from-env");
        assert_eq!(secrets_passphrase_from_env(), Some(b"from-env".to_vec()));
        std::env::set_var(key, "");
        assert_eq!(secrets_passphrase_from_env(), None);
        std::env::remove_var(key);
        assert_eq!(secrets_passphrase_from_env(), None);
        if let Some(value) = prior {
            std::env::set_var(key, value);
        }
    }
}
