//! HTTP(S) media source for remote audio URLs (e.g. Kugou direct links).
//!
//! Implements symphonia's [`MediaSource`] (Read + Seek) on top of HTTP range
//! requests, so remote tracks can be decoded without downloading the whole
//! file first. Sequential playback opens a single open-ended `Range` request
//! and streams it chunk by chunk; seeks drop the body and re-request from the
//! target offset on the next read.
//!
//! The URL is carried in the existing `QueueItemData` path field as an opaque
//! string, so queue/session serialization needs no changes.

use std::{
    collections::{
        HashMap,
        hash_map::{DefaultHasher, Entry},
    },
    ffi::OsStr,
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use symphonia::core::io::MediaSource;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use url::Url;
use zed_reqwest::{
    StatusCode,
    header::{CONTENT_RANGE, RANGE},
};

use crate::media::{symphonia::SymphoniaProvider, traits::MediaStream};

/// Shared client for media streaming. Connection pooling keeps sequential
/// reads cheap; no overall request timeout is set because an open-ended range
/// response stays open for the entire track.
static HTTP_CLIENT: LazyLock<zed_reqwest::Client> = LazyLock::new(|| {
    zed_reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Streaming hops hosts once per track; without explicit caps the pool
        // defaults to 90 s idle keep-alive with unlimited idle connections
        // per host (each pinning hyper's read buffer).
        .pool_max_idle_per_host(1)
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent(concat!("Meliora/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| zed_reqwest::Client::new())
});

/// Upper bound on waiting for any single network event while feeding the
/// decoder (response headers, one body chunk). The client deliberately has no
/// total request timeout — a range body stays open for the whole track — but
/// the reads below run on the playback thread: without this bound a server
/// that stalls mid-body blocks the playback main loop forever, commands stop
/// being consumed, the no-progress skip never fires and playback sits silent
/// on a "playing" UI. Surfacing it as an IO error lets the engine skip the
/// track like any other decode failure.
const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-request cap for one-shot cover downloads (`http_cover_bytes`).
const COVER_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Downloads the bytes of a small remote resource (e.g. a KuGou album-cover
/// image). On any non-success status or empty body returns `None`; the caller
/// treats that as "no artwork". Uses a fresh request with a short timeout so
/// cover fetches never block on the long-lived streaming client above.
pub async fn http_cover_bytes(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    // Reuse the shared pooled client (TLS config + connection pool) with a
    // per-request timeout. A fresh Client per cover built a new rustls
    // config and pool per track and left idle-connection teardown work on
    // the runtime after every fetch.
    let request = HTTP_CLIENT.get(url).timeout(COVER_FETCH_TIMEOUT);

    let response = match request.send().await {
        Ok(response) if response.status().is_success() => response,
        _ => return Ok(None),
    };
    let bytes = match response.bytes().await {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => return Ok(None),
    };
    Ok(Some(bytes.to_vec()))
}

// ---------------------------------------------------------------------------
// 在线图片磁盘缓存
//
// 封面/头像 URL 稳定且会反复渲染，同一张图每次会话都重新下载既费流量
// 又拖慢加载。把它们以原始字节落盘（对齐 KugouMusic.NET 的
// BoundedDiskCachedWebImageLoader）：256MB 容量上限，超过按最旧删除，
// 另设 30 天过期窗口清理带签名参数的过期 URL。磁盘读/写/清扫全部走
// `spawn_blocking`，任何失败都静默降级——缓存只是优化，显示不依赖它。
// ---------------------------------------------------------------------------

/// 缓存子目录（相对 `paths::data_dir()`）。
const IMAGE_CACHE_DIR: &str = "image-cache";
/// 磁盘缓存总容量上限。
const IMAGE_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// 超过该寿命的缓存文件会被清扫（URL 常带签名参数，旧的应作废）。
const IMAGE_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// 累计写盘触发一次全量清扫的阈值，避免每次写盘都遍历缓存目录。
const IMAGE_CACHE_WRITES_BEFORE_PRUNE: u64 = 64;
/// 清扫的低水位：删到上限的 90%，避免反复触发。
const IMAGE_CACHE_LOW_WATERMARK_FACTOR: u64 = 9;

/// 距上次清扫的写盘计数。
static CACHE_WRITES_SINCE_PRUNE: AtomicU64 = AtomicU64::new(0);
/// 清扫是否正在运行（防止并发触发多次全量遍历）。
static CACHE_PRUNE_RUNNING: AtomicBool = AtomicBool::new(false);

fn image_cache_dir() -> PathBuf {
    crate::paths::data_dir().join(IMAGE_CACHE_DIR)
}

/// URL 的 64 位散列作为磁盘文件名（非加密；仅用于标识，与内存缓存同源）。
fn image_cache_key(url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 读盘命中则返回原始字节，否则返回 `None`。
async fn read_cached_cover(url: &str) -> Option<Vec<u8>> {
    let path = image_cache_dir().join(image_cache_key(url));
    crate::RUNTIME
        .spawn_blocking(move || std::fs::read(&path).ok())
        .await
        .ok()
        .flatten()
        .filter(|bytes| !bytes.is_empty())
}

/// 写盘（先写临时文件再 rename，避免留下半截文件）；best effort。
async fn write_cached_cover(url: &str, bytes: &[u8]) {
    if bytes.is_empty() || bytes.len() as u64 > IMAGE_CACHE_MAX_BYTES {
        return;
    }

    let dir = image_cache_dir();
    let file = image_cache_key(url);
    let path = dir.join(&file);
    let bytes = bytes.to_vec();
    let write = crate::RUNTIME.spawn_blocking(move || -> std::io::Result<()> {
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("{file}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    });
    if write.await.is_ok() {
        record_cache_write();
    }
}

/// 累计写盘，达到阈值后异步触发一次全量清扫。
fn record_cache_write() {
    let writes = CACHE_WRITES_SINCE_PRUNE.fetch_add(1, Ordering::Relaxed) + 1;
    if writes < IMAGE_CACHE_WRITES_BEFORE_PRUNE {
        return;
    }
    // 已有清扫在跑就直接跳过；清扫结束时计数会被重置。
    if CACHE_PRUNE_RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::RUNTIME.spawn(async {
        let _ = crate::RUNTIME.spawn_blocking(prune_image_cache).await;
        CACHE_WRITES_SINCE_PRUNE.store(0, Ordering::Relaxed);
        CACHE_PRUNE_RUNNING.store(false, Ordering::Relaxed);
    });
}

/// Fire-and-forget full prune on startup (cheap when the cache is small).
/// The periodic sweep only runs after cache writes, so a long gap without
/// cover fetches would otherwise leave stale entries until the 256MB cap is
/// hit; one sweep at boot keeps them bounded by the 30-day age window.
pub fn prune_image_cache_background() {
    crate::RUNTIME.spawn(async {
        let _ = crate::RUNTIME.spawn_blocking(prune_image_cache).await;
    });
}

/// 全量清扫（阻塞，跑在 spawn_blocking）：
/// 1. 删除超过寿命的文件；2. 总大小超限时按最旧删除到低水位。
fn prune_image_cache() {
    let dir = image_cache_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    let now = SystemTime::now();
    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        // interrupted `write_cached_cover` rename can leave `.tmp` behind;
        // they have no hash naming and are never read back, so drop them
        if path.extension() == Some(OsStr::new("tmp")) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > IMAGE_CACHE_MAX_AGE)
        {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        files.push((path, meta.len(), modified));
    }

    let total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= IMAGE_CACHE_MAX_BYTES {
        return;
    }
    let low_watermark = IMAGE_CACHE_MAX_BYTES / 10 * IMAGE_CACHE_LOW_WATERMARK_FACTOR;
    files.sort_by_key(|(_, _, modified)| *modified);
    let mut used = total;
    for (path, len, _) in files {
        if used <= low_watermark {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            used -= len;
        }
    }
}

/// 封面下载的全局并发上限。快速滚动在线列表时一次可能同时挂载几十个
/// 封面元素，无上限时就是 N 个元素打 N 个并发 HTTP 请求；这里把真正
/// 在途的下载限制在 4 个。许可只在 RUNTIME 的 async 上下文里 await，
/// 不阻塞线程。
static COVER_FETCH_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

/// 正在下载中的封面 URL → per-URL 闸门。同一 URL 的并发调用合并为一次
/// 下载：第一个拿到闸门的调用者负责下载并写盘，其余调用者等待闸门
/// 释放后重读磁盘缓存命中，不再发重复请求。调用方都在 RUNTIME 上
/// （async），闸门用 tokio 的 `AsyncMutex` 才能跨 await 持有；查表的
/// std Mutex 只在无 await 的同步临界区里短暂持有。
static INFLIGHT_COVERS: LazyLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 取出（或新建）`url` 的闸门；查表临界区不含任何 await。
/// 返回值第二项只有条目创建者才是 Some——携带 RAII 守卫，Drop 时回收
/// 表条目（含任务被 abort 的路径：守卫随 future 丢弃而执行，堵住
/// "managed_image 中止取回 → 条目永久残留"的无界增长缺口）。
fn inflight_cover_gate(url: &str) -> (Arc<AsyncMutex<()>>, Option<InflightCoverGuard>) {
    let mut inflight = INFLIGHT_COVERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match inflight.entry(url.to_string()) {
        Entry::Occupied(entry) => (entry.get().clone(), None),
        Entry::Vacant(entry) => {
            let gate = Arc::new(AsyncMutex::new(()));
            entry.insert(gate.clone());
            (
                gate.clone(),
                Some(InflightCoverGuard {
                    url: url.to_string(),
                    gate,
                }),
            )
        }
    }
}

/// 创建者的 RAII 守卫：Drop（作用域结束或任务被 abort）时按代际回收表
/// 条目。等待方不持有守卫——它们从不回收条目，语义与手工版本一致。
struct InflightCoverGuard {
    url: String,
    gate: Arc<AsyncMutex<()>>,
}

impl Drop for InflightCoverGuard {
    fn drop(&mut self) {
        release_inflight_cover_gate(&self.url, &self.gate);
    }
}

/// 下载结束后回收闸门条目，防止 in-flight 表无限增长。只有表中仍是
/// 自己这一代闸门时才移除（避免误删后来者新建的）；等待方拿到闸门后
/// 也会走到这里，保证条目最终一定被清掉。
fn release_inflight_cover_gate(url: &str, gate: &Arc<AsyncMutex<()>>) {
    let mut inflight = INFLIGHT_COVERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if inflight
        .get(url)
        .is_some_and(|current| Arc::ptr_eq(current, gate))
    {
        inflight.remove(url);
    }
}

/// 带磁盘缓存的封面/头像加载：先读盘，未命中才下载并写盘。
/// 语义与 `http_cover_bytes` 一致（`Ok(None)` 表示无图或下载失败）。
///
/// 并发控制：同 URL 的并发调用经 per-URL 闸门合并为一次下载，等待方
/// 复用磁盘缓存结果；不同 URL 的在途下载受 `COVER_FETCH_PERMITS`
/// 限制。下载失败仍返回 `Ok(None)`，调用方走无图占位回退。
pub async fn http_cover_bytes_cached(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    if let Some(bytes) = read_cached_cover(url).await {
        return Ok(Some(bytes));
    }

    let (gate, _creator_guard) = inflight_cover_gate(url);
    let _gate = gate.lock().await;

    // 拿到闸门后再查一次盘：前一个持有者可能已经把这张图下载写盘了。
    if let Some(bytes) = read_cached_cover(url).await {
        return Ok(Some(bytes));
    }

    // 真正的 HTTP 下载限制在 4 个并发（写盘不占许可）。所有退出路径的
    // 闸门条目回收由 _creator_guard 的 Drop 负责（含任务被 abort）。
    let bytes = {
        let _permit = COVER_FETCH_PERMITS
            .acquire()
            .await
            .expect("semaphore is never closed");
        http_cover_bytes(url).await?
    };
    if let Some(bytes) = &bytes {
        write_cached_cover(url, bytes).await;
    }
    Ok(bytes)
}

/// Opens the HTTP(S) URL stored in `path` and probes it with the Symphonia
/// decoder provider.
pub fn open_http_media(path: &Path) -> anyhow::Result<Box<dyn MediaStream>> {
    let text = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("track path is not valid UTF-8"))?;
    let url = Url::parse(text).map_err(|e| anyhow::anyhow!("invalid track URL '{text}': {e}"))?;
    let ext = url_extension(&url);

    let source = HttpRangeSource::connect(url)
        .map_err(|e| anyhow::anyhow!("failed to open '{text}': {e}"))?;

    // Background fill + reconnect so network jitter never parks the
    // playback main loop on a synchronous read (see prefetch.rs).
    let source = crate::media::prefetch::PrefetchSource::new(source);

    SymphoniaProvider
        .open_source(Box::new(source), ext.as_deref().map(OsStr::new))
        .map_err(|e| anyhow::anyhow!("failed to probe remote media: {e}"))
}

/// Upper bound for a "short" extension in [`url_extension`]; longer names are
/// left to content probing.
const MAX_URL_EXTENSION_LEN: usize = 8;

/// Extracts a probable file extension from the URL path component. Returns
/// `None` for anything that does not look like a short alphanumeric extension,
/// letting symphonia probe the format from the content instead.
fn url_extension(url: &Url) -> Option<String> {
    let file_name = url.path().rsplit('/').next()?;
    let (_, ext) = file_name.rsplit_once('.')?;
    if ext.is_empty()
        || ext.len() > MAX_URL_EXTENSION_LEN
        || !ext.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Parses the total length out of a `Content-Range: bytes 0-123/456` header.
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// A seekable byte stream over a remote HTTP(S) resource backed by range
/// requests.
///
/// The response body is wrapped in a mutex: symphonia requires the media
/// source to be `Sync` even though only the playback thread ever touches it.
struct HttpRangeSource {
    url: Url,
    /// Logical position of the next byte `read` will return.
    pos: u64,
    /// Total resource length, when the server reports it.
    total_len: Option<u64>,
    /// Whether the server honors `Range` requests.
    range_supported: bool,
    /// The currently open response body, if any.
    body: Option<Mutex<zed_reqwest::Response>>,
    /// Bytes already received but not yet consumed by `read`.
    pending: Vec<u8>,
    pending_offset: usize,
}

impl HttpRangeSource {
    /// Opens the resource and probes range support with a `bytes=0-` request.
    /// The response body is kept as the initial stream.
    fn connect(url: Url) -> Result<Self, String> {
        let response = crate::RUNTIME
            .block_on(async {
                tokio::time::timeout(
                    STREAM_IO_TIMEOUT,
                    HTTP_CLIENT
                        .get(url.clone())
                        .header(RANGE, "bytes=0-")
                        .send(),
                )
                .await
            })
            .map_err(|_| format!("request timed out after {STREAM_IO_TIMEOUT:?}"))?
            .and_then(|response| response.error_for_status())
            .map_err(|e| format!("request failed: {e}"))?;

        let range_supported = response.status() == StatusCode::PARTIAL_CONTENT;
        let total_len = if range_supported {
            response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(content_range_total)
        } else {
            response.content_length()
        };

        tracing::info!(
            url = %url,
            ?total_len,
            range_supported,
            "opened remote media stream"
        );

        Ok(Self {
            url,
            pos: 0,
            total_len,
            range_supported,
            body: Some(Mutex::new(response)),
            pending: Vec::new(),
            pending_offset: 0,
        })
    }

    /// Issues a range request for the current position. Returns `false` when
    /// the position is at/past the end of the resource.
    fn start_body(&mut self) -> io::Result<bool> {
        if let Some(len) = self.total_len
            && self.pos >= len
        {
            return Ok(false);
        }

        let mut request = HTTP_CLIENT.get(self.url.clone());
        if self.range_supported {
            request = request.header(RANGE, format!("bytes={}-", self.pos));
        } else if self.pos != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote server does not support range requests",
            ));
        }

        let response = crate::RUNTIME
            .block_on(async { tokio::time::timeout(STREAM_IO_TIMEOUT, request.send()).await })
            .map_err(|_| io::Error::other("HTTP request timed out"))?
            .and_then(|response| response.error_for_status())
            .map_err(|e| io::Error::other(format!("HTTP request failed: {e}")))?;

        // A server that ignores the Range header replies 200 with the body
        // from byte 0; that is only usable when byte 0 is what we asked for.
        if response.status() != StatusCode::PARTIAL_CONTENT && self.pos != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server ignored the range request",
            ));
        }

        self.body = Some(Mutex::new(response));
        Ok(true)
    }

    /// Copies buffered bytes into `buf`, advancing the logical position.
    fn take_pending(&mut self, buf: &mut [u8]) -> usize {
        let available = &self.pending[self.pending_offset..];
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.pending_offset += count;
        if self.pending_offset == self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
        }
        self.pos += count as u64;
        count
    }
}

