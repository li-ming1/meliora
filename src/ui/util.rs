use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, Entity, Render, RenderImage, Rgba};
use rustc_hash::FxHashMap;
use tracing::debug;

use crate::{
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    settings::SettingsGlobal,
    ui::{
        components::icons::{VOLUME, VOLUME_OFF},
        models::PlaybackInfo,
    },
};

/// 读取 `interface.reduced_motion`——全仓动画降级的唯一入口。
///
/// 所有动效（跟随滚动、歌词行强调、封面淡入、拖拽边缘滚动、toast 位移）
/// 都经这里取值，不再各自铺开六行设置链；设置随改随存，读取本身只是实体
/// 解引用，可以在 render 与帧回调里随手调用。
pub fn reduced_motion_enabled(cx: &App) -> bool {
    cx.global::<SettingsGlobal>()
        .model
        .read(cx)
        .interface
        .reduced_motion
}

/// 距可见窗口这么远之外的行仍保持缓存。在带内滚回去会复用已有的行实体，而不是在
/// UI 线程上重跑它们的 DB/stat 构建管线；这条带必须比任何视口都宽，
/// 这样可见的行永不会被修剪掉。
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

    // 修剪掉「上一窗口与当前窗口的并集再加 keep-around 带」之外的视图。
    // 两个滚动方向都能覆盖：向上（current < last）与以往行为完全一致，
    // 而向下滚动如今也会修剪远处上方的行，不再在长距离单向滚动时把整张表的
    // 行视图都累积下来。不要修剪第一个视图，这样与 uniform_list 仍能配合工作。
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

    // 更新渲染计数
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

/// 为一整批「所属元素已不存在」的图片丢掉它们的图集瓦片。与旧的
/// `drop_image_from_app` 不同，这里容忍回收过程中窗口消失（跳过而不是 panic），
/// 这在排空渲染缓存的逐出队列时很关键：缓存活得比任何单个窗口都久。
///
/// panic 的 drop 没有降级路径：构建配置设了 `panic = "abort"`（Cargo.toml），
/// 所以 `drop_image` 内部的 panic——例如 etagere 在图集回收某页之后的过期瓦片
/// 代际断言——会直接终止进程；什么都捕获不到，也泄漏不了。
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
        // Windows 的怪癖：路径可能带 `\\?\` 长路径前缀送达，交给文件管理器之前
        // 必须把它剥掉。
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

/// 把秒数拆成 (时, 分, 秒)；负输入钳位到零。
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

/// 共用跳转定位闸门（scrubber）：只有在曲目真的已加载（`duration_ms > 0`）且播放
/// 未停止时，才跳到当前曲目 `duration_ms`（换算成秒）的 `fraction` 处。播放栏
/// 的 scrubber（`controls::Scrubber`）与沉浸播放卡（`immersive::ImmersiveProgress`）
/// 都把它们的滑块 `on_change` 处理器汇入这里。只在交互时刻调用——绝不在渲染路径上。
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

/// 播放栏次级控件与沉浸播放卡共用的静音切换判定：`volume <= 0.0` 视为已静音；
/// 对已静音的按钮切换会恢复 `prev_volume`，对有声的则静音（音量 0.0）。
/// 返回 `(target_volume, icon_path)`；调用方保留自己的外观（tooltip、尺寸、悬停
/// 样式），只消费这个判定。纯函数、零分配。
pub fn volume_toggle_target(volume: f64, prev_volume: f64) -> (f64, &'static str) {
    if volume <= 0.0 {
        (prev_volume, VOLUME_OFF)
    } else {
        (0.0, VOLUME)
    }
}

/// 已格式化时间标签的整秒缓存。这些标签每秒才变一次，而周围的视图在位置 tick 上
/// 以 ~30Hz 重渲染，所以把格式化好的标签集以整秒 `(position_secs, duration_secs)`
/// 为键保存，键不动就原样复用——失效点正好就是秒边界。对标签元组泛型：播放栏
/// scrubber 缓存三个标签（位置 / 时长 / 「剩余 -」），沉浸播放卡两个（已播 / 总长）。
/// 命中路径上的 `T: Clone` 是 `SharedString` 的引用计数自增，不是分配
/// （GPUI 热路径纪律）。
pub struct CachedTimeText<T> {
    position_secs: u64,
    duration_secs: u64,
    labels: T,
}

impl<T: Clone + Default> CachedTimeText<T> {
    /// 未播种的缓存：`u64::MAX` 这个键永远匹配不上真实的秒数，从而强制首次
    /// `resolve` 去构建。
    pub fn unseeded() -> Self {
        Self {
            position_secs: u64::MAX,
            duration_secs: u64::MAX,
            labels: T::default(),
        }
    }
}

impl<T: Clone> CachedTimeText<T> {
    /// 整秒键没变时返回缓存的标签，否则经 `build(position_secs, duration_secs)`
    /// 重建并存下结果。`build` 每个秒边界至多运行一次。
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
        // 同一个整秒键：交回缓存的那一组，`build` 不会重跑。
        let again = cache.resolve(1, 60, |_, _| String::from("rebuilt"));
        assert_eq!(again, "1/60");
        // 秒边界移动了：用新键重建。
        let moved = cache.resolve(2, 60, |p, d| format!("{p}/{d}"));
        assert_eq!(moved, "2/60");
        // 键的两半中任一半移动都会使其失效。
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
        // BGRA：大多是灰像素，夹着几个鲜艳的红——红色必须胜出。
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
        // 只有一桶接近纯黑：提升必须把亮度抬起来。
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
