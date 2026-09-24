//! Symmetric encrypt/decrypt for a site's own data (AES-256-GCM).
//!
//! Two key modes:
//!   * default — a per-origin key derived from a random machine master key
//!     (`crypto.key` in the data dir) and the origin, so a site can encrypt on
//!     one visit and decrypt on the next with no key handling of its own, and
//!     one site can never read another's ciphertext.
//!   * password — key = SHA-256("pw:" + password), for data the user wants
//!     portable across machines behind a passphrase.
//!
//! Output is base64( 12-byte nonce || ciphertext+tag ). Decrypt reverses it.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::Path;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Load (or create) the 32-byte machine master key.
fn master(data_dir: &Path) -> [u8; 32] {
    let path = data_dir.join("crypto.key");
    if let Ok(b) = std::fs::read(&path) {
        if b.len() == 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            return k;
        }
    }
    use rand::RngCore;
    let mut k = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut k);
    let _ = std::fs::write(&path, k);
    k
}

fn key(data_dir: &Path, origin: &str, password: Option<&str>) -> [u8; 32] {
    let mut h = Sha256::new();
    match password {
        Some(p) => {
            h.update(b"pw:");
            h.update(p.as_bytes());
        }
        None => {
            h.update(master(data_dir));
            h.update(b"|");
            h.update(origin.as_bytes());
        }
    }
    h.finalize().into()
}

pub fn encrypt(data_dir: &Path, origin: &str, password: Option<&str>, plaintext: &[u8]) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(&key(data_dir, origin, password)).map_err(|e| e.to_string())?;
    use rand::RngCore;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(b64().encode(out))
}

pub fn decrypt(data_dir: &Path, origin: &str, password: Option<&str>, token: &str) -> Result<Vec<u8>, String> {
    let raw = b64().decode(token.trim()).map_err(|_| "not valid base64".to_string())?;
    if raw.len() < 13 {
        return Err("ciphertext too short".into());
    }
    let (nonce, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(&key(data_dir, origin, password)).map_err(|e| e.to_string())?;
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "wrong key or corrupted data".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_isolation() {
        let dir = std::env::temp_dir().join(format!("ws_crypto_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Per-origin: same origin round-trips; another origin cannot read it.
        let tok = encrypt(&dir, "https://a.example", None, b"secret").unwrap();
        assert_eq!(decrypt(&dir, "https://a.example", None, &tok).unwrap(), b"secret");
        assert!(decrypt(&dir, "https://b.example", None, &tok).is_err());

        // Password mode is origin-independent.
        let tok = encrypt(&dir, "https://a.example", Some("hunter2"), b"data").unwrap();
        assert_eq!(decrypt(&dir, "https://z.example", Some("hunter2"), &tok).unwrap(), b"data");
        assert!(decrypt(&dir, "https://a.example", Some("wrong"), &tok).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
