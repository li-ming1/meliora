use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

#[cfg(feature = "online_sources")]
use gpui::SharedString;
use gpui::{
    App, Bounds, Corners, Element, ElementId, GlobalElementId, ImageId, InspectorElementId,
    IntoElement, LayoutId, ObjectFit, Pixels, Refineable, RenderImage, Style, StyleRefinement,
    Styled, Window,
};
use image::{Frame, Pixel};
use rustc_hash::FxHashMap;
use smallvec::smallvec;
use sqlx::SqlitePool;
use tracing::{error, info};

use crate::sync::RecoveredMutex;
use crate::{
    media::{lookup_table::try_open_media, traits::MediaProviderFeatures},
    ui::app::Pool,
};

// 沉浸页背景纯 CPU 渲染管线（v4 清晰裁剪带），自本文件拆出的自包含图像
// 处理段（`backdrop_sharp_fill` / `enhance_render_image` / 去噪 / 锐化）。
// 文件位置经 `#[path]` 钉在本目录——Rust 2018 子模块默认会找
// managed_image/backdrop_pipeline.rs，而这里按仓库目录惯例平铺。
#[path = "backdrop_pipeline.rs"]
mod backdrop_pipeline;

use self::backdrop_pipeline::{backdrop_sharp_fill, enhance_render_image};

/// 曲目同目录封面文件名白名单：3 个词干 × 3 个扩展名，只查一层目录、名字大小写不敏感。
/// 取代原 `globwalk` 通配——候选相同、readdir 顺序相同，且每次查找不再编译 glob。
const ART_FILE_NAMES: [&str; 9] = [
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "front.jpg",
    "front.jpeg",
    "front.png",
];

fn find_art_file_for_path(path: &Path) -> Option<Arc<Path>> {
    let parent = path.parent()?;

    std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .find(|entry| {
            let name = entry.file_name();
            ART_FILE_NAMES
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        })
        .map(|entry| Arc::from(entry.path()))
}

/// 限制全部 `ManagedImage` 元素的并发封面解码数：每条已在途解码瞬态持有一份全尺寸 RGBA
/// 缓冲（3000px 封面 ~36MB），而一次快速网格滚动可同时让数十个瓦片未命中渲染缓存。
/// 与扫描器的封面解码上限同构——渲染缓存约束的是*驻留量*，本信号量约束的是*已在途量*。
static DECODE_PERMITS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(4));

/// 端到端串行化沉浸页背景重管线（全尺寸解码 → 裁剪带 → 重采样 → 锐化）。单条管线瞬态峰值
/// ~105 MB 堆——仅 4096² RGBA 源就占 67 MB，其裁剪带再占 ~38 MB——而 `arm_backdrop`
/// （当前曲）与 `prefetch_next_track_art`（下一曲）每次切歌各启动一条。它们原本自由重叠：
/// `DECODE_PERMITS` 只闸解码步，锐化段在任何上限之外裸跑，快切时被取代的任务还在后台跑完。
/// 两三条并发管线在重度模式平台期之上叠出 ~250 MB 瞬态，把 private commit 连续两个采样
/// 推过硬阀门（2026-10-02 重启：沉浸模式切歌期间 180 s 爬坡 341→521 MB）。容量 1 的阀门
/// 把叠加变成 FIFO 队列：当前曲的 arm 最先入队、约 1 s 内换图，预取排在它后面，峰值恰好
/// 是一条管线。缓存命中的 retrieve 完全绕过——其查找发生在 acquire 之前，所以即使有新管线
/// 排队，预取/回跳的背景仍然即时。
static BACKDROP_PIPELINE_GATE: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(1));

/// 原地交换 R 与 B 通道，使 `image` crate 的缓冲匹配 GPUI 期望的 BGRA 顺序。
pub(crate) fn rgb_to_bgr(image: &mut image::RgbaImage) {
    image.pixels_mut().for_each(|v| {
        let ch = v.channels_mut();
        ch.swap(0, 2);
    });
}

fn decode_rgba_to_render_image(mut image: image::RgbaImage) -> anyhow::Result<Arc<RenderImage>> {
    rgb_to_bgr(&mut image);
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(image)])))
}

/// 扫描器写入 `thumb` 列的 BMP 缩略图最长边（70×70，见 scan decode）。解码尺寸不超过此值
/// 的可以复用这份廉价预缩放源，更大的只能解码完整封面。
const STORED_THUMB_PX: u32 = 72;

/// 解码 `data`，在转 RGBA 前先缩到最长边不超过 `bound` 的方形。在解码阶段缩放可让瞬态 RGBA
/// 缓冲（以及任何持有它的东西）被 UI 实际绘制尺寸约束住。
fn decode_to_render_image_scaled(data: &[u8], bound: u32) -> anyhow::Result<Arc<RenderImage>> {
    let image = image::load_from_memory(data)?;
    decode_rgba_to_render_image(dynamic_to_rgba_scaled(image, bound))
}

/// 取得一个 [`DECODE_PERMITS`] 名额，在阻塞线程池上把 `data` 解码成 `bound` 封顶的
/// `RenderImage`。所有字节缓冲解码路径（`HttpCover` / `HttpCoverLarge` / DB 封面）共用，
/// 好让已在途上限与阻塞池交接只存在一处。`retrieve_uncached` 里基于路径的 `TrackFile`
/// 分支直接获取同一个信号量，并且必须继续这样做。
async fn decode_bounded_with_permit(
    data: impl AsRef<[u8]> + Send + 'static,
    bound: u32,
) -> anyhow::Result<Option<Arc<RenderImage>>> {
    let _permit = DECODE_PERMITS
        .acquire()
        .await
        .expect("semaphore is never closed");
    crate::RUNTIME
        .spawn_blocking(move || decode_to_render_image_scaled(data.as_ref(), bound).map(Some))
        .await?
}

/// 把已解码图像转成 RGBA，优先 steal-or-expand 而非 `to_rgba8` 的全尺寸拷贝：PNG 解码结果
/// 原样交出缓冲，Rgb8（JPEG）原地反向填充扩展，只有冷门色彩类型才付 `to_rgba8`。缩略图化
/// （bound > 0）先在 DynamicImage 上做，因此转换永远见不到全尺寸数据——TrackFile 路径原本
/// 先全尺寸转换再缩略，会同时持有一张 ~2500px 封面的 Rgb8+Rgba8（dhat: 23.6MB 瞬态，
/// 2026-09-16）。
fn dynamic_to_rgba_scaled(image: image::DynamicImage, bound: u32) -> image::RgbaImage {
    let image = if bound > 0 {
        image.thumbnail(bound, bound)
    } else {
        image
    };
    match image {
        image::DynamicImage::ImageRgba8(rgba) => rgba,
        image::DynamicImage::ImageRgb8(rgb) => {
            let (w, h) = rgb.dimensions();
            let mut buf = rgb.into_raw();
            expand_rgb8_to_rgba8_in_place(&mut buf, w, h);
            image::RgbaImage::from_raw(w, h, buf)
                .expect("rgb8 expansion preserves the w*h*4 length")
        }
        other => other.to_rgba8(),
    }
}

/// 把 Rgb8 缓冲原地扩展为 Rgba8：每像素先加长一字节，再从后往前填充（像素 i 读源
/// `3i..3i+3`、写目标 `4i..4i+4`；更大的 i 只会碰到 ≥ `4i+4` 的字节，位于所有剩余源字节
/// 之上）。省掉第二份全尺寸缓冲，把 JPEG 封面的解码瞬态减半。
fn expand_rgb8_to_rgba8_in_place(buf: &mut Vec<u8>, w: u32, h: u32) {
    let pixels = (w as usize) * (h as usize);
    debug_assert_eq!(buf.len(), pixels * 3);
    buf.resize(pixels * 4, 255);
    // 反向填充只在 i ≥ 3 时才无覆盖：当 `i + k == j` 时写 `4i+k` 会命中读 `3i+j`，
    // 而这在 `i + 2 ≤ 2` 时可能发生。先把前三个像素存起来，之后用副本写回。每个像素的
    // alpha 都显式写入——多数 alpha 字节落在原始 Rgb8 区域内，不在填 255 的尾部。
    let head = pixels.min(3);
    let mut saved = [0u8; 9];
    saved[..head * 3].copy_from_slice(&buf[..head * 3]);
    for i in (3..pixels).rev() {
        let s = i * 3;
        let d = i * 4;
        buf[d] = buf[s];
        buf[d + 1] = buf[s + 1];
        buf[d + 2] = buf[s + 2];
        buf[d + 3] = 255;
    }
    for i in 0..head {
        let d = i * 4;
        buf[d] = saved[i * 3];
        buf[d + 1] = saved[i * 3 + 1];
        buf[d + 2] = saved[i * 3 + 2];
        buf[d + 3] = 255;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ManagedImageKey {
    Album(i64),
    Track(i64),
    TrackFile(PathBuf),
    /// 经 HTTP 拉取的在线专辑封面（酷狗曲目封面 URL）。
    #[cfg(feature = "online_sources")]
    HttpCover(SharedString),
    /// 同一张封面，但拉取时先试音源的大图显示档（酷狗 `stdmusic/480`），失败再退回原缩略图
    /// ——给沉浸页背景与标签用：256px 缩略图拉伸到整窗会糊成一团。见
    /// `online_sources::cover_art`。
    #[cfg(feature = "online_sources")]
    HttpCoverLarge(SharedString),
}

/// 跨元素保活的已解码 `RenderImage` 数量上界。
///
/// 滚动在线列表会不断重建 `ManagedImage` 行；没有这层缓存时，每行都把同一个 URL 重新解码成
/// 新的 `RenderImage` 并重新上传一张新图集纹理。给驻留集合设上限可以让这种 churn 不至于
/// 累加，而重绘最近见过的封面会复用完全相同的 `RenderImage`/图集槽位。逐出会把瓦片排进回收
/// 队列，被逐出的封面回来时从磁盘缓存重新解码。
///
/// 这个上界是*活动瓦片*预算，而每张活动瓦片不论像素尺寸都固定占用 ~2.3 MB 驱动侧提交
/// （2026-09-19 浸泡：192 px 与 256 px 瓦片成本相同，[mem] 探针的 `non_heap_mb` 与该缓存
/// 一一对应）。128 条会把驱动提交持续抬高；64 条仍比空闲基线高 ~150 MB；32 条把最近封面
/// 复用窗口（≈ 6 行网格）保住，成本只有它的一半。
const RENDER_CACHE_MAX: usize = 32;

/// 所有 `ManagedImage` 元素共享的有界已解码封面集合。按 (源, thumb 尺寸) 为键，因为 72px
/// 表格行与 256px 网格瓦片是不同的解码。缓存瓦片只经 `queue_tile_drop` +
/// `drain_pending_tile_drops` 回收恰好一次；元素 `on_release` 把自己持有的引用推进同一个
/// 回收漏斗，而排空只回收批次外无人能再绘制的内容——重复释放一个瓦片会触发 etagere 断言
/// （2026-09-12 崩溃），而从不释放则会在某个存活元素于逐出后重绘它时泄漏一次。
static RENDER_CACHE: OnceLock<Mutex<RenderCache>> = OnceLock::new();

#[derive(Clone, PartialEq, Eq, Hash)]
struct RenderCacheKey {
    key: ManagedImageKey,
    thumb: u32,
}

struct RenderCache {
    cache: FxHashMap<RenderCacheKey, (Arc<RenderImage>, u64)>,
    usage: VecDeque<RenderCacheKey>,
    /// 近似活动像素字节数，跟踪它是为了让 [mem] 探针报告这个缓存的准确占用，
    /// 而不是对峰值数字打哈哈。
    bytes: u64,
}

/// 锁住 `RENDER_CACHE`，首次使用时初始化。
fn lock_render_cache() -> MutexGuard<'static, RenderCache> {
    RENDER_CACHE
        .get_or_init(|| {
            Mutex::new(RenderCache {
                cache: FxHashMap::default(),
                usage: VecDeque::new(),
                bytes: 0,
            })
        })
        .lock_recovered()
}

/// 不初始化地锁住 `RENDER_CACHE`：第一次 [`lock_render_cache`] 创建它之前返回 `None`。
fn try_lock_render_cache() -> Option<MutexGuard<'static, RenderCache>> {
    RENDER_CACHE.get().map(|cache| cache.lock_recovered())
}

