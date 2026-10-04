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
use tokio::io::AsyncWriteExt;

use crate::{
    kugou::KugouClient,
    media::http_source::{http_cover_bytes, shared_http_client},
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

/// Windows 保留设备名：文件名首个点号前的 stem 命中（不区分大小写）即被
/// Win32 当作设备引用，即使带扩展名（如 "CON.flac"）。
const WINDOWS_RESERVED_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Strips characters that are illegal in Windows file names, and prefixes an
/// underscore to reserved device-name stems (CON, COM1-9, ...), which Win32
/// resolves to a device even when a file extension follows.
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
        return "track".into();
    }
    let stem = trimmed.split('.').next().unwrap_or_default();
    if WINDOWS_RESERVED_DEVICES
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
    {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Upper bound on waiting for any single network event (response headers,
/// one body chunk). 请求级不设总死线：整曲 FLAC 几十至上百 MB，慢速链路下
/// 固定总超时必然误杀正常下载，改为按单次网络事件限时、停滞即报错。
const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Streams `url` straight into `dest` through a fixed ~64KB write buffer:
/// the whole track is never held in memory. Returns the accumulated byte
/// count. Any failure — including fewer than 1KB downloaded — removes the
/// partial file first, so an `Err` never leaves debris on disk.
///
/// 走 `chunk()` 而非 `bytes_stream()`：后者被 reqwest 的 `stream` feature
/// 门控（本仓未启用），`chunk()` 无 feature 门，同为逐块拉取。
async fn http_download_to_file(url: &str, dest: &Path) -> Result<u64, String> {
    let request = shared_http_client()
        .get(url)
        .header(USER_AGENT, HeaderValue::from_static(KUGOU_UA));
    let mut response = tokio::time::timeout(STREAM_IO_TIMEOUT, request.send())
        .await
        .map_err(|_| format!("network stalled: no response for {STREAM_IO_TIMEOUT:?}"))?
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
        loop {
            let chunk = tokio::time::timeout(STREAM_IO_TIMEOUT, response.chunk())
                .await
                .map_err(|_| format!("network stalled: no data for {STREAM_IO_TIMEOUT:?}"))?
                .map_err(|err| err.to_string())?;
            let Some(chunk) = chunk else { break };
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
    client: &KugouClient,
    track: &KugouTrackInfo,
    dir: &Path,
) -> Result<PathBuf, String> {
    let (url, quality) = resolve_best_url(client, track).await?;
    let ext = if quality == "flac" { "flac" } else { "mp3" };

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

    // 建目录与音频落盘都走 tokio::fs（内部在阻塞池执行，不占 RUNTIME 仅有的
    // 2 个 worker）；音频边下边写，整首曲子不再整份驻留内存。
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|err| format!("create download dir: {err}"))?;
    http_download_to_file(&url, &audio_path).await?;

    // Lyric sidecar + tags/cover are best-effort: the audio file is already saved.
    let (lrc, krc) = match fetch_lyrics(client, track).await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(hash = %track.hash, error = %err, "kugou download: lyrics fetch failed");
            (None, None)
        }
    };
    let tag_lrc = lrc.clone();
    let sidecar_dir = dir.to_path_buf();
    // spawn_blocking needs 'static: the track reference can't cross the
    // closure, so the hash is cloned out first.
    let sidecar_hash = track.hash.clone();
    if let Err(err) = crate::RUNTIME
        .spawn_blocking(move || {
            if let Some(krc) = &krc
                && let Err(err) = std::fs::write(sidecar_dir.join(format!("{stem}.krc")), krc)
            {
                tracing::warn!(hash = %sidecar_hash, error = %err, "kugou download: failed to write .krc lyrics sidecar");
            }
            if let Some(lrc) = &lrc
                && let Err(err) = std::fs::write(sidecar_dir.join(format!("{stem}.lrc")), lrc)
            {
                tracing::warn!(hash = %sidecar_hash, error = %err, "kugou download: failed to write .lrc lyrics sidecar");
            }
        })
        .await
    {
        tracing::warn!(hash = %track.hash, error = %err, "kugou download: lyrics sidecar task did not complete");
    }
    let cover = if track.cover_url.is_empty() {
        None
    } else {
        match http_cover_bytes(&track.cover_url).await {
            Ok(cover) => cover,
            Err(err) => {
                tracing::warn!(hash = %track.hash, error = %err, "kugou download: cover fetch failed");
                None
            }
        }
    };
    let tag_path = audio_path.clone();
    let tag_track = track.clone();
    match crate::RUNTIME
        .spawn_blocking(move || {
            embed_tags(&tag_path, &tag_track, cover.as_deref(), tag_lrc.as_deref())
        })
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(hash = %track.hash, error = %err, "kugou download: failed to embed tags/cover into audio file");
        }
        Err(err) => {
            tracing::warn!(hash = %track.hash, error = %err, "kugou download: tag embedding task did not complete");
        }
    }

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
        let joined = crate::RUNTIME
            .spawn(async move {
                // 排队等待许可也在 RUNTIME 上，不占下载线程。
                let _permit = DOWNLOAD_PERMITS
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                download_track(&client, &track, &dir).await
            })
            .await;
        // Keep the business error (HTTP status, empty file, …) distinct from a
        // genuinely panicked task; flattening both into one string used to
        // report every failure as "download task panicked".
        let result = match joined {
            Ok(Ok(path)) => Ok(path),
            Ok(Err(err)) => Err(err),
            Err(err) if err.is_panic() => Err("download task panicked".to_string()),
            Err(err) => Err(err.to_string()),
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_device_stems_get_underscore_prefix() {
        for name in ["CON", "PRN", "AUX", "NUL"] {
            assert_eq!(sanitize_filename(name), format!("_{name}"));
        }
        for n in 1..=9u8 {
            for prefix in ["COM", "LPT"] {
                let name = format!("{prefix}{n}");
                assert_eq!(sanitize_filename(&name), format!("_{name}"));
            }
        }
    }

    #[test]
    fn reserved_device_stems_match_case_insensitively() {
        assert_eq!(sanitize_filename("con"), "_con");
        assert_eq!(sanitize_filename("Nul"), "_Nul");
        assert_eq!(sanitize_filename("cOm5"), "_cOm5");
    }

    #[test]
    fn reserved_stem_with_extension() {
        assert_eq!(sanitize_filename("CON.flac"), "_CON.flac");
        assert_eq!(sanitize_filename("lpt3.mp3"), "_lpt3.mp3");
    }

    #[test]
    fn legal_names_pass_through_unchanged() {
        assert_eq!(sanitize_filename("Congo - Live"), "Congo - Live");
        assert_eq!(sanitize_filename("com10"), "com10");
        assert_eq!(sanitize_filename("my.CON"), "my.CON");
        assert_eq!(sanitize_filename("a<b:c"), "a_b_c");
        assert_eq!(sanitize_filename(""), "track");
    }
}
