use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct EncryptedBlob {
    nonce: String,
    ciphertext: String,
}

fn derive_key() -> [u8; 32] {
    let machine_id = machine_uid::get().unwrap_or_else(|_| "webtorapp-fallback".to_string());
    let seed = format!("{machine_id}webtorapp-v1");
    *blake3::hash(seed.as_bytes()).as_bytes()
}

/// Encrypts arbitrary data with a machine-bound AES-256-GCM key and writes it
/// to `path`. Used for anything sensitive enough to warrant encryption at
/// rest - session cookies, saved credentials, etc.
pub fn encrypt_and_save(data: &str, path: &Path) -> Result<()> {
    let key_bytes = derive_key();
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

    let ciphertext = cipher
        .encrypt(&nonce, data.as_bytes())
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    let stored = EncryptedBlob {
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ciphertext),
    };
    std::fs::write(path, serde_json::to_string(&stored)?)?;
    Ok(())
}

/// Reads back data written by [`encrypt_and_save`]. Returns `None` on any
/// failure (missing file, wrong machine, corrupt data) rather than erroring,
/// since callers treat "no saved data" and "couldn't decrypt it" the same way.
pub fn load_and_decrypt(path: &Path) -> Option<String> {
    let json = std::fs::read_to_string(path).ok()?;
    let stored: EncryptedBlob = serde_json::from_str(&json).ok()?;

    let key_bytes = derive_key();
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);

    let nonce_bytes = B64.decode(&stored.nonce).ok()?;
    let ciphertext_bytes = B64.decode(&stored.ciphertext).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = cipher.decrypt(nonce, ciphertext_bytes.as_ref()).ok()?;
    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_derivation_is_deterministic() {
        assert_eq!(derive_key(), derive_key());
    }

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let dir = std::env::temp_dir().join(format!("webtorapp-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob.enc");

        encrypt_and_save("hello world", &path).unwrap();
        let restored = load_and_decrypt(&path).unwrap();
        assert_eq!(restored, "hello world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_returns_none() {
        let path = std::env::temp_dir().join("webtorapp-auth-test-missing.enc");
        assert!(load_and_decrypt(&path).is_none());
    }
}