/// 已解码封面的 RGBA 占用估算字节数。
pub(crate) fn image_bytes(image: &RenderImage) -> u64 {
    let size = image.size(0);
    (size.width.0 as u64) * (size.height.0 as u64) * 4
}

/// 已解码封面 LRU 的活动占用（MiB）。由 [mem] 周期探针上报。
pub fn render_cache_mb() -> u64 {
    let Some(cache) = try_lock_render_cache() else {
        return 0;
    };
    cache.bytes / (1024 * 1024)
}

/// 已解码封面 LRU 的条目数。与 `render_cache_mb` 并列上报，让 [mem] 探针能区分条目集增长
/// （逐曲目缓存 churn）与单条目尺寸增长。
pub fn render_cache_entries() -> usize {
    let Some(cache) = try_lock_render_cache() else {
        return 0;
    };
    cache.cache.len()
}

/// 压力 shed 与沉浸页激活收缩都把渲染缓存缩到这个 LRU 预算：稳态 32 条的一半。保住当前
/// 曲目的封面（几乎必然在 LRU 尾部）和最近几行网格；其余按需从磁盘缓存重解码。
pub(crate) const RENDER_CACHE_PRESSURE_KEEP: usize = 12;

/// 把已解码封面 LRU 收缩到最多 `keep` 条，从最旧端逐出进回收漏斗的正常年龄门（封面 60s /
/// 大图 3s）——年龄门从不缩短；那是在赌 2026-09-08 悬空场景崩溃类，所以被逐出瓦片的驱动
/// 提交要按分钟级才归还。两个调用方：[mem] 探针在 private commit 越过软顶时的压力 shed
/// （正是硬阀门度量的那个持续区间），以及沉浸页激活——背景管线马上要付出 ~105 MB 瞬态，
/// 通用封面瓦片先让位。保留的是 LRU 尾部（最近使用）。
pub(crate) fn render_cache_shrink(keep: usize) {
    let mut cache = lock_render_cache();
    while cache.usage.len() > keep {
        let Some(oldest) = cache.usage.pop_front() else {
            break;
        };
        if let Some((old, old_bytes)) = cache.cache.remove(&oldest) {
            cache.bytes = cache.bytes.saturating_sub(old_bytes);
            queue_tile_drop(oldest, old);
        }
    }
}

/// 沉浸页背景 LRU 的驻留字节（MiB）外加条目数。由 [mem] 周期探针上报：沉浸态激活时应停在
/// ~3 套，退出沉浸页后必须回落到 1（`backdrop_cache_shrink` 的归还纪律）——下一次跨表面
/// 驻留审计就靠这个字段直接验证，而不是靠推断。
pub fn backdrop_cache_stats() -> (usize, u64) {
    let cache = lock_backdrop_cache();
    (cache.cache.len(), cache.bytes / (1024 * 1024))
}

/// 图集瓦片仍待释放的封面，带其缓存键。这条队列是唯一回收漏斗：缓存逐出/替换与元素
/// `on_release` 都推到这里，`drain_pending_tile_drops`（只在事件循环、绝不跨帧）是唯一排空
/// 点——漏斗之外唯一的一次回收是 retrieve 续体里图像落地前元素已消失时的 `strong_count == 1`
/// 兜底（见 `request_layout`）。逐出运行在没有 `App` 的 RUNTIME 上，这正是释放本身必须延后
/// 的原因。没有这套机制，一旦拥有它的元素卸载，被逐出封面的图集页就永远钉住。
static PENDING_TILE_DROPS: OnceLock<Mutex<Vec<PendingTileDrop>>> = OnceLock::new();

struct PendingTileDrop {
    key: RenderCacheKey,
    image: Arc<RenderImage>,
    /// 允许 `drain_pending_tile_drops` 回收这张图像的最早排空时点。只有在无人能再绘制它时
    /// 回收才安全——包括被跳过重绘的视图所*回放*的绘制原语，它们完全不持 Arc。逐出时被跳过
    /// 的视图会一直回放其最后一个场景直到重渲染，所以新逐出的图像必须先老化到点才能释放
    /// 瓦片（2026-09-16 漏斗审计；见 `RECLAIM_DELAY`）。
    due: Instant,
}

/// 排队图像在 `drain_pending_tile_drops` 释放其图集瓦片前的最小年龄。它约束的是"跳过重绘的
/// 视图回放"风险：被 gpui 跳过（未重渲染）的视图会在不持有图像的情况下回放上次绘制的精灵，
/// 所以在该视图仍被跳过时释放它的页会让回放精灵悬空（`texture()` unwrap，2026-09-08 崩溃
/// 类）。60s 覆盖一切现实的静态视图生命周期，同时让回收延迟对内存无关紧要（页是瞬态的，
/// 探针曲线显示基线由分配器/GPU 驻留主导，不是回收延迟）。
const RECLAIM_DELAY: Duration = Duration::from_secs(60);
/// 大图（≥[`LARGE_IMAGE_BYTES`]，沉浸页背景级）用短得多的年龄门老化。唯一绘制方是沉浸页
/// 背景元素（2026-09-29 审计：其余每个 `ManagedImage` 解码都 ≤512px ≤1MB），而该元素每次
/// 移除——晋升、退出、crossfade 结束——都与一次 `ImmersiveView` 重渲染同时发生（同一次状态
/// 变更里的 `cx.notify()`），此后没有任何已录制场景还能回放它的精灵。短门只需盖住 crossfade
/// 窗口（400ms）加帧时序，并且必须始终大于 `BACKDROP_FADE`：crossfade 期间旧元素仍在淡出、
/// 始终挂载。统一的 60s 门会把一连串切歌变成"背景不跟着换歌走"——积压的大图集会让新背景
/// 解码最多等一分钟（`backdrop_decode_allowed`），正是 2026-09-29 的投诉。
const RECLAIM_DELAY_LARGE: Duration = Duration::from_secs(3);

/// 单张排队图像的回收年龄：背景级大图走短门，其余走保守的 60s 门。
fn reclaim_delay_for(image: &RenderImage) -> Duration {
    if image_bytes(image) >= LARGE_IMAGE_BYTES {
        RECLAIM_DELAY_LARGE
    } else {
        RECLAIM_DELAY
    }
}

fn queue_tile_drop(key: RenderCacheKey, image: Arc<RenderImage>) {
    let large = image_bytes(&image) >= LARGE_IMAGE_BYTES;
    TILE_DROP_STATS.pushed.fetch_add(1, Ordering::Relaxed);
    if large {
        TILE_DROP_STATS.pushed_large.fetch_add(1, Ordering::Relaxed);
    }
    let due = reclaim_delay_for(&image);
    PENDING_TILE_DROPS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock_recovered()
        .push(PendingTileDrop {
            key,
            image,
            due: Instant::now() + due,
        });
}

/// 回收漏斗累计计数器，供 `[mem]` 周期探针上报，让日志曲线说明驻留内存为何停在那个位置：
/// 有多少瓦片流量经过漏斗、每次排空各自做了什么判定。
pub(crate) struct TileDropStats {
    pushed: AtomicU64,
    /// 大图级（背景重量级）载荷的累计入队数：2026-10-06 的突发（funnel_pending=146，
    /// private 717MB）之所以无法按尺寸推理，是因为当时只有总数——本字段把洪流分成大/小两类，
    /// 下次突发就能自己报出构成。
    pushed_large: AtomicU64,
    reclaimed: AtomicU64,
    kept_by_cache: AtomicU64,
    kept_by_holders: AtomicU64,
}

static TILE_DROP_STATS: TileDropStats = TileDropStats {
    pushed: AtomicU64::new(0),
    pushed_large: AtomicU64::new(0),
    reclaimed: AtomicU64::new(0),
    kept_by_cache: AtomicU64::new(0),
    kept_by_holders: AtomicU64::new(0),
};

/// `[mem]` 探针的快照：(pending, pushed, reclaimed, kept_by_cache, kept_by_holders,
/// leaked_tiles)。最后一槽恒为 0——`panic = "abort"` 会让 panic 的 `drop_image` 直接致命，
/// 所以没有可统计的余量；这一槽保留只是因为 `src/main.rs` 按位置读取这个元组。
// 只有非测试探针消费这个函数；测试构建会把它报成死代码。
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn tile_drop_stats() -> (u64, u64, u64, u64, u64, u64) {
    let pending = PENDING_TILE_DROPS
        .get()
        .map(|queue| queue.lock_recovered().len() as u64)
        .unwrap_or(0);
    let s = &TILE_DROP_STATS;
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    (
        pending,
        load(&s.pushed),
        load(&s.reclaimed),
        load(&s.kept_by_cache),
        load(&s.kept_by_holders),
        0,
    )
}

/// `[mem]` 探针用的回收漏斗积压重量：`(积压字节 MiB, 积压大图片数, 累计大图入队数)`。
/// 2026-10-06 突发会话（funnel_pending=146，private 717MB）只留下计数、不知道积压的像素
/// 重量，于是峰值构成（活动缓存 vs 排队载荷）只能靠猜。这里改为在探针时刻于队列锁内求和，
/// 而不是 push/drain 的加减记账：30s 节奏下 O(pending)，且没有漂移路径。
// 只有非测试探针消费这个函数；测试构建会把它报成死代码。
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn funnel_pending_detail() -> (u64, u64, u64) {
    let Some(queue) = PENDING_TILE_DROPS.get() else {
        return (0, 0, TILE_DROP_STATS.pushed_large.load(Ordering::Relaxed));
    };
    let queue = queue.lock_recovered();
    let mut bytes = 0u64;
    let mut large = 0u64;
    for entry in queue.iter() {
        let entry_bytes = image_bytes(&entry.image);
        bytes = bytes.saturating_add(entry_bytes);
        if entry_bytes >= LARGE_IMAGE_BYTES {
            large += 1;
        }
    }
    (
        bytes / (1024 * 1024),
        large,
        TILE_DROP_STATS.pushed_large.load(Ordering::Relaxed),
    )
}

