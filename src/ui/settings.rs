pub mod corrupt_settings_dialog;
mod about;
mod equalizer;
mod interface;
#[cfg(feature = "kugou")]
mod kugou;
#[cfg(feature = "netease")]
mod netease;
#[cfg(feature = "kugou")]
use crate::ui::components::icons::KUGOU;
#[cfg(feature = "netease")]
use crate::ui::components::icons::NETEASE;
mod library;
mod playback;

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, TitlebarOptions,
    Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    WindowOptions, div, prelude::FluentBuilder, px,
};

use crate::{
    settings::{Settings, SettingsGlobal, save_settings, storage::DEFAULT_SIDEBAR_WIDTH},
    ui::{
        components::{
            icons::{ADJUSTMENTS, ALERT_CIRCLE, BOOKS, PLAY, WORLD},
            scrollbar::{ScrollableHandle, floating_scrollbar},
            sidebar::{sidebar, sidebar_item},
            window_chrome::window_chrome,
            window_header::header,
        },
        settings::{
            about::AboutSettings, equalizer::EqualizerSettings, interface::InterfaceSettings,
            library::LibrarySettings, playback::PlaybackSettings,
        },
        theme::Theme,
    },
};

#[cfg(feature = "kugou")]
use crate::ui::settings::kugou::KugouSettings;
#[cfg(feature = "netease")]
use crate::ui::settings::netease::NeteaseSettings;

pub fn open_settings_window(cx: &mut App) {
    open_or_focus_settings_window(cx, None);
}

fn find_settings_window(cx: &App) -> Option<WindowHandle<SettingsWindow>> {
    cx.windows()
        .into_iter()
        .find_map(|window| window.downcast::<SettingsWindow>())
}

pub fn open_settings_window_with_section(cx: &mut App, section: SettingsSectionKind) {
    open_or_focus_settings_window(cx, Some(section));
}

fn open_or_focus_settings_window(cx: &mut App, section: Option<SettingsSectionKind>) {
    if let Some(window) = find_settings_window(cx) {
        cx.activate(true);
        cx.defer(move |cx| {
            let _ = window.update(cx, |settings, window, cx| {
                if let Some(section) = section {
                    settings.switch_section(section, cx);
                }
                window.activate_window();
            });
        });
        return;
    }

    let section = section.unwrap_or(SettingsSectionKind::Interface);
    let bounds = WindowBounds::Windowed(gpui::Bounds::centered(
        None,
        gpui::size(px(900.0), px(600.0)),
        cx,
    ));

    cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            window_background: WindowBackgroundAppearance::Opaque,
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(gpui::size(px(640.0), px(420.0))),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(tr!("SETTINGS", "Settings"))),
                appears_transparent: true,
                traffic_light_position: Some(gpui::Point {
                    x: px(12.0),
                    y: px(11.0),
                }),
            }),
            kind: WindowKind::Normal,
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(tr!("SETTINGS").to_string().as_str());
            SettingsWindow::new(section, cx)
        },
    )
    .ok();
}

pub(super) fn close_orphaned_settings_windows(cx: &mut App) {
    if super::app::has_main_window(cx) {
        return;
    }

    let settings_windows = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<SettingsWindow>())
        .collect::<Vec<_>>();

    for settings_window in settings_windows {
        cx.update_window(*settings_window, |_, window, _| {
            window.remove_window();
        })
        .ok();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsSectionKind {
    Interface,
    Library,
    Playback,
    Equalizer,
    #[cfg(feature = "kugou")]
    Kugou,
    #[cfg(feature = "netease")]
    Netease,
    About,
}

impl SettingsSectionKind {
    fn id(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Library => "library",
            Self::Playback => "playback",
            Self::Equalizer => "equalizer",
            #[cfg(feature = "kugou")]
            Self::Kugou => "kugou",
            #[cfg(feature = "netease")]
            Self::Netease => "netease",
            Self::About => "about",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Interface => WORLD,
            Self::Library => BOOKS,
            Self::Playback => PLAY,
            Self::Equalizer => ADJUSTMENTS,
            #[cfg(feature = "kugou")]
            Self::Kugou => KUGOU,
            #[cfg(feature = "netease")]
            Self::Netease => NETEASE,
            Self::About => ALERT_CIRCLE,
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Interface => tr!("INTERFACE", "Interface").into(),
            Self::Library => tr!("LIBRARY", "Library").into(),
            Self::Playback => tr!("PLAYBACK", "Playback").into(),
            Self::Equalizer => tr!("EQUALIZER", "Equalizer").into(),
            #[cfg(feature = "kugou")]
            Self::Kugou => tr!("KUGOU_SECTION", "KuGou Music").into(),
            #[cfg(feature = "netease")]
            Self::Netease => tr!("NETEASE_SECTION", "NetEase Cloud Music").into(),
            Self::About => tr!("ABOUT_SECTION", "About and Help").into(),
        }
    }

    // sections that fill the content area vertically instead of scrolling
    fn fills_height(self) -> bool {
        matches!(self, Self::Equalizer)
    }
}

