use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use gpui::{
    App, Bounds, Corners, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, ObjectFit, Pixels, Refineable, RenderImage, Style, StyleRefinement,
    Styled, Window,
};
#[cfg(feature = "online_sources")]
use gpui::SharedString;
use globwalk::GlobWalkerBuilder;
use image::{Frame, Pixel, imageops};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use sqlx::SqlitePool;
use tracing::error;

use crate::{
    media::{lookup_table::try_open_media, traits::MediaProviderFeatures},
    ui::app::Pool,
};

fn find_art_file_for_path(path: &Path) -> Option<Arc<Path>> {
    let parent = path.parent()?;

    let mut glob =
        GlobWalkerBuilder::from_patterns(parent, &["{folder,cover,front}.{jpg,jpeg,png}"])
            .case_insensitive(true)
            .max_depth(1)
            .build()
            .expect("Failed to build album art glob")
            .filter_map(|e| e.ok());

    glob.next().map(|e| Arc::from(e.path()))
}

/// Caps concurrent cover decodes across all `ManagedImage` elements: each
/// in-flight decode transiently holds a full-size RGBA buffer (a 3000px cover
/// is ~36MB), and one fast grid scroll can miss the render cache for dozens
/// of tiles at once. Mirrors the scanner's artwork decode cap; the render
/// cache above bounds what is *retained*, this bounds what is *in flight*.
static DECODE_PERMITS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(4));

/// Swaps R and B channels in place so `image` crate buffers match GPUI's
/// expected BGRA ordering.
pub(crate) fn rgb_to_bgr(image: &mut image::RgbaImage) {
    image.pixels_mut().for_each(|v| {
        let slice = v.channels();
        *v = *image::Rgba::from_slice(&[slice[2], slice[1], slice[0], slice[3]]);
    });
}

fn decode_rgba_to_render_image(mut image: image::RgbaImage) -> anyhow::Result<Arc<RenderImage>> {
    rgb_to_bgr(&mut image);
    let mut frames: SmallVec<[_; 1]> = SmallVec::new();
    frames.push(Frame::new(image));
    Ok(Arc::new(RenderImage::new(frames)))
}

/// Longest side of the BMP thumbnail the scanner stores in the `thumb`
/// column (70×70, see scan decode). Decodes bounded at or below this can use
/// that cheap pre-scaled source; anything larger decodes the full art.
const STORED_THUMB_PX: u32 = 72;

