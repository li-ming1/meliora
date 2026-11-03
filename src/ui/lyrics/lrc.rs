use gpui::SharedString;

/// 歌词行内的一个带时间的字（酷狗 KRC 卡拉OK 数据）。
/// 文本用 `SharedString`，好让逐帧渲染的克隆只是引用计数加一，而不是新起一次堆上
/// `String` 分配。
#[derive(Clone)]
pub struct LrcWord {
    /// 字的起始偏移，单位毫秒（相对整首曲目）。
    pub time_ms: u64,
    /// 字的持续时长，单位毫秒。
    pub duration_ms: u64,
    pub text: SharedString,
}

/// LRC 文件里的一条带时间的行。
#[derive(Clone)]
pub struct LrcLine {
    /// 时间戳，单位毫秒。
    pub time_ms: u64,
    pub text: SharedString,
    /// 可选的译文，由 KRC 解析填充（`[language:]` 块）。
    pub translation: Option<SharedString>,
    /// 逐字卡拉OK 时间；普通 LRC 行为空。
    pub words: Vec<LrcWord>,
}

/// 解析形如 `mm:ss.xx` 或 `mm:ss.xxx` 的 LRC 时间标签。
/// 返回毫秒时间；格式非法时返回 `None`。
fn parse_time_tag(tag: &str) -> Option<u64> {
    let colon = tag.find(':')?;
    let minutes: u64 = tag[..colon].trim().parse().ok()?;
    let after_colon = &tag[colon + 1..];
    let dot = after_colon.find('.')?;
    let seconds: u64 = after_colon[..dot].trim().parse().ok()?;
    let frac_str = after_colon[dot + 1..].trim();

    // 归一化成毫秒
    let frac_ms = match frac_str.len() {
        1 => frac_str.parse::<u64>().ok()? * 100,
        2 => frac_str.parse::<u64>().ok()? * 10,
        3 => frac_str.parse::<u64>().ok()?,
        _ => return None,
    };

    Some(minutes * 60_000 + seconds * 1_000 + frac_ms)
}

/// 尝试解析 LRC 文件的一行。行上有多个时间标签时返回带对应数量条目的
/// `Some(vec)`；这行没有有效时间标签时返回 `None`。
fn parse_lrc_line(line: &str) -> Option<Vec<LrcLine>> {
    let mut timestamps: Vec<u64> = Vec::new();
    let mut rest = line;

    while rest.starts_with('[') {
        let end = rest.find(']')?;
        let tag = &rest[1..end];
        rest = &rest[end + 1..];

        if let Some(ms) = parse_time_tag(tag) {
            timestamps.push(ms);
        } else {
            // 元数据或未知标签——停止继续消耗标签。
            break;
        }
    }

    if timestamps.is_empty() {
        return None;
    }

    let text: SharedString = rest.trim().into();
    Some(
        timestamps
            .into_iter()
            .map(|time_ms| LrcLine {
                time_ms,
                text: text.clone(),
                translation: None,
                words: Vec::new(),
            })
            .collect(),
    )
}

/// 尝试把 `content` 当作 LRC 文件解析。
///
/// 至少找到一条带时间的行时，返回按时间戳排好序的 `Some(lines)`。
/// 完全没有时间标签时返回 `None`（纯文本）。
pub fn parse_lrc(content: &str) -> Option<Vec<LrcLine>> {
    let mut lines: Vec<LrcLine> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            if let Some(last) = lines.last() {
                lines.push(LrcLine {
                    time_ms: last.time_ms,
                    text: SharedString::default(),
                    translation: None,
                    words: Vec::new(),
                });
            }
            continue;
        }
        if let Some(parsed) = parse_lrc_line(line) {
            lines.extend(parsed);
        }
    }

    if lines.is_empty() {
        return None;
    }

    lines.sort_by_key(|l| l.time_ms);
    Some(lines)
}

/// 判断字符是否为中日韩（CJK）统一表意文字或假名、韩文音节。
fn is_cjk_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{4e00}'..='\u{9fff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{3040}'..='\u{309f}'
            | '\u{30a0}'..='\u{30ff}'
            | '\u{ac00}'..='\u{d7af}'
    )
}

/// 将单行歌词文本分词为拟合卡拉OK音节/词元。
/// 中文字符每个字符独立成词；英文/拉丁字母按连续单词聚拢；标点符号附着在上一个词尾。
fn tokenize_lyric_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current_word = String::new();

    for ch in text.chars() {
        if is_cjk_char(ch) {
            if !current_word.is_empty() {
                tokens.push(std::mem::take(&mut current_word));
            }
            tokens.push(ch.to_string());
        } else if ch.is_alphanumeric() {
            current_word.push(ch);
        } else if ch == ' ' {
            if !current_word.is_empty() {
                current_word.push(ch);
                tokens.push(std::mem::take(&mut current_word));
            } else if let Some(last) = tokens.last_mut() {
                last.push(ch);
            } else {
                tokens.push(ch.to_string());
            }
        } else if !current_word.is_empty() {
            current_word.push(ch);
            tokens.push(std::mem::take(&mut current_word));
        } else if let Some(last) = tokens.last_mut() {
            last.push(ch);
        } else {
            tokens.push(ch.to_string());
        }
    }

    if !current_word.is_empty() {
        tokens.push(current_word);
    }

    tokens
}

