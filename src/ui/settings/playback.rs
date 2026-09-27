use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, px,
};

use crate::{
    power::PowerManager,
    settings::{Settings, SettingsGlobal, save_settings},
    ui::components::{
        checkbox::checkbox,
        label::{Label, label},
        labeled_slider::labeled_slider,
        section_header::section_header,
    },
};

/// One checkbox row: clicking the label toggles one `bool` field of
/// `settings.playback` via `update_playback_settings`. The checkbox id is
/// passed explicitly so both ids of the row pair stay greppable. Rows whose
/// click does more than flip the field (prevent-idle also pushes the value
/// to the `PowerManager`) build inline instead.
fn toggle_row(
    cx: &Context<PlaybackSettings>,
    label_id: &'static str,
    check_id: &'static str,
    title: impl Into<SharedString>,
    subtext: Option<SharedString>,
    checked: bool,
    toggle: fn(&mut crate::settings::playback::PlaybackSettings),
) -> Label {
    let mut row = label(label_id, title);
    if let Some(subtext) = subtext {
        row = row.subtext(subtext);
    }
    row.cursor_pointer()
        .w_full()
        .on_click(cx.listener(move |this, _, _, cx| {
            super::update_playback_settings(&this.settings, cx, toggle);
        }))
        .child(checkbox(check_id, checked))
}

pub struct PlaybackSettings {
    settings: Entity<Settings>,
    /// Generation counter for the trailing-edge save debounce (see
    /// `schedule_save`): a detached task saves only when its generation is
    /// still the newest, so a new tick supersedes the pending save.
    save_generation: Arc<AtomicU64>,
}

impl PlaybackSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let settings = cx.global::<SettingsGlobal>().model.clone();
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            Self {
                settings,
                save_generation: Arc::new(AtomicU64::new(0)),
            }
        })
    }

    /// Trailing-edge debounce for slider drags (equalizer-view pattern): every
    /// tick bumps the generation and schedules a single save ~300ms out, so
    /// `save_settings` - and the PlaybackInterface push it performs - runs
    /// once per drag instead of once per mouse-move tick. The disk write keeps
    /// its own 500ms trailing-edge debounce inside `save_settings`.
    ///
    /// The flush is detached and keyed on the generation counter instead of
    /// being stored as a page-owned `Task`: dropping the page (section
    /// switch / settings-window close) cancels a stored Task, which would
    /// silently lose the trailing save - the live preamp value sits in the
    /// settings model but never reaches `save_settings`. The save is routed
    /// through the app-lifetime settings entity so it survives the page.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let generation = self.save_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let generation_counter = Arc::clone(&self.save_generation);
        let settings = self.settings.clone();
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            // A newer edit superseded this tick; the newest task owns the save.
            if generation_counter.load(Ordering::Relaxed) != generation {
                return;
            }
            settings.update(cx, |settings, cx| save_settings(cx, settings));
        })
        .detach();
    }
}

impl Render for PlaybackSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playback = self.settings.read(cx).playback.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(section_header(tr!("PLAYBACK")))
            .child(toggle_row(
                cx,
                "playback-always-repeat",
                "playback-always-repeat-check",
                tr!("PLAYBACK_ALWAYS_REPEAT", "Always repeat"),
                Some(
                    tr!(
                        "PLAYBACK_ALWAYS_REPEAT_SUBTEXT",
                        "Disables the \"Off\" repeat mode."
                    )
                    .into(),
                ),
                playback.always_repeat,
                |playback| playback.always_repeat = !playback.always_repeat,
            ))
            .child(toggle_row(
                cx,
                "playback-prev-track-jump-first",
                "playback-prev-track-jump-first-check",
                tr!(
                    "PLAYBACK_PREVIOUS_JUMPS",
                    "Previous button jumps to the beginning of the track if \
                    more than 5 seconds has elapsed"
                ),
                None,
                playback.prev_track_jump_first,
                |playback| playback.prev_track_jump_first = !playback.prev_track_jump_first,
            ))
            .child(toggle_row(
                cx,
                "playback-keep-current-on-clear",
                "playback-keep-current-on-clear-check",
                tr!(
                    "PLAYBACK_KEEP_CURRENT_ON_CLEAR",
                    "Keep current track when clearing queue"
                ),
                Some(
                    tr!(
                        "PLAYBACK_KEEP_CURRENT_ON_CLEAR_SUBTEXT",
                        "Preserves the currently playing song instead of removing all tracks."
                    )
                    .into(),
                ),
                playback.keep_current_on_queue_clear,
                |playback| {
                    playback.keep_current_on_queue_clear = !playback.keep_current_on_queue_clear;
                },
            ))
            .child(toggle_row(
                cx,
                "playback-consume",
                "playback-consume-check",
                tr!("PLAYBACK_CONSUME", "Consume mode"),
                Some(
                    tr!(
                        "PLAYBACK_CONSUME_SUBTEXT",
                        "Removes songs from the queue after they finish playing."
                    )
                    .into(),
                ),
                playback.consume,
                |playback| playback.consume = !playback.consume,
            ))
            .child({
                let settings = self.settings.clone();
                label(
                    "playback-rg-fallback-preamp",
                    tr!("PLAYBACK_RG_FALLBACK_PREAMP", "ReplayGain fallback pre-amp"),
                )
                .subtext(tr!(
                    "PLAYBACK_RG_FALLBACK_PREAMP_SUBTEXT",
                    "Applied when tracks have no ReplayGain data."
                ))
                .w_full()
                .child(
                    labeled_slider("rg-fallback-preamp")
                        .slider_id("rg-fallback-preamp-track")
                        .w(px(250.0))
                        .min(-6.0)
                        .max(6.0)
                        .value(playback.replaygain.fallback_preamp_db as f32)
                        .default_value(0.0)
                        .format_value(|v| -> SharedString { format!("{:+.1} dB", v).into() })
                        .on_change({
                            let weak_self = cx.weak_entity();
                            move |v, _, cx| {
                                // live value lands in the model WITHOUT notifying the settings
                                // model: a per-tick model notify cascades into the app-wide
                                // refresh_windows observer (app.rs) - a full repaint of every
                                // window per mouse move while dragging. Only this page entity
                                // is notified so the slider and its readout track the drag;
                                // the debounced save applies the preamp to the playback
                                // thread once the drag settles.
                                settings.update(cx, |settings, _| {
                                    settings.playback.replaygain.fallback_preamp_db = v as f64;
                                });
                                if let Some(this) = weak_self.upgrade() {
                                    this.update(cx, |this, cx| {
                                        cx.notify();
                                        this.schedule_save(cx);
                                    });
                                }
                            }
                        }),
                )
            })
            .child(
                label(
                    "playback-prevent-idle",
                    tr!("PLAYBACK_PREVENT_IDLE", "Prevent system idle when playing"),
                )
                .subtext(tr!(
                    "PLAYBACK_PREVENT_IDLE_SUBTEXT",
                    "Stops the screensaver and system sleep during playback."
                ))
                .cursor_pointer()
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| {
                    let prevent_idle = !this.settings.read(cx).playback.prevent_idle;
                    super::update_playback_settings(&this.settings, cx, |playback| {
                        playback.prevent_idle = prevent_idle;
                    });
                    let power = cx.global::<PowerManager>().clone();
                    power.set_prevent_idle(cx, prevent_idle);
                }))
                .child(checkbox(
                    "playback-prevent-idle-check",
                    playback.prevent_idle,
                )),
            )
    }
}
