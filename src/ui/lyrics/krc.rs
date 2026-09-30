//! 酷狗 KRC 逐字歌词：解密 + 解析。
//!
//! KRC 下载内容的 `content` 字段是 base64；解密分两步：
//! 1. 丢弃前 4 字节后，与固定 16 字节 key 按位异或；
//! 2. zlib 解压，得到带 `<偏移,时长,音量>词` 标签的明文。
//!
//! 明文结构（与 KugouMusic.NET 的 KrcParser 相对齐）：
//! - `[ar:...]` 等元数据行；
//! - `[language:base64]` 行，base64 解码后是 JSON，其中 type=1 的
//!   lyricContent 按行给出翻译、type=0 给出音译；
//!
//! 歌词行 `[开始ms,持续ms]`+`<字偏移ms,字长ms,音量>字`。
//!
//! 行级时间/正文/翻译 + 逐字时间均已解析，供卡拉OK 逐字高亮使用。

#[cfg(feature = "kugou")]
use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
#[cfg(feature = "kugou")]
use flate2::read::ZlibDecoder;
use gpui::SharedString;
use serde::Deserialize;

use super::lrc::{LrcLine, LrcWord};

/// KRC 固定异或 key（前 4 字节为长度/头，之后逐一异或此表）。
#[cfg(feature = "kugou")]
const KRC_KEY: [u8; 16] = [
    64, 71, 97, 119, 94, 50, 116, 71, 81, 54, 49, 45, 206, 210, 110, 105,
];

/// `[language:]` JSON：`content` 数组按 `type` 区分翻译(1)与音译(0)。
#[derive(Deserialize)]
struct KrcLanguage {
    content: Vec<KrcLanguageSection>,
}

#[derive(Deserialize)]
struct KrcLanguageSection {
    #[serde(rename = "type")]
    kind: i32,
    /// 每行一条的字符串列表（如 `[["第一行翻译"],["第二行翻译"],...]`）。
    #[serde(rename = "lyricContent")]
    lyric_content: Vec<Vec<String>>,
}

/// 解密 base64 KRC 内容，返回解压后的明文；失败（非 base64 / 解压错误 /
/// 非 UTF-8）返回 `None`。
#[cfg(feature = "kugou")]
pub fn decrypt_krc(base64_content: &str) -> Option<String> {
    let bytes = STANDARD.decode(base64_content.trim()).ok()?;
    if bytes.len() <= 4 {
        return None;
    }

    let mut data = bytes[4..].to_vec();
    for (i, byte) in data.iter_mut().enumerate() {
        *byte ^= KRC_KEY[i % KRC_KEY.len()];
    }

    let mut decoder = ZlibDecoder::new(&data[..]);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    // 解压不出任何内容（坏数据/空流）时视为无效，让调用方回退 LRC。
    if out.is_empty() {
        return None;
    }
    String::from_utf8(out).ok()
}

/// 解析 KRC 明文歌词。返回按时间排序的行列表；没有任何歌词行时返回
/// `None`（此时调用方应回退到 LRC）。
pub fn parse_krc(content: &str) -> Option<Vec<LrcLine>> {
    // 第一遍：收集 `[language:]` 块里的翻译/音译。
    let (mut translations, mut romanizations) = (None, None);
    for line in content.lines() {
        let line = line.trim();
        if let Some(encoded) = line.strip_prefix("[language:") {
            let Some(encoded) = encoded.strip_suffix(']') else {
                continue;
            };
            if let Some(container) = decode_language_block(encoded) {
                for section in &container.content {
                    match section.kind {
                        1 => translations = Some(section.lyric_content.clone()),
                        0 => romanizations = Some(section.lyric_content.clone()),
                        _ => {}
                    }
                }
            }
        }
    }

    // 第二遍：解析歌词行；翻译按行索引对齐，音译拼到正文后面。
    let mut lines: Vec<LrcLine> = Vec::new();
    let mut line_index = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('[') {
            continue;
        }
        let Some((start_ms, _duration_ms, raw)) = parse_timed_line(line) else {
            continue; // 元数据 / [language:] 行
        };

        let (mut text, words) = parse_words(raw, start_ms);
        let translation = translations.as_ref().and_then(|list| {
            list.get(line_index)
                .and_then(|entry| entry.first())
                .cloned()
        });
        let romanization = romanizations
            .as_ref()
            .and_then(|list| list.get(line_index).map(|entry| entry.concat()));

        if let Some(romanization) = romanization
            && !romanization.is_empty()
        {
            text.push_str(&format!("  〔{romanization}〕"));
        }

        lines.push(LrcLine {
            time_ms: start_ms,
            text: text.into(),
            translation: translation.map(SharedString::from),
            words,
        });
        line_index += 1;
    }

    if lines.is_empty() {
        return None;
    }

    lines.sort_by_key(|line| line.time_ms);
    Some(lines)
}

