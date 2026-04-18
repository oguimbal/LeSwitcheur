//! License token verification.
//!
//! Tokens are issued by the backend (`leswitcheur.app`) as `base64url(json_payload).base64url(sig)`,
//! signed with Ed25519. The app embeds the corresponding public key and verifies
//! tokens offline — no network call after activation.

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Public Ed25519 verification key (raw 32-byte encoding, hex).
///
/// Generated with `bun run scripts/gen-ed25519.ts` in the webapp repo.
/// The matching private key is stored as a Cloudflare Worker secret.
pub const PUBLIC_KEY_HEX: &str =
    "d47a67ab5dbaeb531222b750d0c16f6265f3a632da1054a1a936b189d21fc5d9";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LicenseToken {
    /// Opaque license key (e.g. `LSWT-XXXX-XXXX-XXXX`).
    pub key: String,
    /// Unix epoch seconds at which the token was issued.
    pub issued_at: u64,
}

/// Decode a hex string into exactly N bytes.
fn decode_hex<const N: usize>(s: &str) -> Result<[u8; N]> {
    if s.len() != N * 2 {
        bail!("hex length {} != {}", s.len(), N * 2);
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| anyhow!("invalid hex at {}: {e}", i * 2))?;
    }
    Ok(out)
}

/// Verify a token string against a raw 32-byte Ed25519 public key.
///
/// Accepts format `base64url(payload_json).base64url(signature)`.
pub fn verify(token: &str, public_key: &[u8; 32]) -> Result<LicenseToken> {
    let (payload_b64, sig_b64) = token
        .split_once('.')
        .ok_or_else(|| anyhow!("token missing '.' separator"))?;

    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| anyhow!("payload base64: {e}"))?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| anyhow!("signature base64: {e}"))?;

    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| anyhow!("signature wrong length: {e}"))?;
    let key = VerifyingKey::from_bytes(public_key).map_err(|e| anyhow!("invalid pubkey: {e}"))?;
    key.verify(&payload, &signature)
        .map_err(|e| anyhow!("signature invalid: {e}"))?;

    let token: LicenseToken =
        serde_json::from_slice(&payload).map_err(|e| anyhow!("payload json: {e}"))?;
    Ok(token)
}

/// Convenience: verify using the embedded public key.
pub fn verify_embedded(token: &str) -> Result<LicenseToken> {
    let key = decode_hex::<32>(PUBLIC_KEY_HEX)?;
    verify(token, &key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn sign_token(key: &str, issued_at: u64, sk: &SigningKey) -> String {
        let payload = serde_json::to_vec(&LicenseToken {
            key: key.into(),
            issued_at,
        })
        .unwrap();
        let sig = sk.sign(&payload);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        )
    }

    #[test]
    fn valid_token_roundtrips() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let token = sign_token("LSWT-ABCD-EFGH-JKLM", 1_700_000_000, &sk);
        let decoded = verify(&token, &pk).unwrap();
        assert_eq!(decoded.key, "LSWT-ABCD-EFGH-JKLM");
        assert_eq!(decoded.issued_at, 1_700_000_000);
    }

    #[test]
    fn corrupted_signature_fails() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let mut token = sign_token("x@y.z", 0, &sk);
        // Flip a bit in the signature segment.
        let last = token.pop().unwrap();
        token.push(if last == 'A' { 'B' } else { 'A' });
        assert!(verify(&token, &pk).is_err());
    }

    #[test]
    fn tampered_payload_fails() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let token = sign_token("user@example.com", 1, &sk);
        // Swap payload for a different one but keep the signature.
        let (_, sig) = token.split_once('.').unwrap();
        let fake_payload = URL_SAFE_NO_PAD.encode(br#"{"key":"LSWT-XXXX-XXXX-XXXX","issued_at":2}"#);
        let forged = format!("{fake_payload}.{sig}");
        assert!(verify(&forged, &pk).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let sk = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng);
        let pk_other = other.verifying_key().to_bytes();
        let token = sign_token("a@b.c", 0, &sk);
        assert!(verify(&token, &pk_other).is_err());
    }

    #[test]
    fn missing_separator_fails() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        assert!(verify("notatoken", &pk).is_err());
    }
}
