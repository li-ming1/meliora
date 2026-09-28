//! Full-screen immersive listening view: a blurred cover ambience backdrop,
//! large synced lyrics, a slim spectrum ribbon and auto-hiding transport
//! controls. Lives inside the main window — `MainWindow::render` swaps the
//! whole library layout for this view while `Models::immersive` is set — and
//! the OS fullscreen toggle rides along via `player::ToggleImmersive`.
//!
//! Animation discipline (see `GPUI_HARDCORE_PERFORMANCE.md` and the
//! 2026-09-27 glyph-atlas lesson in `scroll_follow.rs`):
//! - every frame is data-driven (spectrum/position broadcasts) or
//!   transition-driven (line glide, controls fade), with the frame loop
//!   stopped the moment nothing animates any more;
//! - lyric glyph geometry never interpolates: font sizes come from a
//!   discrete set and line offsets snap to whole pixels, only colors lerp;
//! - the ambience backdrop is a small (≤160px) decode stretched over the
//!   window — upscale softness plus a modest blur radius, so the GPU blur
//!   pass never sees a full-resolution source.

use std::{
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, MouseMoveEvent, ObjectFit, ParentElement, PathBuilder, Render,
    Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription, Window, canvas,
    div, linear_color_stop, linear_gradient, point, px, relative,
};
use tracing::warn;

use crate::{
    playback::{interface::PlaybackInterface, queue::QueueItemData, thread::PlaybackState},
    settings::SettingsGlobal,
    ui::{
        app::Pool,
        components::{
            icons::{MINIMIZE, NEXT_TRACK, PAUSE, PLAY, PREV_TRACK, icon},
            managed_image::{ManagedImageKey, managed_image},
            tooltip::build_tooltip,
        },
        equalizer::{
            mapping::spectrum_db_to_y,
            spectrum::{SpectrumData, SpectrumState, ensure_analyzer},
        },
        lyrics::{Lyrics, lerp_color},
        models::{CurrentTrack, Models, PlaybackInfo},
        scroll_follow::ease_out_cubic,
        theme::Theme,
        util::format_duration,
    },
};

/// Vertical distance between lyric lines, in px.
const LINE_PITCH_PX: f32 = 64.0;
/// Lyric font sizes — discrete glyph-atlas-safe levels only.
const CURRENT_LINE_SIZE: f32 = 36.0;
const NEIGHBOR_LINE_SIZE: f32 = 24.0;
const TRANSLATION_SIZE: f32 = 15.0;
/// Lyric lines rendered on either side of the active one.
const LINE_WINDOW: i64 = 2;
/// Height of the lyric viewport: five pitches (window + headroom).
const LYRICS_VIEWPORT_HEIGHT: f32 = LINE_PITCH_PX * 5.0;
const LINE_ANIMATION: Duration = Duration::from_millis(320);
/// Controls hide after this much pointer inactivity (checked on the position
/// tick, so the view never polls while paused).
const CONTROLS_IDLE_HIDE: Duration = Duration::from_secs(3);
const CONTROLS_FADE: Duration = Duration::from_millis(300);
/// Central cover as a fraction of the viewport height.
const COVER_FRACTION: f32 = 0.30;
/// Ambience backdrop decode cap — small source, stretched soft.
const BACKDROP_THUMB_PX: u32 = 160;
/// Accent extraction decode size; 64px is plenty for a 12-bit histogram.
const ACCENT_THUMB_PX: u32 = 64;
/// Spectrum ribbon: dB floor below which a frame counts as silence.
const RIBBON_SILENCE_DB: f32 = -89.0;

/// Enters or leaves immersive mode: flips [`Models::immersive`] and toggles
/// the main window's OS fullscreen state in step. Every entry point (keybind,
/// playbar button, exit button) funnels through here.
pub fn set_immersive(entering: bool, cx: &mut App) {
    let immersive = cx.global::<Models>().immersive.clone();
    if *immersive.read(cx) == entering {
        return;
    }
    immersive.write(cx, entering);
    crate::ui::app::toggle_main_window_fullscreen(cx);
}

