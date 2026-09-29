//! Full-screen immersive listening view, laid out like a hi-fi lyrics
//! presentation: the artwork fills the screen at full decoded sharpness (no
//! blur — one vertical + one horizontal gradient carve out the reading
//! zones), a spinning vinyl disc with the cover as its label sits on the
//! left above the track names and a frosted-glass player card (seekable
//! progress, transport, volume), and the right half is the lyrics column
//! with karaoke word timing, wheel browsing and click-to-seek.
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
//! - every frame is data-driven (position broadcasts) or transition-driven
//!   (line glide), with the frame loop stopped the moment nothing animates
//!   any more;
//! - lyric glyph geometry never interpolates: font sizes come from a
//!   discrete set and line offsets snap to whole pixels, only colors lerp
//!   (karaoke word colors included);
//! - the "vinyl" is now just the circular cover art at disc size: the dark
//!   platter, groove rings and sheen went away across 2026-09-28 feedback
//!   (a rotating sheen arc rendered displaced/oversized on DirectX via
//!   `Svg::with_transformation`, and the exposed platter ring read as a
//!   pointless border around the cover);
//! - the backdrop is its own full-sharpness uncached decode (held by the
//!   element alone, recycled through the orphan-tile funnel on track
//!   switch) with a mild unsharp pass for low-resolution sources.

use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ObjectFit, ParentElement, Render, Rgba, ScrollWheelEvent,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription, Window, div,
    linear_color_stop, linear_gradient, px, relative,
};
use tracing::warn;

use crate::{
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    settings::SettingsGlobal,
    ui::{
        app::Pool,
        components::{
            icons::{MINIMIZE, NEXT_TRACK, PAUSE, PLAY, PREV_TRACK, VOLUME, VOLUME_OFF, icon},
            managed_image::{ManagedImageKey, managed_image},
            slider::slider,
            tooltip::build_tooltip,
        },
        lyrics::{Lyrics, lerp_color, word_progress},
        models::{CurrentTrack, Models, PlaybackInfo, Queue},
        scroll_follow::ease_out_cubic,
        theme::Theme,
        util::{extract_accent, format_duration},
    },
};