/// 为没有逐字时间的标准 LRC 歌词行，智能拟合生成平滑的卡拉OK逐字时间列表。
pub fn interpolate_missing_words(lines: &mut [LrcLine]) {
    let total_lines = lines.len();
    if total_lines == 0 {
        return;
    }

    for i in 0..total_lines {
        // 如果当前行已有逐字数据（例如来自 KRC），则保留原始精细数据
        if !lines[i].words.is_empty() || lines[i].text.trim().is_empty() {
            continue;
        }

        let start_ms = lines[i].time_ms;
        let tokens = tokenize_lyric_text(&lines[i].text);
        if tokens.is_empty() {
            continue;
        }

        // 估算行持续总时长
        let duration_ms = if i + 1 < total_lines {
            let next_start = lines[i + 1].time_ms;
            if next_start > start_ms {
                let delta = next_start - start_ms;
                if delta <= 8000 {
                    // 正常句间距：在下一句开始前留出约 250ms 的气口/呼吸位
                    delta.saturating_sub(250).clamp(800, 7500)
                } else {
                    // 超过 8 秒通常有长间奏或伴奏，按字数估算合理时长（每词/字约 280ms）
                    let est = (tokens.len() as u64 * 280).clamp(1500, 6000);
                    est.min(delta.saturating_sub(500))
                }
            } else {
                (tokens.len() as u64 * 280).clamp(1200, 5000)
            }
        } else {
            // 最后一行按字数估算
            (tokens.len() as u64 * 300).clamp(1500, 6000)
        };

        // 计算各词的相对权重
        let count = tokens.len();
        let weights: Vec<f32> = tokens
            .iter()
            .enumerate()
            .map(|(idx, t)| {
                let char_len = t.chars().count().max(1);
                let mut w = (char_len as f32).sqrt().clamp(1.0, 3.5);
                // 唱腔习惯：最后一个字/词通常有尾音拖长
                if idx + 1 == count {
                    w *= 1.35;
                }
                w
            })
            .collect();

        let total_weight: f32 = weights.iter().sum();
        let total_weight = if total_weight > 0.0 {
            total_weight
        } else {
            1.0
        };

        let mut current_offset = start_ms;
        let mut words = Vec::with_capacity(count);

        for (idx, token) in tokens.into_iter().enumerate() {
            let w = weights[idx];
            let raw_dur = (duration_ms as f32 * (w / total_weight)).round() as u64;
            let word_dur = raw_dur.max(60);

            words.push(LrcWord {
                time_ms: current_offset,
                duration_ms: word_dur,
                text: token.into(),
            });

            current_offset += word_dur;
        }

        lines[i].words = words;
    }
}

#[cfg(test)]
mod tests {
    use super::{interpolate_missing_words, parse_lrc, parse_lrc_line, parse_time_tag};

    fn line_tuples(lines: Vec<super::LrcLine>) -> Vec<(u64, String)> {
        lines
            .into_iter()
            .map(|line| (line.time_ms, line.text.to_string()))
            .collect()
    }

    #[test]
    fn parse_time_tag_supports_one_two_and_three_digit_fractions() {
        assert_eq!(parse_time_tag("01:02.3"), Some(62_300));
        assert_eq!(parse_time_tag("01:02.34"), Some(62_340));
        assert_eq!(parse_time_tag("01:02.345"), Some(62_345));
    }

    #[test]
    fn parse_time_tag_rejects_invalid_tags() {
        assert_eq!(parse_time_tag("01:02"), None);
        assert_eq!(parse_time_tag("01.02.34"), None);
        assert_eq!(parse_time_tag("aa:02.34"), None);
        assert_eq!(parse_time_tag("01:bb.34"), None);
        assert_eq!(parse_time_tag("01:02.xx"), None);
        assert_eq!(parse_time_tag("01:02."), None);
    }

    #[test]
    fn parse_lrc_line_expands_multiple_timestamps() {
        let parsed = parse_lrc_line("[00:01.00][00:02.50]Hello").unwrap();

        assert_eq!(
            line_tuples(parsed),
            vec![(1_000, "Hello".to_string()), (2_500, "Hello".to_string())]
        );
    }

    #[test]
    fn parse_lrc_ignores_metadata_tags() {
        let parsed = parse_lrc("[ar:Artist]\n[ti:Song]\n[00:01.00]Hello").unwrap();

        assert_eq!(line_tuples(parsed), vec![(1_000, "Hello".to_string())]);
    }

    #[test]
    fn parse_lrc_duplicates_previous_timestamp_for_blank_lines() {
        let parsed = parse_lrc("[00:01.00]Hello\n\n[00:02.00]World").unwrap();

        assert_eq!(
            line_tuples(parsed),
            vec![
                (1_000, "Hello".to_string()),
                (1_000, String::new()),
                (2_000, "World".to_string()),
            ]
        );
    }

    #[test]
    fn parse_lrc_sorts_output_by_timestamp() {
        let parsed = parse_lrc("[00:03.00]Third\n[00:01.00]First\n[00:02.00]Second").unwrap();

        assert_eq!(
            line_tuples(parsed),
            vec![
                (1_000, "First".to_string()),
                (2_000, "Second".to_string()),
                (3_000, "Third".to_string()),
            ]
        );
    }

    #[test]
    fn interpolate_missing_words_generates_smooth_syllables() {
        let mut lines = parse_lrc("[00:01.00]我和我的祖国\n[00:04.00]一刻也不能分割").unwrap();
        interpolate_missing_words(&mut lines);

        let first = &lines[0];
        assert_eq!(first.words.len(), 6);
        assert_eq!(first.words[0].text.as_ref(), "我");
        assert_eq!(first.words[0].time_ms, 1_000);
        assert!(first.words.last().unwrap().time_ms > 1_000);
        assert!(first.words.last().unwrap().time_ms < 4_000);

        // 拼接后的字符必须还原原歌词
        let reconstructed: String = first.words.iter().map(|w| w.text.as_ref()).collect();
        assert_eq!(reconstructed, "我和我的祖国");
    }
}
