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
/// Gap between a dot and its hover ring.
const DOT_RING_PAD: f32 = 3.0;
const DOT_HIT_RADIUS: f32 = 16.0;
const CURVE_HIT_DISTANCE: f32 = 8.0;
const Q_FLASH_MS: u128 = 600;
/// Pointer travel before a press counts as a drag rather than a click.
const DRAG_START_THRESHOLD_PX: f32 = 3.0;
/// A click this soon after an add is the double-click's second press landing
/// on the new dot, not a toggle.
const ADD_ECHO_MS: u128 = 500;

const GRID_FREQS: [f32; 13] = [
    20.0, 30.0, 50.0, 100.0, 200.0, 300.0, 500.0, 1_000.0, 2_000.0, 3_000.0, 5_000.0, 10_000.0,
    20_000.0,
];
// decade and 5x marks get text labels
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

/// Slot the graph writes its plot area into each frame, so the view can anchor popovers to dots.
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

/// Cached axis-label shapes. The 17 labels are static text, so they are only
/// re-shaped when the font, label color, DPI scale or plot geometry changes.
struct LabelCache {
    font: Font,
    color: Hsla,
    scale: f32,
    origin: Point<Pixels>,
    size: Size<Pixels>,
    lines: Rc<Vec<(ShapedLine, Point<Pixels>)>>,
}

/// How often a drag pushes new values downstream: mouse moves arrive at
/// 125-1000 Hz and every push used to clone the config into the DSP channel
/// and re-run the compensation grid.
const DRAG_PUSH_INTERVAL: Duration = Duration::from_millis(33);

/// Built tessellations for one stable frame, reused while nothing visible
/// changed. Painting still hands the scene its own copy of the painted path
/// (paint_path takes it by value), but a cache hit skips PathBuilder's
/// intermediate buffers and the curve sampling a rebuild would redo — a
/// static page (paused spectrum, no drag) then tessellates nothing at all.
#[derive(Default)]
struct EqGraphPaths {
    spectrum_pre_fill: Option<Path<Pixels>>,
    spectrum_post_fill: Option<Path<Pixels>>,
    spectrum_post_stroke: Option<Path<Pixels>>,
    band_stroke: Option<Path<Pixels>>,
    composite_fill: Option<Path<Pixels>>,
    composite_stroke: Option<Path<Pixels>>,
}

struct PathCache {
    config: EqualizerSettings,
    selected: Option<usize>,
    origin: Point<Pixels>,
    size: Size<Pixels>,
    scale: f32,
    rate: f64,
    spectrum_pre: Rc<Vec<f32>>,
    spectrum_post: Rc<Vec<f32>>,
    paths: Rc<EqGraphPaths>,
}

#[derive(Default)]
struct GraphState {
    drag: Option<DragState>,
    hover: Option<Hover>,
    q_flash: Option<(Instant, usize)>,
    last_add: Option<(Instant, usize)>,
    /// The last values a drag pushed downstream, for the push coalescing.
    last_push: Option<(Instant, usize, f64, f64)>,
    cache: Option<CurveCache>,
    label_cache: Option<LabelCache>,
    paths: Option<PathCache>,
}

/// Summed response of the enabled bands. Bypass ignored so the curve stays
/// visible for editing while the EQ is off.
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

/// `freq_to_x` without the axis clamp: a band nudged off-axis starts its drag
/// outside the plot, and the delta must stay relative to that real spot.
fn freq_to_x_unclamped(freq: f32, width: f32) -> f32 {
    let t = (freq.log10() - MIN_FREQ.log10()) / (MAX_FREQ.log10() - MIN_FREQ.log10());
    t * width
}

/// Nearest dot within reach of the cursor, else the composite curve when its
/// edge passes close enough.
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

/// One sample per 2 physical pixels, dense enough that segments read as a smooth curve.
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