/// Vertical distance between lyric lines, in px.
const LINE_PITCH_PX: f32 = 44.0;
/// Lyric font sizes — discrete glyph-atlas-safe levels only.
const CURRENT_LINE_SIZE: f32 = 22.0;
const NEIGHBOR_LINE_SIZE: f32 = 18.0;
const TRANSLATION_SIZE: f32 = 13.0;
/// Lyric lines rendered on either side of the active one (13-line window).
const LINE_WINDOW: i64 = 6;
/// Height of the lyric viewport: thirteen pitches.
const LYRICS_VIEWPORT_HEIGHT: f32 = LINE_PITCH_PX * 13.0;
/// Per-line opacity by distance from the active line (index = distance).
const LINE_OPACITY: [f32; 7] = [1.0, 0.82, 0.64, 0.47, 0.33, 0.22, 0.14];
const LINE_ANIMATION: Duration = Duration::from_millis(320);
/// Vinyl disc as a fraction of the left column height.
const VINYL_SIZE_FRACTION: f32 = 0.34;
/// Cover as a fraction of the disc diameter — 1.0: the cover IS the disc.
/// (The dark platter used to peek out around a smaller label; 2026-09-28
/// feedback: with the grooves gone that ring read as a pointless border.)
const VINYL_LABEL_FRACTION: f32 = 1.0;
/// Cover decode for the vinyl label — small, render-cached.
const LABEL_THUMB_PX: u32 = 512;
/// Backdrop decode cap for full-screen sharpness — uncached, element-held.
const BACKDROP_THUMB_PX: u32 = 2048;
/// Wheel browsing: after this long without scrolling the view glides back
/// to the active line (the sidebar's lyrics panel uses the same pattern).
const SCROLL_RETURN_AFTER: Duration = Duration::from_secs(2);

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
    volume: Entity<f64>,
    prev_volume: Entity<f64>,
    playback_state: Entity<PlaybackState>,
    current_track: Entity<Option<CurrentTrack>>,
    queue: Entity<Queue>,
    /// The sidebar's lyrics model — shared so online fetches are reused.
    lyrics: Entity<Lyrics>,
    /// `Lyrics::parsed_generation` last mirrored into `parsed`.
    synced_lyrics_generation: u64,

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
    /// Wheel-browsing offset from the active line, in lines. Returns to 0
    /// after [`SCROLL_RETURN_AFTER`] of wheel idle (position ticks drive the
    /// check, so paused playback never resets the browse position).
    browse_offset: i64,
    /// Sub-line wheel accumulator (fractional lines between snaps).
    scroll_accum: f32,
    /// Last wheel interaction; `None` once the browse offset has returned.
    last_scroll: Option<Instant>,
    /// Animated display center = active line + browse offset, glided
    /// whenever either moves. Whole-pixel snapped per line at render time.
    visual_line: f32,
    line_anim: Option<(f32, f32, Instant)>,

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
                this.track_lyric_line(*pos.read(cx));
                // Wheel-browsing returns to the active line after the idle
                // window; position ticks drive the check so paused playback
                // never resets the browse position.
                if this.browse_offset != 0
                    && this
                        .last_scroll
                        .is_some_and(|at| at.elapsed() >= SCROLL_RETURN_AFTER)
                {
                    this.browse_offset = 0;
                    this.last_scroll = None;
                    this.sync_display_target(true);
                }
                // Drives the progress bar, the vinyl rotation and any
                // in-flight line glide.
                cx.notify();
            })
            .detach();

            cx.observe(&info.duration, |this, _, cx| {
                if this.active {
                    cx.notify();
                }
            })
            .detach();

            cx.observe(&info.volume, |this, _, cx| {
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
                volume: info.volume.clone(),
                prev_volume: info.prev_volume.clone(),
                playback_state: info.playback_state.clone(),
                current_track: info.current_track.clone(),
                queue,
                lyrics,
                synced_lyrics_generation: 0,
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
                browse_offset: 0,
                scroll_accum: 0.0,
                last_scroll: None,
                visual_line: -1.0,
                line_anim: None,
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
        self.resolve_track_presentation(cx);
        self.sync_lyrics(cx);
    }

    fn deactivate(&mut self) {
        self.line_anim = None;
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
        // A fresh track ends any wheel-browsing session.
        self.browse_offset = 0;
        self.last_scroll = None;
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
                // Large-display variant: the queue carries 256px thumbnails,
                // which turn to mush across a fullscreen backdrop.
                cover_key = Some(ManagedImageKey::HttpCoverLarge(url));
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

        // Accent color: extracted from the label's small decode (render
        // cached), analyzed off-thread.
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
                        key.retrieve(pool, LABEL_THUMB_PX, true)
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
        self.sync_display_target(false);
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
        // A jump of more than one line is a seek (click-to-seek, scrubber,
        // prev/next): the wheel-browse session ends so the window recenters
        // on the line the user jumped to. Natural playback steps by one.
        if let (Some(new), Some(old)) = (new_line, self.current_line)
            && new.abs_diff(old) > 1
        {
            self.browse_offset = 0;
            self.last_scroll = None;
        }
        self.current_line = new_line;
        self.sync_display_target(true);
    }

    /// Recenters the lyric window on the active line plus the wheel-browse
    /// offset, gliding when the target moved.
    fn sync_display_target(&mut self, animate: bool) {
        let len = self.parsed.as_ref().map_or(0, |parsed| parsed.len() as i64);
        let base = self.current_line.map_or(-1, |line| line as i64);
        let min = -LINE_WINDOW - base;
        let max = len - 1 + LINE_WINDOW - base;
        self.browse_offset = self.browse_offset.clamp(min, max);
        let target = (base + self.browse_offset) as f32;
        if (target - self.visual_line).abs() < 0.01 {
            self.visual_line = target;
            self.line_anim = None;
        } else if !animate {
            self.visual_line = target;
            self.line_anim = None;
        } else {
            self.line_anim = Some((self.visual_line, target, Instant::now()));
        }
    }

    fn needs_animation_frame(&self) -> bool {
        self.line_anim.is_some()
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
        let Some((from, target, started_at)) = self.line_anim else {
            return;
        };
        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;
        if reduced_motion {
            self.visual_line = target;
            self.line_anim = None;
        } else {
            let progress =
                (started_at.elapsed().as_secs_f32() / LINE_ANIMATION.as_secs_f32()).clamp(0.0, 1.0);
            self.visual_line = from + (target - from) * ease_out_cubic(progress);
            if progress >= 1.0 {
                self.visual_line = target;
                self.line_anim = None;
            }
        }
        if self.needs_animation_frame() {
            self.schedule_frame(window, cx);
        }
        cx.notify();
    }
}

