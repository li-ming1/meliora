//! Parse NetEase Cloud Music / QQ Music playlist share links into plain song
//! names, so they can be matched back into KuGou and imported as a new
//! playlist. Purely HTTP + JSON; no KuGou signing is involved here.
//! Mirrors `KuGou.Net/ExternalPlaylists/*` in spirit and endpoint.

use std::{sync::OnceLock, time::Duration};

use serde_json::Value;
use zed_reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};

use super::crypto;

/// A parsed external playlist: playlist name and a flat list of song titles
/// (matching into KuGou happens later).
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
    let set = |h: &mut HeaderMap, n: &str, v: &str| {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(n), HeaderValue::try_from(v)) {
            h.insert(n, v);
        }
    };
    set(&mut headers, "User-Agent", BROWSER_UA);
    set(&mut headers, "Referer", referer);
    headers
}

/// Parses a pasted NetEase/QQ share text into an `ExternalPlaylist`.
pub async fn parse_share_link(text: &str) -> Result<ExternalPlaylist, String> {
    let url = extract_url(text).ok_or_else(|| "未识别到有效链接".to_string())?;
    if hosted(&url, &["music.163.com", "163cn.tv"]) {
        netease(&url).await
    } else if hosted(&url, &["y.qq.com", "qqmusic.qq.com", "music.qq.com", "c.y.qq.com"]) {
        qq(&url).await
    } else {
        Err("仅支持网易云和 QQ 音乐歌单链接".to_string())
    }
}

/// Copies the URL out of arbitrary share text (handles trailing punctuation).
fn extract_url(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let start = trimmed.find("http")?;
    let rest = &trimmed[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(
        rest[..end]
            .trim_end_matches(|c: char| {
                matches!(c, ')' | ']' | '】' | '」' | '』' | '，' | ',' | '。' | '；' | ';')
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
    needles.iter().any(|n| host.contains(n))
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

/// Follows a short link and returns its final URL.
async fn resolve(url: &str) -> Result<String, String> {
    let resp = http().get(url).send().await.map_err(|e| format!("短链解析失败：{e}"))?;
    Ok(resp.url().to_string())
}

fn dedupe(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
    v
}

// ==== NetEase Cloud Music ====

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
    let sub = &url[after..];
    let start = sub.find("id=")? + 3;
    let rest = &sub[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    (!rest[..end].is_empty()).then(|| rest[..end].to_string())
}

async fn netease(raw: &str) -> Result<ExternalPlaylist, String> {
    let url = if hosted(raw, &["163cn.tv"]) {
        resolve(raw).await?
    } else {
        raw.to_string()
    };
    let id = netease_id(&url).ok_or_else(|| "未在网易云链接中解析到歌单ID".to_string())?;
    let headers = browser_headers("https://music.163.com/");

    let resp = http()
        .post("https://music.163.com/api/v6/playlist/detail")
        .headers(headers.clone())
        .body(format!("id={id}"))
        .send()
        .await
        .map_err(|e| format!("请求网易云歌单失败：{e}"))?;
    let text = resp.text().await.map_err(|e| format!("读取响应失败：{e}"))?;
    let body: Value =
        serde_json::from_str(&text).map_err(|e| format!("网易云响应解析失败：{e}"))?;

    let playlist = body
        .get("playlist")
        .ok_or_else(|| "网易云响应异常，未找到歌单信息".to_string())?;
    let name = playlist
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "导入歌单".to_string());

    let ids: Vec<i64> = playlist
        .get("trackIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("id").and_then(Value::as_i64))
        .collect();

    let songs = if ids.is_empty() {
        // fallback: the playlist payload may embed partial `tracks`
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
        return Err("网易云歌单未解析到歌曲，可能为私密歌单".to_string());
    }
    Ok(ExternalPlaylist {
        name,
        songs,
    })
}

async fn netease_song_names(headers: &HeaderMap, ids: &[i64]) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in ids.chunks(400) {
        let mut payload = String::from("[");
        for (i, id) in chunk.iter().enumerate() {
            if i > 0 {
                payload.push(',');
            }
            payload.push_str(&format!("{{\"id\":{id}}}"));
        }
        payload.push(']');

        let Ok(resp) = http()
            .post("https://music.163.com/api/v3/song/detail")
            .headers(headers.clone())
            .body(payload)
            .send()
            .await
        else {
            continue;
        };
        let Ok(text) = resp.text().await else { continue };
        let Ok(body) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(songs) = body.get("songs").and_then(Value::as_array) {
            out.extend(songs.iter().filter_map(|s| {
                s.get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
            }));
        }
    }
    out
}

// ==== QQ Music ====

fn qq_id(url: &str) -> Option<i64> {
    if let Some(pos) = url.find("playlist/") {
        let rest = &url[pos + "playlist/".len()..];
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
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
    // generic trailing `id=NNN`
    let start = url.find("id=")? + 3;
    let rest = &url[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if end > 0 {
        rest[..end].parse().ok()
    } else {
        None
    }
}

const QQ_PLATFORMS: [&str; 7] = ["-1", "android", "iphone", "h5", "wxfshare", "iphone_wx", "windows"];

struct QqPage {
    name: String,
    total: Option<i64>,
    songs: Vec<String>,
}

async fn qq(raw: &str) -> Result<ExternalPlaylist, String> {
    let url = if raw.contains("fcgi-bin") {
        resolve(raw).await?
    } else {
        raw.to_string()
    };
    let id = qq_id(&url).ok_or_else(|| "未在 QQ 音乐链接中解析到歌单ID".to_string())?;
    let headers = browser_headers("https://y.qq.com/");

    let first = qq_page(&headers, id, 0, 30).await?;
    let name = if first.name.is_empty() {
        "导入歌单".to_string()
    } else {
        first.name
    };
    let total = first.total.unwrap_or(first.songs.len() as i64);
    let mut songs = first.songs;
    let mut begin = 30i64;
    while (songs.len() as i64) < total && begin < 10_000 {
        match qq_page(&headers, id, begin, 30).await {
            Ok(page) if !page.songs.is_empty() => {
                songs.extend(page.songs);
                begin += 30;
            }
            _ => break,
        }
    }

    let songs = dedupe(songs);
    if songs.is_empty() {
        return Err("QQ 音乐歌单未解析到歌曲".to_string());
    }
    Ok(ExternalPlaylist {
        name,
        songs,
    })
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

        let Ok(resp) = http().post(&url).headers(headers.clone()).body(body).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(text) = resp.text().await else { continue };
        if let Some(page) = qq_parse(&text) {
            return Ok(page);
        }
    }
    Err("QQ 音乐歌单数据获取失败".to_string())
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
    let total = data
        .pointer("/dirinfo/songnum")
        .and_then(Value::as_i64);
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

fn qq_sign(param: &str) -> String {
    const L1: [u8; 16] = [212, 45, 80, 68, 195, 163, 163, 203, 157, 220, 254, 91, 204, 79, 104, 6];
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