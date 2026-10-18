use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::*;
use palette::IntoColor;
use tracing::error;

use crate::{
    playback::dsp::equalizer::{Biquad, MAX_GAIN_DB, band_response_db, omega},
    settings::equalizer::{EqBandSettings, EqualizerSettings, MAX_EQ_BANDS},
    ui::{
        equalizer::mapping::{
            CURVE_MAX_DB, CURVE_MIN_DB, DB_WINDOW, MAX_FREQ, MIN_FREQ, db_to_y, db_to_y_unclamped,
            format_hz, freq_to_x, scroll_q, spectrum_db_to_y, x_to_freq, y_to_db,
        },
        theme::Theme,
    },
};

type BandChangeHandler = dyn FnMut(usize, f64, f64, &mut App);
type SelectHandler = dyn FnMut(Option<usize>, &mut App);
type IndexHandler = dyn FnMut(usize, &mut App);
type ScrollQHandler = dyn FnMut(usize, f64, &mut App);
type AddHandler = dyn FnMut(f64, f64, &mut App) -> usize;
type DragActiveHandler = dyn FnMut(bool, &mut App);

const DOT_RADIUS: f32 = 8.0;
/// 圆点与其 hover 环之间的间隙。
const DOT_RING_PAD: f32 = 3.0;
const DOT_HIT_RADIUS: f32 = 16.0;
const CURVE_HIT_DISTANCE: f32 = 8.0;
const Q_FLASH_MS: u128 = 600;
/// 指针移动多少之后，一次按下算拖拽而不是点击。
const DRAG_START_THRESHOLD_PX: f32 = 3.0;
/// 新增之后这么短的一次点击是双击的第二次按下正好落在新圆点上，而不是切换选中。
const ADD_ECHO_MS: u128 = 500;

const GRID_FREQS: [f32; 13] = [
    20.0, 30.0, 50.0, 100.0, 200.0, 300.0, 500.0, 1_000.0, 2_000.0, 3_000.0, 5_000.0, 10_000.0,
    20_000.0,
];
// 十倍频程与 5x 刻度带文字标签
const LABEL_FREQS: [f32; 6] = [50.0, 100.0, 500.0, 1_000.0, 5_000.0, 10_000.0];

#[derive(Clone, Copy, PartialEq)]
enum Hover {
    Dot(usize),
    Curve(Point<Pixels>),
}

#[derive(Clone, Copy)]
struct DragState {
    index: usize,
    start: Point<Pixels>,
    start_frequency: f64,
    start_gain_db: f64,
    shift: bool,
    was_selected: bool,
    moved: bool,
}

/// 图表每帧写入其绘图区域的槽位，好让视图能把浮层锚定到圆点上。
pub type PlotSlot = Rc<Cell<Option<Bounds<Pixels>>>>;

struct CurveCache {
    config: EqualizerSettings,
    selected: Option<usize>,
    width: f32,
    height: f32,
    scale: f32,
    rate: f64,
    composite: Rc<Vec<f32>>,
    band: Option<Rc<Vec<f32>>>,
}

/// 缓存的坐标轴标签形状。17 个标签是静态文本，所以只在字体、标签颜色、DPI 缩放或
/// 绘图几何变化时重新成形。
struct LabelCache {
    font: Font,
    color: Hsla,
    scale: f32,
    origin: Point<Pixels>,
    size: Size<Pixels>,
    lines: Rc<Vec<(ShapedLine, Point<Pixels>)>>,
}

/// 拖拽向下游推送新值的频率：鼠标移动以 125-1000 Hz 到达，而过去每次推送都会把配置
/// 克隆进 DSP 通道并重跑补偿网格。
const DRAG_PUSH_INTERVAL: Duration = Duration::from_millis(33);

/// 一个稳定帧里构建好的镶嵌结果，在可见内容没变时复用。绘制仍会把已铺好路径的副本
/// 交给自己的场景（paint_path 按值接收），但缓存命中能跳过 PathBuilder 的中间缓冲和
/// 重建会重做的曲线采样——静止页面（频谱暂停、无拖拽）于是完全不镶嵌。
/// 拆成两个独立记 key 的半边：播放中的频谱每帧换它的 Rc 指针，只重建频谱半边，
/// 绝不碰未变动的曲线半边（[`EqCurvePaths`]）。
#[derive(Default)]
struct EqSpectrumPaths {
    pre_fill: Option<Path<Pixels>>,
    post_fill: Option<Path<Pixels>>,
    post_stroke: Option<Path<Pixels>>,
}

/// 镶嵌缓存的曲线半边；见 [`EqSpectrumPaths`]。
#[derive(Default)]
struct EqCurvePaths {
    band_stroke: Option<Path<Pixels>>,
    composite_fill: Option<Path<Pixels>>,
    composite_stroke: Option<Path<Pixels>>,
}

/// 频谱路径，以分析器的 Rc 指针加绘图几何为 key（不含 scale：
/// `build_spectrum_paths` 只消费绘图几何）。
struct SpectrumPathCache {
    origin: Point<Pixels>,
    size: Size<Pixels>,
    spectrum_pre: Rc<Vec<f32>>,
    spectrum_post: Rc<Vec<f32>>,
    paths: Rc<EqSpectrumPaths>,
}

/// 与曲线缓存一同失效的曲线路径（config/selected/geometry/rate）。
struct CurvePathCache {
    config: EqualizerSettings,
    selected: Option<usize>,
    origin: Point<Pixels>,
    size: Size<Pixels>,
    scale: f32,
    rate: f64,
    paths: Rc<EqCurvePaths>,
}