/// The lyric lines inside the window around the animated display center:
/// `(line index, whole-pixel top offset, is_current)`. Pure so the clamp and
/// snap math is unit-testable. The highlighted line is the one actually
/// playing, even while the user browses away with the wheel.
fn lyric_window(
    visual_line: f32,
    parsed_len: usize,
    current_line: Option<usize>,
) -> Vec<(usize, f32, bool)> {
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
            let top = ((index as f32 - visual_line) + LINE_WINDOW as f32) * LINE_PITCH_PX;
            Some((index, top.round(), Some(index) == current_line))
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
        let volume = *self.volume.read(cx);
        let prev_volume = *self.prev_volume.read(cx);

        // Backdrop: full-sharpness decode, Cover-fit, NO blur — sharpness is
        // the point; the gradients below carve the reading zones.
        let backdrop = div().absolute().inset_0().overflow_hidden().when_some(
            self.image_key.clone(),
            |el, key| {
                el.child(
                    managed_image(("immersive-bg", image_gen), key)
                        .thumb_max(BACKDROP_THUMB_PX)
                        .enhanced()
                        .uncached()
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover),
                )
            },
        );
        // Global weight: heavier at the bottom (card zone), light at the top
        // (0deg = to top: the 0% stop sits at the bottom edge).
        let vertical_shade = div().absolute().inset_0().bg(linear_gradient(
            0.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.42), 0.0),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.12), 1.0),
        ));
        // Right lyrics zone: transparent over the artwork, dark toward the
        // right edge (90deg = to right).
        let right_shade = div().absolute().inset_0().bg(linear_gradient(
            90.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.0), 0.30),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.52), 1.0),
        ));

        // ── Left column: circular cover, names, frosted player card ──────
        // The cover fills the whole disc circle (label fraction 1.0); the
        // dark layer behind it only shows through while the cover decodes or
        // when a track has no art at all. See module docs for why nothing
        // rotates and the grooves/sheen are gone.
        let vinyl_label = div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .when_some(self.image_key.clone(), |el, key| {
                el.child(
                    managed_image(("immersive-label", image_gen), key)
                        .thumb_max(LABEL_THUMB_PX)
                        .h(relative(VINYL_LABEL_FRACTION))
                        .aspect_square()
                        .rounded_full()
                        .object_fit(ObjectFit::Cover),
                )
            })
            .when_none(&self.image_key, |el| {
                el.child(
                    div()
                        .h(relative(VINYL_LABEL_FRACTION))
                        .aspect_square()
                        .rounded_full()
                        .bg(Rgba::new(1.0, 1.0, 1.0, 0.06)),
                )
            });
        let vinyl_disc = div()
            .absolute()
            .inset_0()
            .rounded_full()
            .bg(Rgba::new(0.045, 0.045, 0.058, 1.0));
        let vinyl = div()
            .relative()
            .h(relative(VINYL_SIZE_FRACTION))
            .aspect_square()
            .child(vinyl_disc)
            .child(vinyl_label);

        let track_info = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(5.0))
            .children(self.track_name.clone().map(|name| {
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(26.0))
                    .line_height(relative(1.15))
                    .text_color(text)
                    .text_center()
                    .child(name)
            }))
            .children(self.artist_name.clone().map(|artist| {
                div()
                    .text_size(px(15.0))
                    .text_color(text_secondary)
                    .text_center()
                    .child(artist)
            }));

        let transport_button =
            |id: &'static str, icon_path: &'static str, size: f32| -> Stateful<Div> {
                div()
                    .id(id)
                    .p(px(7.0))
                    .rounded(px(9.0))
                    .hover(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
                    .active(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.16)))
                    .child(icon(icon_path).size(px(size)).text_color(text))
            };
        let transport_row = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                transport_button("immersive-prev", PREV_TRACK, 18.0).on_click(|_, _, cx| {
                    cx.global::<PlaybackInterface>().previous();
                }),
            )
            .child(
                transport_button("immersive-play", if playing { PAUSE } else { PLAY }, 22.0)
                    .p(px(10.0))
                    .rounded_full()
                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.12))
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
                transport_button("immersive-next", NEXT_TRACK, 18.0).on_click(|_, _, cx| {
                    cx.global::<PlaybackInterface>().next();
                }),
            );
        let volume_group =
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .id("immersive-volume-btn")
                        .p(px(4.0))
                        .rounded(px(8.0))
                        .hover(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
                        .on_click(move |_, _, cx| {
                            let interface = cx.global::<PlaybackInterface>();
                            if volume <= 0.0 {
                                interface.set_volume(prev_volume);
                            } else {
                                interface.set_volume(0.0);
                            }
                        })
                        .child(
                            icon(if volume <= 0.0 { VOLUME_OFF } else { VOLUME })
                                .size(px(16.0))
                                .text_color(text_secondary),
                        ),
                )
                .child(
                    slider()
                        .id("immersive-volume")
                        .w(px(90.0))
                        .h(px(6.0))
                        .rounded_full()
                        .value(volume as f32)
                        .on_change(move |v, _, cx| {
                            cx.global::<PlaybackInterface>().set_volume(v as f64);
                        }),
                )
                .child(div().text_size(px(11.0)).text_color(text_secondary).child(
                    SharedString::from(format!("{}%", (volume * 100.0).round() as i64)),
                ));
        let progress_row =
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .w_full()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(text_secondary)
                        .child(SharedString::from(format_duration(position_s, false))),
                )
                .child(
                    slider()
                        .id("immersive-progress")
                        .flex_1()
                        .h(px(6.0))
                        .rounded_full()
                        .value(progress)
                        .on_change(move |v, _, cx| {
                            if duration_ms > 0 {
                                let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                                if state != PlaybackState::Stopped {
                                    cx.global::<PlaybackInterface>()
                                        .seek(v as f64 * duration_ms as f64 / 1000.0);
                                }
                            }
                        }),
                )
                .child(div().text_size(px(11.0)).text_color(text_secondary).child(
                    SharedString::from(format_duration(duration_s.max(0), false)),
                ));
        let player_card = div()
            .w(relative(0.90))
            .max_w(px(460.0))
            .rounded(px(16.0))
            .bg(Rgba::new(0.05, 0.05, 0.07, 0.62))
            .backdrop_blur(px(14.0))
            .border_1()
            .border_color(Rgba::new(1.0, 1.0, 1.0, 0.10))
            .p(px(18.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(progress_row)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(transport_row)
                    .child(volume_group),
            );

        let left_column = div()
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(relative(0.44))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(22.0))
            .px(px(40.0))
            .child(vinyl)
            .child(track_info)
            .child(player_card);

        // ── Right column: lyrics ─────────────────────────────────────────
        // Every line is click-to-seek; the wheel browses away from the
        // active line (returning after the idle window); the active line
        // renders word-by-word karaoke when the track carries per-word
        // timing. Colors lerp only — glyph geometry never interpolates.
        let lyric_children = self.parsed.as_ref().map(|parsed| {
            let visual_line = self.visual_line;
            let current_line = self.current_line;
            let position_ms = position_ms;
            lyric_window(visual_line, parsed.len(), current_line)
                .into_iter()
                .map(move |(index, top, is_current)| {
                    let line = &parsed[index];
                    let distance = (index as f32 - visual_line).abs().round() as i64;
                    let opacity = LINE_OPACITY
                        .get(distance as usize)
                        .copied()
                        .unwrap_or(*LINE_OPACITY.last().unwrap());
                    let color = if is_current {
                        accent
                    } else {
                        lerp_color(text, text_secondary, 0.4)
                    };
                    let line_time_ms = line.time_ms;
                    // Karaoke: un-sung words sit at the dimmed neighbor tone
                    // and sweep to the accent as each word's window passes
                    // (the current line's own color is already the accent, so
                    // it cannot be the sweep start). Word colors change per
                    // tick — glyph geometry stays fixed.
                    let text_child: Div = if is_current && !line.words.is_empty() {
                        let unsung = lerp_color(text, text_secondary, 0.4);
                        let mut row = div().flex().flex_wrap().items_baseline();
                        for word in &line.words {
                            let progress = word_progress(word, position_ms);
                            row = row.child(
                                div()
                                    .child(word.text.clone())
                                    .text_color(lerp_color(unsung, accent, progress)),
                            );
                        }
                        row
                    } else {
                        div().child(line.text.clone())
                    };
                    div()
                        .id(index)
                        .cursor_pointer()
                        .hover(move |el| el.opacity(opacity.max(0.9)))
                        .on_click(move |_, _, cx| {
                            // Direct-closure handler (sidebar lyric rows use
                            // the same shape): the click only seeks — the
                            // wheel-browse session unwinds through the
                            // position-tick idle return.
                            let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                            if state != PlaybackState::Stopped {
                                cx.global::<PlaybackInterface>()
                                    .seek(line_time_ms as f64 / 1000.0);
                            }
                        })
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(top))
                        .h(px(LINE_PITCH_PX))
                        .flex()
                        .flex_col()
                        .items_start()
                        .justify_center()
                        .opacity(opacity)
                        .text_color(color)
                        .child(
                            text_child
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
                                .text_left(),
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
                })
        });
        let lyrics_viewport = div()
            .relative()
            .w_full()
            .h(px(LYRICS_VIEWPORT_HEIGHT))
            .overflow_hidden()
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                if !this.active {
                    return;
                }
                let dy = f32::from(ev.delta.pixel_delta(px(LINE_PITCH_PX)).y);
                this.scroll_accum += dy / LINE_PITCH_PX;
                let whole = this.scroll_accum as i64;
                if whole != 0 {
                    this.scroll_accum -= whole as f32;
                    this.browse_offset += whole;
                    this.last_scroll = Some(Instant::now());
                    this.sync_display_target(true);
                    cx.notify();
                }
            }))
            .children(lyric_children.into_iter().flatten());
        let right_column = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(0.46))
            .right_0()
            .flex()
            .flex_col()
            .justify_center()
            .pl(px(24.0))
            .pr(px(52.0))
            .child(lyrics_viewport);

        let exit_button = div()
            .id("immersive-exit")
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .p(px(8.0))
            .rounded(px(10.0))
            .opacity(0.55)
            .hover(|el| el.opacity(1.0).bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
            .on_click(|_: &ClickEvent, _, cx| set_immersive(false, cx))
            .tooltip(build_tooltip(tr!("IMMERSIVE_EXIT", "Exit Immersive Mode")))
            .child(icon(MINIMIZE).size(px(18.0)).text_color(text));

        div()
            .key_context("Immersive")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(Rgba::new(0.02, 0.02, 0.03, 1.0))
            .child(backdrop)
            .child(vertical_shade)
            .child(right_shade)
            .child(left_column)
            .child(right_column)
            .child(exit_button)
    }
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
        let lines: Vec<(usize, f32, bool)> = lyric_window(10.0, 30, Some(10));
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            (4..=16).collect::<Vec<_>>()
        );
        // The active line sits in the middle slot (index 6 of 13).
        let (_, top, is_current) = lines[6];
        assert!(is_current);
        assert_eq!(top, LINE_WINDOW as f32 * LINE_PITCH_PX);
    }

    #[test]
    fn lyric_window_clamps_to_parsed_range() {
        let lines: Vec<(usize, f32, bool)> = lyric_window(1.0, 3, Some(1));
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Before the first line the window still starts at the clamp.
        let lines: Vec<(usize, f32, bool)> = lyric_window(-1.0, 3, None);
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Browsing away: the highlight stays on the playing line even when
        // the window centers elsewhere.
        let lines: Vec<(usize, f32, bool)> = lyric_window(4.0, 30, Some(1));
        assert!(
            lines
                .iter()
                .any(|(index, _, current)| *index == 1 && *current)
        );
        assert!(
            !lines
                .iter()
                .any(|(index, _, current)| *index == 4 && *current)
        );
    }

    #[test]
    fn lyric_window_offsets_snap_to_whole_pixels() {
        for line in lyric_window(3.37, 30, Some(3)) {
            let (_, top, _) = line;
            assert_eq!(top, top.round());
        }
    }
}
