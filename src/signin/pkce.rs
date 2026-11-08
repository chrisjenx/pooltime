//! PKCE (RFC 7636, S256) and the random values of a sign-in flow.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngExt;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// A PKCE verifier and its S256 challenge.
#[derive(Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Pkce {
        Pkce::from_verifier(random_token())
    }

    pub fn from_verifier(verifier: String) -> Pkce {
        let challenge = challenge(&verifier);
        Pkce {
            verifier,
            challenge,
        }
    }
}

/// `base64url(SHA-256(verifier))`, unpadded.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 32 random bytes as 43 unpadded base64url characters (a valid PKCE verifier and OAuth state).
pub fn random_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Constant-time equality for `state` values (the length is not secret).
pub fn state_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_vector() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_values_are_url_safe_and_distinct() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(a.challenge, challenge(&a.verifier));
        assert!((43..=128).contains(&a.verifier.len()));
        for s in [&a.verifier, &a.challenge, &random_token(), &random_token()] {
            assert!(s
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        }
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn state_eq_is_exact() {
        assert!(state_eq("abc", "abc"));
        assert!(!state_eq("abc", "abd"));
        assert!(!state_eq("abc", "abcd"));
        assert!(!state_eq("", "a"));
    }
}
