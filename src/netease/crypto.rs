//! NetEase Cloud Music protocol crypto: AES-128-CBC/ECB with PKCS7, MD5
//! digests and raw (zero-padded) RSA with the hardcoded 1024-bit web key.
//! Mirrors `NeteaseCloudMusicApi/util/crypto.js`.

use std::sync::OnceLock;

use aes::Aes128;
use aes::cipher::{BlockEncryptMut, KeyInit, KeyIvInit};
use cbc::cipher::block_padding::Pkcs7;
use num_bigint_dig::BigUint;

const IV: &[u8; 16] = b"0102030405060708";
const PRESET_KEY: &[u8; 16] = b"0CoJUm6Qyw8W8jud";
const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
/// Same 62-char alphabet the JS client draws the random weapi secret from.
const BASE62: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// Fixed XOR key for the anonymous-token device fingerprint.
const ID_XOR_KEY: &[u8] = b"3go8&$8*3*3h0k(2)2";

/// 1024-bit RSA modulus of the weapi web key (`publicKey` in crypto.js),
/// exponent 0x10001. The web client encrypts the reversed random AES key with
/// it using no padding at all.
const RSA_MODULUS_HEX: &str = "E0B509F6259DF8642DBC35662901477DF22677EC152B5FF68ACE615BB7B725152B3AB17A876AEA8A5AA76D2E417629EC4EE341F56135FCCF695280104E0312ECBDA92557C93870114AF6C9D05C4F7F0C3685B7A46BEE255932575CCE10B424D813CFE4875D3E82047B97DDEF52741D546B8E289DC6935B3ECE0462DB0A22B8E7";

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128EcbEnc = ecb::Encryptor<Aes128>;

/// Random alphanumeric (base62) string, as used for the per-request weapi
/// secret key.
pub fn base62_string(len: usize) -> String {
    (0..len)
        .map(|_| BASE62[rand::random_range(0..BASE62.len())] as char)
        .collect()
}

/// `len` random lowercase hex characters (2*len random bytes -> hex is fine
/// for identifiers; the JS client also just draws WordArray.random bytes).
pub fn random_hex(len: usize) -> String {
    (0..len)
        .map(|_| format!("{:x}", rand::random_range(0..16u8)))
        .collect()
}

/// 52 random uppercase hex characters, mirroring `generateDeviceId()`.
pub fn generate_device_id() -> String {
    let mut out = random_hex(52);
    out.make_ascii_uppercase();
    out
}

