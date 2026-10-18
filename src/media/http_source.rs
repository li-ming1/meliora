//! 远程音频 URL 的 HTTP(S) 媒体源（例如酷狗直链）。
//!
//! 在 HTTP range 请求之上实现 symphonia 的 [`MediaSource`]（Read + Seek），
//! 于是远程曲目无需先整曲下载即可解码。顺序播放只开一次不设尾的 `Range`
//! 请求并逐块流式读取；跳转定位则丢弃响应体，在下一次读取时从目标偏移
//! 重新请求。
//!
//! URL 以不透明字符串承载在现有 `QueueItemData` 的 path 字段里，
//! 因此队列/会话的序列化无需改动。

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
use crate::sync::RecoveredMutex;

/// 媒体流式传输共用的 client。连接池让顺序读取保持廉价；
/// 不设整体请求超时，因为一次开放尾的 range 响应要贯穿整首曲子保持打开。
static HTTP_CLIENT: LazyLock<zed_reqwest::Client> = LazyLock::new(|| {
    zed_reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // 流式传输每首歌换一次主机；不显式设上限时，连接池默认 90 秒空闲
        // keep-alive 且每台主机不限空闲连接数（每条都钉住 hyper 的读缓冲）。
        .pool_max_idle_per_host(1)
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent(concat!("Meliora/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|error| {
            // 与其他 HTTP client 保持一致：后端拒绝这套调优参数时，
            // 应降级为默认配置，而不是中断播放。
            tracing::warn!(%error, "media: falling back to the default HTTP client");
            zed_reqwest::Client::new()
        })
});

// 喂解码器期间等待任意单个网络事件（响应头、一个 body 块）的上限。
// 这个常量现在住在 `media::download` 里、紧挨 `http_download_to_file`——
// 同为按事件限时，只留一份定义。client 刻意不带总请求超时——range 响应体
// 要贯穿整曲打开——但下面的读取跑在播放线程上：没有这个上限，一个在
// body 中途卡停的服务端会永久阻塞播放主循环，命令不再被消费，
// 无进展跳曲也永不触发，于是 UI 显示"播放中"却一片静音。
// 把它以 IO 错误浮出，引擎就能像对待任何其他解码失败一样跳过这首。
use super::download::STREAM_IO_TIMEOUT;

/// 后台填充线程专用的更紧的按事件上限：填充路径上的每个网络事件
/// （`start_body` 请求、一个 body 块）都跑在那里，所以一次事件卡停只换来
/// 一次重连（零位移 seek → 全新的 ranged 请求），而不是在一次阻塞读取上
/// 烧掉 consumer 卡停预算的三分之一。健康的 CDN 每个事件远在 1 秒内送达；
/// consumer 的 30 秒卡停预算容忍约 4 轮重连。`connect`（在播放/准备线程上
/// 内联打开）保持较宽的 [`STREAM_IO_TIMEOUT`]。
const FILL_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// 流中途的 URL 刷新钩子：当一次重新请求被 HTTP 4xx 拒绝时调用一次
/// （开流时还有效的签名 URL 在播放途中过期了）。返回新 URL；
/// provider 已无法给出时返回 `None`。由播放层构造（provider 身份与质量
/// 设置在那一侧），并在打开时安装到源上；跑在填充线程上，
/// 对该 provider 调用的阻塞由钩子自身限时。
pub type MediaUrlRefresh = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// 单次封面下载（`http_cover_bytes`）的请求级上限。
const COVER_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// `http_cover_bytes` 收集响应体的硬上限。正常封面（4096px 档）只有几
/// MB，超限只可能是异常/重定向 CDN 响应；Content-Length 超限在收集开始
/// 前拒绝，头缺失（chunked）时在收集途中截断，而不是收集完再丢弃——写
/// 盘路径的容量检查发生在收集之后，拦不住内存峰值。
const COVER_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 下载一个小远程资源的字节（例如酷狗专辑封面图）。
/// 任何非成功状态或空响应体都返回 `None`；调用方把它当作"无封面"。
/// 用一次全新请求配较短超时，使封面抓取绝不会被上面那个长连接的流式
/// client 拖住。
///
/// 返回聚合缓冲本体（`bytes::Bytes`，clone 即引用计数），调用方按需取用，
/// 不再整体复制。
pub async fn http_cover_bytes(url: &str) -> anyhow::Result<Option<bytes::Bytes>> {
    // 复用带连接池的共用 client（rustls 配置 + 连接池），配 per-request
    // timeout。每张封面新建一个 Client 会为每首歌重建一套 rustls 配置和
    // 连接池，并在每次抓取后把空闲连接的拆除工作留给 runtime。
    let request = HTTP_CLIENT.get(url).timeout(COVER_FETCH_TIMEOUT);

    let mut response = match request.send().await {
        Ok(response) if response.status().is_success() => response,
        _ => return Ok(None),
    };
    // Content-Length 超限直接按 "无图" 返回，不开始收集；头缺失或被压缩
    // 代理改写成 chunked 时该检查失效，由收集循环按累计大小兜底截断。
    let content_len = response.content_length();
    if content_len.is_some_and(|len| len > COVER_MAX_BYTES) {
        return Ok(None);
    }
    let mut collected = Vec::with_capacity(content_len.unwrap_or(0) as usize);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if collected.len() as u64 + chunk.len() as u64 > COVER_MAX_BYTES {
                    return Ok(None);
                }
                collected.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return Ok(None),
        }
    }
    if collected.is_empty() {
        return Ok(None);
    }
    Ok(Some(bytes::Bytes::from(collected)))
}

