use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, SystemTime},
};

use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};
use url::Url;

/// 音频缓存目录名，位于 `%APPDATA%\meliora\data\audio_cache\`。
const AUDIO_CACHE_DIR: &str = "audio_cache";

/// 磁盘音频缓存总上限：默认 1GB（1024 * 1024 * 1024 字节）。
const AUDIO_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// 触发全量清扫的写盘计数阈值：每累计完成 10 首曲目缓存，触发一次后台淘汰。
const AUDIO_CACHE_WRITES_BEFORE_PRUNE: usize = 10;

/// 缓存条目保活期：30 天未访问的陈旧音频文件将被清除。
const AUDIO_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 距离上次清理后累计完成缓存的写盘计数。
static CACHE_WRITES_SINCE_PRUNE: AtomicUsize = AtomicUsize::new(0);
/// 防止并发重入清扫的原子锁。
static CACHE_PRUNE_RUNNING: AtomicBool = AtomicBool::new(false);

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
    let key = audio_cache_key_for_path(path)?;
    let dir = audio_cache_dir();
    if !dir.exists() {
        return None;
    }

    // 检查是否有匹配前缀的完整缓存文件（例如 "kugou_xxx.flac" 或 "kugou_xxx.audio"）
    let entries = fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if (name_str == key || name_str.starts_with(&format!("{key}.")))
            && !name_str.ends_with(".tmp")
        {
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
pub fn spawn_background_cache_task(path: &Path, url: Url, ext_hint: Option<String>) {
    let key = match audio_cache_key_for_path(path) {
        Some(k) => k,
        None => return,
    };

    let dir = audio_cache_dir();
    let ext = ext_hint.unwrap_or_else(|| "audio".to_string());
    let target_path = dir.join(format!("{key}.{ext}"));
    if target_path.exists() {
        return;
    }

    let tmp_path = dir.join(format!("{key}.tmp"));

    crate::RUNTIME.spawn(async move {
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

/// 阻塞式音频缓存清扫函数（运行在 spawn_blocking）。
fn prune_audio_cache() {
    let dir = audio_cache_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    struct CachedItem {
        path: PathBuf,
        size: u64,
        modified: SystemTime,
    }

    let now = SystemTime::now();
    let mut items = Vec::new();
    let mut total_bytes = 0u64;

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }

        // 清理残留的未完成临时文件（超过 1 小时即视为孤立）
        if path.extension().is_some_and(|ext| ext == "tmp") {
            if let Ok(mtime) = meta.modified()
                && now.duration_since(mtime).unwrap_or_default() > Duration::from_secs(3600)
            {
                let _ = fs::remove_file(&path);
            }
            continue;
        }

        let size = meta.len();
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

        // 超过 30 天未访问的文件直接淘汰
        if now.duration_since(modified).unwrap_or_default() > AUDIO_CACHE_MAX_AGE {
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

    // 若总容量超过上限（默认 1GB），按最久未访问（mtime 升序）逐出
    if total_bytes > AUDIO_CACHE_MAX_BYTES {
        items.sort_by_key(|item| item.modified);
        for item in items {
            if total_bytes <= AUDIO_CACHE_MAX_BYTES {
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
}
