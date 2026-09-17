use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::*;
use palette::IntoColor;

use crate::ui::theme::Theme;

type ClickHandler = dyn FnMut(f32, &mut Window, &mut App);
type DoubleClickHandler = dyn FnMut(&mut Window, &mut App);

/// Per-slider drag bookkeeping, kept as keyed element state.
#[derive(Clone)]
struct SliderDragState {
    dragging: bool,
    last_emit: Instant,
    drag_value: f32,
    /// When the drag ended. Until the prop (engine echo) catches up with
    /// `drag_value` — or this grace expires — the fill keeps painting
    /// `drag_value`, so the thumb never snaps back to a stale position on
    /// slow-echo (online) sources.
    released_at: Option<Instant>,
}

impl SliderDragState {
    fn new() -> Self {
        Self {
            dragging: false,
            last_emit: Instant::now(),
            drag_value: 0.0,
            released_at: None,
        }
    }
}

/// How long the fill may keep painting the dragged value after release while
/// waiting for the engine echo to land.
const ECHO_GRACE: Duration = Duration::from_millis(600);
/// Echo within this fraction of the drag target counts as caught up (0.5% of
/// the bar ≈ 1.5 s on a 5-minute track).
const ECHO_EPSILON: f32 = 0.005;

pub struct Slider {
    pub(self) id: Option<ElementId>,
    pub(self) style: StyleRefinement,
    pub(self) value: f32,
    pub(self) on_change: Option<Rc<RefCell<ClickHandler>>>,
    pub(self) on_double_click: Option<Rc<RefCell<DoubleClickHandler>>>,
    pub(self) change_interval: Option<Duration>,
}

impl Slider {
    pub fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn value(mut self, value: f32) -> Self {
        self.value = value;
        self
    }

    pub fn on_change(mut self, func: impl FnMut(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(RefCell::new(func)));
        self
    }

    pub fn on_double_click(mut self, func: impl FnMut(&mut Window, &mut App) + 'static) -> Self {
        self.on_double_click = Some(Rc::new(RefCell::new(func)));
        self
    }

    pub fn change_interval(mut self, interval: Duration) -> Self {
        self.change_interval = Some(interval);
        self
    }
}

impl Styled for Slider {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for Slider {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Slider {
    type RequestLayoutState = ();

    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        self.id.clone()
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        let hitbox_bounds = bounds.extend(Edges {
            top: px(4.0),
            bottom: px(4.0),
            ..Default::default()
        });

        window.insert_hitbox(hitbox_bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = cx.global::<Theme>();
        let default_background = theme.slider_background;
        let default_foreground = theme.slider_foreground;

        let mut corners = Corners::default();
        corners.refine(&self.style.corner_radii);
        let corner_radii = corners.to_pixels(window.rem_size());

        let mut borders = Edges::default();
        borders.refine(&self.style.border_widths);
        let border_widths = borders.to_pixels(window.rem_size());
        let border_color = self.style.border_color.unwrap_or_default();
        let foreground = self
            .style
            .text
            .color
            .unwrap_or(default_foreground.into_color());
        let prop_value = self.value;

        let paint_fill = move |value: f32, window: &mut Window| {
            let mut inner_bounds = bounds;
            inner_bounds.size.width = bounds.size.width * value;
            window.paint_quad(quad(
                inner_bounds,
                corner_radii,
                foreground,
                border_widths,
                border_color,
                BorderStyle::Solid,
            ));
        };

        window.set_cursor_style(CursorStyle::PointingHand, hitbox);

        window.paint_quad(quad(
            bounds,
            corner_radii,
            self.style
                .background
                .clone()
                .and_then(|v| v.color())
                .unwrap_or(default_background.into()),
            Edges::all(px(0.0)),
            rgb(0x000000),
            BorderStyle::Solid,
        ));

        // Drag state, shared between this paint pass (fill position) and the
        // mouse handlers below. Keyed by the slider id; `use_keyed_state`
        // observes the entity and notifies the owning view on change, so
        // drags repaint even when the engine echo is silent (paused).
        let drag_entity = self
            .id
            .clone()
            .map(|id| window.use_keyed_state(id, cx, |_, _| SliderDragState::new()));

        // While dragging (and until the engine echo lands after release) the
        // fill must come from the finger position — the echo lags behind on
        // online tracks and the thumb would otherwise jump backwards mid-drag
        // and after release.
        let fill = match drag_entity.as_ref() {
            Some(state) => {
                let state = state.read(cx);
                let echo_pending = state.released_at.is_some_and(|at| {
                    at.elapsed() < ECHO_GRACE && (prop_value - state.drag_value).abs() > ECHO_EPSILON
                });
                if state.dragging || echo_pending {
                    state.drag_value
                } else {
                    prop_value
                }
            }
            None => prop_value,
        };
        paint_fill(fill, window);

        let (Some(func), Some(drag_entity)) = (self.on_change.as_ref(), drag_entity) else {
            return;
        };

        let on_double_click = self.on_double_click.clone();
        let change_interval = self.change_interval;
        let min_interval = change_interval.unwrap_or(Duration::from_millis(1));

        let drag_state_down = drag_entity.clone();
        let hitbox = hitbox.clone();
        let func_down = func.clone();

        window.on_mouse_event(move |ev: &MouseDownEvent, _, window, cx| {
            if !hitbox.is_hovered(window) {
                return;
            }

            window.prevent_default();
            cx.stop_propagation();

            if ev.click_count == 2 {
                if let Some(on_double_click) = on_double_click.as_ref() {
                    (on_double_click.borrow_mut())(window, cx);
                }
                drag_state_down.update(cx, |state, _| state.dragging = false);
                return;
            }

            let relative = ev.position - bounds.origin;
            let relative_x: f32 = relative.x.into();
            let width: f32 = bounds.size.width.into();
            let value = (relative_x / width).clamp(0.0, 1.0);

            (func_down.borrow_mut())(value, window, cx);
            drag_state_down.update(cx, |state, _| {
                state.dragging = true;
                state.released_at = None;
                state.last_emit = Instant::now();
                state.drag_value = value;
            });
        });

        let drag_state_move = drag_entity.clone();
        let func_move = func.clone();

        window.on_mouse_event(move |ev: &MouseMoveEvent, _, window, cx| {
            let emit = drag_state_move.update(cx, |state, _| {
                if !state.dragging {
                    return None;
                }

                let relative = ev.position - bounds.origin;
                let relative_x: f32 = relative.x.into();
                let width: f32 = bounds.size.width.into();
                let value = (relative_x / width).clamp(0.0, 1.0);

                state.drag_value = value;

                let now = Instant::now();
                let due = now.duration_since(state.last_emit) >= min_interval;
                if due {
                    state.last_emit = now;
                }
                due.then_some(value)
            });

            if let Some(value) = emit {
                (func_move.borrow_mut())(value, window, cx);
            }
        });

        let drag_state_up = drag_entity.clone();
        let func_release = func.clone();
        let flush_on_release = change_interval.is_some();

        window.on_mouse_event(move |_ev: &MouseUpEvent, _, window, cx| {
            let flushed = drag_state_up.update(cx, |state, _| {
                if !state.dragging {
                    return None;
                }
                state.released_at = Some(Instant::now());
                state.dragging = false;
                Some(state.drag_value)
            });

            if flush_on_release && let Some(value) = flushed {
                (func_release.borrow_mut())(value, window, cx);
            }
        });
    }
}

pub fn slider() -> Slider {
    Slider {
        id: None,
        style: StyleRefinement::default(),
        value: 0.0,
        on_change: None,
        on_double_click: None,
        change_interval: None,
    }
}
