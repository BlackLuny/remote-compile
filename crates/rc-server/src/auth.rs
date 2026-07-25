//! Authentication for the three distinct principals (§16): coding agents
//! (bearer token), workers (enrollment-issued token) and console users
//! (password + session cookie).

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

pub const SESSION_COOKIE: &str = "rc_session";

/// Tokens are stored hashed: a leaked database should not hand out fleet
/// access.
pub fn hash_token(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hash password: {e}"))
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Pull a bearer token out of gRPC metadata.
pub fn bearer_from_metadata(md: &tonic::metadata::MetadataMap) -> Option<String> {
    let raw = md.get("authorization")?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
}

/// Pull a named cookie out of a `Cookie` header value.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_verify_and_reject() {
        let h = hash_password("correct horse").unwrap();
        assert!(verify_password(&h, "correct horse"));
        assert!(!verify_password(&h, "wrong horse"));
    }

    #[test]
    fn each_hash_uses_a_fresh_salt() {
        assert_ne!(hash_password("same").unwrap(), hash_password("same").unwrap());
    }

    #[test]
    fn a_corrupt_hash_never_authenticates() {
        assert!(!verify_password("not-a-hash", "anything"));
        assert!(!verify_password("", ""));
    }

    #[test]
    fn token_hashing_is_stable_and_one_way() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), "abc");
        assert_eq!(hash_token("abc").len(), 64);
    }

    #[test]
    fn bearer_parsing_accepts_both_cases() {
        let mut md = tonic::metadata::MetadataMap::new();
        md.insert("authorization", "Bearer secret".parse().unwrap());
        assert_eq!(bearer_from_metadata(&md).as_deref(), Some("secret"));
        md.insert("authorization", "bearer secret".parse().unwrap());
        assert_eq!(bearer_from_metadata(&md).as_deref(), Some("secret"));
    }

    #[test]
    fn missing_or_malformed_authorization_yields_none() {
        let md = tonic::metadata::MetadataMap::new();
        assert!(bearer_from_metadata(&md).is_none());
        let mut md = tonic::metadata::MetadataMap::new();
        md.insert("authorization", "Basic abc".parse().unwrap());
        assert!(bearer_from_metadata(&md).is_none());
    }

    #[test]
    fn cookie_extraction_picks_the_right_pair() {
        let header = "theme=dark; rc_session=abc123; other=1";
        assert_eq!(cookie_value(header, SESSION_COOKIE).as_deref(), Some("abc123"));
        assert!(cookie_value(header, "nope").is_none());
    }
}