#[derive(Default)]
struct GraphState {
    drag: Option<DragState>,
    hover: Option<Hover>,
    q_flash: Option<(Instant, usize)>,
    last_add: Option<(Instant, usize)>,
    /// 拖拽最后一次向下游推送的值，用于推送合并。
    last_push: Option<(Instant, usize, f64, f64)>,
    cache: Option<CurveCache>,
    label_cache: Option<LabelCache>,
    spectrum_paths: Option<SpectrumPathCache>,
    curve_paths: Option<CurvePathCache>,
}

/// 已启用频段的响应叠加。旁路被忽略，好让 EQ 关闭时曲线仍可见以便编辑。
fn composite_db(config: &EqualizerSettings, rate: f64, frequency: f64) -> f64 {
    config
        .bands
        .iter()
        .take(MAX_EQ_BANDS)
        .filter(|band| band.enabled)
        .map(|band| band_response_db(band, rate, frequency))
        .sum()
}

pub(crate) fn dot_position(band: &EqBandSettings, plot: Bounds<Pixels>) -> Point<Pixels> {
    let x = freq_to_x(band.frequency as f32, plot.size.width.into());
    let gain = if band.kind.has_gain() {
        band.gain_db as f32
    } else {
        0.0
    };
    let y = db_to_y(gain, plot.size.height.into());
    point(plot.origin.x + px(x), plot.origin.y + px(y))
}

/// 不带坐标轴钳位的 `freq_to_x`：被推到轴外的频段开始拖拽时位置在绘图区之外，增量
/// 必须相对那个真实位置来计算。
fn freq_to_x_unclamped(freq: f32, width: f32) -> f32 {
    let t = (freq.log10() - MIN_FREQ.log10()) / (MAX_FREQ.log10() - MIN_FREQ.log10());
    t * width
}

/// 光标可及范围内最近的圆点，否则取合成曲线——前提是它的边缘足够接近。
fn hit_test(
    config: &EqualizerSettings,
    rate: f64,
    plot: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<Hover> {
    let mut nearest: Option<(usize, f32)> = None;
    for (index, band) in config.bands.iter().enumerate() {
        let center = dot_position(band, plot);
        let dx: f32 = (position.x - center.x).into();
        let dy: f32 = (position.y - center.y).into();
        let distance = dx.hypot(dy);
        if distance <= DOT_HIT_RADIUS && nearest.is_none_or(|(_, d)| distance < d) {
            nearest = Some((index, distance));
        }
    }
    if let Some((index, _)) = nearest {
        return Some(Hover::Dot(index));
    }

    let x: f32 = (position.x - plot.origin.x).into();
    let y: f32 = (position.y - plot.origin.y).into();
    let frequency = x_to_freq(x, plot.size.width.into());
    let curve_y = db_to_y_unclamped(
        composite_db(config, rate, frequency as f64) as f32,
        plot.size.height.into(),
    );
    if (y - curve_y).abs() <= CURVE_HIT_DISTANCE {
        Some(Hover::Curve(position))
    } else {
        None
    }
}

/// 每 2 个物理像素一个采样，密到分段看起来是平滑曲线。
fn sample_curve(width: f32, scale: f32, db_at: impl Fn(f64) -> f64) -> Rc<Vec<f32>> {
    let columns = ((width * scale) / 2.0).ceil().max(1.0) as usize;
    (0..columns)
        .map(|i| {
            let frequency = x_to_freq(i as f32 * 2.0 / scale, width.max(1.0));
            db_at(frequency as f64).clamp(CURVE_MIN_DB, CURVE_MAX_DB) as f32
        })
        .collect::<Vec<_>>()
        .into()
}

fn curve_path(builder: &mut PathBuilder, samples: &[f32], plot: Bounds<Pixels>, scale: f32) {
    for (i, db) in samples.iter().enumerate() {
        let x = plot.origin.x + px(i as f32 * 2.0 / scale);
        let y = plot.origin.y + px(db_to_y_unclamped(*db, plot.size.height.into()));
        if i == 0 {
            builder.move_to(point(x, y));
        } else {
            builder.line_to(point(x, y));
        }
    }
}

/// 频谱曲线的上边缘，做成 Catmull-Rom 样条，点沿宽度均匀铺开。
fn spectrum_path(builder: &mut PathBuilder, points: &[f32], plot: Bounds<Pixels>) {
    let width: f32 = plot.size.width.into();
    let height: f32 = plot.size.height.into();
    let last = points.len().saturating_sub(1);
    if last == 0 {
        return;
    }
    // 显示点下标 -> 绘图区局部像素
    let at = |i: usize| {
        (
            i as f32 / last as f32 * width,
            spectrum_db_to_y(points[i], height),
        )
    };
    let vertex = |(x, y): (f32, f32)| point(plot.origin.x + px(x), plot.origin.y + px(y));

    builder.move_to(vertex(at(0)));
    for i in 0..last {
        let (x0, y0) = at(i.saturating_sub(1));
        let (x1, y1) = at(i);
        let (x2, y2) = at(i + 1);
        let (x3, y3) = at((i + 2).min(last));
        let ctrl_a = vertex((x1 + (x2 - x0) / 6.0, y1 + (y2 - y0) / 6.0));
        let ctrl_b = vertex((x2 - (x3 - x1) / 6.0, y2 - (y3 - y1) / 6.0));
        builder.cubic_bezier_to(vertex((x2, y2)), ctrl_a, ctrl_b);
    }
}

/// 频谱曲线下直到绘图区底边的填充，空数据时为 None。
fn spectrum_fill_path(points: &[f32], plot: Bounds<Pixels>) -> Option<Path<Pixels>> {
    if points.is_empty() {
        return None;
    }
    let right = plot.origin.x + plot.size.width;
    let bottom_y = plot.origin.y + plot.size.height;
    let mut fill = PathBuilder::fill();
    spectrum_path(&mut fill, points, plot);
    fill.line_to(point(right, bottom_y));
    fill.line_to(point(plot.origin.x, bottom_y));
    fill.build().ok()
}

/// 用最新的分析器数据镶嵌出三条频谱路径。与绘制路径过去每帧构建的形状完全相同。
fn build_spectrum_paths(
    spectrum_pre: &[f32],
    spectrum_post: &[f32],
    plot: Bounds<Pixels>,
) -> EqSpectrumPaths {
    let mut paths = EqSpectrumPaths {
        pre_fill: spectrum_fill_path(spectrum_pre, plot),
        post_fill: spectrum_fill_path(spectrum_post, plot),
        post_stroke: None,
    };
    if !spectrum_post.is_empty() {
        let mut edge = PathBuilder::stroke(px(1.5));
        spectrum_path(&mut edge, spectrum_post, plot);
        paths.post_stroke = edge.build().ok();
    }
    paths
}

/// 从缓存的曲线镶嵌出三条响应曲线路径。
fn build_curve_paths(curves: &CurveCache, plot: Bounds<Pixels>, scale: f32) -> EqCurvePaths {
    let plot_height: f32 = plot.size.height.into();
    let zero_y = plot.origin.y + px(db_to_y(0.0, plot_height));
    let right = plot.origin.x + plot.size.width;
    let mut paths = EqCurvePaths::default();

    if let Some(band) = &curves.band {
        let mut builder = PathBuilder::stroke(px(1.5));
        curve_path(&mut builder, band, plot, scale);
        paths.band_stroke = builder.build().ok();
    }

    let mut fill = PathBuilder::fill();
    curve_path(&mut fill, &curves.composite, plot, scale);
    fill.line_to(point(right, zero_y));
    fill.line_to(point(plot.origin.x, zero_y));
    paths.composite_fill = fill.build().ok();

    let mut stroke = PathBuilder::stroke(px(2.0));
    curve_path(&mut stroke, &curves.composite, plot, scale);
    paths.composite_stroke = stroke.build().ok();

    paths
}

/// 全出血绘图区：坐标轴标签画在它内部，所以不预留边槽。只有面板那 1px 的上边框在
/// 绘图区之外。
fn plot_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(bounds.origin.x, bounds.origin.y + px(1.0)),
        size(bounds.size.width, bounds.size.height - px(1.0)),
    )
}

