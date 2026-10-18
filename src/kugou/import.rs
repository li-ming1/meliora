//! 把网易云音乐 / QQ 音乐的歌单分享链接解析成纯歌名，以便回填匹配进酷狗并导入成新歌单。
//! 纯 HTTP + JSON；这里不涉及任何酷狗签名。
//! 思路与端点都对照 `KuGou.Net/ExternalPlaylists/*`。

use std::{sync::OnceLock, time::Duration};

use cntp_i18n::tr;
use serde_json::Value;
use zed_reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};

use super::crypto;

/// 解析出的外部歌单：歌单名 + 一层扁平的歌名列表（匹配进酷狗发生在之后）。
pub struct ExternalPlaylist {
    pub name: String,
    pub songs: Vec<String>,
}

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0 Safari/537.36";

static IMPORT_HTTP: OnceLock<Client> = OnceLock::new();
fn http() -> &'static Client {
    IMPORT_HTTP.get_or_init(|| {
        Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

fn browser_headers(referer: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [("User-Agent", BROWSER_UA), ("Referer", referer)] {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            headers.insert(name, value);
        }
    }
    headers
}

fn unsupported_link_error() -> String {
    tr!(
        "KUGOU_IMPORT_UNSUPPORTED_LINK",
        "Only NetEase Cloud Music and QQ Music playlist links are supported"
    )
    .to_string()
}

fn netease_no_id_error() -> String {
    tr!(
        "KUGOU_IMPORT_NETEASE_NO_ID",
        "Could not parse a playlist ID from the NetEase Cloud Music link"
    )
    .to_string()
}

/// 把粘贴进来的网易云/QQ 分享文本解析成 `ExternalPlaylist`。
pub async fn parse_share_link(text: &str) -> Result<ExternalPlaylist, String> {
    let url = extract_url(text)
        .ok_or_else(|| tr!("KUGOU_IMPORT_NO_VALID_LINK", "No valid link found").to_string())?;
    if hosted(&url, &["music.163.com", "163cn.tv"]) {
        netease(&url).await
    } else if hosted(
        &url,
        &["y.qq.com", "qqmusic.qq.com", "music.qq.com", "c.y.qq.com"],
    ) {
        qq(&url).await
    } else {
        Err(unsupported_link_error())
    }
}

/// 从任意分享文本里抄出 URL（处理结尾标点）。
fn extract_url(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let start = trimmed.find("http")?;
    let rest = &trimmed[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(
        rest[..end]
            .trim_end_matches(|c: char| {
                matches!(
                    c,
                    ')' | ']' | '】' | '」' | '』' | '，' | ',' | '。' | '；' | ';'
                )
            })
            .to_string(),
    )
}

fn host_of(url: &str) -> String {
    let Some(idx) = url.find("://") else {
        return url.to_lowercase();
    };
    url[idx + 3..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn hosted(url: &str, needles: &[&str]) -> bool {
    let host = host_of(url);
    // 精确主机名或真子域：单纯 `contains` 会让 `music.163.com.evil.com` 命中
    // `music.163.com` 这根针。
    needles
        .iter()
        .any(|n| host == *n || host.ends_with(&format!(".{n}")))
}

fn query_value(query: &str, key: &str) -> Option<String> {
    let q = query.trim_start_matches('?');
    for seg in q.split('&') {
        let (k, v) = seg.split_once('=').unwrap_or((seg, ""));
        if k.eq_ignore_ascii_case(key) && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// 取裸 `id=NNN` 的值，其键必须正好止于 `id`：匹配位置前的字符不能是 `[A-Za-z0-9_]`（否则它是
/// `mid=`/`tid=`/`songid=`/`userid=` 这类更长键的尾巴），且扫描要越过这些更长的键继续，
/// 而不是返回它们的值。
fn bare_id_value(haystack: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(pos) = haystack[from..].find("id=") {
        let abs = from + pos;
        let boundary = abs == 0
            || !haystack[..abs]
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if boundary {
            let rest = &haystack[abs + 3..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            return (end > 0).then(|| &rest[..end]);
        }
        from = abs + 3;
    }
    None
}

/// 跟进短链并返回它的最终 URL。
async fn resolve(url: &str) -> Result<String, String> {
    let resp = http().get(url).send().await.map_err(|e| {
        tr!(
            "KUGOU_IMPORT_SHORTLINK_FAILED",
            "Failed to resolve short link: {{err}}",
            err = e.to_string()
        )
        .to_string()
    })?;
    Ok(resp.url().to_string())
}

fn dedupe(v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(v.len());
    let mut keep: Vec<bool> = Vec::with_capacity(v.len());
    for s in &v {
        keep.push(seen.insert(s.as_str()));
    }
    let mut flags = keep.into_iter();
    v.into_iter().filter(|_| flags.next().unwrap()).collect()
}

// ==== 网易云音乐 ====

fn netease_id(url: &str) -> Option<String> {
    if let Some(id) = query_value(url, "id") {
        return Some(id);
    }
    if let Some(frag) = url.split('#').nth(1)
        && let Some(q) = frag.split('?').nth(1)
        && let Some(id) = query_value(q, "id")
    {
        return Some(id);
    }
    let after = url.find("playlist").or_else(|| url.find("songlist"))?;
    bare_id_value(&url[after..]).map(str::to_string)
}

async fn netease(raw: &str) -> Result<ExternalPlaylist, String> {
    let url = if hosted(raw, &["163cn.tv"]) {
        let resolved = resolve(raw).await?;
        // 重定向后重新校验：短链必须落在预期主机上，我们才能把它的内容当作网易云歌单引用。
        if !hosted(&resolved, &["music.163.com"]) {
            return Err(unsupported_link_error());
        }
        resolved
    } else {
        raw.to_string()
    };
    let id = netease_id(&url).ok_or_else(netease_no_id_error)?;
    // id 会直接进 form body：拒绝任何可能追加额外参数的东西（裸 id 兜底已保证是数字，
    // 但上面的 query/fragment 路径没有）。
    if !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(netease_no_id_error());
    }
    let headers = browser_headers("https://music.163.com/");

    let resp = http()
        .post("https://music.163.com/api/v6/playlist/detail")
        .headers(headers.clone())
        .body(format!("id={id}"))
        .send()
        .await
        .map_err(|e| {
            tr!(
                "KUGOU_IMPORT_NETEASE_REQUEST_FAILED",
                "NetEase Cloud Music playlist request failed: {{err}}",
                err = e.to_string()
            )
            .to_string()
        })?;
    let text = resp.text().await.map_err(|e| {
        tr!(
            "KUGOU_IMPORT_RESPONSE_READ_FAILED",
            "Failed to read the response: {{err}}",
            err = e.to_string()
        )
        .to_string()
    })?;
    let body: Value = serde_json::from_str(&text).map_err(|e| {
        tr!(
            "KUGOU_IMPORT_NETEASE_PARSE_FAILED",
            "Failed to parse the NetEase Cloud Music response: {{err}}",
            err = e.to_string()
        )
        .to_string()
    })?;

    let playlist = body.get("playlist").ok_or_else(|| {
        tr!(
            "KUGOU_IMPORT_NETEASE_BAD_RESPONSE",
            "Unexpected NetEase Cloud Music response: no playlist information"
        )
        .to_string()
    })?;
    let name = playlist
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| tr!("KUGOU_IMPORT_DEFAULT_NAME").to_string());

    let ids: Vec<i64> = playlist
        .get("trackIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("id").and_then(Value::as_i64))
        .collect();

    let songs = if ids.is_empty() {
        // 兜底：歌单载荷可能内嵌部分 `tracks`
        playlist
            .get("tracks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        netease_song_names(&headers, &ids).await
    };

    let songs = dedupe(songs);
    if songs.is_empty() {
        return Err(tr!(
            "KUGOU_IMPORT_NETEASE_NO_SONGS",
            "No songs found in the NetEase Cloud Music playlist; it may be private"
        )
        .to_string());
    }
    Ok(ExternalPlaylist { name, songs })
}

/// 每次 `/api/v3/song/detail` 请求所带的 id 数。
const NETEASE_SONG_DETAIL_CHUNK: usize = 400;

async fn netease_song_names(headers: &HeaderMap, ids: &[i64]) -> Vec<String> {
    let mut out = Vec::new();
    for (chunk_idx, chunk) in ids.chunks(NETEASE_SONG_DETAIL_CHUNK).enumerate() {
        let Some(body) = netease_song_detail(headers, chunk_idx, chunk).await else {
            continue;
        };
        if let Some(songs) = body.get("songs").and_then(Value::as_array) {
            out.extend(songs.iter().filter_map(|s| {
                s.get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
            }));
        } else {
            tracing::warn!(
                chunk = chunk_idx,
                "netease song detail response missing songs array"
            );
        }
    }
    out
}

/// 为一块 id 发一次 `/api/v3/song/detail` 请求。每次失败都带块序号记日志并按 `None` 上报：
/// 一块坏掉最多损失它自己的那些歌，而不是整次导入。
async fn netease_song_detail(
    headers: &HeaderMap,
    chunk_idx: usize,
    chunk: &[i64],
) -> Option<Value> {
    let payload = format!(
        "[{}]",
        chunk
            .iter()
            .map(|id| format!("{{\"id\":{id}}}"))
            .collect::<Vec<_>>()
            .join(",")
    );

    let resp = http()
        .post("https://music.163.com/api/v3/song/detail")
        .headers(headers.clone())
        .body(payload)
        .send()
        .await
        .inspect_err(
            |err| tracing::warn!(chunk = chunk_idx, %err, "netease song detail request failed"),
        )
        .ok()?;
    let text = resp
        .text()
        .await
        .inspect_err(|err| {
            tracing::warn!(
                chunk = chunk_idx,
                %err,
                "failed to read netease song detail body"
            )
        })
        .ok()?;
    serde_json::from_str(&text)
        .inspect_err(|err| {
            tracing::warn!(
                chunk = chunk_idx,
                %err,
                "failed to parse netease song detail JSON"
            )
        })
        .ok()
}

// ==== QQ 音乐 ====

fn qq_id(url: &str) -> Option<i64> {
    if let Some(pos) = url.find("playlist/") {
        let rest = &url[pos + "playlist/".len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if end > 0
            && let Ok(id) = rest[..end].parse()
        {
            return Some(id);
        }
    }
    if let Some(v) = query_value(url, "id")
        && let Ok(id) = v.parse()
    {
        return Some(id);
    }
    // 通用的尾部 `id=NNN`，忽略 `mid=`/`songid=` 这类更长的键
    bare_id_value(url)?.parse().ok()
}

const QQ_PLATFORMS: [&str; 7] = [
    "-1",
    "android",
    "iphone",
    "h5",
    "wxfshare",
    "iphone_wx",
    "windows",
];

/// 遍历 QQ 歌单时每页请求的歌曲数（`song_num` 参数）。
const QQ_PAGE_SIZE: i64 = 30;
/// 分页游标的硬上限，避免一个胡来的 `total` 让循环永不结束。
const QQ_MAX_PAGE_BEGIN: i64 = 10_000;

struct QqPage {
    name: String,
    total: Option<i64>,
    songs: Vec<String>,
}

async fn qq(raw: &str) -> Result<ExternalPlaylist, String> {
    let url = if raw.contains("fcgi-bin") {
        let resolved = resolve(raw).await?;
        // 重定向后的重新校验，缘由与 netease 路径相同。
        if !hosted(
            &resolved,
            &["y.qq.com", "qqmusic.qq.com", "music.qq.com", "c.y.qq.com"],
        ) {
            return Err(unsupported_link_error());
        }
        resolved
    } else {
        raw.to_string()
    };
    let id = qq_id(&url).ok_or_else(|| {
        tr!(
            "KUGOU_IMPORT_QQ_NO_ID",
            "Could not parse a playlist ID from the QQ Music link"
        )
        .to_string()
    })?;
    let headers = browser_headers("https://y.qq.com/");

    let first = qq_page(&headers, id, 0, QQ_PAGE_SIZE).await?;
    let name = if first.name.is_empty() {
        tr!("KUGOU_IMPORT_DEFAULT_NAME").to_string()
    } else {
        first.name
    };
    let total = first.total.unwrap_or(first.songs.len() as i64);
    let mut songs = first.songs;
    let mut begin = QQ_PAGE_SIZE;
    while (songs.len() as i64) < total && begin < QQ_MAX_PAGE_BEGIN {
        match qq_page(&headers, id, begin, QQ_PAGE_SIZE).await {
            Ok(page) if !page.songs.is_empty() => {
                songs.extend(page.songs);
                begin += QQ_PAGE_SIZE;
            }
            // 空页：列表的正常结束，不需要日志
            Ok(_) => break,
            Err(err) => {
                tracing::warn!(id, begin, %err, "QQ playlist page fetch failed; stopping pagination");
                break;
            }
        }
    }

    let songs = dedupe(songs);
    if songs.is_empty() {
        return Err(tr!(
            "KUGOU_IMPORT_QQ_NO_SONGS",
            "No songs found in the QQ Music playlist"
        )
        .to_string());
    }
    Ok(ExternalPlaylist { name, songs })
}

async fn qq_page(headers: &HeaderMap, id: i64, begin: i64, num: i64) -> Result<QqPage, String> {
    for platform in QQ_PLATFORMS {
        let body = qq_body(id, platform, begin, num);
        let sign = qq_sign(&body);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let url = format!("https://u6.y.qq.com/cgi-bin/musics.fcg?sign={sign}&_={ms}");

        let resp = match http()
            .post(&url)
            .headers(headers.clone())
            .body(body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                tracing::debug!(platform, %err, "QQ playlist page request failed");
                continue;
            }
        };
        if !resp.status().is_success() {
            tracing::debug!(platform, status = %resp.status(), "QQ playlist page returned non-success status");
            continue;
        }
        let text = match resp.text().await {
            Ok(text) => text,
            Err(err) => {
                tracing::debug!(platform, %err, "failed to read QQ playlist page body");
                continue;
            }
        };
        if let Some(page) = qq_parse(&text) {
            return Ok(page);
        }
        tracing::debug!(
            platform,
            "QQ playlist page response unusable (bad JSON, non-zero code, or missing data)"
        );
    }
    tracing::warn!(
        id,
        begin,
        "all QQ platforms failed to fetch the playlist page"
    );
    Err(tr!(
        "KUGOU_IMPORT_QQ_FETCH_FAILED",
        "Failed to fetch the QQ Music playlist data"
    )
    .to_string())
}

fn qq_parse(json: &str) -> Option<QqPage> {
    let root: Value = serde_json::from_str(json).ok()?;
    if root.get("code").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let data = root.pointer("/req_0/data")?;
    let name = data
        .pointer("/dirinfo/title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let total = data.pointer("/dirinfo/songnum").and_then(Value::as_i64);
    let songs = data
        .get("songlist")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            s.get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
        })
        .collect();
    Some(QqPage { name, total, songs })
}

fn qq_body(id: i64, platform: &str, begin: i64, num: i64) -> String {
    format!(
        "{{\"req_0\":{{\"module\":\"music.srfDissInfo.aiDissInfo\",\"method\":\"uniform_get_Dissinfo\",\"param\":{{\"disstid\":{id},\"enc_host_uin\":\"\",\"tag\":1,\"userinfo\":1,\"song_begin\":{begin},\"song_num\":{num}}}}},\"comm\":{{\"g_tk\":5381,\"uin\":0,\"format\":\"json\",\"platform\":\"{platform}\"}}}}"
    )
}

/// u6 网关的 `zzb` sign（`sign=` 查询参数）：从大写 MD5 里按下标挑字符，把 hex 字节与 `L1`
/// 异或折叠，再经 base64 表 `T` 重新编码。常量、下标挑取与运算顺序就是这个方案本身——
/// 这里什么都不要改。
fn qq_sign(param: &str) -> String {
    const L1: [u8; 16] = [
        212, 45, 80, 68, 195, 163, 163, 203, 157, 220, 254, 91, 204, 79, 104, 6,
    ];
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";

    let md5 = crypto::md5_hex(param).to_uppercase();
    let chars: Vec<char> = md5.chars().collect();
    let select = |idx: &[usize]| idx.iter().map(|&i| chars[i]).collect::<String>();
    let t1 = select(&[21, 4, 9, 26, 16, 20, 27, 30]);
    let t3 = select(&[18, 11, 3, 2, 1, 7, 6, 25]);
    let hexv = |c: char| -> u8 {
        if c.is_ascii_digit() {
            c as u8 - b'0'
        } else if ('A'..='F').contains(&c) {
            c as u8 - b'A' + 10
        } else {
            c as u8 - b'a' + 10
        }
    };

    let mut ls2 = [0u8; 16];
    for i in 0..16 {
        ls2[i] = hexv(chars[i * 2]).wrapping_mul(16) ^ hexv(chars[i * 2 + 1]) ^ L1[i];
    }

    let mut ls3 = String::new();
    for i in 0..6 {
        if i == 5 {
            ls3.push(T[(ls2[15] >> 2) as usize] as char);
            ls3.push(T[((ls2[15] & 3) << 4) as usize] as char);
        } else {
            let b0 = ls2[i * 3];
            let b1 = ls2[i * 3 + 1];
            let b2 = ls2[i * 3 + 2];
            ls3.push(T[(b0 >> 2) as usize] as char);
            ls3.push(T[((b1 >> 4) | ((b0 & 3) << 4)) as usize] as char);
            ls3.push(T[((b2 >> 6) | ((b1 & 15) << 2)) as usize] as char);
            ls3.push(T[(b2 & 63) as usize] as char);
        }
    }

    let t2: String = ls3.chars().filter(|&c| c != '/' && c != '+').collect();
    format!("zzb{t1}{t2}{t3}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::bare_id_value;

    #[test]
    fn bare_id_matches_exact_keys_and_skips_longer_key_tails() {
        // 精确 `id=` 键照旧可用，包括第一个参数的 host 与首段混在一起、`query_value`
        // 匹配不上的那种链接
        assert_eq!(
            bare_id_value("https://y.qq.com/taoge.html?id=456&host=1"),
            Some("456")
        );
        assert_eq!(
            bare_id_value("…/playlist?id=7217666066&userid=123"),
            Some("7217666066")
        );
        assert_eq!(bare_id_value("id=1"), Some("1"));
        assert_eq!(bare_id_value("…#id=456"), Some("456"));
        // 以 `id=` 结尾的更长键会被跳过，不会被误认作 id
        assert_eq!(bare_id_value("…/playlist?mid=88"), None);
        assert_eq!(bare_id_value("…taoge.html?songid=456"), None);
        assert_eq!(bare_id_value("…?userid=123"), None);
        // 更长键之后真正的 `id=` 仍然能找到
        assert_eq!(bare_id_value("…?mid=1&id=2"), Some("2"));
        // 空值以 None 结束
        assert_eq!(bare_id_value("…?id=&tid=5"), None);
    }
}
