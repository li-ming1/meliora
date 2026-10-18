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

/// 每个滑条的拖动记账，作为 keyed element state 保存。
#[derive(Clone)]
struct SliderDragState {
    dragging: bool,
    last_emit: Instant,
    drag_value: f32,
    /// 拖动结束的时刻。在 prop（引擎回显）追上 `drag_value` 或这段宽限期过期之前，填充条继续
    /// 按 `drag_value` 绘制，因此回显缓慢的（在线）音源上滑块绝不会弹回过期位置。
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

/// 松开之后等待引擎回显落地期间，填充条还可以继续绘制被拖动值的时长。
const ECHO_GRACE: Duration = Duration::from_millis(600);
/// 回显落在拖动目标这个比例内就算追上（0.5% 的进度条 ≈ 5 分钟曲目上的 1.5 s）。
const ECHO_EPSILON: f32 = 0.005;
/// 未设置 `change_interval` 时使用的发射间隔：拖动期间每毫秒至多一次 `on_change`。
const DEFAULT_CHANGE_INTERVAL: Duration = Duration::from_millis(1);
/// 滑条变更事件的防抖：没有它，拖动期间的每次鼠标移动都会往播放通道发一条命令
/// （SetVolume / 跳转定位）。
pub const SLIDER_CHANGE_INTERVAL: Duration = Duration::from_millis(33);
/// 进度条上下的命中区留白，于是光标偏离细轨道几个像素时拖动仍然有效。
const HITBOX_VERTICAL_PAD: Pixels = px(4.0);

/// 给定 `bounds` 内鼠标位置的归一化 `[0, 1]` 滑条值。
fn normalized_value(position: Point<Pixels>, bounds: Bounds<Pixels>) -> f32 {
    let x: f32 = (position - bounds.origin).x.into();
    let width: f32 = bounds.size.width.into();
    (x / width).clamp(0.0, 1.0)
}

pub struct Slider {
    pub(self) id: Option<ElementId>,
    pub(self) style: StyleRefinement,
    pub(self) value: f32,
    pub(self) on_change: Option<Rc<RefCell<ClickHandler>>>,
    pub(self) on_double_click: Option<Rc<RefCell<DoubleClickHandler>>>,
    pub(self) change_interval: Option<Duration>,
}

impl Slider {
    /// 设置元素 id。**交互必需**：拖动状态以此 id 为键（`use_keyed_state`）；没有它时每个鼠标
    /// 处理器都被静默跳过（绘制与光标仍然正常，所以这个故障看不见）。无处理器的滑条
    /// （纯展示条）可以省略。
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
            top: HITBOX_VERTICAL_PAD,
            bottom: HITBOX_VERTICAL_PAD,
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
        // 绘制前先钳位到轨道的半高/半宽：调用方用 `rounded_full` 做药丸形进度条，其相对半径
        // 解析成一个巨大哨兵值，不钳位时会让 DirectX quad SDF 在整个 quad 上退化成 alpha 0，
        // 也就是滑条完全看不见（沉浸页，2026-09-28）。
        let corner_radii = corners
            .to_pixels(window.rem_size())
            .clamp_radii_for_quad_size(bounds.size);

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

        // 拖动状态，在本次绘制阶段（填充位置）与下面的鼠标处理器之间共享。以滑条 id 为键。
        // 这里什么都不 notify：拖动期间的重绘与松开后的宽限窗口由 `on_change` 回调的副作用
        // （引擎回显、设置写入）驱动，不靠任何观察者。
        let drag_entity = self
            .id
            .clone()
            .map(|id| window.use_keyed_state(id, cx, |_, _| SliderDragState::new()));

        // 拖动期间（以及松开后直到引擎回显落地）填充必须来自指尖位置——在线曲目上回显滞后，
        // 否则滑块会在拖动中与松开后往回跳。
        let fill = match drag_entity.as_ref() {
            Some(state) => {
                let state = state.read(cx);
                let echo_pending = state.released_at.is_some_and(|at| {
                    at.elapsed() < ECHO_GRACE
                        && (prop_value - state.drag_value).abs() > ECHO_EPSILON
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

        debug_assert!(
            self.id.is_some() || (self.on_change.is_none() && self.on_double_click.is_none()),
            "Slider: on_change/on_double_click set without .id() — keyed drag state cannot \
             be created and all mouse handling (click, drag, double-click) silently no-ops; \
             call .id(...) on interactive sliders"
        );
        debug_assert!(
            self.on_double_click.is_none() || self.on_change.is_some(),
            "Slider: on_double_click without on_change is never registered"
        );

        let (Some(func), Some(drag_entity)) = (self.on_change.as_ref(), drag_entity) else {
            return;
        };

        let on_double_click = self.on_double_click.clone();
        let change_interval = self.change_interval;
        let min_interval = change_interval.unwrap_or(DEFAULT_CHANGE_INTERVAL);

        let drag_state_down = drag_entity.clone();
        let hitbox = hitbox.clone();
        let func_down = func.clone();

        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if phase != gpui::DispatchPhase::Bubble {
                return;
            }
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

            let value = normalized_value(ev.position, bounds);

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

        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase != gpui::DispatchPhase::Bubble {
                return;
            }
            let emit = drag_state_move.update(cx, |state, _| {
                if !state.dragging {
                    return None;
                }

                let value = normalized_value(ev.position, bounds);
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

        let drag_state_up = drag_entity;
        let func_release = func.clone();
        let flush_on_release = change_interval.is_some();

        window.on_mouse_event(move |_ev: &MouseUpEvent, phase, window, cx| {
            if phase != gpui::DispatchPhase::Bubble {
                return;
            }
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
