//! Download a NetEase online track to local disk: audio (highest quality the
//! account can fetch), `.lrc` + `.yrc` lyrics sidecars, and the cover + tags
//! embedded into the audio file.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{LazyLock, OnceLock, RwLock},
    time::Duration,
};

use cntp_i18n::tr;
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::ItemKey,
    tag::{Accessor, ItemValue, Tag, TagExt, TagItem, TagType},
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::{
    media::http_source::{http_cover_bytes, shared_http_client},
    netease::NeteaseClient,
    toasts::{Toast, emit_toast},
    ui::netease::NeteaseTrackInfo,
};

use super::extract_song_url;

/// 同时进行的曲目下载数上限（每个下载几十至上百 MB 音频流式落盘 + 标签嵌入）。
static DOWNLOAD_PERMITS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(3));

/// 正在下载中的曲目 id。进行中的曲目忽略重复点击；无论成败，下载
/// 结束后都会移除，失败后允许重下。
static PENDING_DOWNLOADS: OnceLock<RwLock<HashSet<i64>>> = OnceLock::new();

fn pending_downloads() -> &'static RwLock<HashSet<i64>> {
    PENDING_DOWNLOADS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Quality ladder tried in order; the first tier that yields a full-track URL
/// wins. Trial clips (`freeTrialInfo`) are never saved.
const QUALITY_LADDER: [&str; 5] = ["hires", "lossless", "exhigh", "higher", "standard"];

/// Replaces characters that are illegal in Windows file names, trims stray
/// whitespace and leading/trailing dots, and falls back to `track` when
/// nothing remains.
fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() { "track" } else { trimmed }.to_string()
}

/// Streams `url` straight into `dest` through a fixed ~64KB write buffer:
/// the whole track is never held in memory. Returns the accumulated byte
/// count. Any failure — including fewer than 1KB downloaded — removes the
/// partial file first, so an `Err` never leaves debris on disk.
///
/// 走 `chunk()` 而非 `bytes_stream()`：后者被 reqwest 的 `stream` feature
/// 门控（本仓未启用），`chunk()` 无 feature 门，同为逐块拉取。
async fn http_download_to_file(url: &str, dest: &Path) -> Result<u64, String> {
    let mut response = shared_http_client()
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let mut total = 0u64;
    let result = async {
        let mut file = tokio::io::BufWriter::with_capacity(
            // 固定 ~64KB：落盘系统调用粒度与网络块到达节奏解耦。
            64 * 1024,
            tokio::fs::File::create(dest)
                .await
                .map_err(|err| format!("write audio: {err}"))?,
        );
        while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
            file.write_all(&chunk)
                .await
                .map_err(|err| format!("write audio: {err}"))?;
            total += chunk.len() as u64;
        }
        // BufWriter 被 drop 不会自动冲刷，成功路径必须显式冲刷并关闭。
        file.flush()
            .await
            .map_err(|err| format!("write audio: {err}"))?;
        file.shutdown()
            .await
            .map_err(|err| format!("write audio: {err}"))?;
        Ok::<(), String>(())
    }
    .await;

    if let Err(err) = result {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(err);
    }
    if total < 1024 {
        let _ = tokio::fs::remove_file(dest).await;
        return Err("downloaded file is empty".into());
    }
    Ok(total)
}

/// Best-quality full-track URL for `track` (hires -> ... -> standard),
/// rejecting trial clips so downloads never save the ~30s preview.
async fn resolve_best_url(
    client: &NeteaseClient,
    track: &NeteaseTrackInfo,
) -> Result<(String, &'static str), String> {
    for level in QUALITY_LADDER {
        if let Ok(resp) = client.song_url(track.id, level).await
            && let Some((url, trial)) = extract_song_url(&resp.body)
            && !trial
        {
            return Ok((url, level));
        }
    }
    Err(tr!(
        "NETEASE_DOWNLOAD_NO_URL",
        "No full-track URL for this song (higher tiers usually need VIP)"
    )
    .to_string())
}

