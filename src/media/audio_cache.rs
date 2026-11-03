use std::{
    fs::{self, File},
    hash::{DefaultHasher, Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, SystemTime},
};

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

/// 流式写入音频缓存并在完成后原子转正的写汇（Sink）。
pub struct AudioCacheSink {
    file: Option<File>,
    tmp_path: PathBuf,
    target_path: PathBuf,
    written_bytes: u64,
    expected_bytes: Option<u64>,
    aborted: bool,
}

impl AudioCacheSink {
    /// 尝试为指定路径开启一个后台音频流缓存通道。
    /// 若无法推导键或已存在缓存，则返回 `None`。
    pub fn try_new(
        path: &Path,
        expected_bytes: Option<u64>,
        ext_hint: Option<&str>,
    ) -> Option<Self> {
        let key = audio_cache_key_for_path(path)?;
        let dir = audio_cache_dir();
        if let Err(e) = fs::create_dir_all(&dir) {
            warn!("Failed to create audio cache dir: {e}");
            return None;
        }

        let ext = ext_hint.unwrap_or("audio");
        let target_path = dir.join(format!("{key}.{ext}"));
        if target_path.exists() {
            // 已存在完整缓存文件，无需重复写入
            return None;
        }

        let tmp_path = dir.join(format!("{key}.tmp"));
        let file = match File::create(&tmp_path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to create audio cache tmp file: {e}");
                return None;
            }
        };

        Some(Self {
            file: Some(file),
            tmp_path,
            target_path,
            written_bytes: 0,
            expected_bytes,
            aborted: false,
        })
    }

    /// 追加写入数据块。
    pub fn append_chunk(&mut self, chunk: &[u8]) {
        if self.aborted || chunk.is_empty() {
            return;
        }

        let Some(file) = &mut self.file else {
            return;
        };

        if let Err(e) = file.write_all(chunk) {
            warn!("Failed to write chunk to audio cache: {e}");
            self.abort_and_cleanup();
            return;
        }
        self.written_bytes += chunk.len() as u64;
    }

    /// 标记发生非线性 Seek，丢弃当前缓存以防止保存半截/乱序文件。
    pub fn abort_due_to_seek(&mut self) {
        if !self.aborted {
            debug!("Aborting audio cache due to seek operation");
            self.abort_and_cleanup();
        }
    }

    /// 流完全读到 EOF 时调用，校验完整性并原子转正。
    pub fn finish_and_commit(mut self) {
        if self.aborted {
            return;
        }

        let mut file = match self.file.take() {
            Some(f) => f,
            None => return,
        };

        // 刷新文件缓冲并校验大小
        if let Err(e) = file.flush() {
            warn!("Failed to flush audio cache file: {e}");
            self.abort_and_cleanup();
            return;
        }

        if self.written_bytes < 1024 {
            // 文件过小，可能是不完整流或错误页
            self.abort_and_cleanup();
            return;
        }

        if let Some(expected) = self.expected_bytes
            && self.written_bytes != expected
        {
            warn!(
                written = self.written_bytes,
                expected, "Audio cache size mismatch, discarding cache"
            );
            self.abort_and_cleanup();
            return;
        }

        // Drop file 释放 Windows 下的文件独占锁，确保 rename 顺利进行
        drop(file);

        if let Err(e) = fs::rename(&self.tmp_path, &self.target_path) {
            warn!("Failed to commit audio cache file: {e}");
            let _ = fs::remove_file(&self.tmp_path);
            return;
        }

        info!(
            path = %self.target_path.display(),
            bytes = self.written_bytes,
            "Successfully cached complete audio track to disk"
        );

        // 记一次写盘，必要时触发后台清扫
        record_audio_cache_write();
    }

    fn abort_and_cleanup(&mut self) {
        self.aborted = true;
        self.file = None;
        let _ = fs::remove_file(&self.tmp_path);
    }
}

impl Drop for AudioCacheSink {
    fn drop(&mut self) {
        self.file = None;
        if !self.aborted && self.tmp_path.exists() {
            let _ = fs::remove_file(&self.tmp_path);
        }
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
