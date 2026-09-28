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
use tracing::error;

use crate::{
    media::{lookup_table::try_open_media, traits::MediaProviderFeatures},
    ui::app::Pool,
};

/// Cover file names accepted for a track's sibling art lookup: three stems ×
/// three extensions, matched case-insensitively within a single directory
/// level. Plain `read_dir` + name compare replaces the former `globwalk`
/// glob — same candidates, same readdir order, no glob compiled per lookup.
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
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(image)])))
}

/// Longest side of the BMP thumbnail the scanner stores in the `thumb`
/// column (70×70, see scan decode). Decodes bounded at or below this can use
/// that cheap pre-scaled source; anything larger decodes the full art.
const STORED_THUMB_PX: u32 = 72;

/// Decode `data`, downscaling to a square of at most `bound` pixels before
/// converting to RGBA. Scaling at decode time keeps the transient RGBA buffer
/// (and anything that retains it) bounded by what the UI actually paints.
fn decode_to_render_image_scaled(data: &[u8], bound: u32) -> anyhow::Result<Arc<RenderImage>> {
    let image = image::load_from_memory(data)?;
    decode_rgba_to_render_image(dynamic_to_rgba_scaled(image, bound))
}

/// Converts a decoded image to RGBA, preferring steal-or-expand over
/// `to_rgba8`'s full-size copy: PNG-decoded images hand their buffer over
/// untouched, Rgb8 (JPEG) expands in place with a backwards fill, and only
/// exotic color types pay `to_rgba8`. Thumbnailing (bound > 0) runs first on
/// the DynamicImage so the conversion never sees full-size data — the
/// TrackFile path used to convert at full size and thumbnail after, holding
/// Rgb8+Rgba8 of a ~2500px cover simultaneously (dhat: 23.6MB transient,
/// 2026-09-16).
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