/// 共享 client 的只读访问器：零散的冷路径请求（下载等）复用同一份 rustls
/// 配置与连接池，而不是每次调用重建。client 刻意不带总超时（流式 range
/// 响应要保持打开整曲时长），调用处必须自带 per-request timeout。
pub(crate) fn shared_http_client() -> &'static zed_reqwest::Client {
    &HTTP_CLIENT
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
/// 按值收聚合字节（`Bytes` 类 clone 即引用计数），闭包直接 move 落盘，
/// 不再为写盘整体复制一遍。
async fn write_cached_cover(url: &str, bytes: impl AsRef<[u8]> + Send + 'static) {
    let slice = bytes.as_ref();
    if slice.is_empty() || slice.len() as u64 > IMAGE_CACHE_MAX_BYTES {
        return;
    }

    let dir = image_cache_dir();
    let file = image_cache_key(url);
    let path = dir.join(&file);
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

/// 启动时 fire-and-forget 做一次全量清扫（缓存很小时很便宜）。
/// 周期性清扫只在缓存写入之后触发，所以长时间没有封面抓取会让陈旧条目
/// 一直留到撞上 256MB 上限；开机扫一次就把它们限制在 30 天的寿命窗口内。
pub fn prune_image_cache_background() {
    crate::RUNTIME.spawn(async {
        // 启动分级：全量清扫要 read_dir + metadata 数千个缓存文件，t=0 跑会
        // 撞首帧窗口。对齐本文件/启动路径的分级先例（updater
        // STARTUP_CHECK_DELAY 8s），这里推迟 30s，让首帧与库扫描先行。
        tokio::time::sleep(Duration::from_secs(30)).await;
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
        // `write_cached_cover` 的 rename 被打断会留下 `.tmp`；它们没有散列命名、
        // 也永远不会被读回，所以直接删掉
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
    let mut inflight = INFLIGHT_COVERS.lock_recovered();
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
    let mut inflight = INFLIGHT_COVERS.lock_recovered();
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
pub async fn http_cover_bytes_cached(url: &str) -> anyhow::Result<Option<bytes::Bytes>> {
    if let Some(bytes) = read_cached_cover(url).await {
        return Ok(Some(bytes.into()));
    }

    let (gate, _creator_guard) = inflight_cover_gate(url);
    let _gate = gate.lock().await;

    // 拿到闸门后再查一次盘：前一个持有者可能已经把这张图下载写盘了。
    if let Some(bytes) = read_cached_cover(url).await {
        return Ok(Some(bytes.into()));
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
        write_cached_cover(url, bytes.clone()).await;
    }
    Ok(bytes)
}

/// 打开 `path` 里存放的 HTTP(S) URL，并用 Symphonia 解码 provider 探测它。
/// `refresh` 为源装上流中途 URL 刷新钩子（过期签名 URL 的自愈）。
pub fn open_http_media(
    path: &Path,
    refresh: Option<MediaUrlRefresh>,
) -> anyhow::Result<Box<dyn MediaStream>> {
    let text = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("track path is not valid UTF-8"))?;
    let url = Url::parse(text).map_err(|e| anyhow::anyhow!("invalid track URL '{text}': {e}"))?;
    let ext = url_extension(&url);

    let source = HttpRangeSource::connect(url, refresh)
        .map_err(|e| anyhow::anyhow!("failed to open '{text}': {e}"))?;

    // 后台填充 + 重连，让网络抖动绝不会把播放主循环按在一次同步读取上
    // （见 prefetch.rs）。
    let source = crate::media::prefetch::PrefetchSource::new(source);

    SymphoniaProvider
        .open_source(Box::new(source), ext.as_deref().map(OsStr::new))
        .map_err(|e| anyhow::anyhow!("failed to probe remote media: {e}"))
}

/// [`url_extension`] 认可为"短"扩展名的长度上限；更长的名字交给内容探测。
const MAX_URL_EXTENSION_LEN: usize = 8;

/// 从 URL 的 path 分量里取出可能的文件扩展名。凡不像短小字母数字扩展名的
/// 一律返回 `None`，让 symphonia 改从内容探测格式。
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

/// 从 `Content-Range: bytes 0-123/456` 头里解析出总长度。
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// 由 range 请求支撑的、可在远程 HTTP(S) 资源上 seek 的字节流。
///
/// 响应体被包在互斥锁里：symphonia 要求媒体源是 `Sync`，
/// 尽管实际只有播放线程会碰它。
struct HttpRangeSource {
    url: Url,
    /// 下一次 `read` 返回的首字节的逻辑位置。
    pos: u64,
    /// 资源总长度（服务端上报时）。
    total_len: Option<u64>,
    /// 服务端是否支持 `Range` 请求。
    range_supported: bool,
    /// 当前已打开的响应体（若有）。
    body: Option<Mutex<zed_reqwest::Response>>,
    /// 已收到但尚未被 `read` 消费的字节。
    pending: Vec<u8>,
    pending_offset: usize,
    /// 针对被拒（过期）签名 URL 的流中途刷新钩子，由播放层构造；
    /// 不知道 provider 身份时为 `None`。
    refresh: Option<MediaUrlRefresh>,
    /// 本源的刷新钩子是否已跑过：每条流只试一次，
    /// 以免每个退避周期都去锤一个失败的 provider API。
    refresh_used: bool,
    /// 当前 URL 被 4xx 拒绝，且刷新（若有）也救不回来：重试永远不会成功，
    /// 因此填充线程必须停手。
    url_dead: bool,
}

impl HttpRangeSource {
    /// 打开资源，并用一次 `bytes=0-` 请求探测 range 支持。
    /// 响应体保留为初始流。
    fn connect(url: Url, refresh: Option<MediaUrlRefresh>) -> Result<Self, String> {
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
            refresh,
            refresh_used: false,
            url_dead: false,
        })
    }

    /// 为当前位置发起一次 range 请求。位置已到/已过资源末尾时返回 `false`。
    fn start_body(&mut self) -> io::Result<bool> {
        if let Some(len) = self.total_len
            && self.pos >= len
        {
            return Ok(false);
        }
        if !self.range_supported && self.pos != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote server does not support range requests",
            ));
        }

        let mut last_err = match self.send_range_request() {
            Ok(response) => return self.accept_body(response).map(|_| true),
            Err(err) => err,
        };

        // 开流时还有效的签名 URL 可能在曲中过期：重新请求会以客户端错误
        // （403/404）被拒。一次刷新尝试从 provider 钩子换来新 URL；若 URL
        // 仍被拒（或根本刷不出来）它就是死的，填充线程就此放弃，
        // 而不是一路重试到 consumer 的卡停预算耗尽。
        if Self::client_rejected(&last_err)
            && !self.refresh_used
            && let Some(hook) = self.refresh.clone()
            && let Some(fresh) = hook()
            && let Ok(url) = Url::parse(fresh.to_string_lossy().as_ref())
        {
            self.refresh_used = true;
            self.url = url;
            tracing::info!(url = %self.url, "refreshed expired media URL mid-stream");
            match self.send_range_request() {
                Ok(response) => return self.accept_body(response).map(|_| true),
                Err(err) => last_err = err,
            }
        }
        if Self::client_rejected(&last_err) {
            self.url_dead = true;
        }
        Err(last_err)
    }

    /// 从当前位置发起一次 ranged GET。客户端拒绝（4xx）会以 `PermissionDenied`
    /// 浮出，这样刷新逻辑无需解析错误字符串就能把它归类。
    fn send_range_request(&self) -> io::Result<zed_reqwest::Response> {
        let mut request = HTTP_CLIENT.get(self.url.clone());
        if self.range_supported {
            request = request.header(RANGE, format!("bytes={}-", self.pos));
        }
        crate::RUNTIME
            .block_on(async { tokio::time::timeout(FILL_IO_TIMEOUT, request.send()).await })
            .map_err(|_| io::Error::other("HTTP request timed out"))?
            .and_then(|response| response.error_for_status())
            .map_err(|e| match e.status() {
                Some(status) if status.is_client_error() => io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("remote URL rejected (HTTP {status})"),
                ),
                _ => io::Error::other(format!("HTTP request failed: {e}")),
            })
    }

    fn client_rejected(err: &io::Error) -> bool {
        err.kind() == io::ErrorKind::PermissionDenied
    }

    /// 把一次成功的 range 响应存为当前活动响应体。若服务端忽略 `Range`、
    /// 对非零偏移回一整份 body 的 200，则返回错误。
    fn accept_body(&mut self, response: zed_reqwest::Response) -> io::Result<()> {
        // 忽略 Range 头的服务端会回 200，body 从第 0 字节开始；
        // 只有当我们请求的就是第 0 字节时它才可用。
        if response.status() != StatusCode::PARTIAL_CONTENT && self.pos != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server ignored the range request",
            ));
        }
        self.body = Some(Mutex::new(response));
        self.url_dead = false;
        Ok(())
    }

    /// 把已缓冲的字节拷进 `buf`，并推进逻辑位置。
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

            // 在独立作用域里取下一个块，好让 body 锁在下面触碰 `self` 之前
            // 就释放；这里的超时限定的是单个填充线程事件——此处卡停触发的
            // 是一次重连（见 FILL_IO_TIMEOUT），而不是播放线程的阻塞
            let chunk = {
                let Some(body) = self.body.as_ref() else {
                    return Ok(0);
                };
                let mut body = body.lock_recovered();
                crate::RUNTIME
                    .block_on(async { tokio::time::timeout(FILL_IO_TIMEOUT, body.chunk()).await })
            };

            match chunk {
                Ok(Ok(Some(chunk))) if !chunk.is_empty() => {
                    // 复用缓冲的容量而不是替换整个 Vec：否则每个 body 块都要
                    // 丢掉一次旧分配，把活儿重新交给分配器，
                    // 而网络块每秒有几十个。
                    self.pending.clear();
                    self.pending.extend_from_slice(&chunk);
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
                        "remote stream stalled: no data for {FILL_IO_TIMEOUT:?}"
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
            // 允许跳到末尾之后，读回来就是 EOF
            SeekFrom::End(delta) => self.total_len.and_then(|len| len.checked_add_signed(delta)),
        };

        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek target is out of range",
            ));
        };

        // 丢掉已打开的流与全部缓冲字节；下一次读取会从新偏移重新请求。
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

impl crate::media::prefetch::FillSource for HttpRangeSource {
    fn url_dead(&self) -> bool {
        self.url_dead
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
