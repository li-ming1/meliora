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

#[cfg(test)]
mod tests {
    use super::{parse_lrc, parse_lrc_line, parse_time_tag};

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
}