/// Decode `data`, downscaling to a square of at most `bound` pixels before
/// converting to RGBA. Scaling at decode time keeps the transient RGBA buffer
/// (and anything that retains it) bounded by what the UI actually paints.
fn decode_to_render_image_scaled(
    data: &[u8],
    bound: u32,
) -> anyhow::Result<Arc<RenderImage>> {
    let image = image::load_from_memory(data)?;
    let image = if bound > 0 {
        image.thumbnail(bound, bound)
    } else {
        image
    };
    decode_rgba_to_render_image(image.to_rgba8())
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ManagedImageKey {
    Album(i64),
    Track(i64),
    TrackFile(PathBuf),
    /// Online album art fetched over HTTP (KuGou / NetEase track cover URL).
    #[cfg(feature = "online_sources")]
    HttpCover(SharedString),
}

/// Upper bound on decoded `RenderImage`s kept alive across elements.
///
/// Scrolling an online list re-creates `ManagedImage` rows; without this cache
/// each one re-decodes the same URL into a fresh `RenderImage` and re-uploads
/// a new atlas texture. Capping the retained set keeps that churn from adding
/// up, and re-painting a recently seen cover reuses the exact same
/// `RenderImage`/atlas slot instead. 64 covers ≈ 16 MB of pixels; each atlas
/// page holds a handful of 256 px tiles, so this keeps the live page count (the
/// largest single contributor to steady-state private bytes) bounded well
/// below what 128 entries allowed. Evictions queue their atlas tiles for
/// reclamation, and evicted covers re-decode from the disk cache on return.
const RENDER_CACHE_MAX: usize = 64;

/// Bounded set of decoded covers shared by all `ManagedImage` elements.
/// Keyed by (source, thumb size) since 72px table rows and 256px grid tiles
/// are different decodes. Cached tiles are reclaimed exactly once through
/// `queue_tile_drop` + `drain_pending_tile_drops`; element `on_release`
/// reclaims only when it holds the last reference (see `drop_image_if_last`
/// there) — double-freeing a tile trips an etagere assertion (2026-09-12
/// crash) and never freeing one leaks it once a surviving element re-paints
/// it after eviction.
static RENDER_CACHE: OnceLock<Mutex<RenderCache>> = OnceLock::new();

#[derive(Clone, PartialEq, Eq, Hash)]
struct RenderCacheKey {
    key: ManagedImageKey,
    thumb: u32,
}

struct RenderCache {
    cache: FxHashMap<RenderCacheKey, (Arc<RenderImage>, u64)>,
    usage: VecDeque<RenderCacheKey>,
    /// Approximate live pixel bytes, tracked so the [mem] probe can report the
    /// exact footprint of this cache instead of hand-waving at peak numbers.
    bytes: u64,
}

/// Estimated RGBA footprint in bytes of a decoded cover.
fn image_bytes(image: &RenderImage) -> u64 {
    let size = image.size(0);
    (size.width.0 as u64) * (size.height.0 as u64) * 4
}

/// Live foot print of the decoded-cover LRU, in MiB. Reported by the [mem]
/// periodic probe.
pub fn render_cache_mb() -> u64 {
    let Some(cache) = RENDER_CACHE.get() else {
        return 0;
    };
    let cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.bytes / (1024 * 1024)
}

/// Images evicted from `RENDER_CACHE` whose atlas tiles still need dropping.
/// Eviction runs on the RUNTIME where no `App` exists, so the tiles are queued
/// here and the next `ManagedImage` layout pass (UI thread) hands them to
/// `drop_image`. Without this, an evicted cover's atlas page stays pinned
/// forever once its owning elements have unmounted.
static PENDING_TILE_DROPS: OnceLock<Mutex<Vec<Arc<RenderImage>>>> = OnceLock::new();

fn queue_tile_drop(image: Arc<RenderImage>) {
    PENDING_TILE_DROPS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(image);
}

/// Reclaims atlas tiles queued by cache evictions. Runs on the UI thread,
/// exclusively from the playback event loop — never from the paint pass.
/// Removing atlas pages mid-paint frees a texture page whose sprites may
/// already be recorded in the same frame, and `DirectXAtlas::texture()`
/// panics on the dangling slot (crash seen on cover-heavy pages, 2026-09-08).
/// The event loop only iterates while events arrive, so its loop uses a
/// timeout to keep draining when playback is idle.
pub fn drain_pending_tile_drops(cx: &mut App) {
    let Some(queue) = PENDING_TILE_DROPS.get() else {
        return;
    };
    // 只回收最后一个持有者：若某元素的 keyed state 还握着同一 Arc（图被
    // 淘汰但元素仍存活），跳过并由该元素 on_release 时按引用计数回收。
    // 持有者只减不增——缓存是唯一分发者，淘汰后不会再有新持有者出现。
    let drained: Vec<Arc<RenderImage>> = queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain(..)
        .filter(|image| Arc::strong_count(image) == 1)
        .collect();
    if !drained.is_empty() {
        crate::ui::util::reclaim_images_from_app(cx, drained);
    }
}

fn render_cache_lookup(key: &ManagedImageKey, thumb: u32) -> Option<Arc<RenderImage>> {
    let cache_key = RenderCacheKey {
        key: key.clone(),
        thumb,
    };

    let cache = RENDER_CACHE.get_or_init(|| {
        Mutex::new(RenderCache {
            cache: FxHashMap::default(),
            usage: VecDeque::new(),
            bytes: 0,
        })
    });
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let hit = cache.cache.get(&cache_key).map(|(image, _)| image.clone())?;
    // Refresh recency without allocating: reinserting the key would push a
    // duplicate, so rotate the existing position to the back instead.
    if let Some(pos) = cache.usage.iter().position(|k| *k == cache_key) {
        cache.usage.remove(pos);
        cache.usage.push_back(cache_key);
    }
    Some(hit)
}

fn render_cache_insert(key: ManagedImageKey, thumb: u32, image: Arc<RenderImage>) {
    let cache_key = RenderCacheKey { key, thumb };
    let new_bytes = image_bytes(&image);

    let cache = RENDER_CACHE.get_or_init(|| {
        Mutex::new(RenderCache {
            cache: FxHashMap::default(),
            usage: VecDeque::new(),
            bytes: 0,
        })
    });
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // Replace an existing entry (possibly decoded by a concurrent element) or
    // evict the least-recently-used slot once the budget is full. Both paths
    // queue the dropped image for atlas-tile reclamation — this is the ONLY
    // reclamation path for cached tiles (single owner), so each image's tile
    // is dropped exactly once.
    if let Some((old_image, old_bytes)) =
        cache.cache.insert(cache_key.clone(), (image, new_bytes))
    {
        cache.bytes = cache.bytes.saturating_sub(old_bytes) + new_bytes;
        queue_tile_drop(old_image);
    } else {
        cache.bytes += new_bytes;
        cache.usage.push_back(cache_key);
        if cache.usage.len() > RENDER_CACHE_MAX {
            if let Some(oldest) = cache.usage.pop_front()
                && let Some((image, bytes)) = cache.cache.remove(&oldest)
            {
                cache.bytes = cache.bytes.saturating_sub(bytes);
                queue_tile_drop(image);
            }
        }
    }
}

impl ManagedImageKey {
    /// Fetches and decodes the artwork, memoized in `RENDER_CACHE` so a cover
    /// that scrolls back into view reuses the same `RenderImage` instead of
    /// paying for a fresh decode + atlas upload per element instance. Only
    /// thumbnails are cached: full-resolution (thumb 0) art is a one-off gallery
    /// decode that would blow the pixel budget at `RENDER_CACHE_MAX`.
    async fn retrieve(
        &self,
        pool: SqlitePool,
        thumb_size: u32,
    ) -> anyhow::Result<Option<Arc<RenderImage>>> {
        if thumb_size > 0
            && let Some(image) = render_cache_lookup(self, thumb_size)
        {
            return Ok(Some(image));
        }

        let decoded = self.retrieve_uncached(pool, thumb_size).await?;
        if thumb_size > 0
            && let Some(image) = &decoded
        {
            render_cache_insert(self.clone(), thumb_size, image.clone());
        }
        Ok(decoded)
    }

    async fn retrieve_uncached(
        &self,
        pool: SqlitePool,
        // Square thumbnail bound in pixels; `0` keeps full size.
        thumb_size: u32,
    ) -> anyhow::Result<Option<Arc<RenderImage>>> {
        match self {
            ManagedImageKey::TrackFile(path) => {
                // Online tracks carry an HTTP URL, not a local path; treat them as having
                // no artwork instead of opening the URL as a file (os error 123 on Windows).
                if !path.is_file() {
                    return Ok(None);
                }
                let path = path.clone();
                crate::RUNTIME
                    .spawn_blocking(move || -> anyhow::Result<Option<Arc<RenderImage>>> {
                        let Some(mut stream) =
                            try_open_media(&path, MediaProviderFeatures::PROVIDES_METADATA)?
                        else {
                            return Ok(None);
                        };
                        // The embedded artwork is captured during the probe,
                        // so reading it does not need the playback decoder.
                        // start_playback() built a full codec registry plus a
                        // decoder instance per track just to fetch
                        // `last_image`, churning the heap on every track
                        // change for nothing.

                        let mut image = if let Ok(Some(data)) = stream.read_image() {
                            image::load_from_memory(&data)?.to_rgba8()
                        } else if let Some(cover_path) = find_art_file_for_path(&path) {
                            let data = std::fs::read(&*cover_path)?;
                            image::load_from_memory(&data)?.to_rgba8()
                        } else {
                            return Ok(None);
                        };

                        if thumb_size > 0 {
                            image = imageops::thumbnail(&image, thumb_size, thumb_size);
                        }

                        Ok(Some(decode_rgba_to_render_image(image)?))
                    })
                    .await?
            }
            #[cfg(feature = "online_sources")]
            ManagedImageKey::HttpCover(url) => {
                let url = url.to_string();
                let bytes = crate::media::http_source::http_cover_bytes_cached(&url).await?;
                let Some(bytes) = bytes else { return Ok(None) };
                let image = {
                    let _permit =
                        DECODE_PERMITS.acquire().await.expect("semaphore is never closed");
                    crate::RUNTIME
                        .spawn_blocking(move || {
                            decode_to_render_image_scaled(&bytes, thumb_size).map(Some)
                        })
                        .await??
                };
                Ok(image)
            }
            ManagedImageKey::Album(id) | ManagedImageKey::Track(id) => {
                let thumb = thumb_size > 0;
                // The `thumb` column holds the scanner's 70×70 BMP, sized for
                // 72px list rows. A larger bound (256px grid tiles, now
                // playing art) must decode the full art and scale down here —
                // `image::thumbnail` never upscales, so a 70px source painted
                // into a 192px+ tile renders visibly mushy.
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

                let image = {
                    let _permit = DECODE_PERMITS.acquire().await.expect("semaphore is never closed");
                    crate::RUNTIME
                        .spawn_blocking(move || {
                            decode_to_render_image_scaled(&image_encoded, thumb_size).map(Some)
                        })
                        .await??
                };

                Ok(image)
            }
        }
    }
}

type ImageBridge = Arc<OnceLock<Option<Arc<RenderImage>>>>;

struct ManagedImageState {
    image: Option<Arc<RenderImage>>,
    bridge: Option<ImageBridge>,
    /// 取回任务的取消句柄。元素 unmount（`on_release`）时中止任务：
    /// 为已经看不见的元素继续做 DB 查询 / HTTP 下载 / 解码是纯浪费
    /// （§33 Cancellation），中止后任务停在下一个 await 点，不会再把
    /// 结果写回 bridge 或 RENDER_CACHE。
    task: Option<tokio::task::AbortHandle>,
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
    /// Square thumbnail bound in pixels; `0` keeps the source at full size.
    /// Decodes cheaply so grid/list art never holds a full-resolution RGBA
    /// buffer just to paint a small tile (was: GB-scale working set).
    thumb_size: u32,
}

impl ManagedImage {
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }

    /// Downscale to a 72×72 square (small list/playback-bar art).
    pub fn thumb(mut self) -> Self {
        self.thumb_size = 72;
        self
    }

    /// Downscale to a square of at most `size` pixels (grid tiles).
    pub fn thumb_max(mut self, size: u32) -> Self {
        self.thumb_size = size;
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
        let entity = window.use_keyed_state("state", cx, move |_window, cx| {
            let pool = cx.global::<Pool>().0.clone();
            let bridge: ImageBridge = Arc::new(OnceLock::new());
            let bridge_clone = bridge.clone();

            let handle = crate::RUNTIME.spawn(async move {
                let result = key.retrieve(pool, thumb_size).await;
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
                        if this
                            .update(cx, |this: &mut ManagedImageState, cx| {
                                this.image = Some(image.clone());
                                this.bridge = None;
                                cx.notify();
                            })
                            .is_err()
                        {
                            // 元素已被释放：state 与其 bridge 的引用随之消
                            // 失，若这是最后一份引用，由这里回收 atlas 瓦
                            // 片（普通 Drop 不回收瓦片）。
                            if Arc::strong_count(&image) == 1 {
                                let _ = cx.update(|cx| {
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

            cx.on_release(|this: &mut ManagedImageState, cx| {
                // 先中止未完成的取回任务。瓦片回收按引用计数：仅当本状态
                // 是最后一份引用时才回收 atlas 瓦片。RENDER_CACHE、待释放
                // 队列、其他元素的 keyed state 都可能还持有同一 Arc，持有
                // 者只减不增（缓存是唯一分发者），最后放手的路径必然看到
                // count == 1——每块瓦片恰好释放一次。直接 drop 会二次释放
                // （etagere 断言，2026-09-12 闪退），无条件跳过则会泄漏
                // 被淘汰后仍被存活元素 paint 复活的瓦片。
                if let Some(task) = this.task.take() {
                    task.abort();
                }
                // image 字段与 bridge 里的就绪图是互斥的两个持有点（任务
                // 完成时先 set bridge、continuation 再写回字段）。state 以
                // 普通 Drop 消失时 bridge 里的 Arc 不会回收瓦片，所以两处
                // 都要按"最后引用"判定后交给回收路径。
                let bridged = this.bridge.take().and_then(|bridge| {
                    // state 是 bridge Arc 的最后持有者（任务闭包已完成或被
                    // 中止），try_unwrap 拿到所有权后才能 take 出内部的图。
                    Arc::try_unwrap(bridge)
                        .ok()
                        .and_then(|mut once| once.take())
                        .flatten()
                });
                let images = this.image.take().into_iter().chain(bridged);
                for image in images {
                    if Arc::strong_count(&image) == 1 {
                        crate::ui::util::reclaim_images_from_app(cx, vec![image]);
                    }
                }
            })
            .detach();

            ManagedImageState {
                image: None,
                bridge: Some(bridge),
                task: Some(abort),
            }
        });

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
    }
}
