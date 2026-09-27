use crate::{
    settings::{Settings, SettingsGlobal, replaygain::ReplayGainMode, save_settings},
    ui::components::{
        icons::{ADJUSTMENTS, icon},
        labeled_slider::labeled_slider,
        popover::{PopoverPosition, popover},
        segmented_control::segmented_control,
        tooltip::build_tooltip,
    },
};
use cntp_i18n::tr;
use gpui::{prelude::FluentBuilder, *};

use super::{observe_notify, playback_toggle_button};
use crate::ui::design::ICON_SM;
use crate::ui::theme::Theme;

pub struct ReplayGainButton {
    settings: Entity<Settings>,
    show_popover: bool,
}

impl ReplayGainButton {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let settings = cx.global::<SettingsGlobal>().model.clone();
            observe_notify(cx, &settings);

            Self {
                settings,
                show_popover: false,
            }
        })
    }

    fn close_popover(&mut self, cx: &mut Context<Self>) {
        self.show_popover = false;
        cx.notify();
    }
}

impl Render for ReplayGainButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let rg_settings = self.settings.read(cx).playback.replaygain;
        let rg_mode = rg_settings.mode;
        let settings = self.settings.clone();
        let show_popover = self.show_popover;

        div()
            .relative()
            .child(
                playback_toggle_button(div().id("rg-button"), theme)
                    .tooltip(build_tooltip(tr!("REPLAY_GAIN", "ReplayGain")))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            window.prevent_default();

                            this.show_popover = !show_popover;
                            cx.notify();
                        }),
                    )
                    .child(
                        icon(ADJUSTMENTS)
                            .size(ICON_SM)
                            .when(rg_mode != ReplayGainMode::Off, |this| {
                                this.text_color(theme.playback_button_toggled)
                            }),
                    ),
            )
            .when(show_popover, |this| {
                let entity = cx.entity().downgrade();
                let click_away_entity = entity.clone();
                this.child(
                    popover()
                        .position(PopoverPosition::TopRight)
                        .edge_offset(px(8.0))
                        .on_dismiss(move |_, cx| {
                            entity.update(cx, |this, cx| this.close_popover(cx)).ok();
                        })
                        .min_w(px(200.0))
                        .on_mouse_down_out(move |_, _, cx| {
                            click_away_entity
                                .update(cx, |this, cx| this.close_popover(cx))
                                .ok();
                        })
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(10.0))
                                .p(px(4.0))
                                .pb(px(8.0))
                                .child(replaygain_mode_section(settings.clone(), rg_mode, theme))
                                .when(rg_mode != ReplayGainMode::Off, |this| {
                                    this.child(replaygain_preamp_section(
                                        settings.clone(),
                                        rg_settings.preamp_db,
                                        theme,
                                    ))
                                }),
                        ),
                )
            })
    }
}

/// The "ReplayGain Mode" label and segmented control; writes the chosen mode
/// straight through to settings.
fn replaygain_mode_section(settings: Entity<Settings>, mode: ReplayGainMode, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .mb(px(5.0))
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(tr!("RG_MODE_LABEL", "ReplayGain Mode")),
        )
        .child(
            segmented_control("rg-mode")
                .fit_content()
                .option(ReplayGainMode::Off, tr!("RG_OFF", "Off"))
                .option(ReplayGainMode::Auto, tr!("RG_AUTO", "Auto"))
                .option(ReplayGainMode::Track, tr!("RG_TRACK", "Track"))
                .option(ReplayGainMode::Album, tr!("RG_ALBUM", "Album"))
                .selected(mode)
                .on_change(move |mode, _, cx| {
                    settings.update(cx, |settings, cx| {
                        settings.playback.replaygain.mode = *mode;
                        save_settings(cx, settings);
                        cx.notify();
                    });
                }),
        )
}

/// The pre-amp label and slider; writes the gain straight through to settings.
fn replaygain_preamp_section(settings: Entity<Settings>, preamp_db: f64, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .mb(px(1.0))
                .child(tr!("RG_PREAMP_LABEL", "Pre-amp")),
        )
        .child(
            labeled_slider("rg-preamp")
                .slider_id("rg-preamp-track")
                .min(-6.0)
                .max(6.0)
                .value(preamp_db as f32)
                .default_value(0.0)
                .format_value(|v| format!("{:+.1} dB", v).into())
                .on_change(move |v, _, cx| {
                    settings.update(cx, |settings, cx| {
                        settings.playback.replaygain.preamp_db = v as f64;
                        save_settings(cx, settings);
                        cx.notify();
                    });
                }),
        )
}
