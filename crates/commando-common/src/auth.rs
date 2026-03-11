use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256(psk, nonce). Used by gateway to prove PSK knowledge.
pub fn compute_hmac(psk: &[u8], nonce: &[u8]) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(psk).expect("HMAC accepts any key length");
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

/// Verify an HMAC against expected PSK and nonce. Used by agent to validate.
/// Constant-time comparison to prevent timing attacks.
pub fn verify_hmac(psk: &[u8], nonce: &[u8], received: &[u8]) -> bool {
    let mut mac =
        HmacSha256::new_from_slice(psk).expect("HMAC accepts any key length");
    mac.update(nonce);
    mac.verify_slice(received).is_ok()
}

/// Generate a cryptographically random 32-byte nonce.
pub fn generate_nonce() -> [u8; 32] {
    rand::rng().random()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_round_trip() {
        let psk = b"test-secret-key";
        let nonce = [42u8; 32];
        let mac = compute_hmac(psk, &nonce);
        assert!(verify_hmac(psk, &nonce, &mac));
    }

    #[test]
    fn hmac_rejects_wrong_key() {
        let nonce = [42u8; 32];
        let mac = compute_hmac(b"correct-key", &nonce);
        assert!(!verify_hmac(b"wrong-key", &nonce, &mac));
    }

    #[test]
    fn hmac_rejects_wrong_nonce() {
        let psk = b"test-key";
        let mac = compute_hmac(psk, &[1u8; 32]);
        assert!(!verify_hmac(psk, &[2u8; 32], &mac));
    }

    #[test]
    fn nonce_is_32_bytes() {
        let nonce = generate_nonce();
        assert_eq!(nonce.len(), 32);
    }

    #[test]
    fn nonces_are_unique() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b);
    }
}