#[derive(Clone, PartialEq)]
enum SettingsSection {
    Interface(Entity<InterfaceSettings>),
    Library(Entity<LibrarySettings>),
    Playback(Entity<PlaybackSettings>),
    Equalizer(Entity<EqualizerSettings>),
    #[cfg(feature = "kugou")]
    Kugou(Entity<KugouSettings>),
    #[cfg(feature = "netease")]
    Netease(Entity<NeteaseSettings>),
    About(Entity<AboutSettings>),
}

impl SettingsSection {
    fn new(section: SettingsSectionKind, cx: &mut App) -> Self {
        match section {
            SettingsSectionKind::Interface => Self::Interface(InterfaceSettings::new(cx)),
            SettingsSectionKind::Library => Self::Library(LibrarySettings::new(cx)),
            SettingsSectionKind::Playback => Self::Playback(PlaybackSettings::new(cx)),
            SettingsSectionKind::Equalizer => Self::Equalizer(EqualizerSettings::new(cx)),
            #[cfg(feature = "kugou")]
            SettingsSectionKind::Kugou => Self::Kugou(KugouSettings::new(cx)),
            #[cfg(feature = "netease")]
            SettingsSectionKind::Netease => Self::Netease(NeteaseSettings::new(cx)),
            SettingsSectionKind::About => Self::About(AboutSettings::new(cx)),
        }
    }

    fn kind(&self) -> SettingsSectionKind {
        match self {
            Self::Interface(_) => SettingsSectionKind::Interface,
            Self::Library(_) => SettingsSectionKind::Library,
            Self::Playback(_) => SettingsSectionKind::Playback,
            Self::Equalizer(_) => SettingsSectionKind::Equalizer,
            #[cfg(feature = "kugou")]
            Self::Kugou(_) => SettingsSectionKind::Kugou,
            #[cfg(feature = "netease")]
            Self::Netease(_) => SettingsSectionKind::Netease,
            Self::About(_) => SettingsSectionKind::About,
        }
    }

    fn element(&self) -> gpui::AnyElement {
        match self {
            Self::Interface(interface) => interface.clone().into_any_element(),
            Self::Library(library) => library.clone().into_any_element(),
            Self::Playback(playback) => playback.clone().into_any_element(),
            Self::Equalizer(equalizer) => equalizer.clone().into_any_element(),
            #[cfg(feature = "kugou")]
            Self::Kugou(kugou) => kugou.clone().into_any_element(),
            #[cfg(feature = "netease")]
            Self::Netease(netease) => netease.clone().into_any_element(),
            Self::About(about) => about.clone().into_any_element(),
        }
    }
}

struct SettingsWindow {
    active: SettingsSection,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    first_render: bool,
    redraw: bool,
}

impl SettingsWindow {
    fn new(initial_section: SettingsSectionKind, cx: &mut App) -> gpui::Entity<Self> {
        let focus_handle = cx.focus_handle();
        let active = SettingsSection::new(initial_section, cx);
        cx.new(|_| Self {
            active,
            scroll_handle: ScrollHandle::new(),
            first_render: true,
            focus_handle,
            redraw: false,
        })
    }