pub fn md5_bytes(data: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub fn md5_hex(data: &[u8]) -> String {
    md5_bytes(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn aes_cbc_encrypt_b64(data: &[u8], key: &[u8; 16]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(Aes128CbcEnc::new(key.into(), IV.into()).encrypt_padded_vec_mut::<Pkcs7>(data))
}

fn aes_ecb_encrypt_hex_upper(data: &[u8], key: &[u8; 16]) -> String {
    Aes128EcbEnc::new(key.into())
        .encrypt_padded_vec_mut::<Pkcs7>(data)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

/// Textbook RSA (no padding) with the web public key: the input is
/// interpreted as a big-endian integer (so it is effectively left-padded with
/// zeros to the key size), output is 256 lowercase hex chars.
fn rsa_raw_encrypt(data: &[u8]) -> String {
    // The modulus is a hardcoded constant; parse it once instead of on every
    // weapi request.
    static RSA_KEY: OnceLock<(BigUint, BigUint)> = OnceLock::new();
    let (modulus, exponent) = RSA_KEY.get_or_init(|| {
        (
            BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16).expect("hardcoded modulus"),
            BigUint::from(0x010001u32),
        )
    });
    let key_len = modulus.bits().div_ceil(8);
    let mut padded = vec![0u8; key_len];
    padded[key_len - data.len()..].copy_from_slice(data);
    let encrypted = BigUint::from_bytes_be(&padded).modpow(exponent, modulus);
    format!(
        "{:0>width$}",
        encrypted.to_str_radix(16),
        width = key_len * 2
    )
}

/// `crypto.weapi(object)`: double AES-128-CBC (fixed preset key, then a random
/// 16-byte key) for `params`, raw RSA of the reversed random key for
/// `encSecKey`. Returns `(params, encSecKey)`.
pub fn weapi(json_text: &str) -> (String, String) {
    let secret = base62_string(16);
    weapi_with_secret(json_text, &secret)
}

/// Deterministic variant of [`weapi`] used by the golden-vector tests.
pub(crate) fn weapi_with_secret(json_text: &str, secret: &str) -> (String, String) {
    assert_eq!(secret.len(), 16, "weapi secret must be 16 chars");
    let inner = aes_cbc_encrypt_b64(json_text.as_bytes(), PRESET_KEY);
    let params = aes_cbc_encrypt_b64(
        inner.as_bytes(),
        secret.as_bytes().try_into().expect("16 bytes"),
    );
    let reversed: String = secret.chars().rev().collect();
    (params, rsa_raw_encrypt(reversed.as_bytes()))
}

/// `crypto.eapi(url, object)`: AES-128-ECB over
/// `{url}-36cd479b6b5-{text}-36cd479b6b5-{md5("nobody{url}use{text}md5forencrypt")}`,
/// uppercase hex output.
pub fn eapi(uri: &str, json_text: &str) -> String {
    let message = format!("nobody{uri}use{json_text}md5forencrypt");
    let digest = md5_hex(message.as_bytes());
    let data = format!("{uri}-36cd479b6b5-{json_text}-36cd479b6b5-{digest}");
    aes_ecb_encrypt_hex_upper(data.as_bytes(), EAPI_KEY)
}

/// `cloudmusic_dll_encode_id` + the final base64 username for
/// `/api/register/anonimous`: `base64("{deviceId} {base64(md5(xor(deviceId)))}")`.
pub fn anonymous_username(device_id: &str) -> String {
    use base64::Engine;
    let xored: Vec<u8> = device_id
        .bytes()
        .enumerate()
        .map(|(i, byte)| byte ^ ID_XOR_KEY[i % ID_XOR_KEY.len()])
        .collect();
    let inner = base64::engine::general_purpose::STANDARD.encode(md5_bytes(&xored));
    base64::engine::general_purpose::STANDARD.encode(format!("{device_id} {inner}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors generated with the reference implementation
    /// (`api-enhanced/util/crypto.js` semantics, node built-ins).
    #[test]
    fn eapi_matches_reference_vector() {
        let text = r#"{"c":"[{\"id\":186016}]","e_r":false}"#;
        let params = eapi("/api/v3/song/detail", text);
        assert_eq!(
            params,
            "11675D69CF25E0559750EF4BF81ECC680607C372872A3914F346ED123220A487D1D3764D2399095F2190002208159F14390DCD8013F9F7D457B5FE53F57D577EC61CD2BA645F7F925CFB111BBE1EB5345C782A668EFE0ABA4EDAC7E9CE0ABEE1EEEB81C0D6EF40D8AAF3F4F1BE84F60F0E3518D12035E83E611685D3C251D7C5"
        );
    }

    #[test]
    fn weapi_matches_reference_vector() {
        let text = r#"{"ids":"[186016]","e_r":false,"csrf_token":""}"#;
        let (params, sec) = weapi_with_secret(text, "abcdefghijklmnop");
        assert_eq!(
            params,
            "6j98g3yF13VGmIImUVxKu7nBNk6F/X0IIqMkhbdxlCeTUfwigYZrQ1FVmju1PVhu0alcSVGJVly775eE+rlbf6Ae1sWy7f74YCdX9ZMfphM="
        );
        assert_eq!(
            sec,
            "d15a1683c992095d0c234c19966605c5c5964911268bbeda8cb8d08d834913e59d53b32358903a121b5fca784c1f5ae44951fd02524df58ecc98e52cc7cf8689b42c2e93ddf05b0592512d87f5960467e2f086c018849d76014d323500e30f13ef4cafbb0cf5a66731a3f1776c75ca35d0062dac70a3e33245afabcf47938487"
        );
    }

    #[test]
    fn anonymous_username_matches_reference_vector() {
        let device_id = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123";
        assert_eq!(
            anonymous_username(device_id),
            "MDEyMzQ1Njc4OUFCQ0RFRjAxMjM0NTY3ODlBQkNERUYwMTIzNDU2Nzg5QUJDREVGMDEyMyBYa2pIc2o5dnlXcTVRNDdCdXYyVWNnPT0="
        );
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn generated_identifiers_have_reference_shape() {
        let device_id = generate_device_id();
        assert_eq!(device_id.len(), 52);
        assert!(device_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(random_hex(64).chars().all(|c| c.is_ascii_hexdigit()));
        let secret = base62_string(16);
        assert_eq!(secret.len(), 16);
        assert!(secret.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
