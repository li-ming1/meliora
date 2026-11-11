//! 元数据标签写入库与音乐库歌手匹配共用的文本归一化。

/// 归一化人名以做身份比对：转小写、按非字母数字切分、token 排序、
/// 用单个空格拼接。扫描器的歌手匹配与标签写入的歌手分组都以它为键，
/// 于是两侧对"同一个歌手"的判定保持一致。
pub fn token_key(value: &str) -> String {
    let lowered = value.to_lowercase();
    let mut tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.sort_unstable();
    tokens.join(" ")
}

/// 解码外部文本侧车（`.lrc` 等）：严格 UTF-8 优先，失败按 GBK 解码（中文曲库
/// 的旧歌词文件常见编码），GBK 也有坏字节时才 lossy 兜底——整文件拒读会让歌词
/// 静默不入库且每次重扫重复失败。
pub fn decode_sidecar_text(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::decode_sidecar_text;

    #[test]
    fn decode_sidecar_text_prefers_utf8_then_gbk() {
        assert_eq!(
            decode_sidecar_text("[00:01.00]歌词".as_bytes()),
            "[00:01.00]歌词"
        );
        // GBK 的「你好」（非合法 UTF-8，GBK 可解）
        assert_eq!(decode_sidecar_text(&[0xC4, 0xE3, 0xBA, 0xC3]), "你好");
    }
}
