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
/// cache bounds what is *retained*, this bounds what is *in flight*.
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

/// Acquires a [`DECODE_PERMITS`] slot and decodes `data` into a
/// `bound`-capped `RenderImage` on the blocking pool. Shared by every
/// byte-buffer decode path (`HttpCover` / `HttpCoverLarge` / DB art) so the
/// in-flight cap and the blocking-pool handoff stay in one place.
async fn decode_bounded_with_permit(
    data: Vec<u8>,
    bound: u32,
) -> anyhow::Result<Option<Arc<RenderImage>>> {
    let _permit = DECODE_PERMITS
        .acquire()
        .await
        .expect("semaphore is never closed");
    crate::RUNTIME
        .spawn_blocking(move || decode_to_render_image_scaled(&data, bound).map(Some))
        .await?
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
/// pushes its held references into the same funnel, where the drain only
/// reclaims what nothing outside the batch can still paint — double-freeing
/// a tile trips an etagere assertion (2026-09-12 crash) and never freeing
/// one leaks it once a surviving element re-paints it after eviction.
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

/// Resident bytes of the immersive-backdrop LRU, in MiB, plus entry count.
/// Reported by the [mem] periodic probe: immersive-active sits at ~3 sets,
/// after leaving the immersive view it must fall back to 1 (the
/// `backdrop_cache_shrink` release discipline) — this field is how the next
/// cross-surface residency audit verifies that directly instead of by
/// inference.
pub fn backdrop_cache_stats() -> (usize, u64) {
    let cache = lock_backdrop_cache();
    (cache.cache.len(), cache.bytes / (1024 * 1024))
}

/// Covers whose atlas tiles still need dropping, tagged with their cache key.
/// This queue is the SINGLE reclaim funnel: cache evictions/replacements and
/// element `on_release` both push here, and `drain_pending_tile_drops` (event
/// loop only, never mid-frame) is its only drain — the one reclaim outside
/// it is the retrieve continuation's `strong_count == 1` fallback when the
/// element vanished before the image landed (see `request_layout`).
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
/// Large (≥[`LARGE_IMAGE_BYTES`], immersive-backdrop-class) images age out
/// on a much shorter gate. Their only painter is the immersive backdrop
/// element (audited 2026-09-29: every other `ManagedImage` decode is ≤512px
/// ≤1MB), and every removal of that element — promote, deactivate, crossfade
/// end — coincides with an `ImmersiveView` re-render (`cx.notify()` in the
/// same state change), after which no recorded scene can replay its sprites.
/// The short gate only has to cover the crossfade window (400ms) plus frame
/// timing, and it MUST stay above `BACKDROP_FADE`: the crossfade keeps the
/// old element mounted while it fades. The uniform 60s gate turned a burst
/// of song switches into "the background stops following the song" — the
/// pending large sets blocked new backdrop decodes for up to a minute
/// (`backdrop_decode_allowed`), which is exactly the 2026-09-29 complaint.
const RECLAIM_DELAY_LARGE: Duration = Duration::from_secs(3);

/// Reclaim age for one queued image: large backdrop-class images use the
/// short gate, everything else the conservative 60s one.
fn reclaim_delay_for(image: &RenderImage) -> Duration {
    if image_bytes(image) >= LARGE_IMAGE_BYTES {
        RECLAIM_DELAY_LARGE
    } else {
        RECLAIM_DELAY
    }
}

fn queue_tile_drop(key: RenderCacheKey, image: Arc<RenderImage>) {
    TILE_DROP_STATS.pushed.fetch_add(1, Ordering::Relaxed);
    let due = reclaim_delay_for(&image);
    PENDING_TILE_DROPS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(PendingTileDrop {
            key,
            image,
            due: Instant::now() + due,
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
    /// Fetches and decodes the artwork, memoized per [`ImageCacheMode`] so a
    /// cover that scrolls back into view reuses the same `RenderImage` instead
    /// of paying for a fresh decode + atlas upload per element instance.
    /// Backdrop mode additionally renders the sharp cover-band pass (see
    /// `backdrop_sharp_fill`; the fixed-size fallback still uses
    /// [`enhance_render_image`]) BEFORE the cache insert, so the cached image
    /// is display-ready and the element paints the exact Arc the cache
    /// holds — one decode, one enhancement, one set of pixels per artwork.
    /// `ImageCacheMode::None` serves decode-and-drop callers: one-shot images
    /// that are never painted (the accent extractor) or whose tile lifetime
    /// is bounded by their element instead.
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
                let rendered = crate::RUNTIME.spawn_blocking({
                    let decoded = Arc::clone(&decoded);
                    move || backdrop_sharp_fill(&decoded, w, h).unwrap_or_else(|| decoded.clone())
                });
                let resampled: Arc<RenderImage> = rendered.await?;
                // 每首歌背景解码一次的唯一观测点：来源小图回退（大图变体
                // 404 后阶梯取到的尺寸）或内嵌小封面在这里现形——
                // 2026-09-29 "背景还是糊" 投诉的取证通道。
                let size = resampled.size(0);
                let src_size = decoded.size(0);
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
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            let queue = queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    /// Decode-cache destination (see [`ImageCacheMode`]). Images painted
    /// exactly once — the now-playing bar's per-track cover — must opt out:
    /// each track's unique URL would otherwise add a fresh cache entry and
    /// atlas tile whose reclamation waits on the render-cache LRU, the measured
    /// per-track commit ratchet of the 2026-09-14 soak. The immersive backdrop
    /// uses [`ImageCacheMode::Backdrop`] (dedicated small LRU + in-retrieve
    /// enhancement) so song revisits and immersive re-entries paint instantly.
    cache: ImageCacheMode,
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
        let entity = window.use_keyed_state("state", cx, move |_window, cx| {
            let pool = cx.global::<Pool>().0.clone();
            let bridge: ImageBridge = Arc::new(OnceLock::new());
            let bridge_clone = bridge.clone();
            let task_key = key.clone();

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
        cache: ImageCacheMode::RenderCache,
    }
}

/// Long side of the resampled backdrop — matches a 1920-wide fullscreen.
/// Sources at or above this pass through untouched: they paint at ~1:1 or a
/// downscale, where their detail IS the maximum available sharpness, and any
/// filtering (including the median meant for JPEG blocks) only melts real
/// 1-2px detail — the measured reason a real-2048 backdrop read blurrier
/// than the vinyl label, which never runs enhance. Anything below is
/// median-denoised, Lanczos-resampled up to this size and sharpened, so the
/// GPU never magnifies the texture.
const ENHANCE_TARGET_PX: u32 = 1920;
/// Unsharp-mask amount applied after the upscale (see [`sharpen_rgba`]).
const ENHANCE_SHARPEN_AMOUNT: f32 = 0.6;
/// Unsharp-mask blur sigma in output pixels: with 7.5× Lanczos magnification
/// edge transitions span several pixels, so the halo radius must scale with
/// the magnification, not stay at 1px.
const ENHANCE_SHARPEN_SIGMA: f32 = 1.5;
/// Channel differences below this (0-255) pass through unsharpened, so
/// Lanczos ringing and sensor noise are not boosted into halos.
const ENHANCE_SHARPEN_THRESHOLD: f32 = 3.0;

/// 沉浸页背景目标尺寸渲染（2026-09-29 第四版·清晰裁剪带）：解码全尺寸
/// 原图 → cover 居中裁剪到目标纵横比 → 3×3 中值去噪（拆掉 JPEG 块边与
/// 椒盐噪声）→ Lanczos3 重采样到精确 `(w, h)` 设备像素 → 阈值 unsharp
/// 补回锐度。元素 `ObjectFit::Cover` 绘制时缩放系数恰为 1.0，GPU 双线性
/// 按纹素中心逐像素取样，零重采样。
///
/// 用户定案（2026-09-29 四轮反馈）：模糊方向整体否决——512+σ/20 糊成
/// 纯色斑，1024+σ/180+压暗仍是"这样糊糊的真的好吗"，最终要求**背景与
/// 圆盘小封面同等清晰**。圆盘清晰的机理是 4× 缩采样把压缩噪声平均掉；
/// 背景要追平同级观感靠两件事：
/// - **源尽量大**：酷狗阶梯加 4096 档（见 `cover_art`），存在时 4096 带
///   缩到 ~1920 设备像素约 0.47×，平均效应与圆盘同级；
/// - **去噪在前、锐化在后**：源只有 2048 时是 0.94× 近 1:1 搬运，JPEG
///   块伤会原样上屏（v2"质量差"的根因）——先中值去噪拆块边，再阈值
///   unsharp（缩小 0.5/σ1.0，放大 0.6/σ1.5，阈值 3）补锐度；顺序保证
///   锐化放大的是真边缘而非块伤。
///
/// 几何仍是 cover 居中裁剪：方形封面铺满宽屏必然裁掉上下各 ~23.5%，这
/// 是全出血背景的物理前提，清晰化不改变它。
///
/// 成本纪律（2026-09-29 "切换不丝滑" 复盘）：4096 源的全管线 ~1s+，其中
/// **中值去噪在 ≥2× 缩采样时是纯浪费**——CatmullRom 在 2× 以上每输出像
/// 素平均 ≥8 源像素，8px 的 JPEG 块被直接平均掉（4096→1920 约 0.47×，
/// 正是圆盘级观感的来源），去噪只在缩采样比 <2×（如 2048→1920 的
/// 0.94× 搬运，块伤会原样上屏）和放大路径执行。缩小用 CatmullRom（双三
/// 次，~Lanczos3 一半成本，缩采样画质等价，锐度由后面的 unsharp 统一
/// 补），放大保持 Lanczos3。本函数是数百 ms 级同步 CPU，**必须经
/// `spawn_blocking` 执行**（见 `retrieve` 的 Backdrop 分支），不得在异
/// 步运行时线程内联调用——否则音频流/封面下载与它抢 worker，切歌卡顿。
fn backdrop_sharp_fill(
    image: &Arc<RenderImage>,
    target_w: u32,
    target_h: u32,
) -> Option<Arc<RenderImage>> {
    let bytes = image.as_bytes(0)?;
    let size = image.size(0);
    let (sw, sh) = (u32::from(size.width), u32::from(size.height));
    if sw == 0 || sh == 0 || target_w == 0 || target_h == 0 {
        return None;
    }
    let rgba = image::RgbaImage::from_raw(sw, sh, bytes.to_vec())?;
    // cover: 保留能铺满目标纵横比的最大居中区域。
    let crop_w = (f64::from(sw).min(f64::from(sh) * f64::from(target_w) / f64::from(target_h)))
        .round()
        .max(1.0) as u32;
    let crop_h = (f64::from(sh).min(f64::from(sw) * f64::from(target_h) / f64::from(target_w)))
        .round()
        .max(1.0) as u32;
    let mut band =
        image::imageops::crop_imm(&rgba, (sw - crop_w) / 2, (sh - crop_h) / 2, crop_w, crop_h)
            .to_image();
    // 中值去噪只在实际需要时执行：缩采样比 <2×（块边会在输出中存活）或
    // 放大（块会被拉伸涂抹）。≥2× 时重采样的平均效应接管，跳过省一半时间。
    let band_long = crop_w.max(crop_h);
    let target_long = target_w.max(target_h);
    if band_long < 2 * target_long {
        denoise_rgba(&mut band);
    }
    let mut out = image::imageops::resize(
        &band,
        target_w,
        target_h,
        if band_long >= target_long {
            image::imageops::FilterType::CatmullRom
        } else {
            image::imageops::FilterType::Lanczos3
        },
    );
    // 阈值 unsharp 补回重采样损失的锐度：缩小轻量档（0.5/σ1.0），放大
    // 档随倍数走（0.6/σ1.5）。阈值 3 以下不动，避免把残余噪声推成 halo。
    let upscale = target_w.max(target_h) > sw.max(sh);
    if upscale {
        sharpen_rgba_with(&mut out, 0.6, ENHANCE_SHARPEN_SIGMA);
    } else {
        sharpen_rgba_with(&mut out, 0.5, 1.0);
    }
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(out)])))
}

