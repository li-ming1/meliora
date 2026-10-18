//! Download a KuGou online track to local disk: audio (auto-highest quality),
//! an LRC lyrics sidecar, and the cover + tags embedded into the audio file.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{LazyLock, OnceLock, RwLock},
};

use cntp_i18n::tr;

use crate::sync::RecoveredRwLock;
use crate::{
    kugou::KugouClient,
    media::{
        download::{embed_tags, http_download_to_file, sanitize_filename},
        http_source::http_cover_bytes,
    },
    toasts::{Toast, emit_toast},
    ui::kugou::KugouTrackInfo,
};

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
    http_download_to_file(&url, &audio_path, &[("User-Agent", KUGOU_UA.to_string())]).await?;

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
    // spawn_blocking needs 'static: the borrowed tag fields are cloned into
    // owned strings (only title/artist/album are embedded).
    let tag_title = track.title.to_string();
    let tag_artist = track.artist.to_string();
    let tag_album = track.album.to_string();
    match crate::RUNTIME
        .spawn_blocking(move || {
            embed_tags(
                &tag_path,
                &tag_title,
                &tag_artist,
                &tag_album,
                cover.as_deref(),
                tag_lrc.as_deref(),
            )
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
    if !pending_downloads().write_recovered().insert(key.clone()) {
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
        pending_downloads().write_recovered().remove(&key);

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