/// 进程级内存压力旗标（0 = 正常，1 = private commit 越过软顶），由 `[mem]` 探针置位并按
/// 滞回清除。压力下沉浸页会跳过下一曲预取（省一整条重管线加一个背景缓存槽位；切歌仍按需
/// 解码），探针则做一次双缓存 shed。Relaxed 足够——这是建议性信号，漏看一个采样窗口没有
/// 正确性后果。
static MEMORY_PRESSURE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

// 只有非测试探针会置位（测试构建不带探针），与上面的 `tile_drop_stats` 同理；
// 读取侧在沉浸页视图里。
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn set_memory_pressure(on: bool) {
    MEMORY_PRESSURE.store(u8::from(on), std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn memory_pressure() -> bool {
    MEMORY_PRESSURE.load(std::sync::atomic::Ordering::Relaxed) != 0
}

/// 把不属于 RENDER_CACHE 的图（如 `MelioraImageCache` 的逐出/释放、登录二
/// 维码）也推进同一回收漏斗。哨兵键永远不会命中缓存（thumb = u32::MAX 且
/// 路径为空），drain 的"缓存仍持有"判定对其恒为 false。
pub(crate) fn queue_orphan_tile_drop(image: Arc<RenderImage>) {
    queue_tile_drop(
        RenderCacheKey {
            key: ManagedImageKey::TrackFile(PathBuf::new()),
            thumb: u32::MAX,
        },
        image,
    );
}

/// 渲染缓存是否仍在 `key` 下持有 `image`（同一份分配）。排空用它来放过那些仍有活动缓存
/// 条目在服务中的瓦片。
fn render_cache_holds(key: &RenderCacheKey, image: &Arc<RenderImage>) -> bool {
    let Some(cache) = try_lock_render_cache() else {
        return false;
    };
    cache
        .cache
        .get(key)
        .is_some_and(|(current, _)| Arc::ptr_eq(current, image))
}

/// 单个排空批次规划的结论。`reclaim` 是图集瓦片可以恰好释放一次的图像集合；计数器存在的
/// 目的是让测试能断言某张图为何被保活。
#[derive(Default)]
pub(crate) struct ReclaimPlan {
    pub reclaim: Vec<Arc<RenderImage>>,
    /// 渲染缓存仍在某个推入的键下服务这张图：活动瓦片，这里绝不回收（缓存会在逐出时自己推）。
    pub kept_by_cache: usize,
    /// 批次之外至少还有一个强引用的图（活动元素状态、续体局部克隆……）：它们可能再次绘制，
    /// 所以瓦片必须留着。
    pub kept_by_holders: usize,
}

/// [`drain_pending_tile_drops`] 的纯核心：按图像身份归组并决定哪些图的瓦片可以回收。
///
/// 决策所依据的不变量：归组会消耗批次携带的每一份 Arc，并为每张唯一图像恰好保留 ONE 个
/// 检视 Arc，因此归组后 `Arc::strong_count == 1 + (批次外的每个持有者)`。所以回收正确的
/// 充要条件是 `strong_count == 1` 且缓存不再服务该图：本函数之外无人能再次绘制它，
/// 于是它的瓦片被恰好释放一次。
///
/// （旧的算术 `strong_count - batch_copies` 两处都错：同一张图被推 2 次以上且无外部持有者时
/// 在 release 下 usize 下溢，于是永远跳过回收——常见"逐出+卸载"路径上的永久图集瓦片泄漏；
/// 而推 2 次再加一个活动持有者时算出 0，会释放元素仍在绘制的瓦片。）
///
/// `cache_holds` 报告渲染缓存是否仍在 `key` 下持有 `image`（同一份分配）。对该图被推入的
/// 每个键都查询，不只是第一个，所以多键推入无法绕过它。
fn plan_tile_reclaims(
    batch: Vec<(RenderCacheKey, Arc<RenderImage>)>,
    mut cache_holds: impl FnMut(&RenderCacheKey, &Arc<RenderImage>) -> bool,
) -> ReclaimPlan {
    // 按插入顺序去重：每张唯一图像只留一个检视 Arc。重复推入（同一张图被数个卸载中的元素
    // 以及缓存各推一份）在这里收敛；多出的 Arc 由本循环丢弃，这正是下面
    // `strong_count == 1` 判据成立的前提。
    let mut order: Vec<ImageId> = Vec::new();
    let mut group: FxHashMap<ImageId, (RenderCacheKey, Arc<RenderImage>, bool)> =
        FxHashMap::default();
    for (key, image) in batch {
        let id = image.id;
        let cache_hold = cache_holds(&key, &image);
        let entry = group.entry(id).or_insert_with(|| {
            order.push(id);
            (key, image, false)
        });
        if !entry.2 && cache_hold {
            entry.2 = true;
        }
    }

    let mut plan = ReclaimPlan::default();
    for id in order {
        let (_, image, cache_hold) = &group[&id];
        if *cache_hold {
            plan.kept_by_cache += 1;
        } else if Arc::strong_count(image) == 1 {
            plan.reclaim.push(image.clone());
        } else {
            plan.kept_by_holders += 1;
        }
    }
    plan
}

/// 回收缓存逐出入队的图集瓦片。只在 UI 线程、只由播放事件循环执行——绝不从绘制阶段调用。
/// 在绘制中途移除图集页会释放同一帧里可能已录制进精灵的纹理页，而 `DirectXAtlas::texture()`
/// 对悬空槽位 panic（封面密集页面上实测到的崩溃，2026-09-08）。事件循环只在有事件时迭代，
/// 所以其循环用超时来在播放空闲时继续排空。
pub fn drain_pending_tile_drops(cx: &mut App) {
    let Some(queue) = PENDING_TILE_DROPS.get() else {
        return;
    };
    let now = Instant::now();
    let (ready, young) = {
        let mut queue = queue.lock_recovered();
        partition_ready(queue.drain(..).collect(), now)
    };

    if !ready.is_empty() {
        let batch: Vec<(RenderCacheKey, Arc<RenderImage>)> = ready
            .into_iter()
            .map(|entry| (entry.key, entry.image))
            .collect();
        let plan = plan_tile_reclaims(batch, render_cache_holds);
        let stats = &TILE_DROP_STATS;
        stats
            .reclaimed
            .fetch_add(plan.reclaim.len() as u64, Ordering::Relaxed);
        stats
            .kept_by_cache
            .fetch_add(plan.kept_by_cache as u64, Ordering::Relaxed);
        stats
            .kept_by_holders
            .fetch_add(plan.kept_by_holders as u64, Ordering::Relaxed);
        if !plan.reclaim.is_empty() {
            crate::ui::util::reclaim_images_from_app(cx, plan.reclaim);
        }
    }

    // 未到期条目必须先回队再返回：drain 挂在播放事件循环上，几乎每秒
    // 都会执行，一张刚入队的图（回收年龄门未到）若在这里被丢弃，它的
    // 最后一份 Arc 就地消失，瓦片永远无人回收——pushed 持续增长而其余
    // 计数恒为零的 [mem] 曲线（2026-09-26 两个会话）正是这条路径。
    // 回队的时机必须在回收判定之后：年轻条目若提前入队，它的 Arc 会稀释就绪条目的
    // strong_count 判据。
    if !young.is_empty() {
        queue.lock_recovered().extend(young);
    }
}

/// 把队列分成"现在可回收瓦片"与"还要继续老化"两部分。就绪条目离开队列（释放其 Arc，
/// 从而保持排空处 `strong_count == 1` 判据有效）；年轻条目由调用方带着原始 due 时刻放回
/// 队列——重置它要么让瓦片永久泄漏（延迟无界增长），要么在跳过视图仍在回放时释放它
/// （2026-09-08 崩溃类）。
fn partition_ready(
    entries: Vec<PendingTileDrop>,
    now: Instant,
) -> (Vec<PendingTileDrop>, Vec<PendingTileDrop>) {
    entries.into_iter().partition(|entry| entry.due <= now)
}

fn render_cache_lookup(key: &ManagedImageKey, thumb: u32) -> Option<Arc<RenderImage>> {
    let cache_key = RenderCacheKey {
        key: key.clone(),
        thumb,
    };

    let mut cache = lock_render_cache();

    let hit = cache
        .cache
        .get(&cache_key)
        .map(|(image, _)| image.clone())?;
    // 不分配地刷新新旧度：重新插入键会推入重复项，所以改为把现有位置轮转到尾部。
    if let Some(pos) = cache.usage.iter().position(|k| *k == cache_key) {
        cache.usage.remove(pos);
        cache.usage.push_back(cache_key);
    }
    Some(hit)
}

fn render_cache_insert(key: ManagedImageKey, thumb: u32, image: Arc<RenderImage>) {
    let cache_key = RenderCacheKey { key, thumb };
    let new_bytes = image_bytes(&image);

    let mut cache = lock_render_cache();

    // 替换已有条目（可能是并发元素解码出来的），或预算满后逐出最久未用槽位。两条路径都把
    // 被丢弃的图像排进图集瓦片回收——这是缓存瓦片的唯一回收路径（单一持有者），所以每张图
    // 的瓦片恰好被释放一次。
    if let Some((old_image, old_bytes)) = cache.cache.insert(cache_key.clone(), (image, new_bytes))
    {
        cache.bytes = cache.bytes.saturating_sub(old_bytes) + new_bytes;
        queue_tile_drop(cache_key, old_image);
    } else {
        cache.bytes += new_bytes;
        cache.usage.push_back(cache_key);
        if cache.usage.len() > RENDER_CACHE_MAX
            && let Some(oldest) = cache.usage.pop_front()
            && let Some((image, bytes)) = cache.cache.remove(&oldest)
        {
            cache.bytes = cache.bytes.saturating_sub(bytes);
            queue_tile_drop(oldest, image);
        }
    }
}

impl ManagedImageKey {
    /// 拉取并解码封面，按 [`ImageCacheMode`] 记忆化，因此滚回视野的封面复用同一个
    /// `RenderImage`，而不是每个元素实例都付一次新解码 + 图集上传。Backdrop 模式还会在
    /// 插入缓存之前完成锐利裁剪带渲染（见 `backdrop_sharp_fill`；定长兜底仍用
    /// [`enhance_render_image`]），于是缓存里的图永远"拿来即绘"，元素绘制的是缓存持有的
    /// 那个确切 Arc——每张封面一次解码、一次增强、一套像素。`ImageCacheMode::None`
    /// 服务解码即弃的调用方：从不绘制的一次性图像（取色器），或瓦片寿命由元素自身约束的。
    pub(crate) async fn retrieve(
        &self,
        pool: SqlitePool,
        thumb_size: u32,
        cache: ImageCacheMode,
    ) -> anyhow::Result<Option<Arc<RenderImage>>> {
        let backdrop_target = match cache {
            ImageCacheMode::RenderCache => {
                if thumb_size > 0
                    && let Some(image) = render_cache_lookup(self, thumb_size)
                {
                    return Ok(Some(image));
                }
                None
            }
            ImageCacheMode::Backdrop(target) => {
                let thumb = match target {
                    Some((w, h)) => pack_backdrop_thumb(w, h),
                    None => thumb_size,
                };
                let cache_key = RenderCacheKey {
                    key: self.clone(),
                    thumb,
                };
                if let Some(image) = backdrop_cache_lookup(&cache_key) {
                    return Ok(Some(image));
                }
                target
            }
            ImageCacheMode::None => None,
        };
        // 串行化整条背景重管线（见 [`BACKDROP_PIPELINE_GATE`]）：解码、
        // 裁剪带与锐化全程只允许一条在飞。缓存命中的 retrieve 在上面就
        // 返回了，不经过这里——预取命中/回跳不被排队的新管线拖住。
        let _backdrop_pipeline = match cache {
            ImageCacheMode::Backdrop(_) => Some(
                BACKDROP_PIPELINE_GATE
                    .acquire()
                    .await
                    .expect("semaphore is never closed"),
            ),
            _ => None,
        };

        // 目标尺寸模式解码全尺寸原图（≥目标的源不被 thumbnail 压缩，裁剪
        // 与重采样只做一次），旧路径按方阵 bound 解码。
        let decode_bound = match backdrop_target {
            Some(_) => 0,
            None => thumb_size,
        };
        let decoded = self.retrieve_uncached(pool, decode_bound).await?;
        let Some(decoded) = decoded else {
            return Ok(None);
        };
        // 增强只属于背景两条路径：RenderCache（封面/标签）与 None（取色）
        // 保持解码原样——否则 512px 标签会被放大成 1920² 条目污染
        // RENDER_CACHE，256px 取色图也白付一次全尺寸增强。
        let image = match (cache, backdrop_target) {
            (ImageCacheMode::Backdrop(Some((w, h))), _) if w > 0 && h > 0 => {
                // 去噪+重采样是数百 ms 级同步 CPU（4096 源更高），必须在
                // 阻塞线程池执行：内联在运行时 worker 上会与音频流/封面
                // 下载等异步任务抢线程，切歌即卡顿（2026-09-29 "切换不
                // 丝滑" 的根因之一）。
                // decoded 按 move 交出所有权：4096² 全尺寸源（~67MB）只在
                // 裁剪带搬运时有用了，sharp_fill 内部搬完即 drop——去噪/
                // 重采样/锐化的数百 ms 里不再驻留，串行化后的单管线瞬态
                // 峰值从 ~121MB 压到 ~105MB。
                let src_size = decoded.size(0);
                let rendered = crate::RUNTIME.spawn_blocking({
                    move || match backdrop_sharp_fill(decoded, w, h) {
                        Ok(rendered) => rendered,
                        Err(original) => original,
                    }
                });
                let resampled: Arc<RenderImage> = rendered.await?;
                // 每首歌背景解码一次的唯一观测点：来源小图回退（大图变体
                // 404 后阶梯取到的尺寸）或内嵌小封面在这里现形——
                // 2026-09-29 "背景还是糊" 投诉的取证通道。
                let size = resampled.size(0);
                info!(
                    target: "backdrop",
                    key = ?self,
                    source = format!("{}x{}", u32::from(src_size.width), u32::from(src_size.height)),
                    width = u32::from(size.width),
                    height = u32::from(size.height),
                    "immersive backdrop decoded"
                );
                resampled
            }
            (ImageCacheMode::Backdrop(None), _) => {
                let enhanced = enhance_render_image(&decoded).unwrap_or_else(|| decoded.clone());
                // 旧路径观测点：与上面同目的。
                let size = enhanced.size(0);
                info!(
                    target: "backdrop",
                    key = ?self,
                    width = u32::from(size.width),
                    height = u32::from(size.height),
                    "immersive backdrop decoded (fixed-size path)"
                );
                enhanced
            }
            _ => decoded,
        };
        match cache {
            ImageCacheMode::RenderCache => {
                if thumb_size > 0 {
                    render_cache_insert(self.clone(), thumb_size, image.clone());
                }
            }
            ImageCacheMode::Backdrop(_) => {
                let cache_thumb = match backdrop_target {
                    Some((w, h)) => pack_backdrop_thumb(w, h),
                    None => thumb_size,
                };
                backdrop_cache_insert(
                    RenderCacheKey {
                        key: self.clone(),
                        thumb: cache_thumb,
                    },
                    image.clone(),
                );
            }
            ImageCacheMode::None => {}
        }
        Ok(Some(image))
    }

    async fn retrieve_uncached(
        &self,
        pool: SqlitePool,
        // 方形缩略图上限（像素）；`0` 表示保持全尺寸。
        thumb_size: u32,
    ) -> anyhow::Result<Option<Arc<RenderImage>>> {
        match self {
            ManagedImageKey::TrackFile(path) => {
                // 在线曲目带的是 HTTP URL 而非本地路径，要当作没有封面处理，
                // 不能把 URL 当文件打开（Windows 上 os error 123）。
                if !path.is_file() {
                    return Ok(None);
                }
                let path = path.clone();
                // 本分支是所有解码路径中最重的一条（开文件+格式探测+完整
                // 解码），与字节缓冲路径同占 DECODE_PERMITS：快速滚动时
                // 无许可的阻塞任务会把在线封面/背景解码挤出阻塞池。
                let _permit = DECODE_PERMITS
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                crate::RUNTIME
                    .spawn_blocking(move || -> anyhow::Result<Option<Arc<RenderImage>>> {
                        let Some(mut stream) =
                            try_open_media(&path, MediaProviderFeatures::PROVIDES_METADATA)?
                        else {
                            return Ok(None);
                        };
                        // 内嵌封面在探测阶段就已经捕获，读它不需要播放解码器。
                        // start_playback() 会为了取 `last_image` 给每首歌建一整套编解码
                        // 注册表加一个解码器实例，每次切歌白折腾一遍堆。

                        let decoded = if let Ok(Some(data)) = stream.read_image() {
                            Some(image::load_from_memory(&data)?)
                        } else if let Some(cover_path) = find_art_file_for_path(&path) {
                            let data = std::fs::read(&*cover_path)?;
                            Some(image::load_from_memory(&data)?)
                        } else {
                            None
                        };
                        let Some(dyn_image) = decoded else {
                            return Ok(None);
                        };
                        // 缩略图（从不放大）在转换之前跑在 DynamicImage 上，
                        // 于是缩略与 RGBA 扩展都永远见不到全尺寸缓冲。
                        Ok(Some(decode_rgba_to_render_image(dynamic_to_rgba_scaled(
                            dyn_image, thumb_size,
                        ))?))
                    })
                    .await?
            }
            #[cfg(feature = "online_sources")]
            ManagedImageKey::HttpCover(url) => {
                let bytes = crate::media::http_source::http_cover_bytes_cached(url).await?;
                let Some(bytes) = bytes else { return Ok(None) };
                Ok(decode_bounded_with_permit(bytes, thumb_size).await?)
            }
            #[cfg(feature = "online_sources")]
            ManagedImageKey::HttpCoverLarge(url) => {
                // 缩略图类消费（圆盘标签 512 等）阶梯封顶显示×4：512 显示
                // 用 2048 源已是 4× 过采样，观感与 4096 无差、解码成本
                // 1/4；thumb 0（背景全尺寸路径）cap=0 即不封顶，保持
                // 4096 阶梯。
                let cap = thumb_size.saturating_mul(4);
                let bytes =
                    crate::online_sources::cover_art::fetch_display_cover_bytes_capped(url, cap)
                        .await?;
                let Some(bytes) = bytes else { return Ok(None) };
                Ok(decode_bounded_with_permit(bytes, thumb_size).await?)
            }
            ManagedImageKey::Album(id) | ManagedImageKey::Track(id) => {
                let thumb = thumb_size > 0;
                // `thumb` 列存的是扫描器的 70×70 BMP，为 72px 列表行准备。更大的上限
                // （256px 网格瓦片、正在播放封面）必须解码完整封面再在这里缩小——
                // `image::thumbnail` 从不放大，把 70px 源画进 192px+ 瓦片会明显发糊。
                let stored_thumb_fits = thumb_size <= STORED_THUMB_PX;
                let query = match (self, thumb) {
                    (ManagedImageKey::Album(_), true) if stored_thumb_fits => {
                        include_str!("../../../queries/assets/find_album_thumb.sql")
                    }
                    (ManagedImageKey::Album(_), _) => {
                        include_str!("../../../queries/assets/find_album_art.sql")
                    }
                    (ManagedImageKey::Track(_), true) if stored_thumb_fits => {
                        include_str!("../../../queries/assets/find_track_thumb.sql")
                    }
                    (ManagedImageKey::Track(_), _) => {
                        include_str!("../../../queries/assets/find_track_art.sql")
                    }
                    (ManagedImageKey::TrackFile(_), _) => unreachable!(),
                    #[cfg(feature = "online_sources")]
                    (ManagedImageKey::HttpCover(_), _) => unreachable!(),
                    #[cfg(feature = "online_sources")]
                    (ManagedImageKey::HttpCoverLarge(_), _) => unreachable!(),
                };
                let Some((image_encoded,)): Option<(Option<Vec<u8>>,)> =
                    sqlx::query_as(query).bind(id).fetch_optional(&pool).await?
                else {
                    return Ok(None);
                };
                let Some(image_encoded) = image_encoded else {
                    return Ok(None);
                };

                if image_encoded.is_empty() {
                    return Ok(None);
                }

                Ok(decode_bounded_with_permit(image_encoded, thumb_size).await?)
            }
        }
    }
}

type ImageBridge = Arc<OnceLock<Option<Arc<RenderImage>>>>;

/// 解码结果的缓存去向（[`ManagedImageKey::retrieve`] 第三参）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImageCacheMode {
    /// 常规封面：进共享的 `RENDER_CACHE`（32 条 LRU，条目 ≤512px）。
    RenderCache,
    /// 解码即弃：一次性图像（取色等从不绘制的用途）——像素只被调用方
    /// 短暂持有，零缓存条目、零图集瓦片。
    None,
    /// 沉浸页全屏背景：独立的小容量 LRU（与 `RENDER_CACHE` 分开，避免
    /// 14MB 级大图挤占封面条目）。`Some((w, h))` 携带背景元素的**设备像
    /// 素目标尺寸**——解码后经 [`backdrop_sharp_fill`] 清晰裁剪带渲染到
    /// 该尺寸（cover 裁剪 → 中值去噪 → 重采样 → 阈值 unsharp），缓
    /// 存里永远是"拿来即绘"且与屏幕 1:1 的成图（2026-09-29 实测定案：
    /// 2048 纹理按 0.94 非整数比例双线性采样本身就会带来半像素级模糊，
    /// 唯一根治法是让纹理与屏幕逐像素对齐）；`None` 为旧的定长解码路径。
    /// 逐出走同一回收漏斗。
    Backdrop(Option<(u32, u32)>),
}

/// 把目标尺寸打包进缓存键的 thumb 字段（各 ≤65535，高 16 位宽、低 16 位高）。
fn pack_backdrop_thumb(w: u32, h: u32) -> u32 {
    (w.min(0xFFFF) << 16) | h.min(0xFFFF)
}

/// 沉浸页背景专用 LRU（容量 3）：同一首歌切走再切回、退出沉浸页再进，
/// 都命中同一份 `RenderImage`（同一 Arc → 同一批图集瓦片），零解码零重
/// 传。容量 3 覆盖"当前 + 预取的下一首 + crossfade 中的旧层"三套并存
/// （2026-09-29 预取优化：切歌瞬间整条链路零解码，详见沉浸页
/// `prefetch_next_track_art`）。
const BACKDROP_CACHE_CAP: usize = 3;

static BACKDROP_CACHE: OnceLock<Mutex<RenderCache>> = OnceLock::new();

fn lock_backdrop_cache() -> MutexGuard<'static, RenderCache> {
    BACKDROP_CACHE
        .get_or_init(|| {
            Mutex::new(RenderCache {
                cache: FxHashMap::default(),
                usage: VecDeque::new(),
                bytes: 0,
            })
        })
        .lock_recovered()
}

