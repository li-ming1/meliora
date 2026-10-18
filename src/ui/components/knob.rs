use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use gpui::*;
use palette::IntoColor;
use tracing::error;

use crate::ui::{components::textbox::Textbox, theme::Theme};

type ChangeHandler = dyn FnMut(f32, &mut App);
type ValueFormatter = dyn Fn(f32) -> SharedString;
type ValueParser = dyn Fn(&str) -> Option<f32>;

const WIDTH: f32 = 56.0;
const LABEL_HEIGHT: f32 = 16.0;
const DIAMETER: f32 = 40.0;
/// 标签、旋钮本体与读数行之间的垂直间距。
const LABEL_GAP: f32 = 2.0;
const ARC_RADIUS: f32 = 17.5;
const READOUT_HEIGHT: f32 = 28.0;
const READOUT_OFFSET: f32 = LABEL_HEIGHT + LABEL_GAP + DIAMETER + LABEL_GAP;
const HEIGHT: f32 = READOUT_OFFSET + READOUT_HEIGHT;
const DRAG_PIXELS: f32 = 200.0;
/// 按住 shift 时施加到拖动/滚动位移上的微调倍率。
const SHIFT_SCALE: f32 = 0.1;
const SCROLL_STEP: f32 = 0.01;
/// 拖动向下游推送新值的频率：鼠标移动以 125-1000 Hz 到达，而每次推送都会重跑整条频段编辑链
/// （EQ DSP 通道、补偿网格、曲线重采样、视图重绘）。
const DRAG_PUSH_INTERVAL: Duration = Duration::from_millis(33);
/// 轨道弧、数值弧与位置刻线的线宽。
const ARC_STROKE_WIDTH: f32 = 2.5;
const TICK_STROKE_WIDTH: f32 = 2.0;
/// 标签与读数说明的字号。
const CAPTION_SIZE: f32 = 12.0;
const ERROR_FLASH: Duration = Duration::from_millis(600);

// 弧从 7 点位置顺时针走到 5 点位置，正上方为零位
const ARC_START: f32 = -3.0 * std::f32::consts::FRAC_PI_4;
const ARC_SWEEP: f32 = 3.0 * std::f32::consts::FRAC_PI_2;

fn arc_point(center: Point<Pixels>, radius: f32, angle: f32) -> Point<Pixels> {
    point(
        center.x + px(radius * angle.sin()),
        center.y - px(radius * angle.cos()),
    )
}

// 把圆弧描成若干 ≤90 度的三次贝塞尔段，角度以弧度计、从 12 点位置顺时针
pub(crate) fn arc_path(
    builder: &mut PathBuilder,
    center: Point<Pixels>,
    radius: f32,
    from: f32,
    to: f32,
) {
    let sweep = to - from;
    if sweep.abs() < f32::EPSILON {
        return;
    }

    builder.move_to(arc_point(center, radius, from));

    let segments = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil() as u32;
    for i in 1..=segments {
        let start_angle = from + sweep * (i - 1) as f32 / segments as f32;
        let end = from + sweep * i as f32 / segments as f32;
        let k = 4.0 / 3.0 * ((end - start_angle) / 4.0).tan() * radius;
        let start = arc_point(center, radius, start_angle);
        let end_point = arc_point(center, radius, end);
        builder.cubic_bezier_to(
            end_point,
            point(
                start.x + px(start_angle.cos() * k),
                start.y + px(start_angle.sin() * k),
            ),
            point(
                end_point.x - px(end.cos() * k),
                end_point.y - px(end.sin() * k),
            ),
        );
    }
}