fn shape_label(window: &mut Window, text: SharedString, color: Hsla) -> ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    };
    window
        .text_system()
        .shape_line(text, px(12.0), &[run], None)
}

/// 静态的频率/dB 坐标轴标签及其绘图区局部锚点。
fn shape_axis_labels(
    window: &mut Window,
    label_color: Hsla,
    plot: Bounds<Pixels>,
) -> Vec<(ShapedLine, Point<Pixels>)> {
    let plot_width: f32 = plot.size.width.into();
    let plot_height: f32 = plot.size.height.into();

    // 坐标轴标签就在全出血绘图区内部：频率贴着底边，dB 值坐在左侧各自网格线上方
    let mut labels = Vec::new();
    for freq in LABEL_FREQS {
        let line = shape_label(window, format_hz(freq as f64).into(), label_color);
        let x = plot.origin.x + px(freq_to_x(freq, plot_width)) - line.width / 2.0;
        let x = x
            .min(plot.origin.x + plot.size.width - line.width - px(4.0))
            .max(plot.origin.x + px(4.0));
        labels.push((line, point(x, plot.origin.y + plot.size.height - px(16.0))));
    }
    for db in (-30..=30).step_by(6) {
        let text: SharedString = if db == 0 {
            "0".into()
        } else {
            format!("{db:+}").into()
        };
        let line = shape_label(window, text, label_color);
        let y = plot.origin.y + px(db_to_y(db as f32, plot_height)) - px(14.0);
        let y = y
            .min(plot.origin.y + plot.size.height - px(16.0))
            .max(plot.origin.y + px(2.0));
        labels.push((line, point(plot.origin.x + px(6.0), y)));
    }
    labels
}

/// 锚在圆点上方的小读数胶囊，水平方向钳进绘图区。
fn readout_pill(
    window: &mut Window,
    text: SharedString,
    color: Hsla,
    dot: Point<Pixels>,
    plot: Bounds<Pixels>,
) -> (ShapedLine, Bounds<Pixels>) {
    let line = shape_label(window, text, color);
    let width = line.width + px(12.0);
    // 用 min/max 而不是 clamp：区间反了时钉在边上，而不是 panic
    let x = (dot.x - width / 2.0)
        .min(plot.origin.x + plot.size.width - width)
        .max(plot.origin.x);
    let y = (dot.y - px(30.0)).max(plot.origin.y);
    (line, Bounds::new(point(x, y), size(width, px(20.0))))
}

pub struct EqGraph {
    id: ElementId,
    style: StyleRefinement,
    config: EqualizerSettings,
    sample_rate: f64,
    selected: Option<usize>,
    spectrum_pre: Rc<Vec<f32>>,
    spectrum_post: Rc<Vec<f32>>,
    on_band_change: Option<Rc<RefCell<BandChangeHandler>>>,
    on_select: Option<Rc<RefCell<SelectHandler>>>,
    on_remove: Option<Rc<RefCell<IndexHandler>>>,
    on_toggle_enabled: Option<Rc<RefCell<IndexHandler>>>,
    on_scroll_q: Option<Rc<RefCell<ScrollQHandler>>>,
    on_add: Option<Rc<RefCell<AddHandler>>>,
    on_drag_active: Option<Rc<RefCell<DragActiveHandler>>>,
    plot_slot: Option<PlotSlot>,
}