/// base64 解码并解析 `[language:]` 的 JSON；静默忽略坏块。
fn decode_language_block(encoded: &str) -> Option<KrcLanguage> {
    let bytes = STANDARD.decode(encoded).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 解析歌词行 `[开始ms,持续ms]正文`，返回 (start_ms, duration_ms, 正文)。
fn parse_timed_line(line: &str) -> Option<(u64, u64, &str)> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let tag = &rest[..close];
    let (start, duration) = tag.split_once(',')?;
    Some((
        start.trim().parse().ok()?,
        duration.trim().parse().ok()?,
        &rest[close + 1..],
    ))
}

/// 解析正文里 `<偏移,时长,音量>字` 的字级标签，返回 (拼合的纯文本, 逐字列表)。
/// 字时间 = 行开始时间 + 标签内偏移。不匹配标签形态的 `<` 按字面保留。
fn parse_words(raw: &str, line_start: u64) -> (String, Vec<LrcWord>) {
    let mut text = String::with_capacity(raw.len());
    let mut words: Vec<LrcWord> = Vec::new();
    let mut rest = raw;

    while let Some(lt) = rest.find('<') {
        text.push_str(&rest[..lt]);
        let Some(relative_end) = rest[lt..].find('>') else {
            text.push_str(&rest[lt..]);
            rest = "";
            break;
        };
        let tag = &rest[lt + 1..lt + relative_end];
        if !is_word_tag(tag) {
            text.push('<');
            rest = &rest[lt + 1..];
            continue;
        }

        let mut fields = tag.split(',');
        let offset = fields
            .next()
            .and_then(|f| f.trim().parse().ok())
            .unwrap_or(0);
        let duration = fields
            .next()
            .and_then(|f| f.trim().parse().ok())
            .unwrap_or(0);

        let word_end = lt + relative_end + 1;
        let tail = &rest[word_end..];
        let word_text = tail.split('<').next().unwrap_or("");
        text.push_str(word_text);
        words.push(LrcWord {
            time_ms: line_start + offset,
            duration_ms: duration,
            text: word_text.into(),
        });
        rest = &tail[word_text.len()..];
    }

    text.push_str(rest);
    (text, words)
}

