use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use url::Url;

/// 音频缓存目录名，位于 `%APPDATA%\meliora\data\audio_cache\`。
const AUDIO_CACHE_DIR: &str = "audio_cache";

/// 磁盘音频缓存总上限：默认 1GB（1024 * 1024 * 1024 字节）。
const AUDIO_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// 超限时逐出到 90% 低水位：满缓存（实测 49 首约 902MB）下若只删到刚好
/// 1GB，几乎每次写盘都要重跑一遍全量目录扫描。
const AUDIO_CACHE_LOW_WATERMARK_FACTOR: u64 = 9;

/// 触发全量清扫的写盘计数阈值：每累计完成 10 首曲目缓存，触发一次后台淘汰。
const AUDIO_CACHE_WRITES_BEFORE_PRUNE: usize = 10;

/// 缓存条目保活期：30 天未访问的陈旧音频文件将被清除。
const AUDIO_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 运行时开关：设置页「启用音频缓存」直接推到这里，读写两侧都按它短路。
static AUDIO_CACHE_ENABLED: AtomicBool = AtomicBool::new(true);

/// 设置或 watcher 回读时调用；关掉的同时取消在途下载，避免继续占带宽写盘。
pub fn set_audio_cache_enabled(enabled: bool) {
    AUDIO_CACHE_ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        abort_active_download();
    }
}

pub fn audio_cache_enabled() -> bool {
    AUDIO_CACHE_ENABLED.load(Ordering::Relaxed)
}

/// 距离上次清理后累计完成缓存的写盘计数。
static CACHE_WRITES_SINCE_PRUNE: AtomicUsize = AtomicUsize::new(0);
/// 防止并发重入清扫的原子锁。
static CACHE_PRUNE_RUNNING: AtomicBool = AtomicBool::new(false);

/// 单槽在途下载：保存 `JoinHandle` 才能等 future 真正 drop 后再删 tmp——
/// abort 本身不释放 tokio 文件句柄，Windows 上立即 `remove_file` 会失败。
struct ActiveDownload {
    handle: JoinHandle<()>,
    tmp_path: PathBuf,
}
static ACTIVE_DOWNLOAD: Mutex<Option<ActiveDownload>> = Mutex::new(None);

fn take_active_download() -> Option<ActiveDownload> {
    let mut guard = ACTIVE_DOWNLOAD.lock().ok()?;
    guard.take()
}

/// 切歌/关闭开关：abort 后在后台等句柄释放，再删掉半截 tmp。
fn abort_active_download() {
    let Some(active) = take_active_download() else {
        return;
    };
    active.handle.abort();
    crate::RUNTIME.spawn(async move {
        let _ = active.handle.await;
        let _ = tokio::fs::remove_file(&active.tmp_path).await;
    });
}

/// 清除缓存：abort 并把句柄交回调用方，等 future drop 后再删目录。
fn take_aborted_download() -> Option<JoinHandle<()>> {
    let active = take_active_download()?;
    active.handle.abort();
    Some(active.handle)
}

/// 获取音频缓存主目录路径。
pub fn audio_cache_dir() -> PathBuf {
    crate::paths::data_dir().join(AUDIO_CACHE_DIR)
}

/// 根据音频路径（URL）推导唯一的稳定缓存键。
/// 优先使用酷狗独有的内容 hash（即使 CDN 或签名变了依然稳稳命中）；
/// 若无法提取 hash，则按 scheme + host + path 计算哈希。
pub fn audio_cache_key_for_path(path: &Path) -> Option<String> {
    #[cfg(feature = "online_sources")]
    if let Some(matched) = crate::online_sources::identify_path(path) {
        match matched.identity {
            crate::online_sources::OnlineIdentity::Kugou { hash, .. } if !hash.is_empty() => {
                return Some(format!("kugou_{hash}"));
            }
            _ => {}
        }
    }

    let text = path.to_str()?;
    if let Ok(url) = Url::parse(text) {
        let stable_url = format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            url.path()
        );
        let mut hasher = DefaultHasher::new();
        stable_url.hash(&mut hasher);
        return Some(format!("url_{:016x}", hasher.finish()));
    }

    None
}

/// 探测本地磁盘是否已经存在完整缓存的音频文件。
/// 存在且大小合格（> 1024 字节）时返回 `Some(PathBuf)`。
pub fn get_cached_audio_path(path: &Path) -> Option<PathBuf> {
    if !audio_cache_enabled() {
        return None;
    }
    let key = audio_cache_key_for_path(path)?;
    find_cached_audio_in(&audio_cache_dir(), &key)
}

