//! Full-screen immersive listening view: a blurred cover ambience backdrop,
//! large synced lyrics, a slim spectrum ribbon hugging the bottom edge and
//! auto-hiding transport controls floating over it. Lives inside the main
//! window — `MainWindow::render` swaps the whole library layout for this view
//! while `Models::immersive` is set — and the OS fullscreen toggle rides
//! along via `player::ToggleImmersive`.
//!
//! Lyrics are mirrored from the sidebar's [`Lyrics`] entity instead of being
//! loaded a second time: that entity owns the sidecar/DB/online fetch
//! pipeline (KuGou/NetEase included), its bounded cache and the scan-reset
//! logic. The mirror is generation-guarded so a re-notify never re-clones.
//!
//! Track presentation (cover, names, accent) is keyed off the queue position
//! — the single source of truth for "what is playing", as in InfoSection —
//! and re-resolved on both `SongChanged` and `QueuePositionChanged`, because
//! the former lands while the position still points at the previous track.
//!
//! Animation discipline (see `GPUI_HARDCORE_PERFORMANCE.md` and the
//! 2026-09-27 glyph-atlas lesson in `scroll_follow.rs`):
//! - every frame is data-driven (spectrum/position broadcasts) or
//!   transition-driven (line glide, controls fade), with the frame loop
//!   stopped the moment nothing animates any more;
//! - lyric glyph geometry never interpolates: font sizes come from a
//!   discrete set and line offsets snap to whole pixels, only colors lerp;
//! - the ambience backdrop shares the center cover's 512px decode (same
//!   render-cache entry) so the GPU blur pass never sees a full-resolution
//!   source, and the overlay is one vertical gradient instead of a flat
//!   black slab.

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
    playback::{interface::PlaybackInterface, thread::PlaybackState},
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
        models::{CurrentTrack, Models, PlaybackInfo, Queue},
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
/// Cover decode size, shared by the center cover, the ambience backdrop
/// (same render-cache entry) and the accent extractor.
const COVER_THUMB_PX: u32 = 512;
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
    queue: Entity<Queue>,
    /// The sidebar's lyrics model — shared so online fetches are reused.
    lyrics: Entity<Lyrics>,
    /// `Lyrics::parsed_generation` last mirrored into `parsed`.
    synced_lyrics_generation: u64,
    /// `(published spectrum, tap viewers)` — `None` until the analyzer exists.
    spectrum: Option<(Entity<SpectrumData>, Arc<AtomicUsize>)>,
    spectrum_viewing: bool,

    // Track presentation, re-resolved on track/queue-position changes.
    image_key: Option<ManagedImageKey>,
    image_element_key: u64,
    track_name: Option<SharedString>,
    artist_name: Option<SharedString>,
    meta_subscription: Option<Subscription>,
    accent: Option<Rgba>,
    /// Which cover the pending/existing accent was extracted from.
    accent_for: Option<ManagedImageKey>,
    /// `(queue position, current track path)` last resolved; both triggers
    /// fire per switch, the second one must be a no-op.
    resolved_signature: Option<(usize, Option<PathBuf>)>,

    // Lyrics, mirrored from the shared `Lyrics` entity.
    parsed: Option<Rc<Vec<crate::ui::lyrics::lrc::LrcLine>>>,
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
    pub fn new(cx: &mut App, lyrics: Entity<Lyrics>) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            let immersive_flag = cx.global::<Models>().immersive.clone();
            let queue = cx.global::<Models>().queue.clone();

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

            // Both per-switch triggers funnel into one guarded resolve:
            // SongChanged lands while the queue position still points at the
            // previous track, QueuePositionChanged completes the picture.
            cx.observe(&info.current_track, |this, _, cx| {
                if this.active {
                    this.resolve_track_presentation(cx);
                }
            })
            .detach();
            cx.observe(&queue, |this, _, cx| {
                if this.active {
                    this.resolve_track_presentation(cx);
                }
            })
            .detach();

            // The Lyrics entity notifies unconditionally when a load (local
            // or online fetch) lands; the generation guard keeps re-notifies
            // from re-cloning the parsed lines.
            cx.observe(&lyrics, |this: &mut ImmersiveView, lyrics, cx| {
                if !this.active {
                    return;
                }
                let generation = lyrics.read(cx).parsed_generation();
                if generation != this.synced_lyrics_generation {
                    this.synced_lyrics_generation = generation;
                    this.sync_lyrics(cx);
                    cx.notify();
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
                queue,
                lyrics,
                synced_lyrics_generation: 0,
                spectrum: None,
                spectrum_viewing: false,
                image_key: None,
                image_element_key: 0,
                track_name: None,
                artist_name: None,
                meta_subscription: None,
                accent: None,
                accent_for: None,
                resolved_signature: None,
                parsed: None,
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
        self.resolve_track_presentation(cx);
        self.sync_lyrics(cx);
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

    /// Re-resolves cover/names/accent for whatever is playing now. Fired by
    /// both `SongChanged` and `QueuePositionChanged`; the signature guard
    /// makes the second fire per switch a no-op.
    fn resolve_track_presentation(&mut self, cx: &mut Context<Self>) {
        let (position, item) = {
            let queue = self.queue.read(cx);
            let position = queue.position;
            let item = queue
                .data
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(position)
                .cloned();
            (position, item)
        };
        let track_path = self
            .current_track
            .read(cx)
            .as_ref()
            .map(|track| track.get_path().clone());
        let signature = (position, track_path);
        if self.resolved_signature.as_ref() == Some(&signature) {
            return;
        }
        let track_path = signature.1.clone();
        self.resolved_signature = Some(signature);

        self.image_element_key += 1;
        self.track_name = None;
        self.artist_name = None;
        drop(self.meta_subscription.take());

        let mut cover_key = None;
        if let Some(item) = &item {
            // Online tracks carry a cover URL on their queue item; local
            // files fall back to embedded art / sidecar files.
            #[cfg(feature = "online_sources")]
            if let Some(url) = item
                .get_data(cx)
                .read(cx)
                .clone()
                .and_then(|data| data.cover_url)
                .filter(|url| !url.is_empty())
            {
                cover_key = Some(ManagedImageKey::HttpCover(url));
            }
            if cover_key.is_none() {
                cover_key = Some(ManagedImageKey::TrackFile(item.get_path().clone()));
            }

            let data = item.get_data(cx);
            let item_path = item.get_path().clone();
            let is_current_slot = track_path.as_ref().is_some_and(|path| *path == item_path);
            // Names only from the slot that actually holds the playing track
            // — a stale slot (SongChanged before QueuePositionChanged) must
            // not latch, the position-change resolve corrects it instead.
            if is_current_slot && let Some(ui_data) = data.read(cx).clone() {
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
        self.image_key = cover_key;

        // Accent color: extracted from the same 512px decode the cover uses
        // (one decode per track, render-cached), analyzed off-thread.
        if let Some(key) = self.image_key.clone()
            && self.accent_for.as_ref() != Some(&key)
        {
            self.accent = None;
            self.accent_for = Some(key.clone());
            let pool = cx.global::<Pool>().0.clone();
            let key_for_guard = key.clone();
            cx.spawn(async move |this, cx| {
                let accent = crate::RUNTIME
                    .spawn(async move {
                        key.retrieve(pool, COVER_THUMB_PX, true)
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
                    if this.active && this.accent_for.as_ref() == Some(&key_for_guard) {
                        this.accent = accent;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn adopt_names(&mut self, data: crate::playback::queue::QueueItemUIData) {
        self.track_name = data.name.or(self.track_name.take());
        self.artist_name = data.artist_name.or(self.artist_name.take());
    }

    /// Mirrors the shared lyrics entity's parsed lines and snaps to the
    /// current line without gliding across the whole list.
    fn sync_lyrics(&mut self, cx: &mut Context<Self>) {
        self.synced_lyrics_generation = self.lyrics.read(cx).parsed_generation();
        self.parsed = self.lyrics.read(cx).parsed_lines().map(Rc::new);
        let pos_ms = *self.position.read(cx);
        self.current_line = None;
        self.line_anim = None;
        if let Some(parsed) = &self.parsed {
            let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
            self.current_line = if idx == 0 { None } else { Some(idx - 1) };
        }
        self.visual_line = self.current_line.map_or(-1.0, |line| line as f32);
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

        // PlaybackInfo positions and durations are both in milliseconds.
        let position_ms = *self.position.read(cx);
        let duration_ms = *self.duration.read(cx);
        let position_s = (position_ms / 1000) as i64;
        let duration_s = (duration_ms / 1000) as i64;
        let progress = if duration_ms > 0 {
            (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let playing = *self.playback_state.read(cx) == PlaybackState::Playing;
        let controls_alpha = self.controls_opacity;

        // Ambience backdrop: the cover's own 512px decode (shared cache
        // entry) stretched to Cover, one mild blur pass on top.
        let backdrop = div().absolute().inset_0().overflow_hidden().when_some(
            self.image_key.clone(),
            |el, key| {
                el.child(
                    managed_image(("immersive-bg", image_gen), key)
                        .thumb_max(COVER_THUMB_PX)
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover)
                        .blur(px(24.0)),
                )
            },
        );
        // One vertical gradient instead of a flat black slab: light at the
        // top so the ambience reads, heavier toward the controls.
        let dim_overlay = div().absolute().inset_0().bg(linear_gradient(
            0.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.30), 0.0),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.62), 1.0),
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
                        .thumb_max(COVER_THUMB_PX)
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
            .gap(px(16.0))
            .pb(px(110.0))
            .child(cover)
            .child(track_info)
            .child(lyrics_viewport);

        // Spectrum ribbon: thin pre-EQ curve hugging the bottom edge, behind
        // the floating controls.
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
        .bottom_0()
        .left_0()
        .right_0()
        .h(px(72.0));

        // Transport controls floating over the ribbon — no reserved block,
        // the whole bottom overlay fades with pointer inactivity.
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
                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.10))
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
        let progress_row =
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .w(px(680.0))
                .max_w(relative(0.85))
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
        let bottom_overlay = div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.0))
            .pb(px(12.0))
            .opacity(controls_alpha)
            .when(controls_alpha <= 0.02, |el| el.hidden())
            .child(transport_row)
            .child(progress_row);

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
            .child(ribbon)
            .child(center_column)
            .child(bottom_overlay)
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
/// pixel — a 512px decode is ~44k reads, off the UI thread.
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
