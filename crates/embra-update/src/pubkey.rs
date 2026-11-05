//! The release signing keys: Ed25519 public keys as OpenSSL writes them
//! (SubjectPublicKeyInfo PEM, 44 DER bytes: a fixed 12-byte prefix and the
//! 32-byte key). Several `PUBLIC KEY` blocks in one file are several keys
//! (rotation); `#` lines and anything outside a block are ignored.

use base64::Engine as _;
use std::path::Path;

/// The key the image ships (`scripts/update-signing.pub`, baked in).
pub const SHIPPED_KEYS: &str = "/usr/share/embra/update-signing.pub";
/// The operator's own keys, a STATE drop-in.
pub const STATE_KEYS: &str = "/embra/state/update-signing.pub";
/// DER: SEQUENCE { SEQUENCE { OID 1.3.101.112 }, BIT STRING (33 bytes, 0 unused) }.
const SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

/// Every Ed25519 key in a PEM text; a block that is not one is skipped.
pub fn parse_keys(text: &str) -> Vec<[u8; 32]> {
    let mut keys = Vec::new();
    let mut body: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "-----BEGIN PUBLIC KEY-----" {
            body = Some(String::new());
        } else if line == "-----END PUBLIC KEY-----" {
            if let Some(b) = body.take()
                && let Ok(der) = base64::engine::general_purpose::STANDARD.decode(b)
                && der.len() == 44
                && der[..12] == SPKI_PREFIX
            {
                let mut key = [0u8; 32];
                key.copy_from_slice(&der[12..]);
                keys.push(key);
            }
        } else if let Some(b) = body.as_mut() {
            b.push_str(line);
        }
    }
    keys
}

/// The keys of every readable file among `paths`.
pub fn load_keys(paths: &[&Path]) -> Vec<[u8; 32]> {
    paths.iter().filter_map(|p| std::fs::read_to_string(p).ok()).flat_map(|t| parse_keys(&t)).collect()
}

/// A key as the PEM OpenSSL writes (the tests).
#[cfg(test)]
pub fn to_pem(key: &[u8; 32]) -> String {
    let mut der = SPKI_PREFIX.to_vec();
    der.extend_from_slice(key);
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    format!("-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openssl_spki_pem_yields_the_thirty_two_key_bytes_and_a_wrong_prefix_is_refused() {
        let key = [7u8; 32];
        let pem = to_pem(&key);
        assert_eq!(parse_keys(&pem), vec![key]);
        let two = format!("# a comment\n{pem}\n{}", to_pem(&[9u8; 32]));
        assert_eq!(parse_keys(&two), vec![key, [9u8; 32]]);
        // An RSA-looking block (another prefix) and a short one are skipped.
        let bad = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VuAyEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n-----END PUBLIC KEY-----\n";
        assert!(parse_keys(bad).is_empty(), "X25519's OID is not Ed25519's");
        assert!(parse_keys("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n").is_empty());
        assert!(parse_keys("# nothing here\n").is_empty());
        assert!(load_keys(&[Path::new("/nonexistent/keys.pub")]).is_empty());
    }
}
