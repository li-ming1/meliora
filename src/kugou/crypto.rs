//! KuGou protocol crypto: MD5 digests and raw (zero-padded) RSA with the
//! hardcoded 1024-bit service key. Mirrors `KuGouMusicApi/util/crypto.js`.

use std::sync::OnceLock;

use base64::Engine;
use num_bigint_dig::BigUint;

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

/// 服务钥 (n, e)。模数/指数是编译期常量，OnceLock 只解析一次
/// （原先每次加密都重复解析 PEM）。
fn service_key() -> &'static (BigUint, BigUint) {
    static KEY: OnceLock<(BigUint, BigUint)> = OnceLock::new();
    KEY.get_or_init(|| parse_spki_pem(PUBLIC_KEY_PEM).expect("hardcoded key is valid"))
}

/// 读取一个 DER TLV，返回 (tag, 内容起始, 内容结束)。只需覆盖本常量
/// 用到的单字节 tag 与短格式 / 0x81-0x82 两字节长格式长度编码。
fn der_tlv(data: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
    let tag = *data.get(pos)?;
    let first = *data.get(pos + 1)?;
    let (header, len) = if first < 0x80 {
        (2usize, first as usize)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 2 {
            return None;
        }
        let mut len = 0usize;
        for byte in data.get(pos + 2..pos + 2 + count)? {
            len = (len << 8) | usize::from(*byte);
        }
        (2 + count, len)
    };
    let start = pos.checked_add(header)?;
    let end = start.checked_add(len)?;
    (end <= data.len()).then_some((tag, start, end))
}

/// 期望指定 tag 的 TLV，返回内容区间；不符则 None。
fn der_expect(data: &[u8], pos: usize, tag: u8) -> Option<(usize, usize)> {
    let (actual, start, end) = der_tlv(data, pos)?;
    (actual == tag).then_some((start, end))
}

/// 最小 SPKI/DER 解析（固定常量 PEM，不引入通用 ASN.1 依赖）。结构固定：
/// SEQUENCE { SEQUENCE { OID rsaEncryption, NULL }, BIT STRING { SEQUENCE { INTEGER n, INTEGER e } } }
/// INTEGER 的前导 0x00 填充字节整段交给 from_bytes_be（对数值无影响）。
/// 失败返回 None，由调用方按原 expect 风格 panic。
fn parse_spki_pem(pem: &str) -> Option<(BigUint, BigUint)> {
    // PEM 主体 = 去掉 BEGIN/END 行后拼接的 base64
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    // 1.2.840.113549.1.1.1 (rsaEncryption)
    const RSA_ENCRYPTION_OID: [u8; 9] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];

    let (spki, spki_end) = der_expect(&der, 0, 0x30)?;
    let (alg, alg_end) = der_expect(&der, spki, 0x30)?;
    let (oid, oid_end) = der_expect(&der, alg, 0x06)?;
    if der[oid..oid_end] != RSA_ENCRYPTION_OID {
        return None;
    }
    let (_, null_end) = der_expect(&der, oid_end, 0x05)?;
    if null_end != alg_end {
        return None;
    }
    // BIT STRING 首字节是未用位数，SPKI 恒为 0
    let (bits, bits_end) = der_expect(&der, alg_end, 0x03)?;
    if der.get(bits) != Some(&0) {
        return None;
    }
    let (key, key_end) = der_expect(&der, bits + 1, 0x30)?;
    if key_end != bits_end {
        return None;
    }
    // RSAPublicKey ::= SEQUENCE { n INTEGER, e INTEGER }
    let (n, n_end) = der_expect(&der, key, 0x02)?;
    let (e, e_end) = der_expect(&der, n_end, 0x02)?;
    if e_end != key_end || bits_end != spki_end || spki_end != der.len() {
        return None;
    }
    Some((
        BigUint::from_bytes_be(&der[n..n_end]),
        BigUint::from_bytes_be(&der[e..e_end]),
    ))
}

/// `cryptoRSAEncrypt`: textbook RSA on zero-padded input (no OAEP/PKCS#1),
/// returns lowercase hex. Callers that need uppercase do it themselves.
pub fn rsa_raw_encrypt(data: &str) -> String {
    let (n, e) = service_key();
    let key_len = n.bits().div_ceil(8);
    let bytes = data.as_bytes();
    let mut padded = vec![0u8; key_len];
    padded[..bytes.len()].copy_from_slice(bytes);
    let encrypted = BigUint::from_bytes_be(&padded).modpow(e, n);
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