impl EqGraph {
    /// DSP 运行所用的设备采样率，好让画出的曲线与实际听感的滤波器一致。
    pub fn sample_rate(mut self, sample_rate: f64) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    /// 最新的分析器曲线，dB 值沿绘图宽度铺开；空 vec 表示什么都不画。
    pub fn spectrum(mut self, pre: Rc<Vec<f32>>, post: Rc<Vec<f32>>) -> Self {
        self.spectrum_pre = pre;
        self.spectrum_post = post;
        self
    }

    /// 拖拽圆点期间持续触发，带上该频段新的频率与增益。
    pub fn on_band_change(
        mut self,
        on_band_change: impl FnMut(usize, f64, f64, &mut App) + 'static,
    ) -> Self {
        self.on_band_change = Some(Rc::new(RefCell::new(on_band_change)));
        self
    }

    pub fn on_select(mut self, on_select: impl FnMut(Option<usize>, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(RefCell::new(on_select)));
        self
    }

    pub fn on_remove(mut self, on_remove: impl FnMut(usize, &mut App) + 'static) -> Self {
        self.on_remove = Some(Rc::new(RefCell::new(on_remove)));
        self
    }

    pub fn on_toggle_enabled(
        mut self,
        on_toggle_enabled: impl FnMut(usize, &mut App) + 'static,
    ) -> Self {
        self.on_toggle_enabled = Some(Rc::new(RefCell::new(on_toggle_enabled)));
        self
    }

    pub fn on_scroll_q(mut self, on_scroll_q: impl FnMut(usize, f64, &mut App) + 'static) -> Self {
        self.on_scroll_q = Some(Rc::new(RefCell::new(on_scroll_q)));
        self
    }

    /// 新增一个频段并返回它的下标，好让图表能立刻拖住新圆点。
    pub fn on_add(mut self, on_add: impl FnMut(f64, f64, &mut App) -> usize + 'static) -> Self {
        self.on_add = Some(Rc::new(RefCell::new(on_add)));
        self
    }

    pub fn on_drag_active(mut self, on_drag_active: impl FnMut(bool, &mut App) + 'static) -> Self {
        self.on_drag_active = Some(Rc::new(RefCell::new(on_drag_active)));
        self
    }

    /// 每帧接收绘图区域，好让浮层能锚定到某个频段的圆点。
    pub fn plot_slot(mut self, slot: PlotSlot) -> Self {
        self.plot_slot = Some(slot);
        self
    }
}

impl Styled for EqGraph {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for EqGraph {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub struct EqGraphPrepaint {
    hitbox: Hitbox,
    plot: Bounds<Pixels>,
    spectrum_paths: Rc<EqSpectrumPaths>,
    curve_paths: Rc<EqCurvePaths>,
    labels: Rc<Vec<(ShapedLine, Point<Pixels>)>>,
    dots: Vec<Point<Pixels>>,
    hover: Option<Hover>,
    drag: Option<DragState>,
    readout: Option<(ShapedLine, Bounds<Pixels>)>,
    flash: Option<(ShapedLine, Bounds<Pixels>)>,
}

impl Element for EqGraph {
    type RequestLayoutState = ();
    type PrepaintState = EqGraphPrepaint;

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
        style.size.width = relative(1.).into();
        style.size.height = px(420.0).into();
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
        let plot = plot_bounds(bounds);
        if let Some(slot) = &self.plot_slot
            && slot.get() != Some(plot)
        {
            slot.set(Some(plot));
            // 重渲染，让锚定的频段编辑器跟随新布局
            window.refresh();
        }
        let plot_width: f32 = plot.size.width.into();
        let plot_height: f32 = plot.size.height.into();
        let scale = window.scale_factor();

        let theme = cx.global::<Theme>();
        let label_color = theme.text_secondary.into_color();
        let text_color = theme.text.into_color();

