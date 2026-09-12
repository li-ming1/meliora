use std::time::Duration;

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Task, Window, div, px,
};

use crate::{
    power::PowerManager,
    settings::{Settings, SettingsGlobal, save_settings},
    ui::components::{
        checkbox::checkbox,
        label::label,
        labeled_slider::labeled_slider,
        section_header::section_header,
    },
};

pub struct PlaybackSettings {
    settings: Entity<Settings>,
    /// Trailing-edge debounce task for slider edits; replacing it cancels the
    /// pending save so a drag collapses into one `save_settings` call.
    save_task: Option<Task<()>>,
}

impl PlaybackSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let settings = cx.global::<SettingsGlobal>().model.clone();
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            Self {
                settings,
                save_task: None,
            }
        })
    }

    /// Trailing-edge debounce for slider drags (equalizer-view pattern): every
    /// tick reschedules a single save ~300ms out, so `save_settings` - and the
    /// PlaybackInterface push it performs - runs once per drag instead of once
    /// per mouse-move tick. The disk write keeps its own 500ms trailing-edge
    /// debounce inside `save_settings`.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            this.update(cx, |this, cx| {
                this.settings
                    .update(cx, |settings, cx| save_settings(cx, settings));
            })
            .ok();
        }));
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
            .child(
                label(
                    "playback-always-repeat",
                    tr!("PLAYBACK_ALWAYS_REPEAT", "Always repeat"),
                )
                .subtext(tr!(
                    "PLAYBACK_ALWAYS_REPEAT_SUBTEXT",
                    "Disables the \"Off\" repeat mode."
                ))
                .cursor_pointer()
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| {
                    super::update_playback_settings(&this.settings, cx, |playback| {
                        playback.always_repeat = !playback.always_repeat;
                    });
                }))
                .child(checkbox(
                    "playback-always-repeat-check",
                    playback.always_repeat,
                )),
            )
            .child(
                label(
                    "playback-prev-track-jump-first",
                    tr!(
                        "PLAYBACK_PREVIOUS_JUMPS",
                        "Previous button jumps to the beginning of the track if \
                        more than 5 seconds has elapsed"
                    ),
                )
                .cursor_pointer()
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| {
                    super::update_playback_settings(&this.settings, cx, |playback| {
                        playback.prev_track_jump_first = !playback.prev_track_jump_first;
                    });
                }))
                .child(checkbox(
                    "playback-prev-track-jump-first-check",
                    playback.prev_track_jump_first,
                )),
            )
            .child(
                label(
                    "playback-keep-current-on-clear",
                    tr!(
                        "PLAYBACK_KEEP_CURRENT_ON_CLEAR",
                        "Keep current track when clearing queue"
                    ),
                )
                .subtext(tr!(
                    "PLAYBACK_KEEP_CURRENT_ON_CLEAR_SUBTEXT",
                    "Preserves the currently playing song instead of removing all tracks."
                ))
                .cursor_pointer()
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| {
                    super::update_playback_settings(&this.settings, cx, |playback| {
                        playback.keep_current_on_queue_clear =
                            !playback.keep_current_on_queue_clear;
                    });
                }))
                .child(checkbox(
                    "playback-keep-current-on-clear-check",
                    playback.keep_current_on_queue_clear,
                )),
            )
            .child(
                label("playback-consume", tr!("PLAYBACK_CONSUME", "Consume mode"))
                    .subtext(tr!(
                        "PLAYBACK_CONSUME_SUBTEXT",
                        "Removes songs from the queue after they finish playing."
                    ))
                    .cursor_pointer()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        super::update_playback_settings(&this.settings, cx, |playback| {
                            playback.consume = !playback.consume;
                        });
                    }))
                    .child(checkbox("playback-consume-check", playback.consume)),
            )
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