fn backdrop_cache_lookup(key: &RenderCacheKey) -> Option<Arc<RenderImage>> {
    let mut cache = lock_backdrop_cache();
    let image = cache.cache.get(key).map(|(image, _)| image.clone())?;
    // LRU touch：命中即移到队尾。
    cache.usage.retain(|k| k != key);
    cache.usage.push_back(key.clone());
    Some(image)
}

/// 插入并按容量逐出；逐出项经回收漏斗释放（背景类大图走
/// [`RECLAIM_DELAY_LARGE`] 3s 短门，见 [`reclaim_delay_for`]）。
fn backdrop_cache_insert(key: RenderCacheKey, image: Arc<RenderImage>) {
    let mut cache = lock_backdrop_cache();
    let bytes = image_bytes(&image);
    if let Some((old, old_bytes)) = cache.cache.insert(key.clone(), (image.clone(), bytes)) {
        cache.bytes = cache.bytes.saturating_sub(old_bytes);
        // 同键替换且不是同一份（罕见）：旧图走漏斗。
        if !Arc::ptr_eq(&old, &image) {
            queue_tile_drop(key.clone(), old);
        }
    }
    cache.bytes += bytes;
    cache.usage.retain(|k| k != &key);
    cache.usage.push_back(key);
    while cache.usage.len() > BACKDROP_CACHE_CAP {
        if let Some(oldest) = cache.usage.pop_front()
            && let Some((old, old_bytes)) = cache.cache.remove(&oldest)
        {
            cache.bytes = cache.bytes.saturating_sub(old_bytes);
            queue_tile_drop(oldest, old);
        }
    }
}