/// 构建并描出一条路径；无法细分（tessellate）的路径直接丢弃。
fn paint_stroked_path(
    window: &mut Window,
    color: Rgba,
    width: f32,
    build: impl FnOnce(&mut PathBuilder),
) {
    let mut builder = PathBuilder::stroke(px(width));
    build(&mut builder);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// 说明文本的绘制每帧每个旋钮都会跑一次（EQ 界面显示三个，标签与读数各一份），而 `error!`
/// 写入是同步刷盘，于是一次失败的绘制会按帧率打到日志上。绘制失败是持续性的（字体/图集后端
/// 状态），所以只记录第一次；绘制恢复后旋钮照常渲染，那不需要日志行。
static PAINT_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

/// 把预先排版好的说明文本在 `bounds` 内水平居中、按给定上边缘绘制，失败时以 `what` 记录。
fn paint_centered_caption(
    caption: ShapedLine,
    bounds: Bounds<Pixels>,
    top: Pixels,
    what: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let x = bounds.origin.x + (bounds.size.width - caption.width) / 2.0;
    if let Err(e) = caption.paint(point(x, top), px(14.0), TextAlign::Left, None, window, cx)
        && !PAINT_FAILURE_LOGGED.swap(true, Ordering::Relaxed)
    {
        error!("Failed to paint knob {what}: {:?}", e);
    }
}

/// 拖动/滚动位移倍率：满灵敏度，或按住 shift 微调时用 [`SHIFT_SCALE`]。
fn drag_scale(fine: bool) -> f32 {
    if fine { SHIFT_SCALE } else { 1.0 }
}

/// 把垂直拖动位移映射到 0..1 的旋钮位置。
fn drag_position(start_position: f32, start_y: Pixels, current_y: Pixels, scale: f32) -> f32 {
    let dy: f32 = (start_y - current_y).into();
    (start_position + dy / DRAG_PIXELS * scale).clamp(0.0, 1.0)
}

fn shape_caption(window: &mut Window, text: SharedString, color: Hsla, font: Font) -> ShapedLine {
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    };
    window
        .text_system()
        .shape_line(text, px(CAPTION_SIZE), &[run], None)
}

/// 一份缓存的已排版说明文本（标签或读数）：键字段覆盖排版消费的一切，所以未变的说明跨帧绝不
/// 重排。存于 [`KnobState`]，而它只对带 id 的旋钮跨帧保活。
struct CaptionCache {
    text: SharedString,
    color: Hsla,
    font: Font,
    font_size: Pixels,
    scale_factor: f32,
    line: ShapedLine,
}

/// 键仍然匹配时返回 `slot` 里缓存的说明文本，否则用 [`shape_caption`] 重新排版并回填该槽位：
/// prepaint 每帧都跑，但只有真正的文本/样式变化才需要重排（拖动会改变读数文本并故意未命中）。
fn cached_caption(
    slot: &mut Option<CaptionCache>,
    text: SharedString,
    color: Hsla,
    font: Font,
    scale_factor: f32,
    window: &mut Window,
) -> ShapedLine {
    let font_size = px(CAPTION_SIZE);
    if let Some(cache) = slot
        && cache.text == text
        && cache.color == color
        && cache.font == font
        && cache.font_size == font_size
        && cache.scale_factor == scale_factor
    {
        return cache.line.clone();
    }
    let line = shape_caption(window, text.clone(), color, font.clone());
    *slot = Some(CaptionCache {
        text,
        color,
        font,
        font_size,
        scale_factor,
        line: line.clone(),
    });
    line
}

fn arc_center(bounds: Bounds<Pixels>) -> Point<Pixels> {
    point(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + px(LABEL_HEIGHT + LABEL_GAP + DIAMETER / 2.0),
    )
}

fn textbox_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(bounds.origin.x, bounds.origin.y + px(READOUT_OFFSET)),
        size(bounds.size.width, px(READOUT_HEIGHT)),
    )
}

pub struct Knob {
    id: ElementId,
    style: StyleRefinement,
    value: f32,
    default_value: Option<f32>,
    bipolar: bool,
    label: Option<SharedString>,
    format: Rc<ValueFormatter>,
    parse: Option<Rc<ValueParser>>,
    on_change: Option<Rc<RefCell<ChangeHandler>>>,
    disabled: bool,
    restore_focus: Option<FocusHandle>,
    parse_range: (f32, f32),
}

impl Knob {
    pub fn value(mut self, value: f32) -> Self {
        self.value = value;
        self
    }

    pub fn default_value(mut self, value: f32) -> Self {
        self.default_value = Some(value);
        self
    }

