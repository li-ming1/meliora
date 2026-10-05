use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, Entity, Render, RenderImage, Rgba};
use rustc_hash::FxHashMap;
use tracing::debug;

use crate::{
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    ui::{
        components::icons::{VOLUME, VOLUME_OFF},
        models::PlaybackInfo,
    },
};

/// Rows this far outside the visible window stay cached. Scrolling back
/// within the band reuses the existing row entities instead of re-running
/// their DB/stat construction pipeline on the UI thread; the band must stay
/// wider than any viewport so visible rows are never pruned.
pub const VIEW_KEEP_AROUND: usize = 128;

pub fn prune_views<T>(
    views_model: &Entity<FxHashMap<usize, Entity<T>>>,
    render_counter: &Entity<usize>,
    current: usize,
    cx: &mut App,
) -> bool
where
    T: Render,
{
    let last = *render_counter.read(cx);
    let mut to_remove: Vec<usize> = Vec::new();
    let mut did_remove = false;

    // Prune views outside the union of the previous and current windows
    // plus the keep-around band. Covers both scroll directions: upward
    // (current < last) behaves exactly as before, and downward scrolling
    // now also prunes far-above rows instead of accumulating the whole
    // table's row views on a long one-way scroll. Don't prune the first
    // view so this still works with uniform_list.
    let lower = last.min(current).saturating_sub(VIEW_KEEP_AROUND);
    let upper = last.max(current) + 1 + VIEW_KEEP_AROUND;
    for idx in views_model.read(cx).keys() {
        if (*idx < lower || *idx >= upper) && *idx != 0_usize {
            to_remove.push(*idx);
        }
    }

    if !to_remove.is_empty() {
        views_model.update(cx, |m, _| {
            for idx in to_remove {
                debug!("Removing view at index: {}", idx);
                did_remove |= m.remove(&idx).is_some();
            }
        });
    }

    // update the render counter
    render_counter.update(cx, |m, _| {
        *m = current;
    });

    did_remove
}

pub fn create_or_retrieve_view<K: Copy + Eq + std::hash::Hash + 'static, T>(
    views_model: &Entity<FxHashMap<K, Entity<T>>>,
    key: K,
    creation_fn: impl FnOnce(&mut App) -> Entity<T>,
    cx: &mut App,
) -> Entity<T>
where
    T: Render,
{
    let view = views_model.read(cx).get(&key).cloned();
    match view {
        Some(view) => view,
        None => {
            let view = creation_fn(cx);
            views_model.update(cx, |m, _| {
                m.insert(key, view.clone());
            });
            view
        }
    }
}

/// Drops the atlas tiles for a batch of images whose owning elements are gone.
/// Unlike the old `drop_image_from_app` this tolerates windows disappearing
/// mid-reclaim (skips instead of panicking), which matters when draining the
/// render cache's eviction queue: the cache outlives any single window.
///
/// There is no degradation path for a panicked drop: the build sets
/// `panic = "abort"` (Cargo.toml), so a panic inside `drop_image` — e.g.
/// etagere's stale-tile generation assertion after the atlas recycles a page —
/// terminates the process outright; nothing can be caught or leaked.
pub fn reclaim_images_from_app(cx: &mut App, images: Vec<Arc<RenderImage>>) {
    if images.is_empty() {
        return;
    }
    cx.defer(move |cx| {
        for image in images {
            for window in cx.windows() {
                let image = image.clone();
                let _ = window.update(cx, move |_, window, _| {
                    let _ = window.drop_image(image);
                });
            }
        }
    });
}

pub fn reveal_path_for_file_manager(path: &Path, cx: &mut App) {
    #[cfg(windows)]
    {
        // Windows quirk: paths can arrive with a `\\?\` extended-length prefix,
        // which must be stripped before handing them to the file manager.
        let path_for_reveal = match path.to_string_lossy().strip_prefix("\\\\?\\") {
            Some(stripped) => PathBuf::from(stripped),
            None => path.to_path_buf(),
        };

        cx.reveal_path(path_for_reveal.as_path());
    }

    #[cfg(not(windows))]
    {
        cx.reveal_path(path);
    }
}

/// Splits a second count into (hours, minutes, seconds); negative inputs
/// clamp to zero.
fn split_duration(secs: i64) -> (i64, i64, i64) {
    let secs = secs.max(0);
    (secs / 3_600, (secs % 3_600) / 60, secs % 60)
}

