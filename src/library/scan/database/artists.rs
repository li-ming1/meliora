use rustc_hash::FxHashSet;

use crate::library::scan::artist_match::token_key;

/// 专辑歌手排序 tag 是否提到这个名字（直接出现或 "Last, First" 形式）。
/// 没被提到的名字是客串歌手，绝不能拿到 album_artist 行。
pub(super) fn sort_mentions_artist(sort: &str, name: &str) -> bool {
    let strip = |t: &str| {
        t.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    let sort_tokens: FxHashSet<String> = sort
        .to_lowercase()
        .split_whitespace()
        .map(strip)
        .filter(|t| !t.is_empty())
        .collect();
    name.to_lowercase()
        .split_whitespace()
        .map(strip)
        .all(|token| !token.is_empty() && sort_tokens.contains(&token))
}

pub(super) fn push_album_artist_name(
    names: &mut Vec<(String, Option<String>)>,
    name: &str,
    key: Option<String>,
) {
    if !names.iter().any(|(existing, _)| existing == name) {
        names.push((name.to_string(), key));
    }
}

pub(super) fn decode_artist_list(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    serde_json::from_str(value).unwrap_or_else(|_| {
        // 迁移被打断或手工跳过过时，也能接受迁移前的数据库
        value
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    })
}

// 为这点东西单独建一张表实在有点荒唐，所以直接用 json
pub(super) fn encode_artist_list(values: &[String]) -> Option<String> {
    (!values.is_empty()).then(|| serde_json::to_string(values).expect("artist list serializes"))
}

/// 显示歌手是真正的署名歌手（而非 "Various Artists" 这类泛用 tag）时为 true。
pub(super) fn display_is_credited(rows: &[TrackArtistRow], display: &str) -> bool {
    let display_lower = display.to_lowercase();
    rows.iter().any(|row| {
        row.artist_names
            .as_deref()
            .is_some_and(|name| name.trim().to_lowercase() == display_lower)
            || decode_artist_list(row.artists.as_deref())
                .iter()
                .any(|name| name.to_lowercase() == display_lower)
    })
}

#[derive(sqlx::FromRow)]
pub(super) struct TrackArtistRow {
    pub(super) artists: Option<String>,
    pub(super) artist_sort: Option<String>,
    pub(super) album_artist_keys: Option<String>,
    pub(super) artist_names: Option<String>,
}

/// 取自曲目 tag 的专辑歌手名字与排序值。若无人认领，就把所有署名名字都链接上。
pub(super) fn derive_claimed_artists(
    rows: &[TrackArtistRow],
) -> (Vec<(String, Option<String>)>, Vec<String>) {
    let mut names: Vec<(String, Option<String>)> = Vec::new();
    let mut fallback: Vec<String> = Vec::new();

    for row in rows {
        let sort = row.artist_sort.as_deref().filter(|s| !s.trim().is_empty());
        let artists = decode_artist_list(row.artists.as_deref());
        if artists.is_empty() {
            continue;
        }
        let parts = decode_artist_list(row.album_artist_keys.as_deref());
        for part in &parts {
            if !fallback.iter().any(|existing| existing == part) {
                fallback.push(part.clone());
            }
        }

        if parts.is_empty() {
            // 单个歌手永远保留自己的排序值以便合并别名——多个歌手时只链接
            // 出现在排序值里的名字
            let inherited = if artists.len() == 1 {
                sort.map(str::to_string)
            } else {
                None
            };
            for name in &artists {
                if artists.len() > 1
                    && let Some(sort) = sort
                    && !sort_mentions_artist(sort, name)
                {
                    continue;
                }
                push_album_artist_name(&mut names, name, inherited.clone());
            }
        } else {
            // 只有被某个排序片段认领的名字才会被链接，并以该片段为键
            for name in &artists {
                let Some(part) = parts.iter().find(|part| token_key(part) == token_key(name))
                else {
                    continue;
                };
                let key = if artists.len() == 1 {
                    sort.map(str::to_string)
                } else {
                    Some(part.to_string())
                };
                push_album_artist_name(&mut names, name, key);
            }
        }
    }

    (names, fallback)
}