pub struct ImmersiveView {
    focus_handle: FocusHandle,
    /// Mirrors `Models::immersive`; drives the spectrum viewer registration
    /// and gates every observer's `notify`.
    active: bool,

    position: Entity<u64>,
    duration: Entity<u64>,
    playback_state: Entity<PlaybackState>,
    current_track: Entity<Option<CurrentTrack>>,
    /// `(published spectrum, tap viewers)` — `None` until the analyzer exists.
    spectrum: Option<(Entity<SpectrumData>, Arc<AtomicUsize>)>,
    spectrum_viewing: bool,

    // Track presentation, (re)resolved on every track change while active.
    image_key: Option<ManagedImageKey>,
    image_element_key: u64,
    track_name: Option<SharedString>,
    artist_name: Option<SharedString>,
    meta_subscription: Option<Subscription>,
    accent: Option<Rgba>,
    /// Which track path the pending/existing accent belongs to.
    accent_for: Option<Option<PathBuf>>,

    // Lyrics (parsed off-thread, loader shared with the sidebar panel).
    parsed: Option<Rc<Vec<crate::ui::lyrics::lrc::LrcLine>>>,
    lyrics_generation: u64,
    current_line: Option<usize>,
    /// Animated "active line" position, glided between line indices so line
    /// changes read as a vertical glide. Whole-pixel snapped per line at
    /// render time.
    visual_line: f32,
    line_anim: Option<(f32, f32, Instant)>,

    // Auto-hiding controls.
    controls_visible: bool,
    controls_opacity: f32,
    hide_anim: Option<(f32, f32, Instant)>,
    last_activity: Instant,
    frame_scheduled: bool,
}