/// 紧凑时长格式：不足 1 小时为 "m:ss"（如 "2:22"、"0:45"），达到 1 小时为 "h:mm:ss"（如 "1:02:03"）。
/// 直接复用 [`format_duration`]，保证全 app（播放队列等）时长显示一致。
pub fn format_duration_compact(secs: i64) -> String {
    format_duration(secs, false)
}

/// 时长格式：达到 1 小时为 "h:mm:ss"；不足 1 小时为 "m:ss"，`pad_minutes` 为
/// true 时分钟补零成 "mm:ss"。
pub fn format_duration(secs: i64, pad_minutes: bool) -> String {
    let (hours, minutes, seconds) = split_duration(secs);

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else if pad_minutes {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Shared scrubber seek gate: seeks to `fraction` of the current track's
/// `duration_ms` (converted to seconds) only while a track is actually
/// loaded (`duration_ms > 0`) and playback is not stopped. Both the play bar
/// scrubber (`controls::Scrubber`) and the immersive player card
/// (`immersive::ImmersiveProgress`) funnel their slider `on_change` handlers
/// through here. Interaction-time only — never on the render path.
pub fn seek_to_fraction(fraction: f64, duration_ms: u64, cx: &mut App) {
    if duration_ms == 0 {
        return;
    }
    if *cx.global::<PlaybackInfo>().playback_state.read(cx) == PlaybackState::Stopped {
        return;
    }
    cx.global::<PlaybackInterface>()
        .seek(fraction * duration_ms as f64 / 1_000.0);
}

/// Mute-toggle decision shared by the play bar's secondary controls and the
/// immersive player card: `volume <= 0.0` counts as muted; toggling a muted
/// button restores `prev_volume`, an audible one mutes (volume 0.0). Returns
/// `(target_volume, icon_path)`; callers keep their own chrome (tooltips,
/// sizes, hover styles) and only consume the decision. Pure, allocation-free.
pub fn volume_toggle_target(volume: f64, prev_volume: f64) -> (f64, &'static str) {
    if volume <= 0.0 {
        (prev_volume, VOLUME_OFF)
    } else {
        (0.0, VOLUME)
    }
}

/// Whole-second cache for formatted time labels. The labels only change once
/// per second while the surrounding views re-render at ~30Hz on position
/// ticks, so the formatted label set is kept keyed by the whole-second
/// `(position_secs, duration_secs)` pair and reused verbatim until the key
/// moves — invalidation is exactly the second boundary. Generic over the
/// label tuple: the play bar scrubber caches three labels (position /
/// duration / "-remaining"), the immersive player card two (elapsed / total).
/// The `T: Clone` on the hit path is `SharedString` refcount bumps, not
/// allocations (GPUI hot-path discipline).
pub struct CachedTimeText<T> {
    position_secs: u64,
    duration_secs: u64,
    labels: T,
}

impl<T: Clone + Default> CachedTimeText<T> {
    /// Unseeded cache: the `u64::MAX` key never matches a real second count,
    /// forcing the first `resolve` to build.
    pub fn unseeded() -> Self {
        Self {
            position_secs: u64::MAX,
            duration_secs: u64::MAX,
            labels: T::default(),
        }
    }
}

impl<T: Clone> CachedTimeText<T> {
    /// Returns the cached labels while the whole-second key is unchanged,
    /// otherwise rebuilds via `build(position_secs, duration_secs)` and
    /// stores the result. `build` runs at most once per second boundary.
    pub fn resolve(
        &mut self,
        position_secs: u64,
        duration_secs: u64,
        build: impl FnOnce(u64, u64) -> T,
    ) -> T {
        if self.position_secs == position_secs && self.duration_secs == duration_secs {
            self.labels.clone()
        } else {
            let labels = build(position_secs, duration_secs);
            self.position_secs = position_secs;
            self.duration_secs = duration_secs;
            self.labels = labels.clone();
            labels
        }
    }
}

/// 饱和度加权的封面主色提取（BGRA `RenderImage`）：沉浸页与歌词面板共用的
/// 每首歌主题色来源。极暗的胜出色会被提亮，保证在暗色底上仍然可读。
pub(crate) fn extract_accent(image: &RenderImage) -> Option<Rgba> {
    let bytes = image.as_bytes(0)?;
    if bytes.is_empty() {
        return None;
    }
    dominant_accent_bgra(bytes)
}

/// 分桶主色核心，作用于原始 BGRA 字节（每像素 4 字节）。12 位直方图
/// （每通道 4 位）；权重 = 饱和度 + 一个小底数，灰白封面也能产出可用色调。
/// 每隔 3 个像素采样一次 —— 512² 解码约 8.7 万次、256² 约 2.2 万次采样，
/// 在 UI 线程之外执行。
fn dominant_accent_bgra(bytes: &[u8]) -> Option<Rgba> {
    const PIXEL_STRIDE: usize = 3;
    let mut buckets: FxHashMap<u16, (u64, u64, u64, u64)> = FxHashMap::default();
    let mut sampled = 0usize;
    for pixel in bytes.as_chunks::<4>().0.iter().step_by(PIXEL_STRIDE) {
        let (b, g, r, a) = (
            pixel[0] as u32,
            pixel[1] as u32,
            pixel[2] as u32,
            pixel[3] as u32,
        );
        if a < 128 {
            continue;
        }
        sampled += 1;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let saturation = (max - min)
            .checked_mul(255)
            .and_then(|v| v.checked_div(max))
            .unwrap_or(0);
        let weight = (saturation + 16) as u64;
        let key = (((r >> 4) as u16) << 8) | (((g >> 4) as u16) << 4) | ((b >> 4) as u16);
        let entry = buckets.entry(key).or_insert((0, 0, 0, 0));
        entry.0 += r as u64 * weight;
        entry.1 += g as u64 * weight;
        entry.2 += b as u64 * weight;
        entry.3 += weight;
    }
    if sampled == 0 {
        return None;
    }

    let (_, (sum_r, sum_g, sum_b, total)) = buckets
        .iter()
        .max_by_key(|(_, (_, _, _, weight))| *weight)?;
    if *total == 0 {
        return None;
    }
    let mut red = (*sum_r as f64 / *total as f64 / 255.0) as f32;
    let mut green = (*sum_g as f64 / *total as f64 / 255.0) as f32;
    let mut blue = (*sum_b as f64 / *total as f64 / 255.0) as f32;

    // 相对亮度低于下限的极暗主色向上提亮。
    let luminance = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    const MIN_LUMINANCE: f32 = 0.35;
    if luminance < MIN_LUMINANCE {
        let lift = MIN_LUMINANCE / luminance.max(0.02);
        red = (red * lift).min(1.0);
        green = (green * lift).min(1.0);
        blue = (blue * lift).min(1.0);
    }
    Some(Rgba::new(red, green, blue, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_time_text_reuses_labels_until_second_moves() {
        let mut cache = CachedTimeText::unseeded();
        let first = cache.resolve(1, 60, |p, d| format!("{p}/{d}"));
        assert_eq!(first, "1/60");
        // Same whole-second key: the cached set is handed back, `build` is
        // not re-run.
        let again = cache.resolve(1, 60, |_, _| String::from("rebuilt"));
        assert_eq!(again, "1/60");
        // Second boundary moved: rebuild with the new key.
        let moved = cache.resolve(2, 60, |p, d| format!("{p}/{d}"));
        assert_eq!(moved, "2/60");
        // Either half of the key moving invalidates.
        let duration_moved = cache.resolve(2, 61, |p, d| format!("{p}/{d}"));
        assert_eq!(duration_moved, "2/61");
    }

    /// 把单个 BGRA 像素重复 `count` 次铺成原始字节缓冲。
    fn repeated_pixel(pixel: [u8; 4], count: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(count * 4);
        for _ in 0..count {
            bytes.extend_from_slice(&pixel);
        }
        bytes
    }

    #[test]
    fn dominant_accent_prefers_saturated_pixels() {
        // BGRA: mostly grey pixels, a few vivid red ones — red must win.
        let grey = [128u8, 128, 128, 255];
        let red = [40u8, 30, 220, 255];
        let mut bytes = repeated_pixel(grey, 100);
        bytes.extend(repeated_pixel(red, 20));
        let accent = dominant_accent_bgra(&bytes).unwrap();
        assert!(accent.red > 0.6, "red channel should dominate: {accent:?}");
        assert!(accent.blue < 0.3 && accent.green < 0.3);
    }

    #[test]
    fn dominant_accent_ignores_transparent_pixels() {
        let bytes = repeated_pixel([0, 0, 0, 0], 32);
        assert!(dominant_accent_bgra(&bytes).is_none());
    }

    #[test]
    fn dominant_accent_lifts_dark_winners() {
        // A single near-black bucket: the lift must raise luminance.
        let dark = [10u8, 12, 16, 255];
        let bytes = repeated_pixel(dark, 64);
        let accent = dominant_accent_bgra(&bytes).unwrap();
        let luminance = 0.2126 * accent.red + 0.7152 * accent.green + 0.0722 * accent.blue;
        assert!(
            luminance >= 0.34,
            "dark accent should be lifted: {accent:?}"
        );
    }
}