        let config = &self.config;
        let selected = self.selected;
        let rate = self.sample_rate;
        let (drag, hover, q_flash, labels, spectrum_paths, curve_paths) = window
            .with_optional_element_state(id, |v, window| {
                let state: Rc<RefCell<GraphState>> = v.flatten().unwrap_or_default();
                {
                    let mut state_ref = state.borrow_mut();
                    let stale = state_ref.cache.as_ref().is_none_or(|cache| {
                        cache.config != *config
                            || cache.selected != selected
                            || cache.width != plot_width
                            || cache.height != plot_height
                            || cache.scale != scale
                            || cache.rate != rate
                    });
                    if stale {
                        // 每个已启用频段一个 Biquad，在所有列上求值
                        let biquads: Vec<Biquad> = config
                            .bands
                            .iter()
                            .take(MAX_EQ_BANDS)
                            .filter(|band| band.enabled)
                            .map(|band| {
                                Biquad::new(band.kind, rate, band.frequency, band.gain_db, band.q)
                            })
                            .collect();
                        let composite = sample_curve(plot_width, scale, |f| {
                            let omega = omega(f, rate);
                            biquads.iter().map(|b| b.magnitude_db(omega)).sum()
                        });
                        let band = selected
                            .and_then(|i| config.bands.get(i))
                            .filter(|band| band.enabled)
                            .map(|band| {
                                let biquad = Biquad::new(
                                    band.kind,
                                    rate,
                                    band.frequency,
                                    band.gain_db,
                                    band.q,
                                );
                                sample_curve(plot_width, scale, |f| {
                                    biquad.magnitude_db(omega(f, rate))
                                })
                            });
                        state_ref.cache = Some(CurveCache {
                            config: config.clone(),
                            selected,
                            width: plot_width,
                            height: plot_height,
                            scale,
                            rate,
                            composite,
                            band,
                        });
                    }
                }

                // 静态坐标轴标签：只在字体/颜色/DPI/几何变动时重新成形
                let font = window.text_style().font();
                let labels = {
                    let mut state_ref = state.borrow_mut();
                    let stale = state_ref.label_cache.as_ref().is_none_or(|cache| {
                        cache.font != font
                            || cache.color != label_color
                            || cache.scale != scale
                            || cache.origin != plot.origin
                            || cache.size != plot.size
                    });
                    if stale {
                        let lines = Rc::new(shape_axis_labels(window, label_color, plot));
                        state_ref.label_cache = Some(LabelCache {
                            font,
                            color: label_color,
                            scale,
                            origin: plot.origin,
                            size: plot.size,
                            lines,
                        });
                    }
                    state_ref
                        .label_cache
                        .as_ref()
                        .expect("label cache just rebuilt")
                        .lines
                        .clone()
                };

                // 镶嵌好的路径，两个独立记 key 的半边：频谱半边只在分析器数据或绘图
                // 几何移动时重建，曲线半边跟着曲线缓存走——播放中的频谱不会去动响应曲线
                let spectrum_paths = {
                    let mut state_ref = state.borrow_mut();
                    let stale = state_ref.spectrum_paths.as_ref().is_none_or(|cache| {
                        cache.origin != plot.origin
                            || cache.size != plot.size
                            || !Rc::ptr_eq(&cache.spectrum_pre, &self.spectrum_pre)
                            || !Rc::ptr_eq(&cache.spectrum_post, &self.spectrum_post)
                    });
                    if stale {
                        let paths =
                            build_spectrum_paths(&self.spectrum_pre, &self.spectrum_post, plot);
                        state_ref.spectrum_paths = Some(SpectrumPathCache {
                            origin: plot.origin,
                            size: plot.size,
                            spectrum_pre: self.spectrum_pre.clone(),
                            spectrum_post: self.spectrum_post.clone(),
                            paths: Rc::new(paths),
                        });
                    }
                    state_ref
                        .spectrum_paths
                        .as_ref()
                        .expect("spectrum path cache just rebuilt")
                        .paths
                        .clone()
                };
                let curve_paths = {
                    let mut state_ref = state.borrow_mut();
                    let stale = state_ref.curve_paths.as_ref().is_none_or(|cache| {
                        cache.config != *config
                            || cache.selected != selected
                            || cache.origin != plot.origin
                            || cache.size != plot.size
                            || cache.scale != scale
                            || cache.rate != rate
                    });
                    if stale {
                        let cache = &state_ref.cache.as_ref().expect("curve cache fresh");
                        let paths = build_curve_paths(cache, plot, scale);
                        state_ref.curve_paths = Some(CurvePathCache {
                            config: config.clone(),
                            selected,
                            origin: plot.origin,
                            size: plot.size,
                            scale,
                            rate,
                            paths: Rc::new(paths),
                        });
                    }
                    state_ref
                        .curve_paths
                        .as_ref()
                        .expect("curve path cache just rebuilt")
                        .paths
                        .clone()
                };

                let (drag, hover, q_flash) = {
                    let state_ref = state.borrow();
                    (state_ref.drag, state_ref.hover, state_ref.q_flash)
                };
                (
                    (drag, hover, q_flash, labels, spectrum_paths, curve_paths),
                    if id.is_some() { Some(state) } else { None },
                )
            });

        let dots: Vec<Point<Pixels>> = self
            .config
            .bands
            .iter()
            .map(|band| dot_position(band, plot))
            .collect();

        let readout = drag
            .and_then(|drag| self.config.bands.get(drag.index).map(|band| (drag, band)))
            .map(|(drag, band)| {
                let text: SharedString = if band.kind.has_gain() {
                    format!("{} · {:+.1} dB", format_hz(band.frequency), band.gain_db).into()
                } else {
                    format_hz(band.frequency).into()
                };
                readout_pill(window, text, text_color, dots[drag.index], plot)
            });

        let flash = q_flash
            .filter(|(at, _)| at.elapsed().as_millis() < Q_FLASH_MS)
            .and_then(|(_, i)| self.config.bands.get(i).map(|band| (i, band)))
            .map(|(i, band)| {
                let q = format!("{:.2}", band.q);
                let q = q.trim_end_matches('0').trim_end_matches('.');
                readout_pill(window, format!("Q {q}").into(), text_color, dots[i], plot)
            });