/// Fixed-size backdrop enhancement — the [`ImageCacheMode::Backdrop(None)`]
/// fallback used when the window's device-pixel target is unknown (the exact
/// target path is [`backdrop_sharp_fill`]). Returns a fresh `RenderImage`:
/// sources already at or above [`ENHANCE_TARGET_PX`] pass through untouched;
/// smaller sources get 3×3 median denoise (melts JPEG block steps — after a
/// 7.5× upscale each block would smear across 60px), a Lanczos resample to
/// display size, and a thresholded unsharp pass to restore edge acutance.
/// RGB channels only, alpha untouched.
fn enhance_render_image(image: &Arc<RenderImage>) -> Option<Arc<RenderImage>> {
    let bytes = image.as_bytes(0)?;
    let size = image.size(0);
    let (width, height) = (u32::from(size.width), u32::from(size.height));
    if width == 0 || height == 0 {
        return Some(Arc::clone(image));
    }
    if width.max(height) >= ENHANCE_TARGET_PX {
        return Some(Arc::clone(image));
    }
    let mut rgba = image::RgbaImage::from_raw(width, height, bytes.to_vec())?;
    denoise_rgba(&mut rgba);
    let long_side = rgba.width().max(rgba.height());
    let scale = f64::from(ENHANCE_TARGET_PX) / f64::from(long_side);
    let new_width = ((f64::from(rgba.width()) * scale) as u32).max(1);
    let new_height = ((f64::from(rgba.height()) * scale) as u32).max(1);
    rgba = image::imageops::resize(
        &rgba,
        new_width,
        new_height,
        image::imageops::FilterType::Lanczos3,
    );
    sharpen_rgba(&mut rgba);
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(rgba)])))
}