/// 在给定目录里找该 key 的完整缓存文件（例如 "kugou_xxx.flac"）。前缀只在
/// 循环外构造一次：满缓存下每次开曲都遍历约 50 个目录项，旧写法每项都
/// 为同一个 key 分配一次 String。
fn find_cached_audio_in(dir: &Path, key: &str) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }

    let entries = fs::read_dir(dir).ok()?;
    let prefix = format!("{key}.");
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if (name_str == key || name_str.starts_with(&prefix)) && !name_str.ends_with(".tmp") {
            let candidate = entry.path();
            if let Ok(meta) = candidate.metadata()
                && meta.is_file()
                && meta.len() > 1024
            {
                return Some(candidate);
            }
        }
    }

    None
}

/// 在后台异步启动整曲流式缓存任务。
/// 不阻塞播放，全速从头拉取完整音频并原子转正到本地磁盘。
pub fn spawn_background_cache_task(
    path: &Path,
    url: Url,
    ext_hint: Option<String>,
    known_miss: bool,
) {
    if !audio_cache_enabled() {
        return;
    }
    let key = match audio_cache_key_for_path(path) {
        Some(k) => k,
        None => return,
    };

    let dir = audio_cache_dir();
    // 起播路径刚在 `get_cached_audio_path` 里按 key 扫过目录（未命中），
    // `known_miss` 让它跳过第二次 O(N) 扫描；预打开挂接路径没查过，仍需查。
    // 按 key 认任意扩展名：只 stat `{key}.{ext}` 会让已缓存的曲目再下一整曲。
    if !known_miss && find_cached_audio_in(&dir, &key).is_some() {
        return;
    }
    let ext = ext_hint.unwrap_or_else(|| "audio".to_string());
    let target_path = dir.join(format!("{key}.{ext}"));

    let tmp_path = dir.join(format!("{key}.tmp"));
    let active_tmp_path = tmp_path.clone();

    // 1. 取消上一首尚未下载完成的在途任务，并回收它写了一半的 tmp
    abort_active_download();

    // 2. 启动新的下载任务，先经历开播防抖窗口
    let task = crate::RUNTIME.spawn(async move {
        // 开播 2.5 秒稳定防抖：快速切歌试听直接早退，不消耗网络与磁盘
        tokio::time::sleep(Duration::from_millis(2500)).await;
        // 防抖窗口里用户可能已经关掉了缓存：不再为它打开连接与 tmp。
        if !audio_cache_enabled() {
            return;
        }

        if target_path.exists() {
            return;
        }

        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            warn!("Failed to create audio cache dir: {e}");
            return;
        }

        // 使用共享客户端发起请求
        let mut response = match crate::media::http_source::shared_http_client()
            .get(url.clone())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                debug!(status = %resp.status(), "Audio cache download rejected by server");
                return;
            }
            Err(e) => {
                debug!(err = %e, "Audio cache download failed to connect");
                return;
            }
        };

        let file = match tokio::fs::File::create(&tmp_path).await {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to create audio cache tmp file: {e}");
                return;
            }
        };

        let mut writer = tokio::io::BufWriter::new(file);
        let mut total_written = 0u64;

        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    // 下载途中关掉开关：立即停流并清掉半截文件，而不是把整曲拉完。
                    if !audio_cache_enabled() {
                        drop(writer);
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return;
                    }
                    if let Err(e) = writer.write_all(&chunk).await {
                        warn!("Audio cache write error: {e}");
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return;
                    }
                    total_written += chunk.len() as u64;
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("Audio cache stream error: {e}");
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return;
                }
            }
        }

        if let Err(e) = writer.flush().await {
            warn!("Audio cache flush error: {e}");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return;
        }

        if let Err(e) = writer.shutdown().await {
            warn!("Audio cache shutdown error: {e}");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return;
        }

        drop(writer);

        if total_written < 1024 {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return;
        }

        if let Err(e) = tokio::fs::rename(&tmp_path, &target_path).await {
            warn!("Audio cache rename failed: {e}");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return;
        }

        info!(
            path = %target_path.display(),
            bytes = total_written,
            "Successfully cached complete online track in background"
        );

        record_audio_cache_write();
    });

    if let Ok(mut guard) = ACTIVE_DOWNLOAD.lock() {
        *guard = Some(ActiveDownload {
            handle: task,
            tmp_path: active_tmp_path,
        });
    }
}

