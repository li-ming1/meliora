use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, Entity, Render, RenderImage, Rgba};
use rustc_hash::FxHashMap;
use tracing::debug;

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

    for idx in to_remove {
        did_remove = true;
        views_model.update(cx, |m, _| {
            debug!("Removing view at index: {}", idx);
            m.remove(&idx);
        });
    }

    // update the render counter
    render_counter.update(cx, |m, _| {
        *m = current;
    });

    did_remove
}

pub fn create_or_retrieve_view<T>(
    views_model: &Entity<FxHashMap<usize, Entity<T>>>,
    key: usize,
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
    for (n, pixel) in bytes.as_chunks::<4>().0.iter().enumerate() {
        if n % PIXEL_STRIDE != 0 {
            continue;
        }
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
