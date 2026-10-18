use std::time::{Duration, Instant};

use gpui::*;
use palette::Mix;

use crate::ui::scroll_follow::ease_out_cubic;
use crate::ui::util::reduced_motion_enabled;

/// hover/active 背景过渡的时长。
const HOVER_DURATION: Duration = Duration::from_millis(120);

/// 低于这个增量算"已经到达目标"：为它们启动过渡会毫无可见变化却排定帧。
const SETTLE_EPSILON: f32 = 0.001;

#[derive(Clone, Copy, Default)]
struct HoverState {
    /// 当前插值结果，0 = 基色，1 = hover。
    value: f32,
    /// 正在动画奔赴的目标值。
    target: f32,
    /// 进行中的过渡：起始时刻与它开始时所在的值。
    animating: Option<(Instant, f32)>,
}

/// 一个交互元素，随指针进出在 `base` 与 `hover` 之间淡入淡出其背景。由元素 keyed state 加
/// 自我排定的帧循环驱动，因此视图侧不需要任何动画管线。
#[derive(IntoElement)]
pub struct HoverTransition {
    id: ElementId,
    div: Stateful<Div>,
    base: Rgba,
    hover: Rgba,
}

impl HoverTransition {
    pub fn new(id: impl Into<ElementId>, div: Stateful<Div>, base: Rgba, hover: Rgba) -> Self {
        Self {
            id: id.into(),
            div,
            base,
            hover,
        }
    }
}

impl StatefulInteractiveElement for HoverTransition {}

impl InteractiveElement for HoverTransition {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.div.interactivity()
    }
}

impl Styled for HoverTransition {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl RenderOnce for HoverTransition {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| HoverState::default());

        // 推进任何在飞的过渡；直到它稳定下来之前继续排帧。
        state.update(cx, |s, _| {
            if let Some((start, from)) = s.animating {
                let progress =
                    (start.elapsed().as_secs_f32() / HOVER_DURATION.as_secs_f32()).clamp(0.0, 1.0);
                s.value = from + (s.target - from) * ease_out_cubic(progress);
                if progress >= 1.0 {
                    s.value = s.target;
                    s.animating = None;
                } else {
                    window.request_animation_frame();
                }
            }
        });

        let value = state.read(cx).value;
        let background = self.base.mix(self.hover, value);

        self.div
            .bg(background)
            .on_hover(move |hovered, _window, cx| {
                // `Entity::update` + `Context::notify` 在渲染之外是安全的；
                // `window.request_animation_frame()` 不是（current_view 为空）。
                // 装饰性动画自行检查动效偏好（与 drag_drop.rs / immersive.rs 同一纪律；
                // gpui 的 request_animation_frame 文档有此要求，而本应用从不设置
                // 从不设置 App::set_reduce_motion）。
                let reduced_motion = reduced_motion_enabled(cx);
                state.update(cx, |s, cx| {
                    let target = if *hovered { 1.0 } else { 0.0 };
                    if (s.value - target).abs() <= SETTLE_EPSILON {
                        return;
                    }
                    if reduced_motion {
                        // 硬切：精确落到目标值并丢弃任何在飞的动画，让状态一步收敛——
                        // 不排帧请求，也不留半程值。
                        s.value = target;
                        s.target = target;
                        s.animating = None;
                    } else {
                        s.target = target;
                        s.animating = Some((Instant::now(), s.value));
                    }
                    cx.notify();
                });
            })
    }
}

/// 把任意交互 div 变成 hover 淡入过渡的扩展。
pub trait TransitionExt {
    fn into_transition(self, base: Rgba, hover: Rgba) -> HoverTransition;
}

impl TransitionExt for Stateful<Div> {
    fn into_transition(self, base: Rgba, hover: Rgba) -> HoverTransition {
        HoverTransition {
            id: Element::id(&self).expect("transition element needs an explicit id"),
            div: self,
            base,
            hover,
        }
    }
}