/// 累计完成写盘，达到阈值后在后台线程执行一次有界 LRU 清理。
fn record_audio_cache_write() {
    let writes = CACHE_WRITES_SINCE_PRUNE.fetch_add(1, Ordering::Relaxed) + 1;
    if writes < AUDIO_CACHE_WRITES_BEFORE_PRUNE {
        return;
    }
    if CACHE_PRUNE_RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }

    crate::RUNTIME.spawn(async {
        let _ = crate::RUNTIME.spawn_blocking(prune_audio_cache).await;
        CACHE_WRITES_SINCE_PRUNE.store(0, Ordering::Relaxed);
        CACHE_PRUNE_RUNNING.store(false, Ordering::Relaxed);
    });
}

/// 启动时延迟执行一次后台音频缓存清理（推迟 30s 免抢启动期 IO）。
pub fn prune_audio_cache_background() {
    crate::RUNTIME.spawn(async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let _ = crate::RUNTIME.spawn_blocking(prune_audio_cache).await;
    });
}

/// 手动清除的结果：`failed` 多为 Windows 上仍被播放中的缓存文件，下次清扫接手。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearOutcome {
    pub files: usize,
    pub bytes: u64,
    pub failed: usize,
}

/// 取消在途下载、等它释放 tmp 句柄，再清空缓存目录（保留目录本身）。
/// 由设置页在 `RUNTIME` 上调用；删除跑在 `spawn_blocking`，不占 UI 线程。
pub async fn clear_audio_cache() -> ClearOutcome {
    if let Some(handle) = take_aborted_download() {
        let _ = handle.await;
    }
    crate::RUNTIME
        .spawn_blocking(|| clear_files_in(&audio_cache_dir()))
        .await
        .unwrap_or_default()
}

fn clear_files_in(dir: &Path) -> ClearOutcome {
    let Ok(entries) = fs::read_dir(dir) else {
        return ClearOutcome::default();
    };
    let mut outcome = ClearOutcome::default();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {
                outcome.files += 1;
                outcome.bytes += meta.len();
            }
            Err(_) => outcome.failed += 1,
        }
    }
    CACHE_WRITES_SINCE_PRUNE.store(0, Ordering::Relaxed);
    outcome
}