/// Expands an Rgb8 buffer to Rgba8 in place: grow by one byte per pixel, then
/// fill backwards (pixel i reads src `3i..3i+3` and writes dst `4i..4i+4`;
/// larger-i writes only ever touch bytes ≥ `4i+4`, above every remaining
/// source byte). Avoids allocating a second full-size buffer, halving the
/// decode transient for JPEG covers.
fn expand_rgb8_to_rgba8_in_place(buf: &mut Vec<u8>, w: u32, h: u32) {
    let pixels = (w as usize) * (h as usize);
    debug_assert_eq!(buf.len(), pixels * 3);
    buf.resize(pixels * 4, 255);
    // The backwards fill is clobber-free only for i ≥ 3: write `4i+k` hits
    // read `3i+j` whenever `i + k == j`, possible while `i + 2 ≤ 2`. Park the
    // first three pixels and write them from the saved copy afterwards. Alpha
    // is written explicitly for every pixel — most alpha bytes sit inside the
    // original Rgb8 region, not the 255-filled tail.
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

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ManagedImageKey {
    Album(i64),
    Track(i64),
    TrackFile(PathBuf),
    /// Online album art fetched over HTTP (KuGou / NetEase track cover URL).
    #[cfg(feature = "online_sources")]
    HttpCover(SharedString),
    /// Same art, but the fetch tries the provider's large-display variant of
    /// the URL first (KuGou `stdmusic/480`, NetEase `?param=1024y1024`) with
    /// a fallback to the original thumbnail — for the immersive backdrop and
    /// label, where a 256px thumbnail stretched across the window reads as
    /// mush. See `online_sources::cover_art`.
    #[cfg(feature = "online_sources")]
    HttpCoverLarge(SharedString),
}

/// Upper bound on decoded `RenderImage`s kept alive across elements.
///
/// Scrolling an online list re-creates `ManagedImage` rows; without this cache
/// each one re-decodes the same URL into a fresh `RenderImage` and re-uploads
/// a new atlas texture. Capping the retained set keeps that churn from adding
/// up, and re-painting a recently seen cover reuses the exact same
/// `RenderImage`/atlas slot instead. Evictions queue their atlas tiles for
/// reclamation, and evicted covers re-decode from the disk cache on return.
///
/// The cap is the *live tile* budget, and each live tile measures a flat
/// ~2.3 MB of driver-side commit regardless of its pixel size (2026-09-19
/// soak: 192 px and 256 px tiles cost the same, the [mem] probe's
/// `non_heap_mb` tracks the cache one-to-one). 128 entries ratcheted the
/// driver commit; 64 still held ~150 MB above the idle floor; 32 keeps the
/// recent-cover reuse window (≈ 6 grid rows) at half that.
const RENDER_CACHE_MAX: usize = 32;

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

/// Locks `RENDER_CACHE`, initializing it on first use.
fn lock_render_cache() -> MutexGuard<'static, RenderCache> {
    RENDER_CACHE
        .get_or_init(|| {
            Mutex::new(RenderCache {
                cache: FxHashMap::default(),
                usage: VecDeque::new(),
                bytes: 0,
            })
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Locks `RENDER_CACHE` without initializing it: `None` until the first
/// [`lock_render_cache`] call has created it.
fn try_lock_render_cache() -> Option<MutexGuard<'static, RenderCache>> {
    RENDER_CACHE.get().map(|cache| {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    })
}

/// Estimated RGBA footprint in bytes of a decoded cover.
pub(crate) fn image_bytes(image: &RenderImage) -> u64 {
    let size = image.size(0);
    (size.width.0 as u64) * (size.height.0 as u64) * 4
}

/// Live foot print of the decoded-cover LRU, in MiB. Reported by the [mem]
/// periodic probe.
pub fn render_cache_mb() -> u64 {
    let Some(cache) = try_lock_render_cache() else {
        return 0;
    };
    cache.bytes / (1024 * 1024)
}

/// Entry count of the decoded-cover LRU. Reported next to `render_cache_mb`
/// so the [mem] probe can tell a growing entry set (per-track cache churn)
/// from a growing per-entry size.
pub fn render_cache_entries() -> usize {
    let Some(cache) = try_lock_render_cache() else {
        return 0;
    };
    cache.cache.len()
}

/// Covers whose atlas tiles still need dropping, tagged with their cache key.
/// This queue is the SINGLE reclaim funnel: cache evictions/replacements and
/// element `on_release` both push here, and `drain_pending_tile_drops` (event
/// loop only, never mid-frame) is the only place that calls `drop_image`.
/// Eviction runs on the RUNTIME where no `App` exists, which is why the drop
/// itself has to be deferred. Without any of this, an evicted cover's atlas
/// page stays pinned forever once its owning elements have unmounted.
static PENDING_TILE_DROPS: OnceLock<Mutex<Vec<PendingTileDrop>>> = OnceLock::new();

struct PendingTileDrop {
    key: RenderCacheKey,
    image: Arc<RenderImage>,
    /// Earliest drain that may reclaim this image. Reclaiming is only safe
    /// once nothing can still paint the image — including the *replayed*
    /// paint ops of a cache-skipped view, which hold no Arc at all. A view
    /// skipped at eviction time replays its last scene until it re-renders,
    /// so a fresh eviction must age out before its tiles may be freed
    /// (2026-09-16 funnel audit; see `RECLAIM_DELAY`).
    due: Instant,
}

/// Minimum age of a queued image before `drain_pending_tile_drops` may free
/// its atlas tiles. Bounds the "skipped-view replay" hazard: a view that gpui
/// skipped (no re-render) replays the sprites of its last paint without
/// holding the image, so freeing the image's page while that view is still
/// skipped would dangle the replayed sprite (`texture()` unwrap, 2026-09-08
/// crash class). 60s covers every realistic static-view lifetime while
/// keeping reclaim latency irrelevant for memory (pages are transient, and
/// the probe curve showed the floor is dominated by allocator/GPU residency,
/// not reclaim latency).
const RECLAIM_DELAY: Duration = Duration::from_secs(60);

fn queue_tile_drop(key: RenderCacheKey, image: Arc<RenderImage>) {
    TILE_DROP_STATS.pushed.fetch_add(1, Ordering::Relaxed);
    PENDING_TILE_DROPS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(PendingTileDrop {
            key,
            image,
            due: Instant::now() + RECLAIM_DELAY,
        });
}

/// Cumulative reclaim-funnel counters for the `[mem]` periodic probe, so the
/// log curve shows why resident memory sits where it does: how much tile
/// traffic went through the funnel, what each drain decided, and — most
/// importantly — how many atlas tiles leaked through a panicked drop (each
/// permanently pins a ~4 MB shared GPU page, see `patches/UPSTREAM_NOTES.md`).
pub(crate) struct TileDropStats {
    pushed: AtomicU64,
    reclaimed: AtomicU64,
    kept_by_cache: AtomicU64,
    kept_by_holders: AtomicU64,
    leaked_tiles: AtomicU64,
}

static TILE_DROP_STATS: TileDropStats = TileDropStats {
    pushed: AtomicU64::new(0),
    reclaimed: AtomicU64::new(0),
    kept_by_cache: AtomicU64::new(0),
    kept_by_holders: AtomicU64::new(0),
    leaked_tiles: AtomicU64::new(0),
};

/// Snapshot for the `[mem]` probe: (pending, pushed, reclaimed,
/// kept_by_cache, kept_by_holders, leaked_tiles).
// Only the non-test probe consumes this; test builds would flag it dead.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn tile_drop_stats() -> (u64, u64, u64, u64, u64, u64) {
    let pending = PENDING_TILE_DROPS
        .get()
        .map(|queue| {
            queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len() as u64
        })
        .unwrap_or(0);
    let s = &TILE_DROP_STATS;
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    (
        pending,
        load(&s.pushed),
        load(&s.reclaimed),
        load(&s.kept_by_cache),
        load(&s.kept_by_holders),
        load(&s.leaked_tiles),
    )
}

/// Records one leaked atlas tile; called by the catch site in `ui::util`.
pub(crate) fn note_tile_drop_panic() {
    TILE_DROP_STATS.leaked_tiles.fetch_add(1, Ordering::Relaxed);
}

/// 把不属于 RENDER_CACHE 的图（如 `MelioraImageCache` 的驱逐/释放、登录二
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

/// Whether the cache still owns `image` under `key` (same allocation). Used
/// by the drain to leave tiles alone that a live cache entry still serves.
fn render_cache_holds(key: &RenderCacheKey, image: &Arc<RenderImage>) -> bool {
    let Some(cache) = try_lock_render_cache() else {
        return false;
    };
    cache
        .cache
        .get(key)
        .is_some_and(|(current, _)| Arc::ptr_eq(current, image))
}

/// Outcome of planning one drain batch. `reclaim` is the set of images whose
/// atlas tiles may be dropped exactly once; the counters exist so tests can
/// assert why an image was kept alive.
#[derive(Default)]
pub(crate) struct ReclaimPlan {
    pub reclaim: Vec<Arc<RenderImage>>,
    /// Images the render cache still serves under a pushed key: live tiles,
    /// never reclaimed here (the cache pushes them itself on eviction).
    pub kept_by_cache: usize,
    /// Images with at least one strong reference outside this batch (a live
    /// element state, a continuation-local clone, ...): they may paint again,
    /// so their tiles must stay.
    pub kept_by_holders: usize,
}

/// Pure core of [`drain_pending_tile_drops`]: groups the batch by image
/// identity and decides which images' tiles can be reclaimed.
///
/// Invariant the decision rests on: grouping consumes every Arc the batch
/// carried and keeps exactly ONE inspection Arc per unique image, so after
/// grouping `Arc::strong_count == 1 + (every holder outside the batch)`.
/// Reclaim is therefore correct iff `strong_count == 1` and the cache does
/// not serve the image: nobody outside this function can ever paint it again,
/// so its tile is dropped exactly once.
///
/// (The previous arithmetic `strong_count - batch_copies` was wrong twice
/// over: with 2+ pushes of the same image and no external holders it
/// underflowed usize in release and skipped the reclaim forever — a permanent
/// atlas-tile leak on the common evict+unmount path — and with 2 pushes plus
/// one live holder it computed 0 and freed a tile an element still painted.)
///
/// `cache_holds` reports whether the render cache still owns `image` under
/// `key` (same allocation). Consulted for every key the image was pushed
/// with, not just the first, so a multi-key push cannot slip past it.
fn plan_tile_reclaims(
    batch: Vec<(RenderCacheKey, Arc<RenderImage>)>,
    mut cache_holds: impl FnMut(&RenderCacheKey, &Arc<RenderImage>) -> bool,
) -> ReclaimPlan {
    // Insertion-ordered dedup: one inspection Arc per unique image. Duplicate
    // pushes (same image queued by several unmounting elements and by the
    // cache) collapse here; their extra Arcs are dropped by this loop, which
    // is exactly what makes the `strong_count == 1` test below valid.
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
    let now = Instant::now();
    let (ready, young) = {
        let mut queue = queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (ready, young) = partition_ready(queue.drain(..).collect(), now);
        (ready, young)
    };

    if !ready.is_empty() {
        let batch: Vec<(RenderCacheKey, Arc<RenderImage>)> = ready
            .into_iter()
            .map(|entry| (entry.key, entry.image))
            .collect();
        let plan = plan_tile_reclaims(batch, |key, image| render_cache_holds(key, image));
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
    // 都会执行，一张刚入队的图（60s 年龄门未到）若在这里被丢弃，它的
    // 最后一份 Arc 就地消失，瓦片永远无人回收——pushed 持续增长而其余
    // 计数恒为零的 [mem] 曲线（2026-09-26 两个会话）正是这条路径。
    // Re-queue the not-yet-due entries only after the reclaim decision, so a
    // young entry's Arc never dilutes the strong_count test of a ready one.
    if !young.is_empty() {
        queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(young);
    }
}

/// Splits the queue into entries whose tiles may be reclaimed now and entries
/// that must age further. Ready entries leave the queue (releasing their Arc
/// so the drain's `strong_count == 1` test stays valid); young entries are
/// pushed back by the caller with their ORIGINAL due instant — resetting it
/// would either leak the tile forever (delay grows unboundedly) or free it
/// while a skipped view still replays it (the 2026-09-08 crash class).
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

    let mut cache = lock_render_cache();

    // Replace an existing entry (possibly decoded by a concurrent element) or
    // evict the least-recently-used slot once the budget is full. Both paths
    // queue the dropped image for atlas-tile reclamation — this is the ONLY
    // reclamation path for cached tiles (single owner), so each image's tile
    // is dropped exactly once.
    if let Some((old_image, old_bytes)) = cache.cache.insert(cache_key.clone(), (image, new_bytes))
    {
        cache.bytes = cache.bytes.saturating_sub(old_bytes) + new_bytes;
        queue_tile_drop(cache_key, old_image);
    } else {
        cache.bytes += new_bytes;
        cache.usage.push_back(cache_key);
        if cache.usage.len() > RENDER_CACHE_MAX {
            if let Some(oldest) = cache.usage.pop_front()
                && let Some((image, bytes)) = cache.cache.remove(&oldest)
            {
                cache.bytes = cache.bytes.saturating_sub(bytes);
                queue_tile_drop(oldest, image);
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
    /// Also consumed by the immersive view's accent-color extractor.
    pub(crate) async fn retrieve(
        &self,
        pool: SqlitePool,
        thumb_size: u32,
        use_cache: bool,
    ) -> anyhow::Result<Option<Arc<RenderImage>>> {
        let cacheable = use_cache && thumb_size > 0;
        if cacheable && let Some(image) = render_cache_lookup(self, thumb_size) {
            return Ok(Some(image));
        }

        let decoded = self.retrieve_uncached(pool, thumb_size).await?;
        if cacheable && let Some(image) = &decoded {
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
                        // Thumbnail (never upscales) runs on the DynamicImage
                        // BEFORE conversion, so neither the thumbnail nor the
                        // RGBA expansion ever sees full-size buffers.
                        Ok(Some(decode_rgba_to_render_image(dynamic_to_rgba_scaled(
                            dyn_image, thumb_size,
                        ))?))
                    })
                    .await?
            }
            #[cfg(feature = "online_sources")]
            ManagedImageKey::HttpCover(url) => {
                let url = url.to_string();
                let bytes = crate::media::http_source::http_cover_bytes_cached(&url).await?;
                let Some(bytes) = bytes else { return Ok(None) };
                let image = {
                    let _permit = DECODE_PERMITS
                        .acquire()
                        .await
                        .expect("semaphore is never closed");
                    crate::RUNTIME
                        .spawn_blocking(move || {
                            decode_to_render_image_scaled(&bytes, thumb_size).map(Some)
                        })
                        .await??
                };
                Ok(image)
            }
            #[cfg(feature = "online_sources")]
            ManagedImageKey::HttpCoverLarge(url) => {
                let bytes =
                    crate::online_sources::cover_art::fetch_display_cover_bytes(&url).await?;
                let Some(bytes) = bytes else { return Ok(None) };
                let image = {
                    let _permit = DECODE_PERMITS
                        .acquire()
                        .await
                        .expect("semaphore is never closed");
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

                let image = {
                    let _permit = DECODE_PERMITS
                        .acquire()
                        .await
                        .expect("semaphore is never closed");
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
    /// 本元素的 (来源, 尺寸) 缓存键——on_release 把图推进回收漏斗时需要
    /// 它来做"缓存是否仍持有"判定。
    cache_key: RenderCacheKey,
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
    /// Whether decodes may live in `RENDER_CACHE` (default). Images painted
    /// exactly once — the now-playing bar's per-track cover — must opt out:
    /// each track's unique URL would otherwise add a fresh cache entry and
    /// atlas tile whose reclamation waits on the render-cache LRU, the measured
    /// per-track commit ratchet of the 2026-09-14 soak.
    cache: bool,
    /// 3×3 median denoise plus small-source Lanczos upscale after decode,
    /// for the full-screen immersive backdrop: cover art is JPEG-compressed
    /// (its 8×8 block steps read as a grid of dark boxes once stretched) and
    /// online thumbnails are far smaller than the window (GPU bilinear
    /// magnification reads mushy). The median melts the block edges, the
    /// Lanczos resample hands the GPU a near-1:1 texture.
    enhance: bool,
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

    /// Skips `RENDER_CACHE` for this image: decoded once for this element
    /// instance, its atlas tile is reclaimed through the funnel as soon as
    /// the element unmounts instead of pinning the page until LRU eviction.
    pub fn uncached(mut self) -> Self {
        self.cache = false;
        self
    }

    /// Runs the backdrop enhancement pass after decoding: median denoise
    /// plus, for sources smaller than [`ENHANCE_UPSCALE_MIN_SOURCE_PX`], a
    /// Lanczos resample up to [`ENHANCE_UPSCALE_TARGET_PX`] on the long side.
    pub fn enhanced(mut self) -> Self {
        self.enhance = true;
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
        let use_cache = self.cache;
        let enhance = self.enhance;
        let entity = window.use_keyed_state("state", cx, move |_window, cx| {
            let pool = cx.global::<Pool>().0.clone();
            let bridge: ImageBridge = Arc::new(OnceLock::new());
            let bridge_clone = bridge.clone();
            let task_key = key.clone();

            let handle = crate::RUNTIME.spawn(async move {
                let result = task_key.retrieve(pool, thumb_size, use_cache).await;
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
                        // The enhancement pass runs before the image reaches
                        // the state/atlas: one off-thread sweep for the
                        // backdrop (any source size — block steps show at
                        // 1:1 too, small sources magnify mushy).
                        let image = if enhance {
                            enhance_render_image(&image).unwrap_or(image)
                        } else {
                            image
                        };
                        if this
                            .update(cx, |this: &mut ManagedImageState, cx| {
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

            cx.on_release(|this: &mut ManagedImageState, _cx| {
                // 先中止未完成的取回任务。瓦片回收统一走回收漏斗：on_release
                // 只负责把本状态持有的两处引用（image 字段 + bridge 内的就绪
                // 图）推进队列，由 drain 按"批外持有者 == 0"判定后恰好释放
                // 一次。直接 drop 会与其他路径的释放交错出二次释放（etagere
                // 代际断言，2026-09-12/13 闪退），无条件跳过则会泄漏被存活
                // 元素 paint 复活的瓦片。
                if let Some(task) = this.task.take() {
                    task.abort();
                }
                // image 字段与 bridge 里的就绪图是互斥的两个持有点（任务
                // 完成时先 set bridge、continuation 再写回字段）。state 以
                // 普通 Drop 消失时 bridge 里的 Arc 不会回收瓦片，所以两处
                // 都要推进回收漏斗。
                let bridged = this.bridge.take().and_then(|bridge| {
                    // state 是 bridge Arc 的最后持有者（任务闭包已完成或被
                    // 中止），try_unwrap 拿到所有权后才能 take 出内部的图。
                    Arc::try_unwrap(bridge)
                        .ok()
                        .and_then(|mut once| once.take())
                        .flatten()
                });
                for image in this.image.take().into_iter().chain(bridged) {
                    queue_tile_drop(this.cache_key.clone(), image);
                }
            })
            .detach();

            ManagedImageState {
                image: None,
                bridge: Some(bridge),
                task: Some(abort),
                cache_key: RenderCacheKey {
                    key: key.clone(),
                    thumb: thumb_size,
                },
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
        cache: true,
        enhance: false,
    }
}

/// Sources below this long side get the Lanczos upscale; at or above it the
/// GPU magnification is small enough to look fine.
const ENHANCE_UPSCALE_MIN_SOURCE_PX: u32 = 1000;
/// Long side of the resampled backdrop — matches a 1920-wide fullscreen.
const ENHANCE_UPSCALE_TARGET_PX: u32 = 1920;

/// Backdrop enhancement for a decoded image, returning a fresh
/// `RenderImage`: 3×3 median denoise (melts JPEG block steps at any source
/// size), then a Lanczos resample to near display size for small sources.
/// RGB channels only, alpha untouched.
fn enhance_render_image(image: &Arc<RenderImage>) -> Option<Arc<RenderImage>> {
    let bytes = image.as_bytes(0)?;
    let size = image.size(0);
    let (width, height) = (u32::from(size.width), u32::from(size.height));
    if width == 0 || height == 0 {
        return Some(Arc::clone(image));
    }
    let mut rgba = image::RgbaImage::from_raw(width, height, bytes.to_vec())?;
    denoise_rgba(&mut rgba);
    let long_side = rgba.width().max(rgba.height());
    if long_side < ENHANCE_UPSCALE_MIN_SOURCE_PX {
        let scale = f64::from(ENHANCE_UPSCALE_TARGET_PX) / f64::from(long_side);
        let new_width = ((f64::from(rgba.width()) * scale) as u32).max(1);
        let new_height = ((f64::from(rgba.height()) * scale) as u32).max(1);
        rgba = image::imageops::resize(
            &rgba,
            new_width,
            new_height,
            image::imageops::FilterType::Lanczos3,
        );
    }
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(rgba)])))
}

/// In-place 3×3 median filter over RGB: each output channel is the median of
/// its 3×3 neighborhood (edges clamped to the nearest pixel). A median melts
/// JPEG's 8×8 block steps and other flat-region noise while keeping genuine
/// edges intact — the unsharp pass this replaces did the opposite (it
/// amplified exactly those block edges into the "黑框框" grid).
fn denoise_rgba(image: &mut image::RgbaImage) {
    let (width, height) = image.dimensions();
    let source = image.clone();
    let sample = |x: i64, y: i64| -> [u8; 4] {
        let x = x.clamp(0, width as i64 - 1) as u32;
        let y = y.clamp(0, height as i64 - 1) as u32;
        source.get_pixel(x, y).0
    };
    for y in 0..height {
        for x in 0..width {
            let mut out = [0u8; 3];
            for channel in 0..3 {
                let mut window = [0u8; 9];
                let mut n = 0;
                for dy in -1i64..=1 {
                    for dx in -1i64..=1 {
                        window[n] = sample(x as i64 + dx, y as i64 + dy)[channel];
                        n += 1;
                    }
                }
                window.sort_unstable();
                out[channel] = window[4];
            }
            let pixel = image.get_pixel_mut(x, y);
            let orig = *pixel;
            pixel.0 = [out[0], out[1], out[2], orig[3]];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, RgbaImage};
    use smallvec::smallvec;
    use std::collections::HashSet;

    #[test]
    fn median_removes_salt_noise_and_keeps_flat_areas() {
        // 3×3: a bright outlier pixel amid dark ones — block-step-like noise
        // the median must erase.
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

        // A flat region stays byte-identical.
        let mut flat = RgbaImage::from_fn(3, 3, |_, _| image::Rgba([128u8, 128, 128, 255]));
        denoise_rgba(&mut flat);
        assert_eq!(*flat.get_pixel(1, 1), image::Rgba([128u8, 128, 128, 255]));
    }

    #[test]
    fn median_keeps_true_edges_in_place() {
        // A hard two-level edge (like a building silhouette against sky) must
        // survive: pixels away from the boundary keep their original values.
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
    fn enhance_render_image_medians_large_sources_without_upscale() {
        // A 1600px source is at/above the upscale floor: block steps still
        // get medianed (they show at 1:1 too), but the size stays put — the
        // GPU magnification is already small.
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            1600,
            2,
            |x, _| if x == 800 {
                image::Rgba([200u8, 200, 200, 255])
            } else {
                image::Rgba([10u8, 10, 10, 255])
            }
        ))]));
        let enhanced = enhance_render_image(&image).unwrap();
        assert!(!Arc::ptr_eq(&image, &enhanced));
        let size = enhanced.size(0);
        assert_eq!((u32::from(size.width), u32::from(size.height)), (1600, 2));
        // The isolated bright column is a 1px outlier in its window → medianed.
        let bytes = enhanced.as_bytes(0).unwrap();
        let outlier = 4 * (800 + 0 * 1600) + 0;
        assert!(bytes[outlier] < 200, "outlier should be removed");
    }

    #[test]
    fn enhance_render_image_upscales_small_sources() {
        // A 480px online thumbnail stretched across the window reads mushy;
        // the enhancement pass must resample it to the display-size target
        // before upload so the GPU scales ~1:1.
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            480,
            360,
            |x, y| image::Rgba([((x * 7 + y) % 255) as u8, 60, 90, 255])
        ))]));
        let enhanced = enhance_render_image(&image).unwrap();
        let size = enhanced.size(0);
        assert_eq!(
            u32::from(size.width),
            ENHANCE_UPSCALE_TARGET_PX,
            "long side must reach the target"
        );
        assert_eq!(u32::from(size.height), 1440, "aspect ratio preserved");
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

    /// Stand-in for `RENDER_CACHE`: maps cache keys to the Arc it serves.
    /// Holding clones here mirrors the real cache, whose entry also counts
    /// towards `Arc::strong_count`.
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

    /// Regression: two pushes of one image with no external holders used to
    /// evaluate `strong_count - batch_copies` = 1 - 2, underflowing usize in
    /// release and skipping the reclaim forever (permanent atlas-tile leak on
    /// the common evict+unmount path).
    #[test]
    fn same_batch_shared_release_reclaims_exactly_once() {
        let img = test_image();
        let a = img.clone(); // element A's reference, pushed by its on_release
        let b = img.clone(); // element B's reference, pushed by its on_release
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
        let holder_b = img.clone(); // element B still mounted

        // Drain 1: only A pushed; B can still paint the image.
        let p1 = run_plan(vec![(key(1, 256), img)], &FakeCache::default());
        assert!(p1.reclaim.is_empty());
        assert_eq!(p1.kept_by_holders, 1);

        // Drain 2: B unmounted and pushed its reference; nobody else holds it.
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

        // Element unmounts and pushes while the cache still serves the image.
        let p = run_plan(vec![(key(1, 256), element_ref)], &cache);
        assert!(p.reclaim.is_empty());
        assert_eq!(p.kept_by_cache, 1);

        // A duplicate push (second element) stays skipped while cached: the
        // cache pushes the tile itself when it eventually evicts.
        let cached = cache.get(&key(1, 256)).unwrap().clone();
        let e1 = cached.clone();
        let e2 = cached.clone();
        drop(cached);
        let p = run_plan(vec![(key(1, 256), e1), (key(1, 256), e2)], &cache);
        assert!(p.reclaim.is_empty());
        assert_eq!(p.kept_by_cache, 1);
    }

    /// The drain must age entries out by their ORIGINAL due instant: resetting
    /// the due on re-queue would either leak the tile forever (due pushed
    /// forward on every drain) or free it while a skipped view still replays
    /// it (due pulled back — the 2026-09-08 crash class).
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

    /// The in-place Rgb8→Rgba8 expansion must match `to_rgba8` exactly,
    /// including the overlap-prone first pixels (i < 3) and the last row.
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

    /// PNG covers arrive as Rgba8 and must be stolen (no second buffer), and
    /// bounded conversion must thumbnail before expanding.
    #[test]
    fn dynamic_conversion_steals_rgba_and_scales() {
        let src =
            image::RgbaImage::from_fn(64, 64, |x, y| image::Rgba([x as u8, y as u8, 42, 255]));

        // bound = 0: the exact buffer is handed over, no clone.
        let full = dynamic_to_rgba_scaled(image::DynamicImage::ImageRgba8(src.clone()), 0);
        assert_eq!(*full, *src);

        // bound = 16: downscaled, still correct dimensions.
        let scaled = dynamic_to_rgba_scaled(image::DynamicImage::ImageRgba8(src), 16);
        assert!(scaled.width() <= 16 && scaled.height() <= 16);
    }

    /// Cache replaces an entry while an element still paints the old image:
    /// the old tile must be reclaimed exactly once, only after the element is
    /// gone, and the replacement must never be reclaimed while cache-held.
    #[test]
    fn replacement_race_reclaims_old_image_only() {
        let mut cache = FakeCache::default();
        let old = test_image();
        let old_id = old.id;
        cache.insert(key(1, 256), old.clone());
        let element_ref = old.clone();
        drop(old);

        // Cache swap: old evicted (queued), fresh decode takes the slot.
        let evicted = cache.remove(&key(1, 256)).unwrap();
        assert!(Arc::ptr_eq(&evicted, &element_ref));
        let new = test_image();
        cache.insert(key(1, 256), new.clone());

        // Drain 1: element still holds the old image → skip.
        let p1 = run_plan(vec![(key(1, 256), evicted)], &cache);
        assert!(p1.reclaim.is_empty());
        assert_eq!(p1.kept_by_holders, 1);

        // Drain 2: element released and pushed → old reclaimed once, new one
        // untouched.
        let p2 = run_plan(vec![(key(1, 256), element_ref)], &cache);
        assert_eq!(p2.reclaim.len(), 1);
        assert_eq!(p2.reclaim[0].id, old_id);
        assert!(cache.holds(&key(1, 256), &new));
    }

    /// The resurrection property: a reclaim decision is made only when zero
    /// strong references exist outside the batch, so no element can ever paint
    /// the image again after its tile is freed — the tile is dropped exactly
    /// once, for every combination of holder counts and duplicate pushes.
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

    /// thumb=0 art never enters the render cache, so its tiles are judged
    /// purely by the remaining strong count.
    #[test]
    fn thumb_zero_uncached_image_reclaims() {
        let img = test_image();
        let plan = run_plan(vec![(key(3, 0), img)], &FakeCache::default());
        assert_eq!(plan.reclaim.len(), 1);
        assert_eq!(plan.kept_by_cache, 0);
    }

    /// Adversarial: one image pushed under two different keys in the same
    /// batch while the cache still serves it under one of them. The hold
    /// check must OR across every pushed key, not just the first — this
    /// locks the "consulted for every key" claim of `plan_tile_reclaims`.
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

    /// The full same-batch triple-push sequence: the cache's eviction push
    /// plus two shared elements' `on_release` pushes of one image. Grouping
    /// must collapse all three Arcs and reclaim exactly once.
    #[test]
    fn cache_evict_and_two_element_releases_in_one_batch_reclaim_once() {
        let mut cache = FakeCache::default();
        let img = test_image();
        cache.insert(key(9, 256), img.clone());
        let a = img.clone();
        let b = img.clone();
        drop(img);

        // The cache evicts (remove + queue) while both elements release.
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

    /// Locks the global invariant across a simulated session: every image's
    /// tile is reclaimed at most once, and every fully-released image is
    /// eventually reclaimed.
    #[test]
    fn every_tile_is_reclaimed_at_most_once_across_drains() {
        let mut cache = FakeCache::default();
        let mut dropped: HashSet<ImageId> = HashSet::new();

        let i1 = test_image();
        let i2 = test_image();
        cache.insert(key(10, 256), i1.clone());
        let h1 = i1.clone(); // element painting the cached cover
        let h2 = i2.clone(); // element painting full-size (thumb=0) art
        drop(i1);
        drop(i2);

        // Round 1: cache evicts i1 (queued) while h1 is alive; h2 unmounts.
        let evicted = cache.remove(&key(10, 256)).unwrap();
        let p1 = run_plan(vec![(key(10, 256), evicted), (key(11, 0), h2)], &cache);
        assert_eq!(p1.reclaim.len(), 1, "i2 reclaimed, i1 kept for h1");
        for img in &p1.reclaim {
            assert!(dropped.insert(img.id), "tile reclaimed twice");
        }
        drop(p1);

        // Round 2: h1 unmounts and pushes → i1 reclaimed exactly once.
        let p2 = run_plan(vec![(key(10, 256), h1)], &cache);
        assert_eq!(p2.reclaim.len(), 1);
        for img in &p2.reclaim {
            assert!(dropped.insert(img.id), "tile reclaimed twice");
        }
        drop(p2);

        assert_eq!(dropped.len(), 2, "every released image reclaimed once");
    }
}
