//! 把酷狗在线曲目下载到本地磁盘：音频（自动取最高音质）、LRC 歌词 sidecar，
//! 以及嵌进音频文件的封面与标签。

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

/// 与签名客户端相同的 Android user agent；用 reqwest 默认 UA 时 CDN 返回 403。
const KUGOU_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

/// 按顺序尝试的音质梯队；第一个拿到可播放 URL 的档位胜出。
const QUALITY_LADDER: [&str; 3] = ["flac", "320", "128"];

/// `track` 的最高音质可播放 URL（flac -> 320 -> 128），并请求完整曲目
/// （`free_part = false`），这样下载绝不会存下那条约 60 秒的试听片段。
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

/// 从同一个搜索候选项取 `track` 的两种格式歌词：按行带时间的纯 `.lrc` 文本，与解密后的
/// 逐字 KRC 文本（卡拉OK）。两者都可能缺失。
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

/// 把 `track` 下载到 `dir` 并返回保存下来的音频文件路径。
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

    // 歌词 sidecar 与标签/封面都是尽力而为：音频文件此时已经存好了。
    let (lrc, krc) = match fetch_lyrics(client, track).await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(hash = %track.hash, error = %err, "kugou download: lyrics fetch failed");
            (None, None)
        }
    };
    let tag_lrc = lrc.clone();
    let sidecar_dir = dir.to_path_buf();
    // spawn_blocking 要 'static：曲目引用跨不过闭包，所以先把 hash 克隆出来。
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
    // spawn_blocking 需要 'static：借来的标签字段被克隆成自有字符串
    // （实际只嵌入 title/artist/album）。
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

/// UI 使用的发射后不管入口：在 Tokio runtime 上下载，并用 toast 报告结果。
/// 开销很低，所以从 click handler 里调用也安全。
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
        // 把业务错误（HTTP 状态、空文件…）与真正 panic 的任务区分开；两者过去被
        // 压成同一个字符串，于是所有失败都报成 "download task panicked"。
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