/// 目录内文件总字节数（测试用）：清除结果本身已带删除量，UI 不必再读一次目录。
#[cfg(test)]
fn usage_bytes_in(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// 阻塞式音频缓存清扫函数（运行在 spawn_blocking）。
fn prune_audio_cache() {
    prune_audio_cache_in(
        &audio_cache_dir(),
        AUDIO_CACHE_MAX_BYTES,
        AUDIO_CACHE_MAX_AGE,
        SystemTime::now(),
    );
}

/// `dir`/`now` 可注入，让低水位逐出与 tmp 容量核算能用临时目录钉住。
fn prune_audio_cache_in(dir: &Path, max_bytes: u64, max_age: Duration, now: SystemTime) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    struct CachedItem {
        path: PathBuf,
        size: u64,
        modified: SystemTime,
    }

    let mut items = Vec::new();
    let mut total_bytes = 0u64;

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }

        // 孤立临时文件（超过 1 小时）直接删；在途下载正持有句柄，不能拿它
        // 当 LRU 逐出候选，但它的体积必须计入总量，否则快速切歌留下的多份
        // 半截文件不受 1GB 上限约束。
        if path.extension().is_some_and(|ext| ext == "tmp") {
            if meta.modified().is_ok_and(|mtime| {
                now.duration_since(mtime).unwrap_or_default() > Duration::from_secs(3600)
            }) {
                let _ = fs::remove_file(&path);
            } else {
                total_bytes += meta.len();
            }
            continue;
        }

        let size = meta.len();
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

        // 超过 30 天未访问的文件直接淘汰
        if now.duration_since(modified).unwrap_or_default() > max_age {
            let _ = fs::remove_file(&path);
            continue;
        }

        total_bytes += size;
        items.push(CachedItem {
            path,
            size,
            modified,
        });
    }

    // 超限时按最久未访问（mtime 升序）逐出到低水位，而不是刚好压回上限。
    let low_watermark = max_bytes / 10 * AUDIO_CACHE_LOW_WATERMARK_FACTOR;
    if total_bytes > max_bytes {
        items.sort_by_key(|item| item.modified);
        for item in items {
            if total_bytes <= low_watermark {
                break;
            }
            if fs::remove_file(&item.path).is_ok() {
                total_bytes = total_bytes.saturating_sub(item.size);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn audio_cache_key_generation_is_stable() {
        let path1 = Path::new("http://example.com/audio/song1.flac?token=123");
        let path2 = Path::new("http://example.com/audio/song1.flac?token=456");
        let key1 = audio_cache_key_for_path(path1).unwrap();
        let key2 = audio_cache_key_for_path(path2).unwrap();
        assert_eq!(
            key1, key2,
            "Stable URL hashing must ignore volatile query tokens"
        );
    }

    #[test]
    fn find_cached_audio_ignores_tmp_and_other_keys() {
        let dir = TestDir::new("audio-cache-lookup");
        let complete = dir.join("kugou_a.flac");
        fs::write(&complete, vec![0u8; 2048]).unwrap();
        fs::write(dir.join("kugou_b.tmp"), vec![0u8; 2048]).unwrap();

        let root = dir.utf8_path();
        assert_eq!(
            find_cached_audio_in(root.as_std_path(), "kugou_a"),
            Some(complete)
        );
        assert_eq!(find_cached_audio_in(root.as_std_path(), "kugou_b"), None);
        assert_eq!(find_cached_audio_in(root.as_std_path(), "missing"), None);
    }

    #[test]
    fn prune_counts_active_tmp_and_evicts_to_low_watermark() {
        let dir = TestDir::new("audio-cache-prune");
        for name in ["a.flac", "b.flac", "c.flac"] {
            fs::write(dir.join(name), vec![0u8; 100]).unwrap();
        }
        let tmp = dir.join("d.tmp");
        fs::write(&tmp, vec![0u8; 50]).unwrap();

        // max_bytes=250 → 低水位 225；三个 100B 音频 + 50B tmp = 350B。
        // 不把 tmp 计入总量只会删一个文件，计入后必须删到 150B。
        prune_audio_cache_in(
            dir.utf8_path().as_std_path(),
            250,
            AUDIO_CACHE_MAX_AGE,
            SystemTime::now(),
        );

        let remaining: Vec<_> = fs::read_dir(dir.utf8_path().as_std_path())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(remaining.len(), 2, "{remaining:?}");
        assert!(tmp.exists(), "在途 tmp 不能作为 LRU 逐出候选");
    }

    #[test]
    fn prune_removes_orphan_tmp_without_touching_fresh_audio() {
        let dir = TestDir::new("audio-cache-orphan");
        let audio = dir.join("a.flac");
        fs::write(&audio, vec![0u8; 100]).unwrap();
        let tmp = dir.join("b.tmp");
        fs::write(&tmp, vec![0u8; 50]).unwrap();

        // 把 now 推到 2 小时后：tmp 越过 1 小时孤立阈值，音频仍在 30 天内。
        let future = SystemTime::now() + Duration::from_secs(2 * 3600);
        prune_audio_cache_in(
            dir.utf8_path().as_std_path(),
            250,
            AUDIO_CACHE_MAX_AGE,
            future,
        );

        assert!(!tmp.exists());
        assert!(audio.exists());
    }

    #[test]
    fn clear_files_removes_only_files_and_reports_bytes() {
        let dir = TestDir::new("audio-cache-clear");
        fs::write(dir.join("a.flac"), vec![0u8; 100]).unwrap();
        fs::write(dir.join("b.tmp"), vec![0u8; 50]).unwrap();
        fs::create_dir(dir.join("nested")).unwrap();

        let root = dir.utf8_path();
        assert_eq!(usage_bytes_in(root.as_std_path()), 150);
        assert_eq!(
            clear_files_in(root.as_std_path()),
            ClearOutcome {
                files: 2,
                bytes: 150,
                failed: 0,
            }
        );
        assert_eq!(usage_bytes_in(root.as_std_path()), 0);
        assert!(dir.join("nested").is_dir(), "只清文件，不动目录结构");
    }

    #[test]
    fn clear_files_on_missing_dir_is_empty() {
        let dir = TestDir::new("audio-cache-clear-missing");
        let missing = dir.join("missing");
        assert_eq!(clear_files_in(&missing), ClearOutcome::default());
        assert_eq!(usage_bytes_in(&missing), 0);
    }

    #[test]
    fn disabling_cache_short_circuits_lookup() {
        set_audio_cache_enabled(false);
        assert!(!audio_cache_enabled());
        assert_eq!(
            get_cached_audio_path(Path::new("http://example.com/a.flac")),
            None
        );
        set_audio_cache_enabled(true);
        assert!(audio_cache_enabled());
    }
}