impl Focusable for ImmersiveView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ImmersiveView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            let immersive_flag = cx.global::<Models>().immersive.clone();

            // Fullscreen toggling itself lives in `set_immersive` (it needs
            // the window handle); this observer only mirrors the flag into
            // the view's own state.
            cx.observe(&immersive_flag, |this: &mut ImmersiveView, flag, cx| {
                this.set_active(*flag.read(cx), cx);
            })
            .detach();

            cx.observe(&info.position, |this, pos, cx| {
                if !this.active {
                    return;
                }
                let pos_ms = *pos.read(cx);
                this.track_lyric_line(pos_ms);
                if this.controls_visible
                    && this.hide_anim.is_none()
                    && this.last_activity.elapsed() >= CONTROLS_IDLE_HIDE
                {
                    this.start_controls_fade(false);
                }
                // Drives the progress bar (and any in-flight transition).
                cx.notify();
            })
            .detach();

            cx.observe(&info.duration, |this, _, cx| {
                if this.active {
                    cx.notify();
                }
            })
            .detach();

            cx.observe(&info.playback_state, |this, _, cx| {
                if this.active {
                    cx.notify();
                }
            })
            .detach();

            cx.observe(&info.current_track, |this, _, cx| {
                if this.active {
                    this.reload_track_content(cx);
                }
            })
            .detach();

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                active: *immersive_flag.read(cx),
                position: info.position.clone(),
                duration: info.duration.clone(),
                playback_state: info.playback_state.clone(),
                current_track: info.current_track.clone(),
                spectrum: None,
                spectrum_viewing: false,
                image_key: None,
                image_element_key: 0,
                track_name: None,
                artist_name: None,
                meta_subscription: None,
                accent: None,
                accent_for: None,
                parsed: None,
                lyrics_generation: 0,
                current_line: None,
                visual_line: -1.0,
                line_anim: None,
                controls_visible: true,
                controls_opacity: 1.0,
                hide_anim: None,
                last_activity: Instant::now(),
                frame_scheduled: false,
            };
            if this.active {
                this.activate(cx);
            }
            this
        })
    }

    /// `Models::immersive` flipped on/off while the view stays alive inside
    /// `MainWindow`.
    fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.activate(cx);
        } else {
            self.deactivate();
        }
        cx.notify();
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        // Spectrum publishes only while someone is watching (viewer-gated).
        ensure_analyzer(cx);
        if self.spectrum.is_none() && cx.has_global::<SpectrumState>() {
            let (data, viewers) = {
                let state = cx.global::<SpectrumState>();
                (state.data.clone(), state.viewers.clone())
            };
            self.spectrum = Some((data, viewers));
        }
        if let Some((_, viewers)) = &self.spectrum {
            viewers.fetch_add(1, Ordering::Relaxed);
            self.spectrum_viewing = true;
        }

        self.last_activity = Instant::now();
        self.controls_visible = true;
        self.controls_opacity = 1.0;
        self.hide_anim = None;
        self.reload_track_content(cx);
    }

    fn deactivate(&mut self) {
        if self.spectrum_viewing {
            if let Some((_, viewers)) = &self.spectrum {
                viewers.fetch_sub(1, Ordering::Relaxed);
            }
            self.spectrum_viewing = false;
        }
        self.line_anim = None;
        self.hide_anim = None;
    }

    /// Re-resolves cover/metadata/lyrics/accent for whatever is playing now.
    fn reload_track_content(&mut self, cx: &mut Context<Self>) {
        self.lyrics_generation += 1;
        let generation = self.lyrics_generation;
        let track_path = self
            .current_track
            .read(cx)
            .as_ref()
            .map(|track| track.get_path().clone());

        self.image_element_key += 1;
        self.image_key = Self::cover_key(cx);
        self.accent = None;
        self.accent_for = Some(track_path.clone());
        self.track_name = None;
        self.artist_name = None;
        self.parsed = None;
        self.current_line = None;
        self.visual_line = -1.0;
        self.line_anim = None;

        // Metadata comes from the queue item at the playback position (the
        // single source of truth for "what is playing", as in InfoSection).
        drop(self.meta_subscription.take());
        if let Some(item) = Self::current_queue_item(cx) {
            let data = item.get_data(cx);
            let item_path = item.get_path().clone();
            if let Some(ui_data) = data.read(cx).clone() {
                self.adopt_names(ui_data);
            }
            self.meta_subscription = Some(cx.observe(&data, move |this, data, cx| {
                if !this.active {
                    return;
                }
                let is_current = this
                    .current_track
                    .read(cx)
                    .as_ref()
                    .is_some_and(|track| track.get_path() == &item_path);
                if is_current && let Some(ui_data) = data.read(cx).clone() {
                    this.adopt_names(ui_data);
                    cx.notify();
                }
            }));
        }

        // Accent color: one small decode per track, analyzed off-thread.
        if let Some(key) = self.image_key.clone() {
            let pool = cx.global::<Pool>().0.clone();
            let expected = track_path.clone();
            cx.spawn(async move |this, cx| {
                let accent = crate::RUNTIME
                    .spawn(async move {
                        key.retrieve(pool, ACCENT_THUMB_PX, true)
                            .await
                            .ok()
                            .flatten()
                            .and_then(|image| extract_accent(&image))
                    })
                    .await
                    .unwrap_or_else(|error| {
                        warn!(%error, "immersive: accent task failed");
                        None
                    });
                this.update(cx, |this, cx| {
                    if this.active && this.accent_for == Some(expected) {
                        this.accent = accent;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }

        // Lyrics: same off-thread loader the sidebar panel uses.
        if let Some(path) = track_path {
            let pool = cx.global::<Pool>().0.clone();
            cx.spawn(async move |this, cx| {
                let loaded = crate::RUNTIME
                    .spawn(async move { Lyrics::load_lyrics_off_thread(&pool, path).await })
                    .await;
                this.update(cx, |this, cx| {
                    if this.lyrics_generation != generation {
                        return;
                    }
                    this.parsed = loaded.ok().and_then(|(_, parsed)| parsed).map(Rc::new);
                    let pos_ms = *this.position.read(cx);
                    this.current_line = None;
                    this.track_lyric_line(pos_ms);
                    this.line_anim = None;
                    this.visual_line = this.current_line.map_or(-1.0, |line| line as f32);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    fn adopt_names(&mut self, data: crate::playback::queue::QueueItemUIData) {
        if self.track_name.is_none() {
            self.track_name = data.name;
        }
        if self.artist_name.is_none() {
            self.artist_name = data.artist_name;
        }
    }

    fn current_queue_item(cx: &App) -> Option<QueueItemData> {
        let queue = cx.global::<Models>().queue.read(cx);
        queue
            .data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(queue.position)
            .cloned()
    }

    /// Cover source for the current track: online cover URL first, then the
    /// file's own embedded art / sidecar files (mirrors InfoSection minus
    /// the library-id route).
    fn cover_key(cx: &mut App) -> Option<ManagedImageKey> {
        let item = Self::current_queue_item(cx)?;
        #[cfg(feature = "online_sources")]
        if let Some(url) = item
            .get_data(cx)
            .read(cx)
            .clone()
            .and_then(|data| data.cover_url)
            .filter(|url| !url.is_empty())
        {
            return Some(ManagedImageKey::HttpCover(url));
        }
        Some(ManagedImageKey::TrackFile(item.get_path().clone()))
    }

    fn track_lyric_line(&mut self, pos_ms: u64) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
        let new_line = if idx == 0 { None } else { Some(idx - 1) };
        if new_line == self.current_line {
            return;
        }
        let from = self.visual_line;
        let target = new_line.map_or(-1.0, |line| line as f32);
        self.current_line = new_line;
        if (target - from).abs() < 0.01 {
            self.visual_line = target;
            self.line_anim = None;
        } else {
            self.line_anim = Some((from, target, Instant::now()));
        }
    }

    fn start_controls_fade(&mut self, visible: bool) {
        let from = self.controls_opacity;
        let to = if visible { 1.0 } else { 0.0 };
        self.controls_visible = visible;
        if (to - from).abs() < 0.01 {
            self.controls_opacity = to;
            self.hide_anim = None;
            return;
        }
        self.hide_anim = Some((from, to, Instant::now()));
    }

    fn needs_animation_frame(&self) -> bool {
        self.line_anim.is_some() || self.hide_anim.is_some()
    }

    fn schedule_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.frame_scheduled {
            return;
        }
        self.frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.frame_scheduled = false;
            this.advance_animations(window, cx);
        });
    }

    fn advance_animations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;

        if let Some((from, target, started_at)) = self.line_anim {
            if reduced_motion {
                self.visual_line = target;
                self.line_anim = None;
                changed = true;
            } else {
                let progress = (started_at.elapsed().as_secs_f32() / LINE_ANIMATION.as_secs_f32())
                    .clamp(0.0, 1.0);
                self.visual_line = from + (target - from) * ease_out_cubic(progress);
                changed = true;
                if progress >= 1.0 {
                    self.visual_line = target;
                    self.line_anim = None;
                }
            }
        }

        if let Some((from, target, started_at)) = self.hide_anim {
            if reduced_motion {
                self.controls_opacity = target;
                self.hide_anim = None;
                changed = true;
            } else {
                let progress = (started_at.elapsed().as_secs_f32() / CONTROLS_FADE.as_secs_f32())
                    .clamp(0.0, 1.0);
                self.controls_opacity = from + (target - from) * ease_out_cubic(progress);
                changed = true;
                if progress >= 1.0 {
                    self.controls_opacity = target;
                    self.hide_anim = None;
                }
            }
        }

        if !reduced_motion && self.needs_animation_frame() {
            self.schedule_frame(window, cx);
        }
        if changed {
            cx.notify();
        }
    }
}

/// The lyric lines inside the window around the animated active line:
/// `(line index, whole-pixel top offset, is_current)`. Pure so the clamp and
/// snap math is unit-testable.
fn lyric_window(visual_line: f32, parsed_len: usize) -> Vec<(usize, f32, bool)> {
    let center = visual_line.round();
    let len = parsed_len as i64;
    (-LINE_WINDOW..=LINE_WINDOW)
        .filter_map(move |offset| {
            let offset = offset as f32;
            let index = center + offset;
            if index < 0.0 || index >= len as f32 {
                return None;
            }
            let index = index as usize;
            let top = ((index as f32 - visual_line) + 2.0) * LINE_PITCH_PX;
            Some((index, top.round(), index as f32 == visual_line))
        })
        .collect()
}

impl Render for ImmersiveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keyboard focus for the Esc binding (AboutDialog pattern; idempotent).
        self.focus_handle.focus(window, cx);

        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;
        if self.needs_animation_frame() {
            if reduced_motion {
                self.advance_animations(window, cx);
            } else {
                self.schedule_frame(window, cx);
            }
        }

        let theme = cx.global::<Theme>();
        let text = theme.text;
        let text_secondary = theme.text_secondary;
        let accent = self.accent.unwrap_or(theme.text);
        let image_gen = self.image_element_key;

        let position_s = (*self.position.read(cx) / 1000) as i64;
        let duration_s = *self.duration.read(cx) as i64;
        let progress = if duration_s > 0 {
            (position_s as f32 / duration_s as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let playing = *self.playback_state.read(cx) == PlaybackState::Playing;
        let controls_alpha = self.controls_opacity;

        // Ambience backdrop: tiny decode, stretched to Cover, mild blur.
        let backdrop = div().absolute().inset_0().overflow_hidden().when_some(
            self.image_key.clone(),
            |el, key| {
                el.child(
                    managed_image(("immersive-bg", image_gen), key)
                        .thumb()
                        .thumb_max(BACKDROP_THUMB_PX)
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover)
                        .blur(px(24.0)),
                )
            },
        );
        let dim_overlay = div()
            .absolute()
            .inset_0()
            .bg(Rgba::new(0.0, 0.0, 0.0, 0.55));
        let bottom_scrim = div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(relative(0.22))
            .bg(linear_gradient(
                0.0,
                linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.0), 0.0),
                linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.55), 1.0),
            ));

        // Central column: cover, names, lyric viewport.
        let cover = div()
            .h(relative(COVER_FRACTION))
            .aspect_square()
            .rounded(px(14.0))
            .shadow_lg()
            .overflow_hidden()
            .when_some(self.image_key.clone(), |el, key| {
                el.child(
                    managed_image(("immersive-cover", image_gen), key)
                        .thumb()
                        .thumb_max(512)
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover),
                )
            })
            .when_none(&self.image_key, |el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.06)));

        let track_info = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .children(self.track_name.clone().map(|name| {
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(22.0))
                    .line_height(relative(1.15))
                    .text_color(text)
                    .child(name)
            }))
            .children(self.artist_name.clone().map(|artist| {
                div()
                    .text_size(px(15.0))
                    .text_color(text_secondary)
                    .child(artist)
            }));

        let lyric_children = self.parsed.as_ref().map(|parsed| {
            let visual_line = self.visual_line;
            let text = text;
            let accent = accent;
            let text_secondary = text_secondary;
            lyric_window(visual_line, parsed.len()).into_iter().map(
                move |(index, top, is_current)| {
                    let line = &parsed[index];
                    let distance = (index as f32 - visual_line).abs().round() as i64;
                    let opacity = match distance {
                        0 => 1.0,
                        1 => 0.6,
                        _ => 0.3,
                    };
                    let color = if is_current {
                        accent
                    } else {
                        lerp_color(text, text_secondary, 0.4)
                    };
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(top))
                        .h(px(LINE_PITCH_PX))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .opacity(opacity)
                        .text_color(color)
                        .child(
                            div()
                                .text_size(px(if is_current {
                                    CURRENT_LINE_SIZE
                                } else {
                                    NEIGHBOR_LINE_SIZE
                                }))
                                .line_height(relative(1.05))
                                .font_weight(if is_current {
                                    FontWeight::BOLD
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_center()
                                .child(line.text.clone()),
                        )
                        .when(is_current && line.translation.is_some(), |el| {
                            el.child(
                                div()
                                    .text_size(px(TRANSLATION_SIZE))
                                    .line_height(relative(1.05))
                                    .text_color(text_secondary)
                                    .child(line.translation.clone().unwrap()),
                            )
                        })
                },
            )
        });
        let lyrics_viewport = div()
            .relative()
            .w(relative(0.86))
            .max_w(px(980.0))
            .h(px(LYRICS_VIEWPORT_HEIGHT))
            .overflow_hidden()
            .children(lyric_children.into_iter().flatten());

        let center_column = div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(20.0))
            .pb(px(96.0))
            .child(cover)
            .child(track_info)
            .child(lyrics_viewport);

        // Spectrum ribbon: thin pre-EQ curve, accent tinted, above the scrim.
        let ribbon_accent = accent;
        let ribbon = canvas(
            move |bounds, _, cx| {
                let data = if cx.has_global::<SpectrumState>() {
                    cx.global::<SpectrumState>().data.read(cx).pre.clone()
                } else {
                    Rc::default()
                };
                (bounds, data)
            },
            move |bounds, (plot, data), window, _| {
                if data.is_empty() || data.iter().all(|db| *db <= RIBBON_SILENCE_DB) {
                    return;
                }
                let width: f32 = plot.size.width.into();
                let height: f32 = plot.size.height.into();
                let last = data.len() - 1;
                let mut builder = PathBuilder::fill();
                builder.move_to(point(bounds.origin.x, bounds.origin.y + px(height)));
                for (i, db) in data.iter().enumerate() {
                    let x = bounds.origin.x + px(i as f32 / last as f32 * width);
                    let y = bounds.origin.y + px(spectrum_db_to_y(*db, height).clamp(0.0, height));
                    builder.line_to(point(x, y));
                }
                builder.line_to(point(
                    bounds.origin.x + px(width),
                    bounds.origin.y + px(height),
                ));
                if let Ok(path) = builder.build() {
                    window.paint_path(
                        path,
                        Rgba::new(
                            ribbon_accent.red,
                            ribbon_accent.green,
                            ribbon_accent.blue,
                            0.30,
                        ),
                    );
                }
            },
        )
        .absolute()
        .bottom(px(86.0))
        .left_0()
        .right_0()
        .h(px(52.0));

        // Transport controls (auto-hiding) + read-only progress.
        let transport_button =
            |id: &'static str, icon_path: &'static str, size: f32| -> Stateful<Div> {
                div()
                    .id(id)
                    .p(px(8.0))
                    .rounded(px(10.0))
                    .hover(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
                    .active(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.16)))
                    .child(icon(icon_path).size(px(size)).text_color(text))
            };
        let progress_row =
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .w(px(560.0))
                .max_w(relative(0.7))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(text_secondary)
                        .child(SharedString::from(format_duration(position_s, false))),
                )
                .child(
                    div()
                        .flex_1()
                        .h(px(4.0))
                        .rounded_full()
                        .bg(Rgba::new(1.0, 1.0, 1.0, 0.18))
                        .child(
                            div()
                                .h_full()
                                .w(relative(progress))
                                .rounded_full()
                                .bg(accent),
                        ),
                )
                .child(div().text_size(px(12.0)).text_color(text_secondary).child(
                    SharedString::from(format_duration(duration_s.max(0), false)),
                ));
        let transport_row = div()
            .flex()
            .items_center()
            .gap(px(14.0))
            .child(
                transport_button("immersive-prev", PREV_TRACK, 20.0).on_click(|_, _, cx| {
                    cx.global::<PlaybackInterface>().previous();
                }),
            )
            .child(
                transport_button("immersive-play", if playing { PAUSE } else { PLAY }, 26.0)
                    .p(px(12.0))
                    .rounded_full()
                    .on_click(|_, _, cx| {
                        let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                        let interface = cx.global::<PlaybackInterface>();
                        if state == PlaybackState::Playing {
                            interface.pause();
                        } else {
                            interface.play();
                        }
                    }),
            )
            .child(
                transport_button("immersive-next", NEXT_TRACK, 20.0).on_click(|_, _, cx| {
                    cx.global::<PlaybackInterface>().next();
                }),
            );
        let controls_bar = div()
            .absolute()
            .bottom(px(18.0))
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .opacity(controls_alpha)
            .when(controls_alpha <= 0.02, |el| el.hidden())
            .child(progress_row)
            .child(transport_row);

        let exit_button = div()
            .id("immersive-exit")
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .p(px(8.0))
            .rounded(px(10.0))
            .opacity(controls_alpha.max(0.5))
            .hover(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
            .on_click(|_: &ClickEvent, _, cx| set_immersive(false, cx))
            .tooltip(build_tooltip(tr!("IMMERSIVE_EXIT", "Exit Immersive Mode")))
            .child(icon(MINIMIZE).size(px(18.0)).text_color(text));

        div()
            .key_context("Immersive")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(Rgba::new(0.04, 0.04, 0.05, 1.0))
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| {
                this.last_activity = Instant::now();
                if !this.controls_visible {
                    this.start_controls_fade(true);
                    cx.notify();
                }
            }))
            .child(backdrop)
            .child(dim_overlay)
            .child(bottom_scrim)
            .child(ribbon)
            .child(center_column)
            .child(controls_bar)
            .child(exit_button)
    }
}

