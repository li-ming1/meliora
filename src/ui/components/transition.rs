use std::time::{Duration, Instant};

use gpui::*;
use palette::Mix;

use crate::ui::scroll_follow::ease_out_cubic;

/// Duration of hover/active background transitions.
const HOVER_DURATION: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Default)]
struct HoverState {
    /// Current interpolated value, 0 = base, 1 = hover.
    value: f32,
    /// Target value we are animating toward.
    target: f32,
    /// Active transition: the start instant and the value it began from.
    animating: Option<(Instant, f32)>,
}

/// An interactive element that fades its background between `base` and `hover`
/// as the pointer enters and leaves. Driven by element keyed state plus a
/// self-scheduling frame loop, so no per-view animation plumbing is required.
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

        // Advance any in-flight transition; keep scheduling frames until it settles.
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
                // `Entity::update` + `Context::notify` is safe outside rendering;
                // `window.request_animation_frame()` is not (empty current_view).
                state.update(cx, |s, cx| {
                    let target = if *hovered { 1.0 } else { 0.0 };
                    if (s.value - target).abs() > 0.001 {
                        s.target = target;
                        s.animating = Some((Instant::now(), s.value));
                        cx.notify();
                    }
                });
            })
    }
}

/// Extension for turning any interactive div into a hover-fading transition.
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
