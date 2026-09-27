//! KuGou protocol crypto: MD5 digests and raw (zero-padded) RSA with the
//! hardcoded 1024-bit service key. Mirrors `KuGouMusicApi/util/crypto.js`.

use rsa::BigUint;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;

const PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDECi0Np2UR87scwrvTr72L6oO0\n1rBbbBPriSDFPxr3Z5syug0O24QyQO8bg27+0+4kBzTBTBOZ/WWU0WryL1JSXRTX\nLgFVxtzIY41Pe7lPOgsfTCn5kZcvKhYKJesKnnJDNr5/abvTGf+rHG3YRwsCHcQ0\n8/q6ifSioBszvb3QiwIDAQAB\n-----END PUBLIC KEY-----";

/// Random alphanumeric string from the same charset as the JS client
/// (digits + uppercase letters).
pub fn random_string(len: usize) -> String {
    const CHARSET: &[u8] = b"1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    (0..len)
        .map(|_| CHARSET[rand::random_range(0..CHARSET.len())] as char)
        .collect()
}

/// Lowercase hex, two digits per byte.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    for byte in &mut b {
        *byte = rand::random::<u8>();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex_lower(&b[0..4]),
        hex_lower(&b[4..6]),
        hex_lower(&b[6..8]),
        hex_lower(&b[8..10]),
        hex_lower(&b[10..16])
    )
}

pub fn md5_hex(data: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data.as_bytes());
    hex_lower(&hasher.finalize())
}

/// Device MID: the MD5 of the GUID interpreted as a 128-bit big-endian
/// number and rendered in decimal (mirrors `calculateMid` in util.js).
pub fn calculate_mid(input: &str) -> String {
    let digest = parse_hex(&md5_hex(input)).expect("md5 hex is always valid");
    BigUint::from_bytes_be(&digest).to_string()
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

fn service_key() -> rsa::RsaPublicKey {
    rsa::RsaPublicKey::from_public_key_pem(PUBLIC_KEY_PEM).expect("hardcoded key is valid")
}

/// `cryptoRSAEncrypt`: textbook RSA on zero-padded input (no OAEP/PKCS#1),
/// returns lowercase hex. Callers that need uppercase do it themselves.
pub fn rsa_raw_encrypt(data: &str) -> String {
    let key = service_key();
    let key_len = key.n().bits().div_ceil(8);
    let bytes = data.as_bytes();
    let mut padded = vec![0u8; key_len];
    padded[..bytes.len()].copy_from_slice(bytes);
    let encrypted = BigUint::from_bytes_be(&padded).modpow(key.e(), key.n());
    format!(
        "{:0>width$}",
        encrypted.to_str_radix(16),
        width = key_len * 2
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn mid_is_decimal_of_md5_hex() {
        let mid = calculate_mid("550e8400-e29b-41d4-a716-446655440000");
        assert!(mid.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(mid.len(), 39); // 128-bit number, no leading zero
    }

    #[test]
    fn random_string_uses_client_charset() {
        let s = random_string(24);
        assert_eq!(s.len(), 24);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn rsa_raw_encrypt_is_128_bytes_hex() {
        let hex = rsa_raw_encrypt(r#"{"token":"abc","clienttime":123}"#);
        assert_eq!(hex.len(), 256);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