    fn switch_section(&mut self, section: SettingsSectionKind, cx: &mut Context<Self>) {
        if self.active.kind() == section {
            return;
        }

        self.active = SettingsSection::new(section, cx);
        self.scroll_handle.scroll_to_top_of_item(0);
        cx.notify();

        // Force a redraw to make sure that scrollbars and
        // padding are properly updated.
        self.redraw = true;
    }

    fn render_section_item(
        &self,
        section: SettingsSectionKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        sidebar_item(section.id())
            .icon(section.icon())
            .child(section.label())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_section(section, cx);
            }))
            .when(self.active.kind() == section, |this| this.active())
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.first_render {
            self.first_render = false;
            self.focus_handle.focus(window, cx);
        }

        if self.redraw {
            self.redraw = false;
            window.request_animation_frame();
        }

        let theme = cx.global::<Theme>();
        let active = &self.active;
        let scroll_handle = self.scroll_handle.clone();
        let scrollbar_always_visible = {
            let settings = cx.global::<SettingsGlobal>();
            let scroll_handle: ScrollableHandle = scroll_handle.clone().into();

            // On the first draw, total_content_height returns 0. In this case,
            // we want to always draw padding to prevent a noticeable jitter.
            settings.model.read(cx).interface.always_show_scrollbars
                && (scroll_handle.total_content_height() <= 0.0
                    || scroll_handle.should_draw_vertical_scrollbar())
        };

        let content = active.element();
        let fills_height = active.kind().fills_height();
        let body = if fills_height {
            div()
                .size_full()
                .overflow_hidden()
                .child(content)
                .into_any_element()
        } else {
            div()
                .id("settings-content-scroll")
                .w_full()
                .overflow_y_scroll()
                .track_scroll(&scroll_handle)
                .flex_shrink(1.0)
                .overflow_x_hidden()
                .child(
                    div()
                        .w_full()
                        .p(px(16.0))
                        .when(scrollbar_always_visible, |div| {
                            // 16px padding + 10px buffer
                            div.pr(px(26.0))
                        })
                        .child(content),
                )
                .into_any_element()
        };
        let sidebar = sidebar()
            .width(DEFAULT_SIDEBAR_WIDTH)
            .h_full()
            .pt(px(8.0))
            .pb(px(8.0))
            .pl(px(8.0))
            .pr(px(8.0))
            .border_r_1()
            .border_color(theme.border_color)
            .overflow_hidden()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .child(self.render_section_item(SettingsSectionKind::Interface, cx))
            .child(self.render_section_item(SettingsSectionKind::Library, cx))
            .child(self.render_section_item(SettingsSectionKind::Playback, cx))
            .child(self.render_section_item(SettingsSectionKind::Equalizer, cx));

        #[cfg(feature = "kugou")]
        let sidebar = sidebar.child(self.render_section_item(SettingsSectionKind::Kugou, cx));

        #[cfg(feature = "netease")]
        let sidebar = sidebar.child(self.render_section_item(SettingsSectionKind::Netease, cx));

        let sidebar =
            sidebar.child(self.render_section_item(SettingsSectionKind::About, cx));

        window_chrome(
            div()
                .track_focus(&self.focus_handle)
                .key_context("SettingsWindow")
                .size_full()
                .flex()
                .flex_col()
                .child(header())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_shrink(1.0)
                        .flex_grow(1.0)
                        .min_h(px(0.0))
                        .child(sidebar)
                        .child(
                            div()
                                .relative()
                                .flex()
                                .flex_grow(1.0)
                                .flex_shrink(1.0)
                                .min_h(px(0.0))
                                .overflow_hidden()
                                .child(body)
                                .when(!fills_height, |this| {
                                    this.child(
                                        floating_scrollbar("settings-scrollbar", scroll_handle)
                                            .right(px(4.0)),
                                    )
                                }),
                        ),
                ),
        )
    }
}

/// Shared by the playback and KuGou settings pages: mutate
/// `settings.playback`, persist, and redraw.
pub(crate) fn update_playback_settings(
    settings: &Entity<Settings>,
    cx: &mut App,
    update: impl FnOnce(&mut crate::settings::playback::PlaybackSettings),
) {
    settings.update(cx, move |settings, cx| {
        update(&mut settings.playback);
        save_settings(cx, settings);
        cx.notify();
    });
}
