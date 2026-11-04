use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::Sha256;
use std::path::Path;
use zeroize::Zeroizing;

const PBKDF2_ITERATIONS: u32 = 210_000;
const KEY_LEN: usize = 32;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KeystoreCrypto {
    cipher: String,
    kdf: String,
    iterations: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KeystoreFile {
    address: String,
    crypto: KeystoreCrypto,
}

fn derive_key(password: &[u8], salt: &[u8]) -> Zeroizing<[u8; KEY_LEN]> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    pbkdf2_hmac::<Sha256>(password, salt, PBKDF2_ITERATIONS, &mut *key);
    key
}

pub const PBKDF2_SHA256_ITERATIONS: u32 = PBKDF2_ITERATIONS;

pub fn derive_passphrase_key(password: &[u8], salt: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2_hmac::<Sha256>(password, salt, PBKDF2_ITERATIONS, &mut key);
    key
}

fn write_secret_json(path: &Path, value: &KeystoreFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    std::io::Write::write_all(&mut opts.open(path)?, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn create_keystore(
    path: &Path,
    secret: &[u8; KEY_LEN],
    password: &[u8],
    address: &str,
) -> anyhow::Result<()> {
    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);
    let key = derive_key(password, &salt);
    let cipher = ChaCha20Poly1305::new_from_slice(&key[..])
        .map_err(|e| anyhow::anyhow!("key init failed: {e}"))?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("nonce init failed: {e}"))?;
    let ciphertext = cipher
        .encrypt(&nonce, secret.as_slice())
        .map_err(|e| anyhow::anyhow!("encrypt failed: {e}"))?;
    write_secret_json(
        path,
        &KeystoreFile {
            address: address.to_lowercase(),
            crypto: KeystoreCrypto {
                cipher: "chacha20poly1305".into(),
                kdf: "pbkdf2-sha256".into(),
                iterations: PBKDF2_ITERATIONS,
                salt: hex::encode(salt),
                nonce: hex::encode(nonce_bytes),
                ciphertext: hex::encode(ciphertext),
            },
        },
    )
}

pub fn unlock_keystore(path: &Path, password: &[u8]) -> anyhow::Result<Zeroizing<[u8; KEY_LEN]>> {
    let raw = std::fs::read(path)?;
    let file: KeystoreFile = serde_json::from_slice(&raw)?;
    if file.crypto.cipher != "chacha20poly1305" || file.crypto.kdf != "pbkdf2-sha256" {
        anyhow::bail!("unsupported keystore crypto");
    }
    let salt = hex::decode(&file.crypto.salt)?;
    let nonce_bytes = hex::decode(&file.crypto.nonce)?;
    let ciphertext = hex::decode(&file.crypto.ciphertext)?;
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    pbkdf2_hmac::<Sha256>(password, &salt, file.crypto.iterations, &mut *key);
    let cipher = ChaCha20Poly1305::new_from_slice(&key[..])
        .map_err(|e| anyhow::anyhow!("key init failed: {e}"))?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("bad password or corrupt keystore"))?;
    let secret = cipher
        .decrypt(&nonce, ciphertext.as_slice())
        .map_err(|_| anyhow::anyhow!("bad password or corrupt keystore"))?;
    if secret.len() != KEY_LEN {
        anyhow::bail!("bad password or corrupt keystore");
    }
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    out.copy_from_slice(&secret);
    Ok(out)
}

pub fn keystore_address(path: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read(path)?;
    let file: KeystoreFile = serde_json::from_slice(&raw)?;
    Ok(file.address)
}

/// Rotate an EVM signing key: verify the old keystore unlocks and its key
/// matches its filed address, then seal a fresh random secret into a new
/// keystore. Refuses to overwrite. Returns (old_address, new_address).
/// Callers must still fund the new address, point the backend config at
/// the new file, restart, and drain/delete the old key.
pub fn rotate_keystore(
    old_path: &Path,
    old_password: &[u8],
    new_path: &Path,
    new_password: &[u8],
) -> anyhow::Result<(String, String)> {
    if new_path.exists() {
        anyhow::bail!("refusing to overwrite {}", new_path.display());
    }
    let old_secret = unlock_keystore(old_path, old_password)?;
    let old_address = keystore_address(old_path)?;
    if eth_address_of_secret(&old_secret)? != old_address.to_lowercase() {
        anyhow::bail!("old keystore address does not match its key");
    }
    let mut fresh = Zeroizing::new([0u8; KEY_LEN]);
    let mut new_address = String::new();
    for _ in 0..8 {
        rand::thread_rng().fill_bytes(&mut fresh[..]);
        if let Ok(addr) = eth_address_of_secret(&fresh) {
            new_address = addr;
            break;
        }
    }
    if new_address.is_empty() {
        anyhow::bail!("could not generate a valid secret key");
    }
    create_keystore(new_path, &fresh, new_password, &new_address)?;
    Ok((old_address, new_address))
}

