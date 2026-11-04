use crate::{
    power::PowerManager,
    settings::{Settings, SettingsGlobal},
    ui::components::{
        checkbox::checkbox,
        label::{Label, label},
        labeled_slider::labeled_slider,
        section_header::section_header,
    },
};
use cntp_i18n::tr;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};

use super::debounced_save::DebouncedSave;

#[cfg(feature = "online_sources")]
use crate::{
    toasts::{Toast, emit_toast},
    ui::components::button::{ButtonIntent, ButtonStyle, button},
};

/// 一个勾选行：点击标签即经 `update_playback_settings` 切换 `settings.playback` 的
/// 一个 `bool` 字段。勾选框 id 显式传入，好让一行的两个 id 都可 grep。点击不只是
/// 翻字段的行（防息屏还要把值推给 `PowerManager`）改为就地构建。
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
    save_debounce: DebouncedSave,
    /// 清除缓存的两段式确认；分节切换会重建实体，确认态随之复位。
    #[cfg(feature = "online_sources")]
    clear_armed: bool,
    #[cfg(feature = "online_sources")]
    clearing: bool,
}

impl PlaybackSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let settings = cx.global::<SettingsGlobal>().model.clone();
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            Self {
                settings,
                save_debounce: DebouncedSave::new(),
                #[cfg(feature = "online_sources")]
                clear_armed: false,
                #[cfg(feature = "online_sources")]
                clearing: false,
            }
        })
    }

    /// 在线音频缓存设置节；非 `online_sources` 构建下该模块不存在，返回空元素。
    #[cfg(feature = "online_sources")]
    fn audio_cache_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.settings.read(cx).playback.audio_cache_enabled;
        let clear_armed = self.clear_armed;
        let clearing = self.clearing;

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(section_header(tr!(
                "PLAYBACK_AUDIO_CACHE_SECTION",
                "Online audio cache"
            )))
            .child(toggle_row(
                cx,
                "playback-audio-cache-enabled",
                "playback-audio-cache-enabled-check",
                tr!(
                    "PLAYBACK_AUDIO_CACHE_ENABLED",
                    "Cache online tracks to disk"
                ),
                Some(
                    tr!(
                        "PLAYBACK_AUDIO_CACHE_ENABLED_SUBTEXT",
                        "Cached tracks open instantly and play without the network. Capped at 1 GB."
                    )
                    .into(),
                ),
                enabled,
                |playback| playback.audio_cache_enabled = !playback.audio_cache_enabled,
            ))
            .child(
                label(
                    "playback-audio-cache-clear",
                    tr!("PLAYBACK_AUDIO_CACHE_CLEAR_TITLE", "Clear cached audio"),
                )
                .subtext(tr!(
                    "PLAYBACK_AUDIO_CACHE_CLEAR_SUBTEXT",
                    "Removes all cached online tracks; they are downloaded again when played."
                ))
                .w_full()
                .child(
                    button()
                        .id("playback-audio-cache-clear-button")
                        .style(ButtonStyle::Regular)
                        .intent(if clear_armed || clearing {
                            ButtonIntent::Danger
                        } else {
                            ButtonIntent::Secondary
                        })
                        .child(if clearing {
                            tr!("PLAYBACK_AUDIO_CACHE_CLEARING", "Clearing…")
                        } else if clear_armed {
                            tr!(
                                "PLAYBACK_AUDIO_CACHE_CLEAR_CONFIRM",
                                "Click again to confirm"
                            )
                        } else {
                            tr!("PLAYBACK_AUDIO_CACHE_CLEAR", "Clear cache")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.clearing {
                                return;
                            }
                            if !this.clear_armed {
                                this.clear_armed = true;
                                cx.notify();
                                return;
                            }
                            this.clear_armed = false;
                            this.clearing = true;
                            cx.notify();

                            cx.spawn(async move |weak, cx| {
                                let outcome = crate::RUNTIME
                                    .spawn(crate::media::audio_cache::clear_audio_cache())
                                    .await
                                    .unwrap_or_default();
                                weak.update(cx, |this, cx| {
                                    this.clearing = false;
                                    cx.notify();
                                    let mb =
                                        format!("{:.1}", outcome.bytes as f64 / (1024.0 * 1024.0));
                                    if outcome.failed > 0 {
                                        emit_toast(Toast::warning(tr!(
                                            "PLAYBACK_AUDIO_CACHE_CLEAR_PARTIAL",
                                            "Some cached files are in use and could not be removed."
                                        )));
                                    } else {
                                        emit_toast(Toast::success(tr!(
                                            "PLAYBACK_AUDIO_CACHE_CLEARED",
                                            "Cleared {{mb}} MB of cached audio",
                                            mb = mb
                                        )));
                                    }
                                })
                                .ok();
                            })
                            .detach();
                        })),
                ),
            )
            .into_any_element()
    }

    #[cfg(not(feature = "online_sources"))]
    fn audio_cache_section(&self, _: &mut Context<Self>) -> AnyElement {
        div().into_any_element()
    }
}

impl Render for PlaybackSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playback = &self.settings.read(cx).playback;
        let always_repeat = playback.always_repeat;
        let prev_track_jump_first = playback.prev_track_jump_first;
        let keep_current_on_queue_clear = playback.keep_current_on_queue_clear;
        let consume = playback.consume;
        let fallback_preamp_db = playback.replaygain.fallback_preamp_db;
        let prevent_idle = playback.prevent_idle;

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
                always_repeat,
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
                prev_track_jump_first,
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
                keep_current_on_queue_clear,
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
                consume,
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
                        .value(fallback_preamp_db as f32)
                        .default_value(0.0)
                        .format_value(|v| -> SharedString { format!("{:+.1} dB", v).into() })
                        .on_change({
                            let weak_self = cx.weak_entity();
                            move |v, _, cx| {
                                // 实时值写入模型，但**不** notify 设置模型：每个 tick 都
                                // notify 模型会级联到应用级的 refresh_windows 观察者
                                // （app.rs）——拖拽时每次鼠标移动都重绘所有窗口。这里只
                                // notify 本页面实体，让滑块与读数跟随拖拽；前置增益等拖拽
                                // 结束后由防抖保存应用到播放线程。
                                settings.update(cx, |settings, _| {
                                    settings.playback.replaygain.fallback_preamp_db = v as f64;
                                });
                                if let Some(this) = weak_self.upgrade() {
                                    this.update(cx, |this, cx| {
                                        cx.notify();
                                        this.save_debounce.schedule(&this.settings, cx);
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
                .child(checkbox("playback-prevent-idle-check", prevent_idle)),
            )
            .child(self.audio_cache_section(cx))
    }
}