/// 退出沉浸模式时收缩背景缓存：从 LRU 尾部逐出多余的成图（经回收漏斗
/// 3s 短门放行像素与瓦片）。普通模式下沉浸页的预取图与上一首不该继续
/// 驻留——`keep=1` 保留当前歌曲一套（重进沉浸页秒开），其余立刻让位，
/// 省下 ~16MB 给普通界面。这是"退出界面即归还"纪律的执行点。
pub(crate) fn backdrop_cache_shrink(keep: usize) {
    let mut cache = lock_backdrop_cache();
    while cache.usage.len() > keep {
        if let Some(oldest) = cache.usage.pop_front()
            && let Some((old, old_bytes)) = cache.cache.remove(&oldest)
        {
            cache.bytes = cache.bytes.saturating_sub(old_bytes);
            queue_tile_drop(oldest, old);
        } else {
            break;
        }
    }
}

/// 队列中的图像像素字节数达到该阈值即视为"大图"（2048px 沉浸页背景
/// ≈ 16.8MB，增强后的 1920px 在线背景 ≈ 14.7MB；常规封面 ≤1MB）。
const LARGE_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// 回收队列中等待年龄门的**去重大图**（≥[`LARGE_IMAGE_BYTES`]，走
/// [`RECLAIM_DELAY_LARGE`] 3s 短门）套数。同一份图可能被缓存逐出和元素
/// 卸载各推一次，按 `RenderImage::id` 去重后的套数才是真实驻留。大图在
/// 队期间其像素与图集瓦片全程驻留，沉浸页只在计数不超过
/// [`BACKDROP_QUEUE_HEADROOM`] 时放行新的背景解码，
/// 把"同时驻留的大图套数"钉死在常数上。
pub(crate) fn large_drops_pending() -> usize {
    PENDING_TILE_DROPS
        .get()
        .map(|queue| {
            let queue = queue.lock_recovered();
            let mut seen: Vec<ImageId> = Vec::new();
            queue
                .iter()
                .filter(|entry| image_bytes(&entry.image) >= LARGE_IMAGE_BYTES)
                .filter(|entry| {
                    // 插入序去重：条目量级是个位数，线性扫足够。
                    if seen.contains(&entry.image.id) {
                        false
                    } else {
                        seen.push(entry.image.id);
                        true
                    }
                })
                .count()
        })
        .unwrap_or(0)
}

/// 允许在队的大图套数（不含缓存与显示中的那套）：3 → 普通切歌与预取
/// 节奏（2-4s/首，每曲至多 1-2 套入队）下阀门永不关闭；极端连跳时最多
/// 积压 3 套，加上缓存 3、显示 1、解码中 1 ≈ 8 套的瞬时上界（~63MB）。
/// 3s 大图短年龄门让积压在数秒内自行排空，而不是把切歌卡死整整一分钟。
/// 注意：阀门只拦"要新解码"的换图——缓存命中的换图经
/// [`backdrop_cache_contains`] 绕过本阀门，预取成果不被排水拖住。
const BACKDROP_QUEUE_HEADROOM: usize = 3;

pub(crate) fn backdrop_decode_allowed() -> bool {
    large_drops_pending() <= BACKDROP_QUEUE_HEADROOM
}

/// 背景缓存命中探测：命中说明目标图已渲染完成（预取/回跳），换图不产
/// 生新解码——`arm_backdrop` 据此绕过背压阀门直接交换，连跳时阀门排水
/// （3s/套）不再拖住已经就绪的下一首。
pub(crate) fn backdrop_cache_contains(key: &ManagedImageKey, w: u32, h: u32) -> bool {
    let cache_key = RenderCacheKey {
        key: key.clone(),
        thumb: pack_backdrop_thumb(w, h),
    };
    lock_backdrop_cache().cache.contains_key(&cache_key)
}

struct ManagedImageState {
    image: Option<Arc<RenderImage>>,
    bridge: Option<ImageBridge>,
    /// 取回任务的取消句柄。元素 unmount（`on_release`）时中止任务：
    /// 为已经看不见的元素继续做 DB 查询 / HTTP 下载 / 解码是纯浪费
    /// （§33 Cancellation），中止后任务停在下一个 await 点，不会再把
    /// 结果写回 bridge 或 RENDER_CACHE。
    task: Option<tokio::task::AbortHandle>,
    /// 本元素的 (来源, 尺寸) 缓存键——on_release 把图推进回收漏斗时需要
    /// 它来做"缓存是否仍持有"判定；request_layout 也用它判断 keyed state
    /// 是否被同位置的新图复用（索引键的列表换页/搜索刷新）。
    cache_key: RenderCacheKey,
}

/// 状态持有一切内容，为回收漏斗整体搬出。bridge 仍可能与一个已中止但尚未丢弃的 retrieve
/// 任务共享；那种情况下要通过共享句柄读取已就绪的图像，而不是把漏斗之外最后一份引用丢掉。
fn take_held_images(state: &mut ManagedImageState) -> impl Iterator<Item = Arc<RenderImage>> {
    let bridged = state
        .bridge
        .take()
        .and_then(|bridge| match Arc::try_unwrap(bridge) {
            Ok(mut once) => once.take().flatten(),
            Err(bridge) => bridge.get().cloned().flatten(),
        });
    state.image.take().into_iter().chain(bridged)
}