/// Fetches the lyrics for `track` from `lyric_new` in both formats: the plain
/// line-timed `.lrc` text and the word-level YRC karaoke text. Either may be
/// absent. Translations (`tlyric`) are folded into the `.lrc` sidecar text so
/// plain players see them too.
async fn fetch_lyrics(
    client: &NeteaseClient,
    track: &NeteaseTrackInfo,
) -> (Option<String>, Option<String>) {
    let Ok(resp) = client.lyric_new(track.id).await else {
        return (None, None);
    };
    let field = |key: &str| {
        resp.body
            .pointer(&format!("/{key}/lyric"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let lrc = field("lrc");
    let tlyric = field("tlyric");
    let yrc = field("yrc");

    let lrc = if lrc.is_empty() {
        None
    } else if !tlyric.is_empty() {
        // NetEase translated LRC is line-timed the same way; append each
        // translation line right after its original (the common bilingual
        // sidecar convention).
        let mut merged = String::new();
        let translations: Vec<(String, String)> = tlyric
            .lines()
            .filter_map(|line| {
                let (time, text) = line.split_once(']')?;
                Some((time.to_string(), text.to_string()))
            })
            .collect();
        for line in lrc.lines() {
            merged.push_str(line);
            merged.push('\n');
            if let Some((time, _)) = line.split_once(']')
                && let Some((_, text)) = translations.iter().find(|(t, _)| *t == time)
            {
                merged.push_str(time);
                merged.push(']');
                merged.push_str(text);
                merged.push('\n');
            }
        }
        Some(merged)
    } else {
        Some(lrc)
    };

    let yrc = (!yrc.is_empty()).then_some(yrc);
    (lrc, yrc)
}

/// Writes title/artist/album, lyrics and the cover into the downloaded file.
fn embed_tags(
    path: &Path,
    track: &NeteaseTrackInfo,
    cover: Option<&[u8]>,
    lrc: Option<&str>,
) -> Result<(), String> {
    let tag_type = match path.extension().and_then(|ext| ext.to_str()) {
        Some("flac") => TagType::VorbisComments,
        _ => TagType::Id3v2,
    };
    let mut tag = Tag::new(tag_type);
    if !track.title.is_empty() {
        tag.set_title(track.title.to_string());
    }
    if !track.artist.is_empty() {
        tag.set_artist(track.artist.to_string());
    }
    if !track.album.is_empty() {
        tag.set_album(track.album.to_string());
    }
    if let Some(lrc) = lrc {
        tag.insert(TagItem::new(
            ItemKey::UnsyncLyrics,
            ItemValue::Text(lrc.to_string()),
        ));
    }
    if let Some(bytes) = cover {
        // JPEG magic bytes; anything else is assumed to be PNG.
        let mime = if bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            MimeType::Jpeg
        } else {
            MimeType::Png
        };
        // lofty 的 Picture::unchecked 只收 owned Vec（内部 Cow::Owned），
        // 这一次拷贝是封面全量在下载链路上的唯一一份。
        let picture = Picture::unchecked(bytes.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build();
        tag.push_picture(picture);
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|err| err.to_string())
}

/// Downloads `track` into `dir` and returns the saved audio file path.
pub async fn download_track(
    client: &NeteaseClient,
    track: &NeteaseTrackInfo,
    dir: &Path,
) -> Result<PathBuf, String> {
    let (url, level) = resolve_best_url(client, track).await?;
    let ext = if level == "lossless" || level == "hires" {
        // the `type` field of the URL response is authoritative, but both
        // tiers are FLAC in practice
        "flac"
    } else {
        "mp3"
    };

    // File name stem: "artist - title", with the track id standing in for a
    // missing title. The audio file and the lyric sidecars below share it.
    let title = if track.title.is_empty() {
        format!("netease-{}", track.id)
    } else {
        track.title.to_string()
    };
    let artist_prefix = if track.artist.is_empty() {
        String::new()
    } else {
        format!("{} - ", track.artist)
    };
    let stem = sanitize_filename(&format!("{artist_prefix}{title}"));
    let audio_path = dir.join(format!("{stem}.{ext}"));

    // 建目录与音频落盘都走 tokio::fs（内部在阻塞池执行，不占 RUNTIME 仅有的
    // 2 个 worker）；音频边下边写，整首曲子不再整份驻留内存。
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|err| format!("create download dir: {err}"))?;
    http_download_to_file(&url, &audio_path).await?;

    // Lyric sidecars + tags/cover are best-effort: the audio file is already saved.
    let (lrc, yrc) = fetch_lyrics(client, track).await;
    let sidecar_dir = dir.to_path_buf();
    // lrc 随闭包落盘一趟后原样归还，省一次整段歌词克隆；落盘任务若 panic
    // 则按 best-effort 丢弃（不嵌入标签）。
    let tag_lrc = crate::RUNTIME
        .spawn_blocking(move || {
            if let Some(yrc) = &yrc {
                let _ = std::fs::write(sidecar_dir.join(format!("{stem}.yrc")), yrc);
            }
            if let Some(lrc) = &lrc {
                let _ = std::fs::write(sidecar_dir.join(format!("{stem}.lrc")), lrc);
            }
            lrc
        })
        .await
        .ok()
        .flatten();
    let cover = if track.cover_url.is_empty() {
        None
    } else {
        http_cover_bytes(&track.cover_url).await.ok().flatten()
    };
    let tag_path = audio_path.clone();
    let tag_track = track.clone();
    let _ = crate::RUNTIME
        .spawn_blocking(move || {
            embed_tags(&tag_path, &tag_track, cover.as_deref(), tag_lrc.as_deref())
        })
        .await;

    Ok(audio_path)
}

/// Fire-and-forget entry point used by the UI: downloads on the Tokio runtime
/// and reports the outcome with a toast. Cheap, so it is safe to call from a
/// click handler.
///
/// 去重 + 并发上限：同一曲目在下载进行中时的再次点击直接忽略；全局
/// 同时只允许 `DOWNLOAD_PERMITS` 个下载在跑，其余在 RUNTIME 上排队
/// await（不阻塞线程）。PENDING 条目在下载结束后移除，失败也允许重下。
pub fn download_track_ui(cx: &mut gpui::App, track: NeteaseTrackInfo) {
    // 去重检查是同步临界区，不跨 await 持锁。
    let key = track.id;
    if !pending_downloads()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key)
    {
        tracing::info!(
            id = track.id,
            "netease download already in flight; ignoring click"
        );
        return;
    }

    let dir = cx
        .global::<crate::settings::SettingsGlobal>()
        .model
        .read(cx)
        .playback
        .effective_download_dir();
    cx.spawn(async move |cx| {
        let client = crate::netease::shared_client();
        let result = crate::RUNTIME
            .spawn(async move {
                // 排队等待许可也在 RUNTIME 上，不占下载线程。
                let _permit = DOWNLOAD_PERMITS
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                download_track(&client, &track, &dir).await
            })
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| "download task panicked".to_string());

        // 无论成败都移除进行中标记（防失败后永远无法重下），再发提示。
        pending_downloads()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);

        cx.update(|_cx| match result {
            Ok(path) => emit_toast(Toast::success(tr!(
                "NETEASE_DOWNLOADED",
                "Downloaded to {{path}}",
                path = path.display().to_string()
            ))),
            Err(err) => emit_toast(Toast::error(tr!(
                "NETEASE_DOWNLOAD_FAILED",
                "Download failed: {{err}}",
                err = err
            ))),
        });
    })
    .detach();
}