pub fn eth_address_of_secret(secret: &[u8; 32]) -> anyhow::Result<String> {
    let key = secp256k1::SecretKey::from_secret_bytes(*secret)
        .map_err(|e| anyhow::anyhow!("invalid secret key: {e}"))?;
    let public = secp256k1::PublicKey::from_secret_key(&key).serialize_uncompressed();
    let mut hasher = tiny_keccak::Keccak::v256();
    use tiny_keccak::Hasher;
    hasher.update(&public[1..]);
    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash);
    Ok(format!("0x{}", hex::encode(&hash[12..])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gf_ks_{name}_{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn keystore_roundtrip() {
        let path = tmp_path("roundtrip");
        let secret = [0xabu8; 32];
        create_keystore(&path, &secret, b"correct horse", "0xABC").unwrap();
        let back = unlock_keystore(&path, b"correct horse").unwrap();
        assert_eq!(&back[..], &secret);
        assert_eq!(keystore_address(&path).unwrap(), "0xabc");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keystore_wrong_password_fails() {
        let path = tmp_path("wrongpw");
        create_keystore(&path, &[1u8; 32], b"right", "0x0").unwrap();
        assert!(unlock_keystore(&path, b"wrong").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keystore_rotate_yields_fresh_unlockable_key() {
        let old = tmp_path("rot-old");
        let new = tmp_path("rot-new");
        let addr7 = eth_address_of_secret(&[7u8; 32]).unwrap();
        create_keystore(&old, &[7u8; 32], b"oldpw", &addr7).unwrap();
        let old_addr = keystore_address(&old).unwrap();
        let (got_old, got_new) = rotate_keystore(&old, b"oldpw", &new, b"newpw").unwrap();
        assert_eq!(got_old, old_addr);
        assert_ne!(got_new, got_old);
        assert_eq!(keystore_address(&new).unwrap(), got_new);
        let secret = unlock_keystore(&new, b"newpw").unwrap();
        assert_eq!(eth_address_of_secret(&secret).unwrap(), got_new);
        assert!(unlock_keystore(&new, b"oldpw").is_err());
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(&new);
    }

    #[test]
    fn keystore_rotate_refuses_overwrite_and_bad_password() {
        let old = tmp_path("rotne-old");
        let new = tmp_path("rotne-new");
        let addr9 = eth_address_of_secret(&[9u8; 32]).unwrap();
        create_keystore(&old, &[9u8; 32], b"pw", &addr9).unwrap();
        std::fs::write(&new, b"sentinel").unwrap();
        assert!(rotate_keystore(&old, b"pw", &new, b"pw2").is_err());
        assert_eq!(std::fs::read(&new).unwrap(), b"sentinel");
        let new2 = tmp_path("rotne-new2");
        assert!(rotate_keystore(&old, b"wrong", &new2, b"pw2").is_err());
        assert!(!new2.exists());
        let lying = tmp_path("rotne-lying");
        create_keystore(&lying, &[9u8; 32], b"pw", "0xdeadbeef").unwrap();
        let new3 = tmp_path("rotne-new3");
        assert!(rotate_keystore(&lying, b"pw", &new3, b"pw2").is_err());
        assert!(!new3.exists());
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(&new);
        let _ = std::fs::remove_file(&lying);
    }

    #[test]
    fn keystore_tampered_ciphertext_fails() {
        let path = tmp_path("tamper");
        create_keystore(&path, &[2u8; 32], b"pw", "0x0").unwrap();
        let mut file: KeystoreFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut ct = hex::decode(&file.crypto.ciphertext).unwrap();
        ct[0] ^= 0xff;
        file.crypto.ciphertext = hex::encode(ct);
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(unlock_keystore(&path, b"pw").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[cfg(unix)]
    fn keystore_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("perms");
        create_keystore(&path, &[3u8; 32], b"pw", "0x0").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&path);
    }
}
