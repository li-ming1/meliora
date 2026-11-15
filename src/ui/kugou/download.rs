//! 把酷狗在线曲目下载到本地磁盘：音频（自动取最高音质）、LRC 歌词 sidecar，
//! 以及嵌进音频文件的封面与标签。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{LazyLock, OnceLock, RwLock},
    time::Duration,
};

use cntp_i18n::tr;

use crate::sync::RecoveredRwLock;
use crate::{
    kugou::KugouClient,
    media::{
        download::{embed_tags, http_download_to_file, sanitize_filename, sweep_stale_part_files},
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

/// `track` 的最高音质可播放 URL（flac -> 320 -> 128，与播放侧共用同一梯队），
/// 并请求完整曲目（`free_part = false`），这样下载绝不会存下那条约 60 秒的
/// 试听片段。顺带取出服务端申报的文件字节数（缺失为 `None`），供落盘后校验截断。
async fn resolve_best_url(
    client: &KugouClient,
    track: &KugouTrackInfo,
) -> Result<(String, &'static str, Option<u64>), String> {
    for quality in crate::online_sources::kugou::quality_ladder("flac")
        .iter()
        .rev()
    {
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
            let expected_size = resp
                .body
                .get("fileSize")
                .and_then(serde_json::Value::as_u64)
                .filter(|size| *size > 0);
            return Ok((url, quality, expected_size));
        }
    }
    Err(tr!(
        "KUGOU_DOWNLOAD_NO_URL",
        "No full-track URL for this song (lossless usually needs VIP)"
    )
    .to_string())
}

/// 读文件头 4 字节（文件不足 4 字节返回 `None`）。
async fn read_audio_head(path: &Path) -> Option<[u8; 4]> {
    use tokio::io::AsyncReadExt as _;
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut head = [0u8; 4];
    file.read_exact(&mut head).await.ok()?;
    Some(head)
}

/// 文件头是否是目标音频格式：FLAC 魔数 / ID3 标签 / MPEG 帧同步（`0xFFEx`）。
/// CDN 的「200 + HTML 错误页」与非音频内容都过不了这一关。
fn audio_magic_matches(ext: &str, head: [u8; 4]) -> bool {
    match ext {
        "flac" => &head == b"fLaC",
        _ => head.starts_with(b"ID3") || (head[0] == 0xFF && head[1] & 0xE0 == 0xE0),
    }
}

/// 选落盘用的文件名主干：目标已存在且体积合规（或魔数合法）时视为已下载——
/// 幂等跳过并复用，否则追加 ` (2)`、` (3)`… 递增序号。下载目录常被设成音乐库，
/// 覆盖会静默毁掉用户已有的同名文件。
async fn unique_stem(
    dir: &Path,
    stem: &str,
    ext: &str,
    expected_size: Option<u64>,
) -> (String, bool) {
    let mut candidate = stem.to_string();
    let mut index = 2u32;
    loop {
        let path = dir.join(format!("{candidate}.{ext}"));
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            return (candidate, false);
        };
        let already_present = match expected_size {
            Some(expected) => {
                let tolerance = expected as f64 * 0.01;
                (meta.len() as f64 - expected as f64).abs() <= tolerance
            }
            // 魔数在文件头，尾部截断必过——补一道与下载路径同款的最小体积
            // 下限（<1KB 不可能是有效音频），把"截断产物被判已下载"的窗口压
            // 到 >1KB 的尾部截断（无声明体积 + 连接关闭定界的残余暴露面，
            // 第四轮 H-2）。
            None => {
                meta.len() >= 1024
                    && read_audio_head(&path)
                        .await
                        .is_some_and(|head| audio_magic_matches(ext, head))
            }
        };
        if already_present {
            return (candidate, true);
        }
        candidate = format!("{stem} ({index})");
        index += 1;
    }
}