/// In-place thresholded unsharp mask over RGB: `out = src + amount·(src −
/// blur(src))` where the difference exceeds the threshold. Runs AFTER the
/// median (which melts JPEG block edges) and the Lanczos upscale, so it
/// boosts genuine edges only — the un-ordered unsharp this replaces once
/// amplified block steps into the "黑框框" grid. Alpha untouched.
fn sharpen_rgba(image: &mut image::RgbaImage) {
    sharpen_rgba_with(image, ENHANCE_SHARPEN_AMOUNT, ENHANCE_SHARPEN_SIGMA);
}

/// Parameterized core of [`sharpen_rgba`]: `amount` scales the unsharp
/// response, `sigma` sets the halo radius in output pixels.
fn sharpen_rgba_with(image: &mut image::RgbaImage, amount: f32, sigma: f32) {
    let blurred = image::imageops::blur(image, sigma);
    for (src, blur) in image.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let s = src.0[c] as f32;
            let diff = s - blur.0[c] as f32;
            if diff.abs() > ENHANCE_SHARPEN_THRESHOLD {
                src.0[c] = (s + amount * diff).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
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
            for (channel, out_cell) in out.iter_mut().enumerate() {
                let mut window = [0u8; 9];
                let mut n = 0;
                for dy in -1i64..=1 {
                    for dx in -1i64..=1 {
                        window[n] = sample(x as i64 + dx, y as i64 + dy)[channel];
                        n += 1;
                    }
                }
                window.sort_unstable();
                *out_cell = window[4];
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
    use image::{Frame, GenericImage, RgbaImage};
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
    fn enhance_render_image_passes_display_size_sources_through() {
        // A real-2048 source paints at ~1:1 — its detail IS the maximum
        // available sharpness. Any filtering (including the block-edge
        // median) would melt genuine 1-2px detail and read blurrier than the
        // unenhanced vinyl label, so it must come back as the SAME image,
        // zero-copy.
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
        // A soft vertical step with cover-art-scale contrast (180 levels):
        // sharpening must increase the steepest adjacent-pixel difference
        // across the edge without touching alpha.
        let mut image = RgbaImage::from_fn(24, 8, |x, _| {
            image::Rgba([if x < 12 { 40 } else { 220 }, 7, 9, 255])
        });
        // Soften the edge to mimic a Lanczos-upscaled transition.
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
        // Alpha stays 255 everywhere.
        assert!(image.pixels().all(|p| p.0[3] == 255));
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
            ENHANCE_TARGET_PX,
            "long side must reach the target"
        );
        assert_eq!(u32::from(size.height), 1440, "aspect ratio preserved");
    }

    #[test]
    fn backdrop_sharp_fill_hits_exact_target() {
        // The element draws the cached image with ObjectFit::Cover at 1:1 GPU
        // sampling — the output must be EXACTLY the window device-pixel
        // target, for both a full-size source and a ladder-fallback thumbnail.
        for source in [2048u32, 256] {
            let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
                source,
                source,
                |x, y| { image::Rgba([((x / 8) % 255) as u8, ((y / 8) % 255) as u8, 90, 255]) }
            ))]));
            let out = backdrop_sharp_fill(&image, 1920, 1020).unwrap();
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
        // Thin 8px white "text strokes" on black — the class of content the
        // user wants as sharp in the backdrop as on the disc label. The sharp
        // pipeline (denoise → Lanczos → thresholded unsharp) must keep the
        // stroke edges at high contrast: no blur stage may melt them away.
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            1024,
            1024,
            |x, y| {
                let on = (x / 8) % 2 == 0 && (400..440).contains(&y);
                image::Rgba([if on { 255 } else { 0 }, 0, 0, 255])
            }
        ))]));
        let out = backdrop_sharp_fill(&image, 1920, 1020).unwrap();
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

    /// Backdrop-class (≥4MB) images age out on the short gate so a burst of
    /// song switches cannot freeze the background for 60s; covers keep the
    /// conservative 60s gate. The short gate must stay above the immersive
    /// crossfade window, which keeps the old element (and its tiles) painting
    /// while it fades out.
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

    /// Crop geometry pinning: denoise/resample happen AFTER the centered
    /// cover-crop, so content outside the crop band must never bleed into the
    /// output — the band is still the geometric cover crop.
    #[test]
    fn backdrop_sharp_fill_matches_target_and_center() {
        // 2048x2048 source: top fifth red, everything else flat green. The
        // centered cover-crop for a 1920x1028 target starts at source row
        // (2048 - 1097) / 2 = 475, below the red band (Lanczos ringing
        // reaches ~12 source rows), so every output pixel must be green.
        let src = RgbaImage::from_fn(2048, 2048, |_, y| {
            if y < 400 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 255, 0, 255])
            }
        });
        let source = Arc::new(RenderImage::new(smallvec![Frame::new(src)]));

        let out = backdrop_sharp_fill(&source, 1920, 1028).expect("render succeeds");
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

        // Degenerate targets fall back to None (caller keeps the original).
        assert!(backdrop_sharp_fill(&source, 0, 100).is_none());
        assert!(backdrop_sharp_fill(&source, 100, 0).is_none());
    }

    /// Small sources (ladder fallback) must still render to the exact target
    /// (GPU then samples 1:1 instead of magnifying a tiny texture).
    #[test]
    fn backdrop_sharp_fill_upscales_small_sources() {
        let source = Arc::new(RenderImage::new(smallvec![Frame::new(RgbaImage::from_fn(
            480,
            360,
            |x, y| { image::Rgba([((x * 7 + y) % 255) as u8, 60, 90, 255]) }
        ))]));
        let out = backdrop_sharp_fill(&source, 1920, 1440).expect("render succeeds");
        let size = out.size(0);
        assert_eq!(u32::from(size.width), 1920);
        assert_eq!(u32::from(size.height), 1440);
    }
}