/// 为 `key` 启动 retrieve 任务及其完成续体，并记录结果所属的缓存键。
fn request_image(
    state: &mut ManagedImageState,
    key: ManagedImageKey,
    thumb_size: u32,
    cache: ImageCacheMode,
    cx: &mut gpui::Context<ManagedImageState>,
) {
    let pool = cx.global::<Pool>().0.clone();
    let bridge: ImageBridge = Arc::new(OnceLock::new());
    let bridge_clone = bridge.clone();
    let task_key = key.clone();
    let result_key = RenderCacheKey {
        key: key.clone(),
        thumb: thumb_size,
    };

    let handle = crate::RUNTIME.spawn(async move {
        let result = task_key.retrieve(pool, thumb_size, cache).await;
        let image = match &result {
            Ok(img) => img.clone(),
            Err(_) => None,
        };
        bridge_clone.set(image).ok();
        result
    });
    let abort = handle.abort_handle();

    cx.spawn(async move |this, cx| {
        // on_release 中止任务后 handle 返回 JoinError：元素已经
        // 不在了，不写回任何状态，也不再触发 notify。
        let result = match handle.await {
            Ok(result) => result,
            Err(e) if e.is_cancelled() => return,
            Err(e) => {
                error!("Image retrieve task failed: {:?}", e);
                return;
            }
        };
        match result {
            Ok(Some(image)) => {
                // 增强在 retrieve 内部完成（Backdrop 模式先增强后
                // 入缓存），元素绘制的 Arc 与缓存持有的是同一份。
                let updated = this.update(cx, |this: &mut ManagedImageState, cx| {
                    // 这条 retrieve 在途时同一元素槽位被换了键（索引键列表）。结果属于
                    // 上一个键：交给回收漏斗，而不是盖在新图上绘制。
                    if this.cache_key != result_key {
                        queue_tile_drop(result_key.clone(), image.clone());
                        return;
                    }
                    // keyed state 被同一元素位置跨内容复用时（如
                    // 正在播放栏逐曲换封面），被覆盖的旧图必须进
                    // 回收漏斗：普通 Drop 不会释放图集瓦片。推送
                    // 本身是安全的——drain 的"批外持有者 == 0"判
                    // 定会拦下仍被绘制或缓存持有的图。
                    if let Some(old) = this.image.take()
                        && !Arc::ptr_eq(&old, &image)
                    {
                        queue_orphan_tile_drop(old);
                    }
                    this.image = Some(image.clone());
                    this.bridge = None;
                    cx.notify();
                });
                if updated.is_err() {
                    // 元素已被释放：state 与其 bridge 的引用随之消
                    // 失，若这是最后一份引用，由这里回收 atlas 瓦
                    // 片（普通 Drop 不回收瓦片）。
                    if Arc::strong_count(&image) == 1 {
                        cx.update(|cx| {
                            crate::ui::util::reclaim_images_from_app(cx, vec![image]);
                        });
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                error!("Failed to retrieve image: {:?}", e);
            }
        }
    })
    .detach();

    state.image = None;
    state.bridge = Some(bridge);
    state.task = Some(abort);
    state.cache_key = RenderCacheKey {
        key,
        thumb: thumb_size,
    };
}

/// 重新键化被复用的元素状态：取消旧的 retrieve，把它持有的所有图像交还漏斗，再按新键重来。
fn restart_image(
    state: &mut ManagedImageState,
    key: ManagedImageKey,
    thumb_size: u32,
    cache: ImageCacheMode,
    cx: &mut gpui::Context<ManagedImageState>,
) {
    if let Some(task) = state.task.take() {
        task.abort();
    }
    let previous = std::mem::replace(
        &mut state.cache_key,
        RenderCacheKey {
            key: key.clone(),
            thumb: thumb_size,
        },
    );
    for image in take_held_images(state) {
        queue_tile_drop(previous.clone(), image);
    }
    request_image(state, key, thumb_size, cache, cx);
}

pub enum ImageReady {
    Available(Arc<RenderImage>),
    Pending(ImageBridge),
    None,
}

pub struct ManagedImage {
    key: ManagedImageKey,
    id: ElementId,
    style: StyleRefinement,
    object_fit: ObjectFit,
    /// 方形缩略图上限（像素）；`0` 表示源保持全尺寸。解码代价低，因此网格/列表封面绝不会
    /// 只为了画一个小瓦片而持有全分辨率 RGBA 缓冲（此前：GB 级工作集）。
    thumb_size: u32,
    /// 解码缓存去向（见 [`ImageCacheMode`]）。只绘制一次的图像必须退出缓存——正在播放栏的
    /// 逐曲封面就是如此：每首歌唯一的 URL 否则都会新增一个缓存条目和图集瓦片，其回收要等
    /// 渲染缓存 LRU，即 2026-09-14 浸泡实测到的"逐曲目提交阶梯"。沉浸页背景用
    /// [`ImageCacheMode::Backdrop`]（专用小容量 LRU + retrieve 内增强），因此切回同一首歌、
    /// 重进沉浸页都能即时绘制。
    cache: ImageCacheMode,
}

impl ManagedImage {
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }

    /// 缩到 72×72 方形（小列表/播放栏封面）。
    pub fn thumb(mut self) -> Self {
        self.thumb_size = 72;
        self
    }

    /// 缩到最长边不超过 `size` 像素的方形（网格瓦片）。
    pub fn thumb_max(mut self, size: u32) -> Self {
        self.thumb_size = size;
        self
    }

    /// 本图像跳过 `RENDER_CACHE`：只为这个元素实例解码一次，其图集瓦片在元素卸载时立即经
    /// 回收漏斗归还，而不是钉住整个页面直到 LRU 逐出。
    pub fn uncached(mut self) -> Self {
        self.cache = ImageCacheMode::None;
        self
    }

    /// 沉浸页全屏背景专用：独立的容量 3 LRU（[`BACKDROP_CACHE_CAP`]），
    /// 解码后在入缓存前完成增强
    /// （`retrieve` 内部）。同一首歌切走再切回、重进沉浸页都命中同一份
    /// `RenderImage` 与图集瓦片——零解码零重传，交换只隔一帧。
    pub fn backdrop_cached(mut self) -> Self {
        self.cache = ImageCacheMode::Backdrop(None);
        self
    }

    /// 背景的设备像素精确模式：解码后清晰裁剪带渲染到 `(w, h)`（全尺寸
    /// 裁剪 → 中值去噪 → Lanczos → 阈值 unsharp，见 [`backdrop_sharp_fill`]），
    /// 绘制时 GPU 以 1:1 采样（无重采样模糊）。thumb_size 打包目标尺寸，
    /// 保证 keyed-state 取回与 on_release 的缓存键和视图侧 `arm_backdrop`
    /// 插入的键完全一致。
    pub fn backdrop_cached_target(mut self, w: u32, h: u32) -> Self {
        self.cache = ImageCacheMode::Backdrop(Some((w, h)));
        self.thumb_size = pack_backdrop_thumb(w, h);
        self
    }
}

impl Styled for ManagedImage {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for ManagedImage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ManagedImage {
    type RequestLayoutState = ImageReady;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let key = self.key.clone();
        let thumb_size = self.thumb_size;
        let cache = self.cache;
        let init_key = key.clone();
        let entity = window.use_keyed_state("state", cx, move |_window, cx| {
            let mut state = ManagedImageState {
                image: None,
                bridge: None,
                task: None,
                cache_key: RenderCacheKey {
                    key: init_key.clone(),
                    thumb: thumb_size,
                },
            };
            cx.on_release(|this: &mut ManagedImageState, _cx| {
                // 先中止未完成的取回任务。瓦片回收统一走回收漏斗：on_release
                // 只负责把本状态持有的引用（image 字段 + bridge 内可能就绪
                // 的图）推进队列，由 drain 按"批外持有者 == 0"判定后恰好释放
                // 一次。直接 drop 会与其他路径的释放交错出二次释放（etagere
                // 代际断言，2026-09-12/13 闪退），无条件跳过则会泄漏被存活
                // 元素 paint 复活的瓦片。
                if let Some(task) = this.task.take() {
                    task.abort();
                }
                let cache_key = this.cache_key.clone();
                for image in take_held_images(this) {
                    queue_tile_drop(cache_key.clone(), image);
                }
            })
            .detach();
            request_image(&mut state, init_key, thumb_size, cache, cx);
            state
        });

        // keyed state 按元素位置寻址，所以索引键列表（在线行、队列）会把同一个槽位交给
        // 另一张封面。就地换键，而不是永远绘制那张过期图像。
        let desired = RenderCacheKey {
            key,
            thumb: thumb_size,
        };
        if entity.read(cx).cache_key != desired {
            entity.update(cx, |state, cx| {
                restart_image(state, desired.key.clone(), thumb_size, cache, cx);
            });
        }

        let (image, bridge) = {
            let state = entity.read(cx);
            (state.image.clone(), state.bridge.clone())
        };

        let ready = if let Some(image) = image {
            ImageReady::Available(image)
        } else if let Some(bridge) = bridge {
            match bridge.get() {
                Some(Some(image)) => {
                    let image = image.clone();
                    entity.update(cx, |this, cx| {
                        this.image = Some(image.clone());
                        this.bridge = None;
                        cx.notify();
                    });
                    ImageReady::Available(image)
                }
                Some(None) => ImageReady::None,
                None => ImageReady::Pending(bridge),
            }
        } else {
            ImageReady::None
        };

        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style, [], cx);

        (layout_id, ready)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let image = match request_layout {
            ImageReady::Available(image) => Some(image.clone()),
            ImageReady::Pending(bridge) => bridge.get().cloned().flatten(),
            ImageReady::None => None,
        };

        if let Some(image) = image {
            let image_size = image.size(0);
            let new_bounds = self.object_fit.get_bounds(bounds, image_size);
            let mut corners = Corners::default();
            corners.refine(&self.style.corner_radii);
            let corner_radii = corners.to_pixels(window.rem_size());
            if let Err(e) =
                window.paint_image(new_bounds, new_bounds, corner_radii, image, 0, false)
            {
                error!("Failed to paint image: {:?}", e);
            }
        }
    }
}

