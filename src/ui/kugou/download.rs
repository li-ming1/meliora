//! Download a KuGou online track to local disk: audio (auto-highest quality),
//! an LRC lyrics sidecar, and the cover + tags embedded into the audio file.

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

use crate::{
    kugou::KugouClient,
    media::http_source::http_cover_bytes,
    toasts::{Toast, emit_toast},
    ui::kugou::KugouTrackInfo,
};
use zed_reqwest::header::{HeaderValue, USER_AGENT};

use super::{extract_song_url, first_lyric_candidate, lyric_search_keyword};

/// 同时进行的曲目下载数上限（每个下载几十 MB 音频 + 写盘 + 标签嵌入）。
static DOWNLOAD_PERMITS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(3));

/// 正在下载中的曲目 hash。进行中的曲目忽略重复点击；无论成败，下载
/// 结束后都会移除，失败后允许重下。
static PENDING_DOWNLOADS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn pending_downloads() -> &'static RwLock<HashSet<String>> {
    PENDING_DOWNLOADS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Same Android user agent the signed client uses; the CDN returns a 403 for
/// the default reqwest UA.
const KUGOU_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

/// Quality ladder tried in order; the first tier that yields a playable URL wins.
const QUALITY_LADDER: [&str; 3] = ["flac", "320", "128"];

/// Strips characters that are illegal in Windows file names.
fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "track".into()
    } else {
        trimmed.to_string()
    }
}

/// Fetches `url` into memory (audio files are a few tens of MB, fine to hold).
async fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let client = zed_reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(url)
        .header(USER_AGENT, HeaderValue::from_static(KUGOU_UA))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|err| err.to_string())
}

/// Best-quality playable URL for `track` (flac -> 320 -> 128), asking for the
/// full track (`free_part = false`) so downloads never save the ~60s trial.
async fn resolve_best_url(
    client: &KugouClient,
    track: &KugouTrackInfo,
) -> Result<(String, &'static str), String> {
    for quality in QUALITY_LADDER {
        if let Ok(resp) = client
            .song_url(
                &track.hash,
                track.mix_song_id,
                track.album_id,
                quality,
                false,
            )
            .await
            && let Some(url) = extract_song_url(&resp.body)
        {
            return Ok((url, quality));
        }
    }
    Err(tr!(
        "KUGOU_DOWNLOAD_NO_URL",
        "No full-track URL for this song (lossless usually needs VIP)"
    )
    .to_string())
}

/// Fetches the lyrics for `track` from the same search candidate in both
/// formats: the plain line-timed `.lrc` text and the decrypted word-level
/// KRC text (karaoke). Either may be absent.
async fn fetch_lyrics(
    client: &KugouClient,
    track: &KugouTrackInfo,
) -> Result<(Option<String>, Option<String>), String> {
    let keyword = lyric_search_keyword(track);
    let search = client
        .search_lyric(&track.hash, &keyword, track.duration)
        .await
        .map_err(|err| err.to_string())?;
    let Some((id, accesskey)) = first_lyric_candidate(&search.body) else {
        return Ok((None, None));
    };
    let lrc = client
        .lyric_lrc(&id, &accesskey)
        .await
        .ok()
        .filter(|lrc| !lrc.trim().is_empty());
    let krc = client
        .lyric_krc(&id, &accesskey)
        .await
        .ok()
        .and_then(|krc| crate::ui::lyrics::krc::decrypt_krc(&krc))
        .filter(|krc| !krc.trim().is_empty());
    Ok((lrc, krc))
}

/// Writes title/artist/album, lyrics and the cover into the downloaded file.
fn embed_tags(
    path: &Path,
    track: &KugouTrackInfo,
    cover: Option<Vec<u8>>,
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
        let mime = if bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            MimeType::Jpeg
        } else {
            MimeType::Png
        };
        let picture = Picture::unchecked(bytes)
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
    client: &KugouClient,
    track: &KugouTrackInfo,
    dir: &Path,
) -> Result<PathBuf, String> {
    let (url, quality) = resolve_best_url(client, track).await?;
    let ext = if quality == "flac" { "flac" } else { "mp3" };

    std::fs::create_dir_all(dir).map_err(|err| format!("create download dir: {err}"))?;

    let stem = sanitize_filename(&format!(
        "{}{}",
        if track.artist.is_empty() {
            String::new()
        } else {
            format!("{} - ", track.artist)
        },
        if track.title.is_empty() {
            track.hash.clone()
        } else {
            track.title.to_string()
        }
    ));
    let audio_path = dir.join(format!("{stem}.{ext}"));

    let bytes = http_get_bytes(&url).await?;
    if bytes.len() < 1024 {
        return Err("downloaded file is empty".into());
    }
    std::fs::write(&audio_path, &bytes).map_err(|err| format!("write audio: {err}"))?;

    // Lyric sidecar + tags/cover are best-effort: the audio file is already saved.
    let (lrc, krc) = fetch_lyrics(client, track).await.unwrap_or((None, None));
    if let Some(krc) = &krc {
        let _ = std::fs::write(dir.join(format!("{stem}.krc")), krc);
    }
    if let Some(lrc) = &lrc {
        let _ = std::fs::write(dir.join(format!("{stem}.lrc")), lrc);
    }
    let cover = if track.cover_url.is_empty() {
        None
    } else {
        http_cover_bytes(&track.cover_url).await.ok().flatten()
    };
    let _ = embed_tags(&audio_path, track, cover, lrc.as_deref());

    Ok(audio_path)
}

/// Fire-and-forget entry point used by the UI: downloads on the Tokio runtime
/// and reports the outcome with a toast. Cheap, so it is safe to call from a
/// click handler.
///
/// 去重 + 并发上限：同一曲目在下载进行中时的再次点击直接忽略；全局
/// 同时只允许 `DOWNLOAD_PERMITS` 个下载在跑，其余在 RUNTIME 上排队
/// await（不阻塞线程）。PENDING 条目在下载结束后移除，失败也允许重下。
pub fn download_track_ui(cx: &mut gpui::App, track: KugouTrackInfo) {
    // 去重检查是同步临界区，不跨 await 持锁（对齐 kugou.rs 的
    // PENDING_FETCHES 模式）。
    let key = track.hash.clone();
    if !pending_downloads()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone())
    {
        tracing::info!(hash = %track.hash, "kugou download already in flight; ignoring click");
        return;
    }

    let dir = cx
        .global::<crate::settings::SettingsGlobal>()
        .model
        .read(cx)
        .playback
        .effective_download_dir();
    cx.spawn(async move |cx| {
        let client = crate::kugou::shared_client();
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
                "KUGOU_DOWNLOADED",
                "Downloaded to {{path}}",
                path = path.display().to_string()
            ))),
            Err(err) => emit_toast(Toast::error(tr!(
                "KUGOU_DOWNLOAD_FAILED",
                "Download failed: {{err}}",
                err = err
            ))),
        });
    })
    .detach();
}
