//! 列表面板的一次性滚动动画：在固定时长内把滚动句柄从当前偏移缓动到目标偏移。

use gpui::{Pixels, px};
use std::time::{Duration, Instant};

use crate::ui::components::scrollbar::ScrollableHandle;

/// 低于这个像素数的位移肉眼不可见，不为它起动画。
const MIN_ANIMATION_DELTA_PX: f32 = 0.1;

/// 一段在飞动画：在跟随器时长内从 `start_scroll_top` 缓动到 `target_scroll_top`。
struct ScrollAnimation {
    start_scroll_top: Pixels,
    target_scroll_top: Pixels,
    started_at: Instant,
}

/// 在固定时长内把滚动句柄缓动到目标偏移。
pub struct SmoothScrollFollow {
    /// 从起点到目标的总动画时长。
    duration: Duration,
    /// 有动画在飞时置位；空闲为 `None`。
    animation: Option<ScrollAnimation>,
}

impl SmoothScrollFollow {
    /// 造一个空闲跟随器，动画时长为 `duration`。
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            animation: None,
        }
    }

    /// 丢弃在飞动画。
    pub fn cancel(&mut self) {
        self.animation = None;
    }

    /// 取消动画并直接跳到 `target_scroll_top`。
    pub fn jump_to(&mut self, scroll_handle: &ScrollableHandle, target_scroll_top: Pixels) {
        self.cancel();

        let current_offset = scroll_handle.offset();
        scroll_handle.set_offset(gpui::Point {
            x: current_offset.x,
            y: -target_scroll_top,
        });
    }

    /// 把在飞动画结算到其目标。返回 true 表示确实结算了一段（调用方应重绘）。
    pub fn snap(&mut self, scroll_handle: &ScrollableHandle) -> bool {
        let Some(animation) = self.animation.as_ref() else {
            return false;
        };

        let current_offset = scroll_handle.offset();
        scroll_handle.set_offset(gpui::Point {
            x: current_offset.x,
            y: -animation.target_scroll_top,
        });
        self.animation = None;
        true
    }

    /// 是否有动画在飞。
    pub fn is_active(&self) -> bool {
        self.animation.is_some()
    }

    /// 从当前偏移起向 `target_scroll_top` 开始缓动。目标落在可见阈值内时
    /// 清空动画，而不是起一段空转。
    pub fn animate_to(&mut self, scroll_handle: &ScrollableHandle, target_scroll_top: Pixels) {
        let current_scroll_top = -scroll_handle.offset().y;

        if (target_scroll_top - current_scroll_top).abs() <= px(MIN_ANIMATION_DELTA_PX) {
            self.animation = None;
            return;
        }

        self.animation = Some(ScrollAnimation {
            start_scroll_top: current_scroll_top,
            target_scroll_top,
            started_at: Instant::now(),
        });
    }

    /// 布防一个跟随目标：`reduced_motion` 下直接跳到位，否则起一段缓动。
    /// 队列 / 歌词 / 专辑详情三个视图的跟随决策共用这一条分支。
    pub fn follow_to(
        &mut self,
        scroll_handle: &ScrollableHandle,
        target_scroll_top: Pixels,
        reduced_motion: bool,
    ) {
        if reduced_motion {
            self.jump_to(scroll_handle, target_scroll_top);
        } else {
            self.animate_to(scroll_handle, target_scroll_top);
        }
    }

    /// 推进一帧：`reduced_motion` 下把在飞动画结算到目标，否则按曲线推进。
    /// 返回 true 表示偏移发生变化（调用方应重绘）。结算后跟随器转为空闲，
    /// 因此调用方无需为降级路径单独判断是否还要排下一帧。
    pub fn advance_or_snap(
        &mut self,
        scroll_handle: &ScrollableHandle,
        reduced_motion: bool,
    ) -> bool {
        if reduced_motion {
            return self.snap(scroll_handle);
        }

        self.advance(scroll_handle)
    }

    /// 把在飞动画推进一帧，按缓动曲线写入偏移。返回 true 表示偏移变了
    /// （调用方应重绘）；动画到达目标后自动转为空闲。
    pub fn advance(&mut self, scroll_handle: &ScrollableHandle) -> bool {
        let Some(animation) = self.animation.as_ref() else {
            return false;
        };

        let progress = (animation.started_at.elapsed().as_secs_f32() / self.duration.as_secs_f32())
            .clamp(0.0, 1.0);
        let eased_progress = ease_out_cubic(progress);

        let current_offset = scroll_handle.offset();
        let current_scroll_top = animation.start_scroll_top
            + (animation.target_scroll_top - animation.start_scroll_top) * eased_progress;

        // Snap the tween to whole CSS pixels. The glyph atlas keys tiles by
        // the glyph origin's fractional subpixel phase, and tiles are never
        // evicted: an unsnapped tween sweeps every visible glyph through all
        // subpixel phases each run, permanently stranding 4 MB subpixel pages
        // per animation (2026-09-27 atlas leak, see `lyrics.rs` font_size).
        // 1 px steps at 60 fps read as a smooth glide for 22 px text.
        scroll_handle.set_offset(gpui::Point {
            x: current_offset.x,
            y: -current_scroll_top.round(),
        });

        if progress >= 1.0 {
            self.animation = None;
        }

        true
    }
}

/// `1 - (1 - t)³` easing: fast start, gentle settle.
pub fn ease_out_cubic(progress: f32) -> f32 {
    1.0 - (1.0 - progress).powi(3)
}