pub fn managed_image(id: impl Into<ElementId>, key: ManagedImageKey) -> ManagedImage {
    ManagedImage {
        key,
        id: id.into(),
        style: StyleRefinement::default(),
        object_fit: ObjectFit::Cover,
        thumb_size: 0,
        cache: ImageCacheMode::RenderCache,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 背景管线段已原样拆入子模块 backdrop_pipeline：这里补齐测试引用的
    // 函数与常量名（测试体一律不动）。
    use super::backdrop_pipeline::*;
    use image::{Frame, GenericImage, RgbaImage};
    use smallvec::smallvec;
    use std::collections::HashSet;

    #[test]
    fn median_removes_salt_noise_and_keeps_flat_areas() {
        // 3×3：暗像素包围的一个亮离群点——正是中值必须抹掉的块阶类噪声。
        let mut image = RgbaImage::from_fn(3, 3, |x, y| {
            if x == 1 && y == 1 {
                image::Rgba([200u8, 200, 200, 255])
            } else {
                image::Rgba([40u8, 40, 40, 255])
            }
        });
        denoise_rgba(&mut image);
        let after = *image.get_pixel(1, 1);
        assert_eq!(after[0], 40, "outlier should be medianed away: {after:?}");

        // 平坦区域必须逐字节保持不变。
        let mut flat = RgbaImage::from_fn(3, 3, |_, _| image::Rgba([128u8, 128, 128, 255]));
        denoise_rgba(&mut flat);
        assert_eq!(*flat.get_pixel(1, 1), image::Rgba([128u8, 128, 128, 255]));
    }

    #[test]
    fn median_keeps_true_edges_in_place() {
        // 硬性两级边缘（像建筑剪影对着天空）必须存活：离边界远的像素保持原值。
        let mut image = RgbaImage::from_fn(5, 5, |x, _| {
            if x < 2 {
                image::Rgba([30u8, 30, 30, 255])
            } else {
                image::Rgba([220u8, 220, 220, 255])
            }
        });
        denoise_rgba(&mut image);
        assert_eq!(*image.get_pixel(0, 2), image::Rgba([30u8, 30, 30, 255]));
        assert_eq!(*image.get_pixel(4, 2), image::Rgba([220u8, 220, 220, 255]));
    }

    #[test]
    fn enhance_render_image_passes_display_size_sources_through() {
        // 真 2048 源以 ~1:1 绘制——它的细节就是可获得的最大锐度。任何滤波（含块边中值）
        // 都会融化真实的 1-2px 细节，看起来比未增强的圆盘标签更糊，所以它必须原样返回同一张图，
        // 零拷贝。
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            2048,
            2,
            |x, y| image::Rgba([((x * 3 + y) % 255) as u8, 60, 90, 255])
        ))]));
        let enhanced = enhance_render_image(&image).unwrap();
        assert!(
            Arc::ptr_eq(&image, &enhanced),
            "display-size sources must pass through untouched"
        );
    }

    #[test]
    fn sharpen_rgba_boosts_edge_contrast() {
        // 对比度为封面真实量级（180 级）的软垂直阶跃：锐化必须增大跨越边缘的最陡相邻像素差，
        // 且不碰 alpha。
        let mut image = RgbaImage::from_fn(24, 8, |x, _| {
            image::Rgba([if x < 12 { 40 } else { 220 }, 7, 9, 255])
        });
        // 把边缘抹软，模拟 Lanczos 放大后的过渡。
        let softened = image::imageops::blur(&image, 1.5);
        image.copy_from(&softened, 0, 0).unwrap();
        let max_diff = |img: &RgbaImage| {
            let mut max = 0u8;
            for y in 0..img.height() {
                for x in 1..img.width() {
                    let a = img.get_pixel(x - 1, y).0[0];
                    let b = img.get_pixel(x, y).0[0];
                    max = max.max(a.abs_diff(b));
                }
            }
            max
        };
        let before = max_diff(&image);
        sharpen_rgba(&mut image);
        let after = max_diff(&image);
        assert!(
            after > before,
            "edge contrast must rise: {before} → {after}"
        );
        // alpha 处处保持 255。
        assert!(image.pixels().all(|p| p.0[3] == 255));
    }

    #[test]
    fn enhance_render_image_upscales_small_sources() {
        // 480px 在线缩略图拉伸到整窗会发糊；增强段必须在上传前把它重采样到显示尺寸目标，
        // 让 GPU 以 ~1:1 缩放。
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            480,
            360,
            |x, y| image::Rgba([((x * 7 + y) % 255) as u8, 60, 90, 255])
        ))]));
        let enhanced = enhance_render_image(&image).unwrap();
        let size = enhanced.size(0);
        assert_eq!(
            u32::from(size.width),
            ENHANCE_TARGET_PX,
            "long side must reach the target"
        );
        assert_eq!(u32::from(size.height), 1440, "aspect ratio preserved");
    }

    #[test]
    fn backdrop_sharp_fill_hits_exact_target() {
        // 元素用 ObjectFit::Cover 以 1:1 GPU 采样绘制缓存图——输出必须恰好是窗口设备像素
        // 目标尺寸，无论源是全尺寸还是阶梯回退缩略图。
        for source in [2048u32, 256] {
            let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
                source,
                source,
                |x, y| { image::Rgba([((x / 8) % 255) as u8, ((y / 8) % 255) as u8, 90, 255]) }
            ))]));
            let out = backdrop_sharp_fill(image, 1920, 1020).ok().unwrap();
            let size = out.size(0);
            assert_eq!(
                (u32::from(size.width), u32::from(size.height)),
                (1920, 1020),
                "source {source}px must still render to the exact target"
            );
        }
    }

    #[test]
    fn backdrop_sharp_fill_preserves_high_contrast_edges() {
        // 黑底上细 8px 白色"文字笔画"——用户要求背景与圆盘标签一样清晰的正是这类内容。
        // 清晰管线（去噪 → Lanczos → 阈值 unsharp）必须让笔画边缘保持高对比：任何模糊段
        // 都不许把它们化掉。
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            1024,
            1024,
            |x, y| {
                let on = (x / 8) % 2 == 0 && (400..440).contains(&y);
                image::Rgba([if on { 255 } else { 0 }, 0, 0, 255])
            }
        ))]));
        let out = backdrop_sharp_fill(image, 1920, 1020).ok().unwrap();
        let bytes = out.as_bytes(0).unwrap().to_vec();
        let sharp = RgbaImage::from_raw(1920, 1020, bytes).unwrap();
        let mut max_diff = 0u8;
        for y in 0..sharp.height() {
            for x in 1..sharp.width() {
                let a = sharp.get_pixel(x - 1, y).0[0];
                let b = sharp.get_pixel(x, y).0[0];
                max_diff = max_diff.max(a.abs_diff(b));
            }
        }
        assert!(
            max_diff >= 128,
            "a 255-level stroke edge must stay high-contrast, got {max_diff}"
        );
    }

    fn test_image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::new(
            2, 2
        ))]))
    }

    fn key(album: i64, thumb: u32) -> RenderCacheKey {
        RenderCacheKey {
            key: ManagedImageKey::Album(album),
            thumb,
        }
    }

    /// `RENDER_CACHE` 的替身：把缓存键映射到它服务的 Arc。这里持有克隆是在模拟真实缓存——
    /// 它的条目同样计入 `Arc::strong_count`。
    #[derive(Default)]
    struct FakeCache(FxHashMap<RenderCacheKey, Arc<RenderImage>>);

    impl FakeCache {
        fn insert(&mut self, k: RenderCacheKey, img: Arc<RenderImage>) {
            self.0.insert(k, img);
        }
        fn get(&self, k: &RenderCacheKey) -> Option<&Arc<RenderImage>> {
            self.0.get(k)
        }
        fn remove(&mut self, k: &RenderCacheKey) -> Option<Arc<RenderImage>> {
            self.0.remove(k)
        }
        fn holds(&self, k: &RenderCacheKey, img: &Arc<RenderImage>) -> bool {
            self.get(k).is_some_and(|c| Arc::ptr_eq(c, img))
        }
    }

    fn run_plan(batch: Vec<(RenderCacheKey, Arc<RenderImage>)>, cache: &FakeCache) -> ReclaimPlan {
        plan_tile_reclaims(batch, |k, img| cache.holds(k, img))
    }

    #[test]
    fn single_element_release_is_reclaimed() {
        let img = test_image();
        let plan = run_plan(vec![(key(1, 256), img)], &FakeCache::default());
        assert_eq!(plan.reclaim.len(), 1);
        assert_eq!((plan.kept_by_cache, plan.kept_by_holders), (0, 0));
    }

    /// 回归用例：同一张图被推两次且无外部持有者时，旧实现算 `strong_count - batch_copies`
    /// = 1 - 2，在 release 下 usize 下溢，于是永远跳过回收（常见"逐出+卸载"路径上的永久
    /// 图集瓦片泄漏）。
    #[test]
    fn same_batch_shared_release_reclaims_exactly_once() {
        let img = test_image();
        let a = img.clone(); // 元素 A 的引用，由其 on_release 推入
        let b = img.clone(); // 元素 B 的引用，由其 on_release 推入
        drop(img);
        let plan = run_plan(
            vec![(key(1, 256), a), (key(1, 256), b)],
            &FakeCache::default(),
        );
        assert_eq!(plan.reclaim.len(), 1);
        assert_eq!(plan.kept_by_holders, 0);
    }

    #[test]
    fn shared_release_across_batches_waits_for_last_holder() {
        let img = test_image();
        let holder_b = img.clone(); // 元素 B 仍挂载

        // 排空 1：只推了 A；B 仍能绘制这张图。
        let p1 = run_plan(vec![(key(1, 256), img)], &FakeCache::default());
        assert!(p1.reclaim.is_empty());
        assert_eq!(p1.kept_by_holders, 1);

        // 排空 2：B 已卸载并推入自己的引用；再无人持有它。
        let p2 = run_plan(vec![(key(1, 256), holder_b)], &FakeCache::default());
        assert_eq!(p2.reclaim.len(), 1);
    }

    #[test]
    fn cache_still_holding_skips_reclaim() {
        let mut cache = FakeCache::default();
        let img = test_image();
        cache.insert(key(1, 256), img.clone());
        let element_ref = img.clone();
        drop(img);

        // 缓存仍在服务这张图时元素卸载并推入它。
        let p = run_plan(vec![(key(1, 256), element_ref)], &cache);
        assert!(p.reclaim.is_empty());
        assert_eq!(p.kept_by_cache, 1);

        // 重复推入（第二个元素）在缓存持有期间仍被跳过：缓存会在真正逐出时自己推这个瓦片。
        let cached = cache.get(&key(1, 256)).unwrap().clone();
        let e1 = cached.clone();
        let e2 = cached.clone();
        drop(cached);
        let p = run_plan(vec![(key(1, 256), e1), (key(1, 256), e2)], &cache);
        assert!(p.reclaim.is_empty());
        assert_eq!(p.kept_by_cache, 1);
    }

    /// 排空必须按条目的原始 due 时刻来老化：回队时重置 due 要么让瓦片永久泄漏（每次排空都把
    /// due 往后推），要么在跳过视图仍在回放时释放它（due 被往前拉——2026-09-08 崩溃类）。
    #[test]
    fn partition_ready_keeps_original_due_for_young_entries() {
        let now = Instant::now();
        let old = PendingTileDrop {
            key: key(1, 256),
            image: test_image(),
            due: now - Duration::from_secs(1),
        };
        let young = PendingTileDrop {
            key: key(2, 256),
            image: test_image(),
            due: now + Duration::from_secs(30),
        };
        let young_due = young.due;

        let (ready, young_out) = partition_ready(vec![old, young], now);
        assert_eq!(ready.len(), 1);
        assert_eq!(young_out.len(), 1);
        assert_eq!(young_out[0].due, young_due);
    }

    /// 原地 Rgb8→Rgba8 扩展必须与 `to_rgba8` 逐字节一致，包括最容易重叠的前几个像素
    /// （i < 3）与最后一行。
    #[test]
    fn rgb8_expansion_matches_to_rgba8() {
        for (w, h) in [(1, 1), (2, 2), (3, 2), (7, 5), (64, 33)] {
            let rgb = image::RgbImage::from_fn(w, h, |x, y| {
                image::Rgb([(x * 7) as u8, (y * 11 + 3) as u8, (x * 13 + y) as u8])
            });
            let expected = image::DynamicImage::ImageRgb8(rgb.clone()).to_rgba8();

            let mut buf = rgb.into_raw();
            expand_rgb8_to_rgba8_in_place(&mut buf, w, h);
            let actual = image::RgbaImage::from_raw(w, h, buf).expect("expansion preserves length");

            assert_eq!(*actual, *expected, "mismatch at {w}x{h}");
        }
    }

    /// PNG 封面以 Rgba8 到达且必须被 steal（不留第二份缓冲），并且有界转换必须先缩略再扩展。
    #[test]
    fn dynamic_conversion_steals_rgba_and_scales() {
        let src =
            image::RgbaImage::from_fn(64, 64, |x, y| image::Rgba([x as u8, y as u8, 42, 255]));

        // bound = 0：交出原缓冲本身，不克隆。
        let full = dynamic_to_rgba_scaled(image::DynamicImage::ImageRgba8(src.clone()), 0);
        assert_eq!(*full, *src);

        // bound = 16：已缩小，尺寸依然正确。
        let scaled = dynamic_to_rgba_scaled(image::DynamicImage::ImageRgba8(src), 16);
        assert!(scaled.width() <= 16 && scaled.height() <= 16);
    }

    /// 缓存替换条目时元素仍在绘制旧图：旧瓦片必须恰好回收一次、只在元素消失之后，而替换进来
    /// 的新图在缓存持有期间绝不能被回收。
    #[test]
    fn replacement_race_reclaims_old_image_only() {
        let mut cache = FakeCache::default();
        let old = test_image();
        let old_id = old.id;
        cache.insert(key(1, 256), old.clone());
        let element_ref = old.clone();
        drop(old);

        // 缓存换图：旧图被逐出（入队），新解码占据该槽位。
        let evicted = cache.remove(&key(1, 256)).unwrap();
        assert!(Arc::ptr_eq(&evicted, &element_ref));
        let new = test_image();
        cache.insert(key(1, 256), new.clone());

        // 排空 1：元素仍持有旧图 → 跳过。
        let p1 = run_plan(vec![(key(1, 256), evicted)], &cache);
        assert!(p1.reclaim.is_empty());
        assert_eq!(p1.kept_by_holders, 1);

        // 排空 2：元素释放并推入 → 旧图回收一次，新图未被触碰。
        let p2 = run_plan(vec![(key(1, 256), element_ref)], &cache);
        assert_eq!(p2.reclaim.len(), 1);
        assert_eq!(p2.reclaim[0].id, old_id);
        assert!(cache.holds(&key(1, 256), &new));
    }

    /// 复活性质：回收判定只在批次之外零强引用时做出，所以瓦片释放后任何元素都不可能再绘制它
    /// ——在持有者数与重复推入的所有组合下，瓦片都被恰好释放一次。
    #[test]
    fn no_reclaim_while_any_external_holder_alive() {
        for holders in 0..=3usize {
            for pushes in 1..=3usize {
                let img = test_image();
                let guards: Vec<_> = (0..holders).map(|_| img.clone()).collect();
                let batch: Vec<_> = (0..pushes).map(|_| (key(2, 72), img.clone())).collect();
                drop(img);

                let plan = run_plan(batch, &FakeCache::default());
                if holders == 0 {
                    assert_eq!(
                        plan.reclaim.len(),
                        1,
                        "holders={holders} pushes={pushes}: must reclaim exactly once"
                    );
                } else {
                    assert!(
                        plan.reclaim.is_empty(),
                        "holders={holders} pushes={pushes}: live holder must prevent reclaim"
                    );
                    assert_eq!(plan.kept_by_holders, 1);
                }
                drop(guards);
            }
        }
    }

    /// thumb=0 的封面从不进入渲染缓存，所以它的瓦片完全由剩余强引用数判定。
    #[test]
    fn thumb_zero_uncached_image_reclaims() {
        let img = test_image();
        let plan = run_plan(vec![(key(3, 0), img)], &FakeCache::default());
        assert_eq!(plan.reclaim.len(), 1);
        assert_eq!(plan.kept_by_cache, 0);
    }

    /// 对抗用例：同一批次里一张图以两个不同键被推入，而缓存仍在其中一个键下服务它。持有判定
    /// 必须对每个推入的键做 OR，而不只看第一个——这锁住 `plan_tile_reclaims` 的
    /// "每个键都查询"承诺。
    #[test]
    fn multi_key_push_cache_hold_on_any_key_keeps_image() {
        let mut cache = FakeCache::default();
        let img = test_image();
        cache.insert(key(7, 256), img.clone());
        let element_ref = img.clone();
        drop(img);

        let orphan_key = RenderCacheKey {
            key: ManagedImageKey::TrackFile(PathBuf::new()),
            thumb: u32::MAX,
        };
        let plan = run_plan(
            vec![
                (key(7, 256), element_ref.clone()),
                (orphan_key, element_ref),
            ],
            &cache,
        );
        assert!(plan.reclaim.is_empty());
        assert_eq!(plan.kept_by_cache, 1);
    }

    /// 完整同批三推序列：缓存的逐出推入 + 两个共享元素对同一张图的 `on_release` 推入。
    /// 归组必须把三份 Arc 全部收敛并恰好回收一次。
    #[test]
    fn cache_evict_and_two_element_releases_in_one_batch_reclaim_once() {
        let mut cache = FakeCache::default();
        let img = test_image();
        cache.insert(key(9, 256), img.clone());
        let a = img.clone();
        let b = img.clone();
        drop(img);

        // 缓存逐出（remove + 入队）时两个元素同时卸载。
        let evicted = cache.remove(&key(9, 256)).unwrap();
        let plan = run_plan(
            vec![(key(9, 256), evicted), (key(9, 256), a), (key(9, 256), b)],
            &cache,
        );
        assert_eq!(plan.reclaim.len(), 1);
        assert_eq!((plan.kept_by_cache, plan.kept_by_holders), (0, 0));
    }

    #[test]
    fn triple_push_in_one_batch_dedups_to_one_reclaim() {
        let img = test_image();
        let batch = vec![
            (key(4, 256), img.clone()),
            (key(4, 256), img.clone()),
            (key(4, 256), img),
        ];
        let plan = run_plan(batch, &FakeCache::default());
        assert_eq!(plan.reclaim.len(), 1);
    }

    /// 在一次模拟会话上锁死全局不变量：每张图的瓦片至多回收一次，且每张被完全释放的图
    /// 最终都会被回收。
    #[test]
    fn every_tile_is_reclaimed_at_most_once_across_drains() {
        let mut cache = FakeCache::default();
        let mut dropped: HashSet<ImageId> = HashSet::new();

        let i1 = test_image();
        let i2 = test_image();
        cache.insert(key(10, 256), i1.clone());
        let h1 = i1.clone(); // 正在绘制缓存封面的元素
        let h2 = i2.clone(); // 正在绘制全尺寸（thumb=0）封面的元素
        drop(i1);
        drop(i2);

        // 轮次 1：缓存逐出 i1（入队）而 h1 仍存活；h2 卸载。
        let evicted = cache.remove(&key(10, 256)).unwrap();
        let p1 = run_plan(vec![(key(10, 256), evicted), (key(11, 0), h2)], &cache);
        assert_eq!(p1.reclaim.len(), 1, "i2 reclaimed, i1 kept for h1");
        for img in &p1.reclaim {
            assert!(dropped.insert(img.id), "tile reclaimed twice");
        }
        drop(p1);

        // 轮次 2：h1 卸载并推入 → i1 恰好回收一次。
        let p2 = run_plan(vec![(key(10, 256), h1)], &cache);
        assert_eq!(p2.reclaim.len(), 1);
        for img in &p2.reclaim {
            assert!(dropped.insert(img.id), "tile reclaimed twice");
        }
        drop(p2);

        assert_eq!(dropped.len(), 2, "every released image reclaimed once");
    }

    /// 背景级（≥4MB）图像走短年龄门老化，因此一连串切歌不会把背景冻结 60s；封面保留保守的
    /// 60s 门。短门必须始终高于沉浸页 crossfade 窗口——淡出期间旧元素（及其瓦片）仍在绘制。
    #[test]
    fn large_backdrop_images_use_the_short_reclaim_gate() {
        assert_eq!(reclaim_delay_for(&test_image()), RECLAIM_DELAY);
        let large = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            1024,
            1024,
            |_, _| image::Rgba([1, 2, 3, 4])
        ))]));
        assert!(image_bytes(&large) >= LARGE_IMAGE_BYTES);
        assert_eq!(reclaim_delay_for(&large), RECLAIM_DELAY_LARGE);
        assert!(RECLAIM_DELAY_LARGE > Duration::from_millis(400));
    }

    /// 裁剪几何钉桩：去噪/重采样发生在居中 cover 裁剪之后，所以裁剪带之外的内容绝不能渗进
    /// 输出——这条带仍是几何意义上的 cover 裁剪。
    #[test]
    fn backdrop_sharp_fill_matches_target_and_center() {
        // 2048x2048 源：顶部五分之一为红，其余平坦绿。1920x1028 目标的居中 cover 裁剪从
        // 源第 (2048 - 1097) / 2 = 475 行开始，低于红带（Lanczos 振铃约达 12 个源行），
        // 所以每个输出像素都必须是绿。
        let src = RgbaImage::from_fn(2048, 2048, |_, y| {
            if y < 400 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 255, 0, 255])
            }
        });
        let source = Arc::new(RenderImage::new(smallvec![Frame::new(src)]));

        let out = backdrop_sharp_fill(source.clone(), 1920, 1028).expect("render succeeds");
        let size = out.size(0);
        assert_eq!(u32::from(size.width), 1920);
        assert_eq!(u32::from(size.height), 1028);

        let bytes = out.as_bytes(0).expect("frame bytes");
        assert!(
            bytes
                .chunks(4)
                .all(|px| px[0] < 8 && px[1] > 240 && px[2] < 8 && px[3] == 255),
            "cover crop must exclude the top red band; flat green survives denoise+resize"
        );

        // 退化目标返回 Err（调用方回退到被原样还回的源图）。
        assert!(backdrop_sharp_fill(source.clone(), 0, 100).is_err());
        assert!(backdrop_sharp_fill(source, 100, 0).is_err());
    }

    /// 小尺寸源（阶梯回退）也必须渲染到精确目标尺寸（这样 GPU 以 1:1 取样，而不是放大小纹理）。
    #[test]
    fn backdrop_sharp_fill_upscales_small_sources() {
        let source = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            480,
            360,
            |x, y| { image::Rgba([((x * 7 + y) % 255) as u8, 60, 90, 255]) }
        ))]));
        let out = backdrop_sharp_fill(source, 1920, 1440).expect("render succeeds");
        let size = out.size(0);
        assert_eq!(u32::from(size.width), 1920);
        assert_eq!(u32::from(size.height), 1440);
    }
}

