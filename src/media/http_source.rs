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
    collections::hash_map::DefaultHasher,
    ffi::OsStr,
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use symphonia::core::io::MediaSource;
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
        .user_agent(concat!("Meliora/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| zed_reqwest::Client::new())
});

/// Downloads the bytes of a small remote resource (e.g. a KuGou album-cover
/// image). On any non-success status or empty body returns `None`; the caller
/// treats that as "no artwork". Uses a fresh request with a short timeout so
/// cover fetches never block on the long-lived streaming client above.
pub async fn http_cover_bytes(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let client = zed_reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("Meliora/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| zed_reqwest::Client::new());

    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if bytes.is_empty() {
        return Ok(None);
    }
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
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        if now.duration_since(modified).is_ok_and(|age| age > IMAGE_CACHE_MAX_AGE) {
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

/// 带磁盘缓存的封面/头像加载：先读盘，未命中才下载并写盘。
/// 语义与 `http_cover_bytes` 一致（`Ok(None)` 表示无图或下载失败）。
pub async fn http_cover_bytes_cached(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    if let Some(bytes) = read_cached_cover(url).await {
        return Ok(Some(bytes));
    }

    let bytes = http_cover_bytes(url).await?;
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

    SymphoniaProvider
        .open_source(Box::new(source), ext.as_deref().map(OsStr::new))
        .map_err(|e| anyhow::anyhow!("failed to probe remote media: {e}"))
}

/// Extracts a probable file extension from the URL path component. Returns
/// `None` for anything that does not look like a short alphanumeric extension,
/// letting symphonia probe the format from the content instead.
fn url_extension(url: &Url) -> Option<String> {
    let file_name = url.path().rsplit('/').next()?;
    let (_, ext) = file_name.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 8 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
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
                HTTP_CLIENT
                    .get(url.clone())
                    .header(RANGE, "bytes=0-")
                    .send()
                    .await
            })
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
            .block_on(request.send())
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
            // released before `self` is touched below
            let chunk = {
                let Some(body) = self.body.as_ref() else {
                    return Ok(0);
                };
                let mut body = body.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                crate::RUNTIME.block_on(body.chunk())
            };

            match chunk {
                Ok(Some(chunk)) if !chunk.is_empty() => {
                    self.pending = chunk.to_vec();
                    self.pending_offset = 0;
                    return Ok(self.take_pending(buf));
                }
                Ok(Some(_)) => continue,
                Ok(None) => {
                    self.body = None;
                    return Ok(0);
                }
                Err(e) => {
                    self.body = None;
                    return Err(io::Error::other(format!("HTTP stream failed: {e}")));
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
            SeekFrom::End(delta) => self.total_len.and_then(|len| add_signed(len, delta)),
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

fn add_signed(len: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        len.checked_add(delta as u64)
    } else {
        len.checked_sub(delta.unsigned_abs())
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