impl Read for HttpRangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if self.pending_offset < self.pending.len() {
            return Ok(self.take_pending(buf));
        }

        loop {
            if self.body.is_none() && !self.start_body()? {
                return Ok(0);
            }

            // fetch the next chunk in its own scope so the body lock is
            // released before `self` is touched below; the timeout bounds how
            // long the playback thread can be parked on one read (see
            // STREAM_IO_TIMEOUT)
            let chunk = {
                let Some(body) = self.body.as_ref() else {
                    return Ok(0);
                };
                let mut body = body.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                crate::RUNTIME
                    .block_on(async { tokio::time::timeout(STREAM_IO_TIMEOUT, body.chunk()).await })
            };

            match chunk {
                Ok(Ok(Some(chunk))) if !chunk.is_empty() => {
                    self.pending = chunk.to_vec();
                    self.pending_offset = 0;
                    return Ok(self.take_pending(buf));
                }
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => {
                    self.body = None;
                    return Ok(0);
                }
                Ok(Err(e)) => {
                    self.body = None;
                    return Err(io::Error::other(format!("HTTP stream failed: {e}")));
                }
                Err(_) => {
                    self.body = None;
                    return Err(io::Error::other(format!(
                        "remote stream stalled: no data for {STREAM_IO_TIMEOUT:?}"
                    )));
                }
            }
        }
    }
}

