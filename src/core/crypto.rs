use aes_gcm_siv::aead::{Aead, KeyInit as AeadKeyInit, OsRng};
use aes_gcm_siv::{Aes256GcmSiv, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const NONCE_LEN: usize = 12;

/// Generates a fresh 256-bit key via the OS CSPRNG, hex-encoded to the same
/// format `Aes256GcmSivEncryptor::from_hex_key` parses. Used by `keyring add
/// encryption-key` when the user leaves the key prompt blank (ADR-0026) --
/// pigeon never generates a key on its own initiative outside that explicit
/// request.
pub(crate) fn generate_hex_key() -> String {
    let key = Aes256GcmSiv::generate_key(&mut OsRng);
    hex::encode(key)
}

/// Behavior shared by every place this CLI needs to turn plaintext bytes
/// into bytes safe to store somewhere an attacker might read without the
/// key, and back again. One real implementor today (`Aes256GcmSivEncryptor`,
/// this module): `encrypt` backs `pigeon job run email-sync`'s upload phase
/// (ADR-0025); `decrypt` backs `pigeon job run decrypt-files` (ADR-0028).
pub(crate) trait Encryptor {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String>;
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, String>;
}

/// AES-256-GCM-SIV (RFC 8452) with a nonce deterministically derived from
/// the plaintext, rather than random -- so `encrypt(key, plaintext)` is
/// always byte-identical for the same `(key, plaintext)` pair. This is what
/// lets `client::upload_if_changed`'s MD5-vs-S3-ETag duplicate detection
/// keep working unchanged against ciphertext (ADR-0025).
pub(crate) struct Aes256GcmSivEncryptor {
    cipher: Aes256GcmSiv,
    nonce_key: [u8; 32],
}

impl Aes256GcmSivEncryptor {
    /// `key_hex` is 64 hex characters (32 bytes / 256 bits). HKDF-SHA256
    /// splits it into two domain-separated subkeys so the one user-supplied
    /// secret is never used directly as both a cipher key and a hash key.
    pub(crate) fn from_hex_key(key_hex: &str) -> Result<Self, String> {
        let key_bytes = hex::decode(key_hex.trim())
            .map_err(|err| format!("encryption key is not valid hex: {err}"))?;
        if key_bytes.len() != 32 {
            return Err(format!(
                "encryption key must decode to 32 bytes (64 hex characters), got {}",
                key_bytes.len()
            ));
        }

        let hk = Hkdf::<Sha256>::new(None, &key_bytes);

        let mut enc_key = [0u8; 32];
        hk.expand(b"pigeon-file-enc", &mut enc_key)
            .map_err(|err| format!("failed to derive encryption subkey: {err}"))?;

        let mut nonce_key = [0u8; 32];
        hk.expand(b"pigeon-file-nonce", &mut nonce_key)
            .map_err(|err| format!("failed to derive nonce subkey: {err}"))?;

        let cipher = <Aes256GcmSiv as AeadKeyInit>::new(Key::<Aes256GcmSiv>::from_slice(&enc_key));

        Ok(Aes256GcmSivEncryptor { cipher, nonce_key })
    }

    fn derive_nonce(&self, plaintext: &[u8]) -> Nonce {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.nonce_key)
            .expect("HMAC-SHA256 accepts a key of any length");
        mac.update(plaintext);
        let digest = mac.finalize().into_bytes();
        *Nonce::from_slice(&digest[..NONCE_LEN])
    }
}

impl Encryptor for Aes256GcmSivEncryptor {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let nonce = self.derive_nonce(plaintext);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|err| format!("encryption failed: {err}"))?;

        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
        if ciphertext.len() < NONCE_LEN {
            return Err("ciphertext too short to contain a nonce".to_string());
        }
        let (nonce_bytes, sealed) = ciphertext.split_at(NONCE_LEN);
        self.cipher
            .decrypt(Nonce::from_slice(nonce_bytes), sealed)
            .map_err(|err| format!("decryption failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const OTHER_KEY_HEX: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    #[test]
    fn encrypt_decrypt_round_trips() {
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        let plaintext = b"hello, pigeon";
        let ciphertext = encryptor.encrypt(plaintext).unwrap();
        assert_eq!(encryptor.decrypt(&ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn encrypt_is_deterministic_for_same_key_and_plaintext() {
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        let plaintext = b"same content, every time";
        assert_eq!(
            encryptor.encrypt(plaintext).unwrap(),
            encryptor.encrypt(plaintext).unwrap()
        );
    }

    #[test]
    fn encrypt_differs_for_different_plaintext() {
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        assert_ne!(
            encryptor.encrypt(b"message one").unwrap(),
            encryptor.encrypt(b"message two").unwrap()
        );
    }

    #[test]
    fn encrypt_differs_for_different_key() {
        let a = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        let b = Aes256GcmSivEncryptor::from_hex_key(OTHER_KEY_HEX).unwrap();
        let plaintext = b"same content, different key";
        assert_ne!(a.encrypt(plaintext).unwrap(), b.encrypt(plaintext).unwrap());
    }

    #[test]
    fn from_hex_key_rejects_wrong_length() {
        assert!(Aes256GcmSivEncryptor::from_hex_key("abcd").is_err());
    }

    #[test]
    fn from_hex_key_rejects_non_hex() {
        let not_hex = "z".repeat(64);
        assert!(Aes256GcmSivEncryptor::from_hex_key(&not_hex).is_err());
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        let mut ciphertext = encryptor.encrypt(b"trust but verify").unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;
        assert!(encryptor.decrypt(&ciphertext).is_err());
    }

    #[test]
    fn decrypt_rejects_short_input() {
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        assert!(encryptor.decrypt(b"too short").is_err());
    }

    #[test]
    fn generate_hex_key_produces_a_valid_key() {
        let key = generate_hex_key();
        assert!(Aes256GcmSivEncryptor::from_hex_key(&key).is_ok());
        assert_ne!(key, generate_hex_key());
    }
}