    pub fn bipolar(mut self) -> Self {
        self.bipolar = true;
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn format(mut self, format: impl Fn(f32) -> SharedString + 'static) -> Self {
        self.format = Rc::new(format);
        self
    }

    pub fn parse(mut self, parse: impl Fn(&str) -> Option<f32> + 'static) -> Self {
        self.parse = Some(Rc::new(parse));
        self
    }

    pub fn on_change(mut self, on_change: impl FnMut(f32, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(RefCell::new(on_change)));
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn restore_focus(mut self, focus: FocusHandle) -> Self {
        self.restore_focus = Some(focus);
        self
    }

    pub fn parse_range(mut self, min: f32, max: f32) -> Self {
        self.parse_range = (min, max);
        self
    }
}

impl Styled for Knob {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for Knob {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[derive(Default)]
struct KnobState {
    drag: Option<(Pixels, f32, bool)>,
    /// 上一次拖动向下游推送的值，供推送合并使用。
    drag_emitted: Option<(Instant, f32)>,
    textbox: Option<Entity<Textbox>>,
    parse_error: Option<Instant>,
    refocus: bool,
    scroll_position: Option<f32>,
    label_caption: Option<CaptionCache>,
    readout_caption: Option<CaptionCache>,
    /// 最近一次 `(值, 格式化文本)`：格式化函数是值的纯函数，所以旋钮静止时 prepaint 可以跳过
    /// 每帧的 SharedString 分配。`readout_caption` 只是避免重排。
    readout_text: Option<(f32, SharedString)>,
}

pub struct KnobPrepaint {
    hitbox: Hitbox,
    label: Option<ShapedLine>,
    readout: Option<ShapedLine>,
    textbox: Option<AnyElement>,
    parse_error: bool,
}

impl Element for Knob {
    type RequestLayoutState = ();
    type PrepaintState = KnobPrepaint;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = px(WIDTH).into();
        style.size.height = px(HEIGHT).into();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let has_id = id.is_some();

        let state = window.with_optional_element_state(id, move |v, _| {
            let state: Rc<RefCell<KnobState>> = v.flatten().unwrap_or_default();
            (state.clone(), if has_id { Some(state) } else { None })
        });

        // 编辑器失去焦点时取消输入态
        let mut textbox = state.borrow().textbox.clone();
        let stale = textbox
            .as_ref()
            .is_some_and(|textbox| !textbox.read(cx).focus_handle().is_focused(window));
        if stale {
            let mut state = state.borrow_mut();
            state.textbox = None;
            state.parse_error = None;
            textbox = None;
        }

        {
            let mut state = state.borrow_mut();

            state.scroll_position = None;

            if state.refocus {
                state.refocus = false;
                if let Some(focus) = &self.restore_focus {
                    focus.focus(window, cx);
                }
            }
        }

        let theme = cx.global::<Theme>();
        let (label_color, readout_color) = if self.disabled {
            (theme.text_disabled, theme.text_disabled)
        } else {
            (theme.text_secondary, theme.text)
        };

        let font = window.text_style().font();
        let scale_factor = window.scale_factor();

        let label = self.label.clone().map(|text| {
            cached_caption(
                &mut state.borrow_mut().label_caption,
                text,
                label_color.into_color(),
                font.clone(),
                scale_factor,
                window,
            )
        });
        let readout = if textbox.is_none() {
            let text = {
                let mut state = state.borrow_mut();
                match state.readout_text.as_ref() {
                    Some((value, text)) if *value == self.value => text.clone(),
                    _ => {
                        let text = (self.format)(self.value);
                        state.readout_text = Some((self.value, text.clone()));
                        text
                    }
                }
            };
            Some(cached_caption(
                &mut state.borrow_mut().readout_caption,
                text,
                readout_color.into_color(),
                font,
                scale_factor,
                window,
            ))
        } else {
            None
        };

        let textbox = textbox.map(|textbox| {
            let mut element = textbox.into_any_element();
            element.prepaint_as_root(
                textbox_bounds(bounds).origin,
                size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::Definite(px(READOUT_HEIGHT)),
                ),
                window,
                cx,
            );
            element
        });

        let parse_error = state
            .borrow()
            .parse_error
            .is_some_and(|since| since.elapsed() < ERROR_FLASH);

        KnobPrepaint {
            hitbox,
            label,
            readout,
            textbox,
            parse_error,
        }
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = cx.global::<Theme>();
        let track_color = theme.slider_background;
        let status_error = theme.status_error;
        let (value_color, tick_color) = if self.disabled {
            (theme.text_disabled, theme.text_disabled)
        } else {
            (theme.slider_foreground, theme.text)
        };

        let center = arc_center(bounds);
        let angle = |position: f32| ARC_START + ARC_SWEEP * position;
        let position = self.value.clamp(0.0, 1.0);
        let from = if self.bipolar { 0.5 } else { 0.0 };

        paint_stroked_path(window, track_color, ARC_STROKE_WIDTH, |builder| {
            arc_path(
                builder,
                center,
                ARC_RADIUS,
                ARC_START,
                ARC_START + ARC_SWEEP,
            );
        });
        paint_stroked_path(window, value_color, ARC_STROKE_WIDTH, |builder| {
            arc_path(builder, center, ARC_RADIUS, angle(from), angle(position));
        });
        paint_stroked_path(window, tick_color, TICK_STROKE_WIDTH, |builder| {
            builder.move_to(arc_point(center, ARC_RADIUS - 4.0, angle(position)));
            builder.line_to(arc_point(center, ARC_RADIUS + 3.0, angle(position)));
        });

        if let Some(label) = prepaint.label.take() {
            paint_centered_caption(
                label,
                bounds,
                bounds.origin.y + px(1.0),
                "label",
                window,
                cx,
            );
        }

        if let Some(readout) = prepaint.readout.take() {
            let readout_top = bounds.origin.y + px(READOUT_OFFSET);
            paint_centered_caption(
                readout,
                bounds,
                readout_top + px(3.5),
                "readout",
                window,
                cx,
            );
        }

        if let Some(textbox) = prepaint.textbox.as_mut() {
            if prepaint.parse_error {
                window.paint_quad(quad(
                    textbox_bounds(bounds),
                    Corners::all(px(4.0)),
                    hsla(0.0, 0.0, 0.0, 0.0),
                    Edges::all(px(1.0)),
                    status_error,
                    BorderStyle::Solid,
                ));
                window.request_animation_frame();
            }
            textbox.paint(window, cx);
        }

        if self.disabled {
            return;
        }
        window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);

        let Some(on_change) = self.on_change.clone() else {
            return;
        };

        let hitbox = prepaint.hitbox.clone();
        let format = self.format.clone();
        let parse = self.parse.clone();
        let default_value = self.default_value;
        let edit_bounds = textbox_bounds(bounds);
        let has_id = id.is_some();
        let raw_value = self.value;
        let restore_focus = self.restore_focus.clone();
        let parse_range = self.parse_range;

        window.with_optional_element_state(id, move |v, window| {
            let state: Rc<RefCell<KnobState>> = v.flatten().unwrap_or_default();

            {
                let state = state.clone();
                let hitbox = hitbox.clone();
                let on_change = on_change.clone();
                let restore_focus = restore_focus.clone();
                window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }

                    // 落在打开的编辑器里的点击交给 textbox
                    if state.borrow().textbox.is_some() && edit_bounds.contains(&ev.position) {
                        return;
                    }

                    window.prevent_default();
                    cx.stop_propagation();

                    match ev.button {
                        MouseButton::Left if ev.click_count == 2 && parse.is_some() => {
                            let parse = parse.clone().unwrap();
                            let commit_change = on_change.clone();
                            let commit_state = state.clone();
                            let textbox = Textbox::new_with_value_submit(
                                cx,
                                StyleRefinement::default(),
                                move |text, cx| match parse(text.trim()) {
                                    Some(value) => {
                                        (commit_change.borrow_mut())(
                                            value.clamp(parse_range.0, parse_range.1),
                                            cx,
                                        );
                                        let mut state = commit_state.borrow_mut();
                                        state.textbox = None;
                                        state.parse_error = None;
                                        state.refocus = true;
                                    }
                                    None => {
                                        commit_state.borrow_mut().parse_error =
                                            Some(Instant::now());
                                        cx.refresh_windows();
                                    }
                                },
                            );
                            textbox.update(cx, |textbox, cx| {
                                textbox.set_value(cx, (format)(raw_value));
                                textbox.select_all(cx);
                            });
                            let focus = textbox.read(cx).focus_handle();
                            focus.focus(window, cx);
                            let mut state = state.borrow_mut();
                            state.textbox = Some(textbox);
                            state.parse_error = None;
                            state.drag = None;
                            window.refresh();
                        }
                        MouseButton::Left => {
                            let mut state = state.borrow_mut();
                            // 开始拖动时收起手输编辑器
                            if state.textbox.take().is_some() {
                                state.parse_error = None;
                                if let Some(focus) = &restore_focus {
                                    focus.focus(window, cx);
                                }
                            }
                            state.drag = Some((ev.position.y, position, ev.modifiers.shift));
                            state.drag_emitted = None;
                        }
                        MouseButton::Right => {
                            if let Some(default) = default_value {
                                (on_change.borrow_mut())(default, cx);
                            }
                        }
                        _ => {}
                    }
                });
            }

            {
                let state = state.clone();
                let on_change = on_change.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some((start_y, start_position, start_shift)) = state.borrow().drag else {
                        return;
                    };
                    let shift = ev.modifiers.shift;
                    // 拖动途中切换了 shift：重设基准，让当前值保持不动，新倍率
                    // 只作用于此后产生的位移
                    let (start_y, start_position) = if shift != start_shift {
                        let current = drag_position(
                            start_position,
                            start_y,
                            ev.position.y,
                            drag_scale(start_shift),
                        );
                        state.borrow_mut().drag = Some((ev.position.y, current, shift));
                        (ev.position.y, current)
                    } else {
                        (start_y, start_position)
                    };
                    let position =
                        drag_position(start_position, start_y, ev.position.y, drag_scale(shift));
                    // 把推送合并到 ~30 Hz 并跳过重复值：每次推送都会重跑频段编辑链
                    // 并重绘视图
                    let now = Instant::now();
                    let (due, unchanged) = {
                        let state = state.borrow();
                        (
                            state
                                .drag_emitted
                                .is_none_or(|(at, _)| now.duration_since(at) >= DRAG_PUSH_INTERVAL),
                            state.drag_emitted.is_some_and(|(_, v)| v == position),
                        )
                    };
                    if due && !unchanged {
                        state.borrow_mut().drag_emitted = Some((now, position));
                        (on_change.borrow_mut())(position, cx);
                    }
                });
            }

            {
                let state = state.clone();
                let on_change = on_change.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    // 拖动结束，把尾部补一次：推送间隔内的最后一次移动被合并掉了，
                    // 所以要发出松开时的位置，
                    // 除非它已经等于上次推送的值
                    let flush = {
                        let mut state = state.borrow_mut();
                        let Some((start_y, start_position, start_shift)) = state.drag else {
                            return;
                        };
                        state.drag = None;
                        let position = drag_position(
                            start_position,
                            start_y,
                            ev.position.y,
                            drag_scale(start_shift),
                        );
                        if state.drag_emitted.is_some_and(|(_, v)| v != position) {
                            state.drag_emitted = Some((Instant::now(), position));
                            Some(position)
                        } else {
                            None
                        }
                    };
                    if let Some(position) = flush {
                        (on_change.borrow_mut())(position, cx);
                    }
                });
            }

            {
                let hitbox = hitbox.clone();
                let state = state.clone();
                window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }

                    window.prevent_default();
                    cx.stop_propagation();

                    let delta: f32 = if ev.delta.precise() {
                        let dy: f32 = ev.delta.pixel_delta(px(1.0)).y.into();
                        dy / DRAG_PIXELS
                    } else {
                        ev.delta.pixel_delta(px(SCROLL_STEP)).y.into()
                    };
                    // 在最后一次已发出的值上累加；绘制期那份已经过期
                    let mut state = state.borrow_mut();
                    let base = state.scroll_position.unwrap_or(position);
                    let position = (base + delta).clamp(0.0, 1.0);
                    // 处在端点时每个滚轮 tick 都钳位到同一个值：
                    // 推送它会让旋钮毫无变化，却重跑频段编辑链
                    // 并重绘一遍。
                    if position == base {
                        return;
                    }
                    state.scroll_position = Some(position);
                    drop(state);
                    (on_change.borrow_mut())(position, cx);
                });
            }

            {
                let state = state.clone();
                let restore_focus = restore_focus.clone();
                window.on_key_event(move |ev: &KeyDownEvent, phase, window, cx| {
                    // capture 阶段：打开的编辑器要先于图形清除选点之前处理
                    if phase != DispatchPhase::Capture || ev.keystroke.key != "escape" {
                        return;
                    }

                    let mut state = state.borrow_mut();
                    if state.textbox.is_none() {
                        return;
                    }

                    state.textbox = None;
                    state.parse_error = None;
                    state.drag = None;
                    if let Some(focus) = &restore_focus {
                        focus.focus(window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    window.refresh();
                });
            }

            ((), if has_id { Some(state) } else { None })
        });
    }
}

pub fn knob(id: impl Into<ElementId>) -> Knob {
    Knob {
        id: id.into(),
        style: StyleRefinement::default(),
        value: 0.0,
        default_value: None,
        bipolar: false,
        label: None,
        format: Rc::new(|value| format!("{value:.2}").into()),
        parse: None,
        on_change: None,
        disabled: false,
        restore_focus: None,
        parse_range: (0.0, 1.0),
    }
}