/// Saturation-weighted dominant color of a BGRA `RenderImage`, used as the
/// immersive page's accent (current lyric line, play button, spectrum tint).
/// Very dark winners are lifted so they still read on the dim backdrop.
fn extract_accent(image: &gpui::RenderImage) -> Option<Rgba> {
    let bytes = image.as_bytes(0)?;
    if bytes.is_empty() {
        return None;
    }
    dominant_accent_bgra(bytes)
}

/// Bucketed dominant-color core over raw BGRA bytes (4 bytes per pixel).
/// 12-bit histogram (4 bits per channel); weight = saturation + a small
/// floor, so grey covers still produce a usable tone. Subsamples every third
/// pixel — a 64px decode is a few thousand reads, off the UI thread.
fn dominant_accent_bgra(bytes: &[u8]) -> Option<Rgba> {
    use rustc_hash::FxHashMap;

    const PIXEL_STRIDE: usize = 3;
    let mut buckets: FxHashMap<u16, (u64, u64, u64, u64)> = FxHashMap::default();
    let mut sampled = 0usize;
    for (n, pixel) in bytes.chunks_exact(4).enumerate() {
        if n % PIXEL_STRIDE != 0 {
            continue;
        }
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
        let saturation = if max == 0 { 0 } else { (max - min) * 255 / max };
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

    // Lift very dark accents above a readability floor (relative luminance).
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
    fn dominant_accent_prefers_saturated_pixels() {
        // BGRA: mostly grey pixels, a few vivid red ones — red must win.
        let grey = [128u8, 128, 128, 255];
        let red = [40u8, 30, 220, 255];
        let mut bytes = Vec::new();
        for _ in 0..100 {
            bytes.extend_from_slice(&grey);
        }
        for _ in 0..20 {
            bytes.extend_from_slice(&red);
        }
        let accent = dominant_accent_bgra(&bytes).unwrap();
        assert!(accent.red > 0.6, "red channel should dominate: {accent:?}");
        assert!(accent.blue < 0.3 && accent.green < 0.3);
    }

    #[test]
    fn dominant_accent_ignores_transparent_pixels() {
        let bytes = vec![0u8; 4 * 32];
        assert!(dominant_accent_bgra(&bytes).is_none());
    }

    #[test]
    fn dominant_accent_lifts_dark_winners() {
        // A single near-black bucket: the lift must raise luminance.
        let dark = [10u8, 12, 16, 255];
        let mut bytes = Vec::new();
        for _ in 0..64 {
            bytes.extend_from_slice(&dark);
        }
        let accent = dominant_accent_bgra(&bytes).unwrap();
        let luminance = 0.2126 * accent.red + 0.7152 * accent.green + 0.0722 * accent.blue;
        assert!(
            luminance >= 0.34,
            "dark accent should be lifted: {accent:?}"
        );
    }

    #[test]
    fn lyric_window_centers_the_active_line() {
        let lines: Vec<(usize, f32, bool)> = lyric_window(4.0, 10);
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![2, 3, 4, 5, 6]
        );
        let (_, top, is_current) = lines[2];
        assert!(is_current);
        assert_eq!(top, 2.0 * LINE_PITCH_PX);
    }

    #[test]
    fn lyric_window_clamps_to_parsed_range() {
        let lines: Vec<(usize, f32, bool)> = lyric_window(2.0, 3);
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Before the first line the window slides down from slot 3.
        let lines: Vec<(usize, f32, bool)> = lyric_window(-1.0, 3);
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn lyric_window_offsets_snap_to_whole_pixels() {
        for line in lyric_window(3.37, 10) {
            let (_, top, _) = line;
            assert_eq!(top, top.round());
        }
    }
}