impl Seek for HttpRangeSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            // seeking past the end is allowed and reads back EOF
            SeekFrom::End(delta) => self.total_len.and_then(|len| len.checked_add_signed(delta)),
        };

        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek target is out of range",
            ));
        };

        // Drop any open stream and buffered bytes; the next read re-requests
        // from the new offset.
        self.body = None;
        self.pending.clear();
        self.pending_offset = 0;
        self.pos = target;

        Ok(target)
    }
}

impl MediaSource for HttpRangeSource {
    fn is_seekable(&self) -> bool {
        self.range_supported
    }

    fn byte_len(&self) -> Option<u64> {
        self.total_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::is_http_path;

    #[test]
    fn is_http_path_matches_schemes_case_insensitively() {
        assert!(is_http_path(Path::new("http://example.com/a.mp3")));
        assert!(is_http_path(Path::new("HTTPS://example.com/a.mp3")));
        assert!(!is_http_path(Path::new("ftp://example.com/a.mp3")));
        assert!(!is_http_path(Path::new("C:\\music\\a.mp3")));
        assert!(!is_http_path(Path::new("music/a.mp3")));
    }

    #[test]
    fn url_extension_ignores_query_and_strips_long_values() {
        let url = Url::parse("https://cdn.example.com/path/track.mp3?sign=abc").unwrap();
        assert_eq!(url_extension(&url).as_deref(), Some("mp3"));

        let url = Url::parse("https://cdn.example.com/track").unwrap();
        assert_eq!(url_extension(&url), None);

        let url = Url::parse("https://cdn.example.com/track.m4a").unwrap();
        assert_eq!(url_extension(&url).as_deref(), Some("m4a"));
    }

    #[test]
    fn content_range_total_parses_length() {
        assert_eq!(content_range_total("bytes 0-0/123456"), Some(123456));
        assert_eq!(content_range_total("bytes 0-/123456"), Some(123456));
        assert_eq!(content_range_total("bytes 0-0/*"), None);
        assert_eq!(content_range_total("garbage"), None);
    }
}
