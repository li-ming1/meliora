//! NetEase YRC (word-level karaoke lyric) parser. Plain text, unlike KuGou's
//! encrypted KRC. Line shapes:
//!   `[6700,2780](6700,240,0)作(6940,240,0)词...` — line timing + word tokens
//!   `[{"t":0,"c":[{"tx":"作词: ..."}]}]`        — credits/metadata line

use gpui::SharedString;
use serde_json::Value;

use super::lrc::{LrcLine, LrcWord};

/// Parses YRC text into timed lines. Returns `None` when nothing parsed.
pub fn parse_yrc(content: &str) -> Option<Vec<LrcLine>> {
    let mut lines = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if !line.starts_with('[') {
            continue;
        }
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };

        if rest.starts_with('{') {
            parse_metadata_line(line, &mut lines);
            continue;
        }

        let Some((times, body)) = rest.split_once(']') else {
            continue;
        };
        let Some((start, duration)) = times.split_once(',') else {
            continue;
        };
        let Ok(time_ms) = start.trim().parse::<u64>() else {
            continue;
        };
        let _line_duration = duration.trim().parse::<u64>().unwrap_or(0);

        let mut words = Vec::new();
        let mut plain = String::new();
        let mut rest = body;
        loop {
            // word tokens look like `(start,duration,0)text`; a line without
            // any word marker is an instrumental/interlude line
            let Some(token_start) = rest.strip_prefix('(') else {
                break;
            };
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
                    text: SharedString::from(text.to_string()),
                });
            }
            rest = remainder;
            if rest.is_empty() {
                break;
            }
        }

        lines.push(LrcLine {
            time_ms,
            text: SharedString::from(plain),
            translation: None,
            words,
        });
    }

    (!lines.is_empty()).then_some(lines)
}

/// Metadata line: `[{"t":0,"c":[{"tx":"作词: ..."},{"tx":"作曲: ..."}]}]`.
/// Multiple tx fragments are concatenated into one plain line.
fn parse_metadata_line(line: &str, lines: &mut Vec<LrcLine>) {
    let Some(end) = line.rfind(']') else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&line[1..end]) else {
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

        // interlude: timed but empty
        assert_eq!(lines[2].time_ms, 100000);
        assert!(lines[2].text.is_empty());
    }

    /// Real payload head from `孤勇者` (captured live, anonymous session):
    /// JSON credits lines carry extra `li`/`or` fields that must be ignored.
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

    #[test]
    fn rejects_non_yrc_text() {
        assert!(parse_yrc("plain text without timestamps").is_none());
        assert!(parse_yrc("[00:01.50]not yrc").is_none());
    }
}
