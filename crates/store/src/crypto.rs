use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use feedbot_core::StoreError;
use rand::RngCore;

pub struct SecretBox {
    cipher: ChaCha20Poly1305,
}

impl SecretBox {
    /// 32 random bytes encoded as 64 lowercase hex characters.
    pub fn generate_hex_key() -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        hex::encode(bytes)
    }

    pub fn from_hex_key(hex_key: &str) -> Result<Self, StoreError> {
        let bytes = hex::decode(hex_key.trim()).map_err(|e| StoreError::Crypto(e.to_string()))?;
        if bytes.len() != 32 {
            return Err(StoreError::Crypto(
                "FEEDBOT__SECRETS__MASTER_KEY 必须是 32 字节 hex（64 个字符）".into(),
            ));
        }
        let key = Key::from_slice(&bytes);
        Ok(Self {
            cipher: ChaCha20Poly1305::new(key),
        })
    }

    /// Encrypt `plaintext` bound to `aad`, so a ciphertext copied to another
    /// row (different owner/platform) fails to decrypt.
    pub fn encrypt(&self, plaintext: &[u8], aad: &[u8]) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
        let mut nonce_bytes = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| StoreError::Crypto(e.to_string()))?;
        Ok((nonce_bytes.to_vec(), ciphertext))
    }

    /// Decrypt with `aad`, falling back to rows written before AAD binding.
    pub fn decrypt(
        &self,
        nonce: &[u8],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, StoreError> {
        if nonce.len() != 12 {
            return Err(StoreError::Crypto("nonce 长度错误".into()));
        }
        let nonce = Nonce::from_slice(nonce);
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .or_else(|_| self.cipher.decrypt(nonce, ciphertext))
            .map_err(|e| StoreError::Crypto(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_is_32_byte_hex() {
        let key = SecretBox::generate_hex_key();
        assert_eq!(key.len(), 64);
        assert!(SecretBox::from_hex_key(&key).is_ok());
        let other = SecretBox::generate_hex_key();
        assert_ne!(key, other);
    }
}
