//! YRC（逐字卡拉OK 歌词）解析器。纯文本，不像酷狗的加密 KRC。该格式源自网易云音乐，
//! 也会从曲目旁边的本地 `.yrc` sidecar 文件读取。行的形态：
//!   `[6700,2780](6700,240,0)作(6940,240,0)词...` — line timing + word tokens
//!   `[{"t":0,"c":[{"tx":"作词: ..."}]}]`        — credits/metadata line

use gpui::SharedString;
use serde_json::Value;

use super::lrc::{LrcLine, LrcWord};

/// 把 YRC 文本解析成带时间的行；什么都没解析出来时返回 `None`。
pub fn parse_yrc(content: &str) -> Option<Vec<LrcLine>> {
    let mut lines = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        // 带时间的行以 `[...` 开头；真实负载里署名/元数据行是裸 JSON 对象
        // （较旧的写入器会把它们包进 `[...]`）。
        if !(line.starts_with('[') || line.starts_with('{')) {
            continue;
        }

        if line.starts_with('{')
            || line
                .strip_prefix('[')
                .is_some_and(|rest| rest.starts_with('{'))
        {
            parse_metadata_line(line, &mut lines);
            continue;
        }

        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };

        let Some((times, body)) = rest.split_once(']') else {
            continue;
        };
        let Some((start, _)) = times.split_once(',') else {
            continue;
        };
        let Ok(time_ms) = start.trim().parse::<u64>() else {
            continue;
        };

        let mut words = Vec::new();
        let mut plain = String::new();
        let mut rest = body;
        // 字 token 形如 `(start,duration,0)text`；没有任何字标记的行是间奏/纯音乐行
        while let Some(token_start) = rest.strip_prefix('(') {
            let Some(close) = token_start.find(')') else {
                break;
            };
            let token = &token_start[..close];
            let after = &token_start[close + 1..];
            let (text, remainder) = match after.find('(') {
                Some(idx) => (&after[..idx], &after[idx..]),
                None => (after, ""),
            };
            let mut parts = token.split(',');
            let word_start = parts.next().and_then(|p| p.trim().parse::<u64>().ok());
            let word_duration = parts.next().and_then(|p| p.trim().parse::<u64>().ok());
            if let (Some(word_start), Some(word_duration)) = (word_start, word_duration)
                && !text.is_empty()
            {
                plain.push_str(text);
                words.push(LrcWord {
                    time_ms: word_start,
                    duration_ms: word_duration,
                    text: SharedString::from(text),
                });
            }
            rest = remainder;
        }

        lines.push(LrcLine {
            time_ms,
            text: SharedString::from(plain),
            translation: None,
            words,
        });
    }

    lines.sort_by_key(|l| l.time_ms);
    (!lines.is_empty()).then_some(lines)
}

/// 元数据行：较旧写入器写出的
/// `[{"t":0,"c":[{"tx":"作词: ..."},{"tx":"作曲: ..."}]}]`，或真实负载里那种裸
/// `{"t":0,...}`。多个 tx 片段会拼接成一行纯文本。JSON 对象按花括号定位，所以两种
/// 包裹形态都能解析。
fn parse_metadata_line(line: &str, lines: &mut Vec<LrcLine>) {
    let (Some(json_start), Some(json_end)) = (line.find('{'), line.rfind('}')) else {
        return;
    };
    if json_start >= json_end {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(&line[json_start..=json_end]) else {
        return;
    };
    let time_ms = value
        .get("t")
        .and_then(Value::as_f64)
        .map(|t| t.max(0.0) as u64)
        .unwrap_or(0);
    let text = value
        .get("c")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("tx").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    lines.push(LrcLine {
        time_ms,
        text: SharedString::from(text),
        translation: None,
        words: Vec::new(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[{\"t\":0,\"c\":[{\"tx\":\"作词: 方文山\"},{\"tx\":\" 作曲: 周杰伦\"}]}]\n\
                          [17540,3060](17540,630,0)海(18170,330,0)边(18500,700,0)潮(19200,1400,0)声\n\
                          [100000,2000]\n";

    #[test]
    fn parses_words_metadata_and_interludes() {
        let lines = parse_yrc(SAMPLE).expect("parses");
        assert_eq!(lines.len(), 3);

        assert_eq!(lines[0].time_ms, 0);
        assert_eq!(lines[0].text, "作词: 方文山 作曲: 周杰伦");
        assert!(lines[0].words.is_empty());

        assert_eq!(lines[1].time_ms, 17540);
        assert_eq!(lines[1].text, "海边潮声");
        assert_eq!(lines[1].words.len(), 4);
        assert_eq!(lines[1].words[0].time_ms, 17540);
        assert_eq!(lines[1].words[0].duration_ms, 630);
        assert_eq!(lines[1].words[3].text, "声");

        // 间奏：带时间但内容为空
        assert_eq!(lines[2].time_ms, 100000);
        assert!(lines[2].text.is_empty());
    }

    /// Real payload head from `孤勇者` (captured live, anonymous session):
    /// JSON 署名行带着多余的 `li`/`or` 字段，必须忽略。
    #[test]
    fn parses_real_payload_with_credit_links() {
        let sample = r#"{"t":0,"c":[{"tx":"作词: "},{"tx":"唐恬","li":"http://p1.music.126.net/x.jpg","or":"orpheus://nm/artist/home?id=28245490"}]}
{"t":0,"c":[{"tx":"作曲: "},{"tx":"钱雷"}]}
[1315,6740](1315,545,0)都(1860,240,0)是(2100,680,0)勇(2780,420,0)敢(3200,1200,0)的
"#;
        let lines = parse_yrc(sample).expect("parses");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "作词: 唐恬");
        assert_eq!(lines[1].text, "作曲: 钱雷");
        assert_eq!(lines[2].text, "都是勇敢的");
        assert_eq!(lines[2].words.len(), 5);
        assert_eq!(lines[2].words[4].text, "的");
    }

    /// 真实负载可能带乱序的行和末尾那个 `{"t":0,...}` 署名；`partition_point` 的
    /// 消费方（歌词面板 / 沉浸视图）按 `time_ms` 做二分查找，所以返回的行必须是排好序的。
    #[test]
    fn sorts_unordered_lines_and_trailing_credit() {
        let sample = "[1315,6740](1315,545,0)都(1860,240,0)是\n\
                      [9000,1000](9000,1000,0)勇\n\
                      {\"t\":0,\"c\":[{\"tx\":\"作词: 唐恬\"}]}\n";
        let lines = parse_yrc(sample).expect("parses");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 0);
        assert_eq!(lines[0].text, "作词: 唐恬");
        assert_eq!(lines[1].time_ms, 1315);
        assert_eq!(lines[1].text, "都是");
        assert_eq!(lines[2].time_ms, 9000);

        // 与 lyrics.rs / immersive.rs 用的是同一种查找：时间 <= pos 的最后一行。
        let pos_ms = 5_000;
        let idx = lines.partition_point(|l| l.time_ms <= pos_ms);
        assert_eq!(idx, 2);
        assert_eq!(lines[idx - 1].time_ms, 1315);
    }

    #[test]
    fn rejects_non_yrc_text() {
        assert!(parse_yrc("plain text without timestamps").is_none());
        assert!(parse_yrc("[00:01.50]not yrc").is_none());
    }
}