/// 落盘后的内容校验：魔数为主（错误页一击命中），fileSize 为辅（仅在服务端
/// 申报时比对，1% 容差吸收容器口径差异，避免试听段之外误杀）。
async fn verify_downloaded_audio(
    path: &Path,
    ext: &str,
    expected_size: Option<u64>,
) -> Result<(), String> {
    let Some(head) = read_audio_head(path).await else {
        return Err("downloaded file is too short to be audio".to_string());
    };
    if !audio_magic_matches(ext, head) {
        return Err(format!(
            "downloaded file is not a valid {ext} stream (the server may have returned an error page)"
        ));
    }
    if let Some(expected) = expected_size {
        let actual = tokio::fs::metadata(path)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        if (actual as f64 - expected as f64).abs() > expected as f64 * 0.01 {
            return Err(format!(
                "downloaded file size mismatch: got {actual} bytes, expected {expected}"
            ));
        }
    }
    Ok(())
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
///
/// `permit` 是调用方从 [`DOWNLOAD_PERMITS`] 取到的并发名额，覆盖本函数全程：
/// 两段 spawn_blocking 期间名额随闭包进出（取消安全，2026-10-31 契约 4），
/// async 帧被 abort 时在途阻塞工作仍持票，下载并发上限不被取消旁路。
pub async fn download_track(
    client: &KugouClient,
    track: &KugouTrackInfo,
    dir: &Path,
    permit: tokio::sync::SemaphorePermit<'static>,
) -> Result<PathBuf, String> {
    // 会话首次下载时清扫历史 `.part` 残骸（崩溃/硬阀门中断遗留；计数化命名
    // 后残骸不再被复用截断，跨崩溃无界累积）。每进程至多一次，read_dir 代价
    // 可忽略；24h 阈值避开他方正在写的临时文件。
    static PART_SWEEP_DONE: OnceLock<()> = OnceLock::new();
    if PART_SWEEP_DONE.get().is_none() {
        let dir_owned = dir.to_path_buf();
        let _ = PART_SWEEP_DONE.set(());
        tokio::task::spawn_blocking(move || {
            sweep_stale_part_files(&dir_owned, Duration::from_secs(24 * 3600))
        })
        .await
        .unwrap_or(0);
    }

    let (url, quality, expected_size) = resolve_best_url(client, track).await?;
    let ext = if quality == "flac" { "flac" } else { "mp3" };

    let stem_base = sanitize_filename(&format!(
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

    // 建目录与音频落盘都走 tokio::fs（内部在阻塞池执行，不占 RUNTIME 仅有的
    // 2 个 worker）；音频边下边写，整首曲子不再整份驻留内存。
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|err| format!("create download dir: {err}"))?;

    // 同名文件已存在且体积/魔数合规 = 之前已下载过：幂等跳过，不重下也不覆盖。
    let (stem, already_downloaded) = unique_stem(dir, &stem_base, ext, expected_size).await;
    let audio_path = dir.join(format!("{stem}.{ext}"));
    if already_downloaded {
        tracing::info!(
            hash = %track.hash,
            path = %audio_path.display(),
            "kugou download: file already present; skipping"
        );
        return Ok(audio_path);
    }

    http_download_to_file(&url, &audio_path, &[("User-Agent", KUGOU_UA.to_string())]).await?;

    // 内容校验：200 的 HTML 错误页、半截流不能以音频名义留下（会经扫描器入库）
    if let Err(err) = verify_downloaded_audio(&audio_path, ext, expected_size).await {
        let _ = tokio::fs::remove_file(&audio_path).await;
        return Err(err);
    }

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
    // 取消安全（2026-10-31 契约 4）：名额随阻塞闭包进出——async 帧在本 await
    // 上被 abort 时，在途 sidecar 写盘仍持有名额，下载并发上限不被取消旁路；
    // 闭包正常返回时名额随元组回到帧上，交给下一段嵌入闭包。
    let (permit, ()) = match crate::RUNTIME
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
            (permit, ())
        })
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            // 阻塞任务没能把名额带回来（任务未跑或 panic 时，名额都已随闭包
            // drop 归还信号量），本函数名下已无票可持；嵌入段若继续跑就成了
            // 无票阻塞工作，故连同封面获取一并放弃——音频文件此时已落盘，
            // 与 sidecar 同属尽力而为路径。
            tracing::warn!(hash = %track.hash, error = %err, "kugou download: lyrics sidecar task did not complete");
            return Ok(audio_path);
        }
    };
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
    // 取消安全（2026-10-31 契约 4）：末段嵌入同样随闭包持票——async 帧在本
    // await 上被 abort 时，在途嵌入仍持有名额；闭包返回后名额随元组回帧，
    // 函数收尾即还票。
    match crate::RUNTIME
        .spawn_blocking(move || {
            let ret = embed_tags(
                &tag_path,
                &tag_title,
                &tag_artist,
                &tag_album,
                cover.as_deref(),
                tag_lrc.as_deref(),
            );
            (permit, ret)
        })
        .await
    {
        Ok((_, Ok(()))) => {}
        Ok((_, Err(err))) => {
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
        // 重复点击此前只记 info 日志：用户看不到"已经在下载"，会以为点击失效。
        emit_toast(Toast::info(tr!(
            "KUGOU_DOWNLOAD_IN_PROGRESS",
            "This track is already downloading"
        )));
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
                // 排队等待许可也在 RUNTIME 上，不占下载线程；名额作参数随
                // download_track 全程持有，两段阻塞闭包期间随闭包进出
                // （取消安全，2026-10-31 契约 4）。
                let permit = DOWNLOAD_PERMITS
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                download_track(&client, &track, &dir, permit).await
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use super::audio_magic_matches;

    /// 复刻 `download_track_ui` → `download_track` 的持票链：名额先在 async
    /// 帧上 acquire，再随 spawn_blocking 闭包进出。外层 await 被 abort 后，
    /// 在途闭包必须继续持有名额——绑在 async 帧上的旧形态此刻已把名额还回
    /// 信号量，下载并发上限被取消旁路（取消安全，2026-10-31 契约 4；同型
    /// 回归测试：managed_image::aborting_waiter_does_not_release_running_decode_permit）。
    #[test]
    fn audio_magic_rejects_html_and_accepts_audio_headers() {
        assert!(audio_magic_matches("flac", *b"fLaC"));
        assert!(audio_magic_matches("mp3", *b"ID3\x04"));
        assert!(audio_magic_matches("mp3", [0xFF, 0xFB, 0x90, 0x64]));
        assert!(!audio_magic_matches("mp3", *b"<htm"));
        assert!(!audio_magic_matches("flac", *b"ID3\x04"));
        assert!(!audio_magic_matches("mp3", [0x00, 0x00, 0x00, 0x00]));
    }

    #[test]
    fn aborting_download_frame_does_not_release_running_permit() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let (first_started_tx, first_started_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();

        let first = crate::RUNTIME.spawn({
            let semaphore = semaphore.clone();
            async move {
                // 借用版 acquire() 的名额借本帧局部 Arc，跨不过 spawn_blocking
                // 的 'static 边界；acquire_owned 取自有名额（内部自带 Arc 引用），
                // 与 download_track 的 SemaphorePermit<'static> 参数同型。
                let permit = semaphore.acquire_owned().await.unwrap();
                let _ = crate::RUNTIME
                    .spawn_blocking(move || {
                        first_started_tx.send(()).unwrap();
                        release_first_rx.recv().unwrap();
                        (permit, ())
                    })
                    .await;
            }
        });
        first_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first blocking task did not start");

        // abort 只取消外层 async 帧；此刻名额在在途闭包手里，不得提前还票。
        first.abort();
        assert_eq!(
            semaphore.available_permits(),
            0,
            "abort 后在途阻塞闭包必须继续持有名额"
        );

        // 闭包完成后名额随返回值交还（JoinHandle 已随帧取消被丢弃，输出
        // 随之 drop 还票）。
        release_first_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while semaphore.available_permits() == 0 {
            assert!(
                Instant::now() < deadline,
                "permit was not returned after the blocking closure finished"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
