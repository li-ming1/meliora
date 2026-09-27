//! One-shot scroll animation for list panels: eases a scroll handle from its
//! current offset to a target offset over a fixed duration.

use gpui::{Pixels, px};
use std::time::{Duration, Instant};

use crate::ui::components::scrollbar::ScrollableHandle;

/// Movement below this many px is invisible; don't tween for it.
const MIN_ANIMATION_DELTA_PX: f32 = 0.1;

/// An in-flight animation, easing from `start_scroll_top` to
/// `target_scroll_top` over the follower's duration.
struct ScrollAnimation {
    start_scroll_top: Pixels,
    target_scroll_top: Pixels,
    started_at: Instant,
}

/// Animates a scroll handle toward a target offset over a fixed duration.
pub struct SmoothScrollFollow {
    /// Total animation time from start to target.
    duration: Duration,
    /// Set while an animation is in flight; `None` when idle.
    animation: Option<ScrollAnimation>,
}

impl SmoothScrollFollow {
    /// Creates an idle follower whose animations run over `duration`.
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            animation: None,
        }
    }

    /// Drops any in-flight animation.
    pub fn cancel(&mut self) {
        self.animation = None;
    }

    /// Cancels any animation and jumps straight to `target_scroll_top`.
    pub fn jump_to(&mut self, scroll_handle: &ScrollableHandle, target_scroll_top: Pixels) {
        self.cancel();

        let current_offset = scroll_handle.offset();
        scroll_handle.set_offset(gpui::Point {
            x: current_offset.x,
            y: -target_scroll_top,
        });
    }

    /// Settles an in-flight animation by jumping to its target. Returns true
    /// when an animation was settled (the caller should repaint).
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

    /// True while an animation is in flight.
    pub fn is_active(&self) -> bool {
        self.animation.is_some()
    }

    /// Starts animating from the current offset toward `target_scroll_top`.
    /// A target within the visibility threshold of the current offset clears
    /// the animation instead of starting a no-op tween.
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

        scroll_handle.set_offset(gpui::Point {
            x: current_offset.x,
            y: -current_scroll_top,
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
