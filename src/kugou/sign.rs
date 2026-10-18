//! 请求签名方案，对照 `KuGouMusicApi/util/helper.js`。
//!
//! 所有变体都归结为「salt + 按字典序排列的参数 + salt」喂给 MD5。
//!
//! 上游按客户端变体选 salt：概念版（lite）客户端用
//! `LnT6xpN3khm36zse0QzvmgTZ3waWdRSA` / `185672dd44712f60bb1736df5a377e82`，
//! 标准版客户端用 `OIlwieks28dk2k092lksi2UIkp` / `57ae12eb6890223e355ccfcb74edf70d`。
//! Meliora 模拟的是概念版客户端（appid 3116、mid 源自 dfid），所以这里硬编码概念版的 salt。

use std::collections::BTreeMap;

use super::crypto::md5_hex;

/// 概念版客户端的 Android 请求 salt（`platform=lite` 下的 `signatureAndroidParams`）。
const ANDROID_SALT: &str = "LnT6xpN3khm36zse0QzvmgTZ3waWdRSA";
const WEB_SALT: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";
/// 概念版客户端的请求 key salt（`platform=lite` 下的 `signKey`）。
const KEY_SALT: &str = "185672dd44712f60bb1736df5a377e82";

fn sorted_param_string(params: &BTreeMap<String, String>) -> String {
    // BTreeMap 本来就按键的字典序保存，与参考客户端用的 JS Object.keys(...).sort() 一致。
    let mut out = String::new();
    for (key, value) in params {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
    }
    out
}

/// `signatureAndroidParams`：salt + 参数 + body + salt。
pub fn signature_android(params: &BTreeMap<String, String>, body: &str) -> String {
    md5_hex(&format!(
        "{ANDROID_SALT}{}{body}{ANDROID_SALT}",
        sorted_param_string(params)
    ))
}

/// `signatureWebParams`：salt + 参数 + salt（不含 body）。
pub fn signature_web(params: &BTreeMap<String, String>) -> String {
    md5_hex(&format!(
        "{WEB_SALT}{}{WEB_SALT}",
        sorted_param_string(params)
    ))
}

/// `signKey`：MD5(hash + salt + appid + mid + userid)，/v5/url 用。
pub fn sign_key(hash: &str, mid: &str, userid: &str, appid: &str) -> String {
    md5_hex(&format!("{hash}{KEY_SALT}{appid}{mid}{userid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> BTreeMap<String, String> {
        let mut p = BTreeMap::new();
        p.insert("clienttime".to_string(), "1700000000".to_string());
        p.insert("appid".to_string(), "1005".to_string());
        p.insert("mid".to_string(), "123456789".to_string());
        p
    }

    /// 参考向量由 node 跑原版 KuGouMusicApi/util/helper.js（`platform=lite`）生成——即
    /// 概念版客户端（Meliora 模拟的那个）对这些输入（含一个 UTF-8 keyword）产出的精确摘要。
    /// 标准版 salt（请求用 `OIlwieks28dk2k092lksi2UIkp`、signKey 用
    /// `57ae12eb6890223e355ccfcb74edf70d`）得到的是 `3760e0a93f2a4eef48e879beac48a6cc` 与
    /// `8f6f15f3c88a5d9fe510c9411c499939`——两者不要混。
    fn reference_params() -> BTreeMap<String, String> {
        let mut p = BTreeMap::new();
        p.insert("appid".to_string(), "1005".to_string());
        p.insert("clienttime".to_string(), "1700000000".to_string());
        p.insert(
            "mid".to_string(),
            "12345678901234567890123456789012".to_string(),
        );
        p.insert("dfid".to_string(), "testdfid".to_string());
        p.insert("keyword".to_string(), "周杰伦".to_string());
        p.insert("uuid".to_string(), "-".to_string());
        p
    }

    #[test]
    fn android_signature_is_stable_md5() {
        let sig = signature_android(&params(), "");
        assert_eq!(sig.len(), 32);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        // body 参与摘要
        assert_ne!(sig, signature_android(&params(), "body"));
    }

    #[test]
    fn signatures_match_the_javascript_client() {
        let params = reference_params();
        // 概念版向量（node，platform=lite）；实现与生产客户端都是概念版，见上面的模块文档
        assert_eq!(
            signature_android(&params, ""),
            "b393d0a09117e5ea0366421a501b197f"
        );
        assert_eq!(signature_web(&params), "40fb8f10e2077f582ecf350ed6838b99");
        assert_eq!(
            sign_key(
                "ABCDEF1234",
                "12345678901234567890123456789012",
                "7749",
                "1005"
            ),
            "5c041d62b8491c4c4f107a8de47713da"
        );
    }

    #[test]
    fn web_signature_differs_from_android() {
        assert_ne!(signature_web(&params()), signature_android(&params(), ""));
    }

    #[test]
    fn sign_key_composes_expected_string() {
        assert_eq!(
            sign_key("hash", "mid", "7", "1005"),
            md5_hex(&format!("hash{KEY_SALT}1005mid7"))
        );
    }
}