        EqGraphPrepaint {
            hitbox,
            plot,
            spectrum_paths,
            curve_paths,
            labels,
            dots,
            hover,
            drag,
            readout,
            flash,
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
        let panel_bg = theme.background_secondary;
        let panel_border = theme.border_color;
        let grid = theme.eq_grid_line;
        let grid_zero = theme.eq_grid_line_zero;
        let curve_color = theme.eq_curve;
        let curve_fill = theme.eq_curve_fill;
        let spectrum_pre = theme.eq_spectrum_pre;
        let spectrum_post = theme.eq_spectrum_post;
        let spectrum_edge = theme.eq_spectrum_edge;
        let band_color = theme.eq_band_curve;
        let dot_color = theme.eq_dot;
        let dot_selected = theme.eq_dot_selected;
        let dot_disabled = theme.eq_dot_disabled;
        let elevated_bg = theme.elevated_background;
        let elevated_border = theme.elevated_border_color;

        let plot = prepaint.plot;

        // 与窗口边缘齐平：直角，只有上边框把它与上方的分区标题分开
        window.paint_quad(quad(
            bounds,
            Corners::default(),
            panel_bg,
            Edges {
                top: px(1.0),
                ..Default::default()
            },
            panel_border,
            BorderStyle::Solid,
        ));

        let plot_width: f32 = plot.size.width.into();
        let plot_height: f32 = plot.size.height.into();
        for freq in GRID_FREQS {
            let x = plot.origin.x + px(freq_to_x(freq, plot_width));
            window.paint_quad(quad(
                Bounds::new(point(x, plot.origin.y), size(px(1.0), plot.size.height)),
                Corners::default(),
                grid,
                Edges::default(),
                rgba(0x000000),
                BorderStyle::Solid,
            ));
        }
        for db in (-30..=30).step_by(6) {
            let y = plot.origin.y + px(db_to_y(db as f32, plot_height));
            window.paint_quad(quad(
                Bounds::new(point(plot.origin.x, y), size(plot.size.width, px(1.0))),
                Corners::default(),
                if db == 0 { grid_zero } else { grid },
                Edges::default(),
                rgba(0x000000),
                BorderStyle::Solid,
            ));
        }

        window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
            // paint_path 按值接收路径，所以每条绘制路径都是镶嵌缓存的一次克隆；
            // 缓存命中仍然省掉 PathBuilder 的反复构建，频谱暂停时也省掉重建。
            let paths = &prepaint.spectrum_paths;
            if let Some(path) = &paths.pre_fill {
                window.paint_path(path.clone(), spectrum_pre);
            }
            if let Some(path) = &paths.post_fill {
                window.paint_path(path.clone(), spectrum_post);
            }
            if let Some(path) = &paths.post_stroke {
                // EQ 旁路时与响应曲线一起调暗
                let mut edge_color: Hsla = spectrum_edge.into_color();
                if !self.config.enabled {
                    edge_color.alpha *= 0.4;
                }
                window.paint_path(path.clone(), edge_color);
            }

            let paths = &prepaint.curve_paths;
            if let Some(path) = &paths.band_stroke {
                window.paint_path(path.clone(), band_color);
            }

            if let Some(path) = &paths.composite_fill {
                window.paint_path(path.clone(), curve_fill);
            }

            let mut stroke_color: Hsla = curve_color.into_color();
            if !self.config.enabled {
                stroke_color.alpha *= 0.4;
            }
            if let Some(path) = &paths.composite_stroke {
                window.paint_path(path.clone(), stroke_color);
            }
        });

        // 标签画在频谱与曲线之上，保证仍然可读
        for (line, origin) in prepaint.labels.iter() {
            if let Err(err) = line.paint(*origin, px(12.0), TextAlign::Left, None, window, cx) {
                error!("Failed to paint equalizer label: {:?}", err);
            }
        }

        if let Some(drag) = prepaint.drag
            && let Some(dot) = prepaint.dots.get(drag.index)
        {
            window.paint_quad(quad(
                Bounds::new(point(dot.x, plot.origin.y), size(px(1.0), plot.size.height)),
                Corners::default(),
                grid_zero,
                Edges::default(),
                rgba(0x000000),
                BorderStyle::Solid,
            ));
            window.paint_quad(quad(
                Bounds::new(point(plot.origin.x, dot.y), size(plot.size.width, px(1.0))),
                Corners::default(),
                grid_zero,
                Edges::default(),
                rgba(0x000000),
                BorderStyle::Solid,
            ));
        }

        for (index, dot) in prepaint.dots.iter().enumerate() {
            let band = self.config.bands[index];
            let is_selected = self.selected == Some(index);
            let color = if !band.enabled {
                dot_disabled
            } else if is_selected {
                dot_selected
            } else {
                dot_color
            };
            let fill = if is_selected { color } else { rgba(0x00000000) };

            if prepaint.hover == Some(Hover::Dot(index)) && prepaint.drag.is_none() {
                let ring = DOT_RADIUS + DOT_RING_PAD;
                window.paint_quad(quad(
                    Bounds::new(
                        point(dot.x - px(ring), dot.y - px(ring)),
                        size(px(ring * 2.0), px(ring * 2.0)),
                    ),
                    Corners::all(px(ring)),
                    rgba(0x00000000),
                    Edges::all(px(1.0)),
                    color,
                    BorderStyle::Solid,
                ));
            }
            window.paint_quad(quad(
                Bounds::new(
                    point(dot.x - px(DOT_RADIUS), dot.y - px(DOT_RADIUS)),
                    size(px(DOT_RADIUS * 2.0), px(DOT_RADIUS * 2.0)),
                ),
                Corners::all(px(DOT_RADIUS)),
                fill,
                Edges::all(px(1.5)),
                color,
                BorderStyle::Solid,
            ));
        }

        if let (Some(Hover::Curve(position)), None) = (prepaint.hover, prepaint.drag)
            && self.config.bands.len() < MAX_EQ_BANDS
        {
            let frequency = x_to_freq((position.x - plot.origin.x).into(), plot_width);
            let db = composite_db(&self.config, self.sample_rate, frequency as f64) as f32;
            let y = plot.origin.y + px(db_to_y_unclamped(db, plot_height));
            let mut ghost: Hsla = dot_selected.into_color();
            ghost.alpha *= 0.6;
            window.paint_quad(quad(
                Bounds::new(
                    point(position.x - px(DOT_RADIUS), y - px(DOT_RADIUS)),
                    size(px(DOT_RADIUS * 2.0), px(DOT_RADIUS * 2.0)),
                ),
                Corners::all(px(DOT_RADIUS)),
                rgba(0x00000000),
                Edges::all(px(1.5)),
                ghost,
                BorderStyle::Solid,
            ));
        }