/// `<数值,数值,数值>` 形式的字级标签（三个数字逗号分隔，无分配）。
fn is_word_tag(tag: &str) -> bool {
    let mut fields = 0;
    for field in tag.split(',') {
        if fields == 3 || field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        fields += 1;
    }
    fields == 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    /// 按真实 KRC 加密流程构造样本：压缩 → 异或 → 补 4 字节头 → base64。
    #[cfg(feature = "kugou")]
    fn encrypt_krc_for_test(plain: &str) -> String {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plain.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut data = vec![0u8; 4];
        data.extend_from_slice(&compressed);
        for (i, byte) in data[4..].iter_mut().enumerate() {
            *byte ^= KRC_KEY[i % KRC_KEY.len()];
        }
        STANDARD.encode(data)
    }

    const KRC_SAMPLE: &str = "\
[ar:\u{4F5C}\u{8005}]
[ti:\u{6B4C}\u{540D}]
[hash:abcdef0123456789]
[language:IGNORED]
[0,2000]<0,500,0>\u{4F60}\u{597D}<500,1500,0>\u{4E16}\u{754C}
[2000,1500]<0,500,0>\u{5929}\u{7A7A}
";

    #[test]
    #[cfg(feature = "kugou")]
    fn decrypt_roundtrips_real_encryption() {
        // 明文里“你”与“世”隔着一个 `<500,1500,0>` 字级标签，逐字断言。
        let decrypted =
            decrypt_krc(&encrypt_krc_for_test(KRC_SAMPLE)).expect("decrypt should succeed");
        assert!(
            decrypted.contains("\u{4F60}\u{597D}<500,1500,0>\u{4E16}\u{754C}"),
            "roundtrip mismatch, decrypted: {:?}",
            decrypted
        );
    }

    #[test]
    #[cfg(feature = "kugou")]
    fn decrypt_rejects_invalid_input() {
        assert!(decrypt_krc("").is_none(), "empty input should be rejected");
        assert!(
            decrypt_krc("aGVsbG8=").is_none(),
            "short payload should be rejected, got: {:?}",
            decrypt_krc("aGVsbG8=")
        );
        assert!(decrypt_krc("!!!not-base64!!!").is_none());
    }

    #[test]
    fn parse_krc_strips_word_tags_and_lines_up_translations() {
        let plain = "\
[0,2000]<0,500,0>\u{4F60}\u{597D}<500,1500,0>\u{4E16}\u{754C}
[2000,1500]<0,500,0>\u{5929}\u{7A7A}
";
        let lines = parse_krc(plain).expect("parse should succeed");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time_ms, 0);
        assert_eq!(lines[0].text, "\u{4F60}\u{597D}\u{4E16}\u{754C}");
        assert_eq!(lines[1].time_ms, 2_000);
        assert_eq!(lines[1].text, "\u{5929}\u{7A7A}");

        // 逐字时间保留（相对歌曲）
        assert_eq!(lines[0].words.len(), 2);
        assert_eq!(lines[0].words[0].time_ms, 0);
        assert_eq!(lines[0].words[0].duration_ms, 500);
        assert_eq!(lines[0].words[0].text, "\u{4F60}\u{597D}");
        assert_eq!(lines[0].words[1].time_ms, 500);
        assert_eq!(lines[0].words[1].text, "\u{4E16}\u{754C}");
    }

    #[test]
    fn parse_krc_reads_translation_from_language_block() {
        // [language:] 的 base64 内容：{"content":[{"type":1,"lyricContent":[["\u{4F60}\u{597D}\u{4E16}\u{754C}"],["\u{5929}\u{7A7A}"]]}]}
        let language = r#"{"content":[{"type":1,"lyricContent":[["\u4f60\u597d\u4e16\u754c"],["\u5929\u7a7a"]]},{"type":0,"lyricContent":[[]]}]}"#;
        let encoded = STANDARD.encode(language);

        let content = format!(
            "[ar:x]\n[language:{encoded}]\n[0,2000]\u{4F60}\u{597D}\u{4E16}\u{754C}\n[2000,1500]\u{5929}\u{7A7A}\n"
        );
        let lines = parse_krc(&content).expect("parse should succeed");
        assert_eq!(
            lines[0].translation.as_deref(),
            Some("\u{4F60}\u{597D}\u{4E16}\u{754C}")
        );
        assert_eq!(lines[1].translation.as_deref(), Some("\u{5929}\u{7A7A}"));
    }

    #[test]
    fn parse_krc_sorts_lines_and_returns_none_without_any_lyric_line() {
        assert!(parse_krc("[ar:x]\n[ti:y]\n").is_none());
        let lines = parse_krc("[2,1]b\n[1,1]a").expect("parse should succeed");
        assert_eq!(lines[0].text, "a");
        assert_eq!(lines[1].text, "b");
    }

    #[test]
    fn parse_words_keeps_literal_angles() {
        // 词文本截止到下一个 `<` 前；字面的 `< 2` 保留为文本。
        let (text, words) = parse_words("<0,500,0>\u{4F60} 1 < 2", 10_000);
        assert_eq!(text, "\u{4F60} 1 < 2");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].time_ms, 10_000);
        assert_eq!(words[0].duration_ms, 500);
        assert_eq!(words[0].text, "\u{4F60} 1 ");
    }

    #[test]
    fn parse_words_handles_missing_closing_bracket() {
        let (text, words) = parse_words("<0,500,0>abc<xyz", 0);
        assert_eq!(text, "abc<xyz");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "abc");
    }
}