/// Top edge of a spectrum curve as a Catmull-Rom spline, points spread evenly across the width.
fn spectrum_path(builder: &mut PathBuilder, points: &[f32], plot: Bounds<Pixels>) {
    let width: f32 = plot.size.width.into();
    let height: f32 = plot.size.height.into();
    let last = points.len().saturating_sub(1);
    if last == 0 {
        return;
    }
    // plot-local pixels for a display point index
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

/// Fill under a spectrum curve down to the plot's bottom edge, None for empty data.
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

/// Tessellates the six paintable paths from the cached curves and the latest
/// spectrum data. Same shapes the paint path used to build per frame.
fn build_paths(
    curves: &CurveCache,
    spectrum_pre: &[f32],
    spectrum_post: &[f32],
    plot: Bounds<Pixels>,
    scale: f32,
) -> EqGraphPaths {
    let plot_height: f32 = plot.size.height.into();
    let zero_y = plot.origin.y + px(db_to_y(0.0, plot_height));
    let right = plot.origin.x + plot.size.width;
    let mut paths = EqGraphPaths::default();

    paths.spectrum_pre_fill = spectrum_fill_path(spectrum_pre, plot);
    paths.spectrum_post_fill = spectrum_fill_path(spectrum_post, plot);
    if !spectrum_post.is_empty() {
        let mut edge = PathBuilder::stroke(px(1.5));
        spectrum_path(&mut edge, spectrum_post, plot);
        paths.spectrum_post_stroke = edge.build().ok();
    }

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

/// Full-bleed plot: the axis labels are drawn inside it, so no gutter is
/// reserved. Only the panel's 1px top border stays outside the plot.
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

/// The static frequency/dB axis labels with their plot-local anchor points.
fn shape_axis_labels(
    window: &mut Window,
    label_color: Hsla,
    plot: Bounds<Pixels>,
) -> Vec<(ShapedLine, Point<Pixels>)> {
    let plot_width: f32 = plot.size.width.into();
    let plot_height: f32 = plot.size.height.into();

    // axis labels live inside the full-bleed plot: frequencies hug the
    // bottom edge, dB values sit just above their grid line at the left
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

/// Small readout pill anchored above a dot, clamped into the plot horizontally.
fn readout_pill(
    window: &mut Window,
    text: SharedString,
    color: Hsla,
    dot: Point<Pixels>,
    plot: Bounds<Pixels>,
) -> (ShapedLine, Bounds<Pixels>) {
    let line = shape_label(window, text, color);
    let width = line.width + px(12.0);
    // min/max instead of clamp, an inverted range pins to the edge rather than panicking
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
    /// Device rate the DSP runs at, so the drawn curve matches the audible filter.
    pub fn sample_rate(mut self, sample_rate: f64) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    /// Latest analyzer curves, dB values spread across the plot width, empty vecs paint nothing.
    pub fn spectrum(mut self, pre: Rc<Vec<f32>>, post: Rc<Vec<f32>>) -> Self {
        self.spectrum_pre = pre;
        self.spectrum_post = post;
        self
    }

    /// Fires continuously while a dot is dragged, with the band's new frequency and gain.
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

    /// Adds a band and returns its index, so the graph can immediately drag the new dot.
    pub fn on_add(mut self, on_add: impl FnMut(f64, f64, &mut App) -> usize + 'static) -> Self {
        self.on_add = Some(Rc::new(RefCell::new(on_add)));
        self
    }

    pub fn on_drag_active(mut self, on_drag_active: impl FnMut(bool, &mut App) + 'static) -> Self {
        self.on_drag_active = Some(Rc::new(RefCell::new(on_drag_active)));
        self
    }

    /// Receives the plot area each frame so popovers can anchor to a band dot.
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
    paths: Rc<EqGraphPaths>,
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
            // re-render so the anchored band editor tracks the new layout
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
        let (drag, hover, q_flash, labels, paths) =
            window.with_optional_element_state(id, |v, window| {
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
                        // one Biquad per enabled band, evaluated across all columns
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

                // static axis labels: re-shaped only when font/color/DPI/geometry move
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

                // tessellated paths: rebuilt only when the curves, the
                // spectrum data or the plot geometry moved
                let paths = {
                    let mut state_ref = state.borrow_mut();
                    let stale = state_ref.paths.as_ref().is_none_or(|cache| {
                        cache.config != *config
                            || cache.selected != selected
                            || cache.origin != plot.origin
                            || cache.size != plot.size
                            || cache.scale != scale
                            || cache.rate != rate
                            || !Rc::ptr_eq(&cache.spectrum_pre, &self.spectrum_pre)
                            || !Rc::ptr_eq(&cache.spectrum_post, &self.spectrum_post)
                    });
                    if stale {
                        let cache = &state_ref.cache.as_ref().expect("curve cache fresh");
                        let paths = build_paths(
                            cache,
                            &self.spectrum_pre,
                            &self.spectrum_post,
                            plot,
                            scale,
                        );
                        state_ref.paths = Some(PathCache {
                            config: config.clone(),
                            selected,
                            origin: plot.origin,
                            size: plot.size,
                            scale,
                            rate,
                            spectrum_pre: self.spectrum_pre.clone(),
                            spectrum_post: self.spectrum_post.clone(),
                            paths: Rc::new(paths),
                        });
                    }
                    state_ref
                        .paths
                        .as_ref()
                        .expect("path cache just rebuilt")
                        .paths
                        .clone()
                };

                let (drag, hover, q_flash) = {
                    let state_ref = state.borrow();
                    (state_ref.drag, state_ref.hover, state_ref.q_flash)
                };
                (
                    (drag, hover, q_flash, labels, paths),
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
            paths,
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

        // flush with the window edges: square corners, only a top border
        // separates the panel from the section header above
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
            // paint_path takes the path by value, so each painted path is one
            // clone from the tessellation cache; the cache hit still saves the
            // PathBuilder churn and, with the spectrum paused, the rebuilds.
            let paths = &prepaint.paths;
            if let Some(path) = &paths.spectrum_pre_fill {
                window.paint_path(path.clone(), spectrum_pre);
            }
            if let Some(path) = &paths.spectrum_post_fill {
                window.paint_path(path.clone(), spectrum_post);
            }
            if let Some(path) = &paths.spectrum_post_stroke {
                // dimmed alongside the response curve while the EQ is bypassed
                let mut edge_color: Hsla = spectrum_edge.into_color();
                if !self.config.enabled {
                    edge_color.alpha *= 0.4;
                }
                window.paint_path(path.clone(), edge_color);
            }

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

        // labels sit on top of the spectrum and curve so they stay readable
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

                    // Adds a band and grabs its dot for an immediate drag.
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
                    // no new bands while the band editor is open
                    let editor_closed = selected.is_none();

                    match hit_test(&config, rate, plot, ev.position) {
                        Some(Hover::Dot(index)) => {
                            window.prevent_default();
                            cx.stop_propagation();
                            match ev.button {
                                MouseButton::Left if ev.click_count == 2 => {
                                    // the second click of an add lands on the new dot, skip it
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
                            // the ghost previews the snapped curve value, match it
                            let gain_db = composite_db(&config, rate, frequency)
                                .clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
                            add_band(frequency, gain_db, ev.position, window, cx);
                        }
                        _ => {
                            if ev.button != MouseButton::Left {
                                return;
                            }
                            // a click on empty plot creates a band at the cursor, but
                            // only with the band editor closed — otherwise it deselects
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
                            // the band vanished mid-drag, still release the view's drag latch
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
                        // the start frequency can sit off-axis after an arrow-key nudge,
                        // map it unclamped so the delta stays relative to the real spot
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
                        // a shift toggle would rescale the whole accumulated delta, so rebase
                        // the drag origin onto the current value and position
                        if ev.modifiers.shift != drag.shift {
                            drag.shift = ev.modifiers.shift;
                            drag.start = ev.position;
                            drag.start_frequency = frequency;
                            drag.start_gain_db = gain_db;
                            state.borrow_mut().drag = Some(drag);
                        }
                        // coalesce the push to ~30 Hz and skip repeat values:
                        // each push clones the config into the DSP channel and
                        // re-runs the compensation grid
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
                    // the next drag starts with a fresh push budget
                    state.borrow_mut().last_push = None;
                    if let Some(drag_active) = &on_drag_active {
                        (drag_active.borrow_mut())(false, cx);
                    }
                    // a click that never moved off the selected dot dismisses it, but a
                    // double-click's second mouseup must not, it reopens the editor
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