        for readout in [&prepaint.readout, &prepaint.flash].into_iter().flatten() {
            let (line, pill) = readout;
            window.paint_quad(quad(
                *pill,
                Corners::all(px(4.0)),
                elevated_bg,
                Edges::all(px(1.0)),
                elevated_border,
                BorderStyle::Solid,
            ));
            if let Err(err) = line.paint(
                point(pill.origin.x + px(6.0), pill.origin.y + px(4.0)),
                px(12.0),
                TextAlign::Left,
                None,
                window,
                cx,
            ) {
                error!("Failed to paint equalizer readout: {:?}", err);
            }
        }
        if prepaint.flash.is_some() {
            window.request_animation_frame();
        }

        if prepaint.hover.is_some() || prepaint.drag.is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);
        }

        let hitbox = prepaint.hitbox.clone();
        let config = Rc::new(self.config.clone());
        let selected = self.selected;
        let rate = self.sample_rate;
        let on_band_change = self.on_band_change.clone();
        let on_select = self.on_select.clone();
        let on_remove = self.on_remove.clone();
        let on_toggle_enabled = self.on_toggle_enabled.clone();
        let on_scroll_q = self.on_scroll_q.clone();
        let on_add = self.on_add.clone();
        let on_drag_active = self.on_drag_active.clone();
        let has_id = id.is_some();

        window.with_optional_element_state(id, move |v, window| {
            let state: Rc<RefCell<GraphState>> = v.flatten().unwrap_or_default();

            {
                let state = state.clone();
                let config = config.clone();
                let hitbox = hitbox.clone();
                let on_select = on_select.clone();
                let on_drag_active = on_drag_active.clone();
                window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    if state.borrow().drag.is_some() {
                        return;
                    }

                    // 新增一个频段并立刻抓住它的圆点开始拖拽。
                    let add_band = |frequency: f64,
                                    gain_db: f64,
                                    position: Point<Pixels>,
                                    window: &mut Window,
                                    cx: &mut App| {
                        let Some(add) = &on_add else { return };
                        window.prevent_default();
                        cx.stop_propagation();
                        let index = (add.borrow_mut())(frequency, gain_db, cx);
                        {
                            let mut state = state.borrow_mut();
                            state.drag = Some(DragState {
                                index,
                                start: position,
                                start_frequency: frequency,
                                start_gain_db: gain_db,
                                shift: ev.modifiers.shift,
                                was_selected: false,
                                moved: false,
                            });
                            state.last_add = Some((Instant::now(), index));
                        }
                        if let Some(drag_active) = &on_drag_active {
                            (drag_active.borrow_mut())(true, cx);
                        }
                    };
                    // 频段编辑器打开时不新增频段
                    let editor_closed = selected.is_none();

                    match hit_test(&config, rate, plot, ev.position) {
                        Some(Hover::Dot(index)) => {
                            window.prevent_default();
                            cx.stop_propagation();
                            match ev.button {
                                MouseButton::Left if ev.click_count == 2 => {
                                    // 新增之后的第二次点击落在新圆点上，跳过它
                                    let fresh =
                                        state.borrow().last_add.is_some_and(|(at, band)| {
                                            band == index && at.elapsed().as_millis() < ADD_ECHO_MS
                                        });
                                    if !fresh {
                                        if let Some(toggle) = &on_toggle_enabled {
                                            (toggle.borrow_mut())(index, cx);
                                        }
                                        if let Some(select) = &on_select {
                                            (select.borrow_mut())(Some(index), cx);
                                        }
                                    }
                                }
                                MouseButton::Left => {
                                    if let Some(select) = &on_select {
                                        (select.borrow_mut())(Some(index), cx);
                                    }
                                    if let Some(band) = config.bands.get(index) {
                                        state.borrow_mut().drag = Some(DragState {
                                            index,
                                            start: ev.position,
                                            start_frequency: band.frequency,
                                            start_gain_db: band.gain_db,
                                            shift: ev.modifiers.shift,
                                            was_selected: selected == Some(index),
                                            moved: false,
                                        });
                                        if let Some(drag_active) = &on_drag_active {
                                            (drag_active.borrow_mut())(true, cx);
                                        }
                                    }
                                }
                                MouseButton::Right => {
                                    if let Some(remove) = &on_remove {
                                        (remove.borrow_mut())(index, cx);
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some(Hover::Curve(_))
                            if ev.button == MouseButton::Left
                                && ev.click_count == 1
                                && editor_closed
                                && config.bands.len() < MAX_EQ_BANDS =>
                        {
                            let width: f32 = plot.size.width.into();
                            let fx: f32 = (ev.position.x - plot.origin.x).into();
                            let frequency = x_to_freq(fx, width) as f64;
                            // 幽灵点预览的是已对齐的曲线值，要与它一致
                            let gain_db = composite_db(&config, rate, frequency)
                                .clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
                            add_band(frequency, gain_db, ev.position, window, cx);
                        }
                        _ => {
                            if ev.button != MouseButton::Left {
                                return;
                            }
                            // 点空白绘图区会在光标处创建一个频段，但仅限频段编辑器
                            // 关闭时——否则它是取消选中
                            if ev.click_count == 1
                                && editor_closed
                                && config.bands.len() < MAX_EQ_BANDS
                                && on_add.is_some()
                            {
                                let width: f32 = plot.size.width.into();
                                let height: f32 = plot.size.height.into();
                                let fx: f32 = (ev.position.x - plot.origin.x).into();
                                let fy: f32 = (ev.position.y - plot.origin.y).into();
                                let frequency = x_to_freq(fx, width) as f64;
                                let gain_db =
                                    f64::from(y_to_db(fy, height)).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
                                add_band(frequency, gain_db, ev.position, window, cx);
                            } else if let Some(select) = &on_select {
                                (select.borrow_mut())(None, cx);
                            }
                        }
                    }
                });
            }

            {
                let state = state.clone();
                let config = config.clone();
                let hitbox = hitbox.clone();
                let on_band_change = on_band_change.clone();
                let on_drag_active = on_drag_active.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }

                    let drag = state.borrow().drag;
                    if let Some(mut drag) = drag {
                        let Some(on_change) = &on_band_change else {
                            return;
                        };
                        let Some(band) = config.bands.get(drag.index) else {
                            // 频段在拖拽中途消失了，仍要释放视图的拖拽闩
                            state.borrow_mut().drag = None;
                            if let Some(drag_active) = &on_drag_active {
                                (drag_active.borrow_mut())(false, cx);
                            }
                            return;
                        };
                        let scale = if drag.shift { 0.1 } else { 1.0 };
                        let width: f32 = plot.size.width.into();
                        let dx: f32 = (ev.position.x - drag.start.x).into();
                        let dy: f32 = (ev.position.y - drag.start.y).into();
                        if !drag.moved && dx.abs() + dy.abs() > DRAG_START_THRESHOLD_PX {
                            drag.moved = true;
                            state.borrow_mut().drag = Some(drag);
                        }
                        if !drag.moved {
                            return;
                        }
                        // 起始频率可能在方向键微调后位于轴外，按不钳位的方式映射，
                        // 让增量始终相对那个真实位置
                        let start_x = freq_to_x_unclamped(drag.start_frequency as f32, width);
                        let frequency = x_to_freq(start_x + dx * scale, width) as f64;
                        let gain_db = if band.kind.has_gain() {
                            let height: f32 = plot.size.height.into();
                            let db = drag.start_gain_db
                                - f64::from(dy * scale) * 2.0 * f64::from(DB_WINDOW)
                                    / f64::from(height);
                            db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB)
                        } else {
                            band.gain_db
                        };
                        // Shift 的切换会把累计增量整体重新缩放，所以把拖拽原点重设到
                        // 当前值与当前位置上
                        if ev.modifiers.shift != drag.shift {
                            drag.shift = ev.modifiers.shift;
                            drag.start = ev.position;
                            drag.start_frequency = frequency;
                            drag.start_gain_db = gain_db;
                            state.borrow_mut().drag = Some(drag);
                        }
                        // 推送合并到约 30 Hz 并跳过重复值：每次推送都会把配置克隆进
                        // DSP 通道并重跑补偿网格
                        let now = Instant::now();
                        let (due, unchanged) = {
                            let state_ref = state.borrow();
                            (
                                state_ref.last_push.is_none_or(|(at, ..)| {
                                    now.duration_since(at) >= DRAG_PUSH_INTERVAL
                                }),
                                state_ref.last_push.is_some_and(|(_, index, f, g)| {
                                    index == drag.index && f == frequency && g == gain_db
                                }),
                            )
                        };
                        if due && !unchanged {
                            state.borrow_mut().last_push =
                                Some((now, drag.index, frequency, gain_db));
                            (on_change.borrow_mut())(drag.index, frequency, gain_db, cx);
                        }
                        return;
                    }

                    let hover = if hitbox.is_hovered(window) {
                        hit_test(&config, rate, plot, ev.position)
                    } else {
                        None
                    };
                    if state.borrow().hover != hover {
                        state.borrow_mut().hover = hover;
                        window.refresh();
                    }
                });
            }

            {
                let state = state.clone();
                let on_drag_active = on_drag_active.clone();
                let on_select = on_select.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some(drag) = state.borrow_mut().drag.take() else {
                        return;
                    };
                    // 下一次拖拽从全新的推送预算开始
                    state.borrow_mut().last_push = None;
                    if let Some(drag_active) = &on_drag_active {
                        (drag_active.borrow_mut())(false, cx);
                    }
                    // 从未离开已选中圆点的一次点击会取消选中，但双击的第二次 mouseup
                    // 不行——它会重新打开编辑器
                    if !drag.moved
                        && drag.was_selected
                        && ev.click_count <= 1
                        && let Some(select) = &on_select
                    {
                        (select.borrow_mut())(None, cx);
                    }
                });
            }

            {
                let state = state.clone();
                let config = config.clone();
                let hitbox = hitbox.clone();
                window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    let Some(on_scroll_q) = &on_scroll_q else {
                        return;
                    };
                    let target = match state.borrow().hover {
                        Some(Hover::Dot(index)) => Some(index),
                        _ => selected,
                    };
                    let Some((target, band)) =
                        target.and_then(|i| config.bands.get(i).map(|band| (i, band)))
                    else {
                        return;
                    };

                    window.prevent_default();
                    cx.stop_propagation();

                    let notches: f32 = if ev.delta.precise() {
                        let dy: f32 = ev.delta.pixel_delta(px(1.0)).y.into();
                        dy / 20.0
                    } else {
                        ev.delta.pixel_delta(px(1.0)).y.into()
                    };
                    (on_scroll_q.borrow_mut())(target, scroll_q(band.q, f64::from(notches)), cx);
                    state.borrow_mut().q_flash = Some((Instant::now(), target));
                    window.refresh();
                });
            }

            ((), if has_id { Some(state) } else { None })
        });
    }
}

pub fn eq_graph(id: impl Into<ElementId>, config: &EqualizerSettings) -> EqGraph {
    EqGraph {
        id: id.into(),
        style: StyleRefinement::default(),
        config: config.clone(),
        sample_rate: 48_000.0,
        selected: None,
        spectrum_pre: Rc::default(),
        spectrum_post: Rc::default(),
        on_band_change: None,
        on_select: None,
        on_remove: None,
        on_toggle_enabled: None,
        on_scroll_q: None,
        on_add: None,
        on_drag_active: None,
        plot_slot: None,
    }
}