/// 图像管线耗时探针（§37 回归防护）：封面解码路径（每个列表/网格瓦片都要付的）与沉浸页背景
/// 管线（2026-09-29 "切换不丝滑"复盘说的那数百 ms 级同步 CPU）。配套探针：`playback` 测试里的
/// 分配守卫覆盖音频侧，`[mem]`/`[frame]` 覆盖驻留/帧侧。
///
/// ```text
/// cargo test --release managed_image::bench -- --ignored --nocapture
/// ```
#[cfg(test)]
mod bench {
    use super::*;
    use crate::test_support::bench_summarize_n;
    use image::{Frame, RgbaImage};
    use smallvec::smallvec;
    use std::sync::Arc;

    /// 类照片的合成封面：平滑渐变 + 每 8px 的块阶，这样 JPEG 压缩与去噪/重采样段看到的都是
    /// 真实结构（纯色会让每个数字都好看）。JPEG 无 alpha，所以缓冲按 Rgb8 编码——正是真实
    /// 封面文件解码回来的形态。
    fn synthetic_jpeg(size: u32) -> Vec<u8> {
        let image = RgbaImage::from_fn(size, size, |x, y| {
            image::Rgba([
                ((x * 255 / size) as u8).wrapping_add(((x / 8 % 2) * 24) as u8),
                ((y * 255 / size) as u8).wrapping_add(((y / 8 % 2) * 24) as u8),
                (((x + y) * 255 / (2 * size)) as u8),
                255,
            ])
        });
        let rgb = image::DynamicImage::ImageRgba8(image).to_rgb8();
        let mut buf = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
        encoder
            .encode(rgb.as_raw(), size, size, image::ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    fn synthetic_source(size: u32) -> Arc<RenderImage> {
        Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            size,
            size,
            |x, y| {
                image::Rgba([
                    ((x * 255 / size) as u8).wrapping_add(((x / 8 % 2) * 24) as u8),
                    ((y * 255 / size) as u8).wrapping_add(((y / 8 % 2) * 24) as u8),
                    (((x + y) * 255 / (2 * size)) as u8),
                    255,
                ])
            },
        ))]))
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn report_image_pipeline_costs() {
        // 4096 必须在列：全语料日志里"背景解析到大图变体"的 15 张去重封面
        // **全部**命中 4096 档（压缩字节 p50 1.36MB / max 3.04MB），AGENTS.md
        // 记的"4096 级仅 ~2%"是 2026-10-06 的 98 样本读数，与本机 29h 语料不符。
        // 4096 既是解码缓冲的最大项（67MB），也是管线瞬态的上界，不测它等于
        // 用偏样本给回归基线定尺。
        for source_px in [4096u32, 2048, 1280] {
            let jpeg = synthetic_jpeg(source_px);
            println!(
                "source: {source_px}px synthetic JPEG, {} KB",
                jpeg.len() / 1024
            );

            // 显示封面路径：按瓦片尺寸封顶解码（每个列表/网格行未命中缓存时要付的）。
            bench_summarize_n("decode scaled @512", 5, 50, || {
                let _ = decode_to_render_image_scaled(&jpeg, 512).unwrap();
            });
            // 完整解码：背景源路径（不设上限）。
            bench_summarize_n(&format!("decode full @{source_px}"), 1, 5, || {
                let _ = decode_to_render_image_scaled(&jpeg, 0).unwrap();
            });
        }

        // 沉浸页背景管线：全尺寸源 → cover 裁剪 → 去噪 → 重采样 → unsharp，
        // 目标是 1920x1080 设备像素。
        // 4096 与上面同一理由：在线封面的背景档实测全部解析到 4096。
        for source_px in [4096u32, 2048, 1280] {
            let source = synthetic_source(source_px);
            bench_summarize_n(
                &format!("backdrop_sharp_fill {source_px}->1080p"),
                1,
                3,
                || {
                    let _ = backdrop_sharp_fill(source.clone(), 1920, 1080).unwrap();
                },
            );
        }

        // 定长兜底路径（目标尺寸未知的首帧）：增强段只在源很小时做实事——2048 源原样直通，
        // 所以探针用它真正服务的那个 480px 阶梯回退尺寸。
        let source = synthetic_source(480);
        bench_summarize_n("enhance_render_image 480 (fallback)", 2, 10, || {
            let _ = enhance_render_image(&source).unwrap();
        });
    }
}
