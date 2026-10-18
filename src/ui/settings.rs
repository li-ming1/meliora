mod about;
pub mod corrupt_settings_dialog;
mod debounced_save;
mod equalizer;
mod interface;
#[cfg(feature = "kugou")]
mod kugou;
#[cfg(feature = "kugou")]
mod online_common;
#[cfg(feature = "kugou")]
use crate::ui::components::icons::KUGOU;
mod library;
mod playback;
mod stats;

use cntp_i18n::tr;
use gpui::{
    Action, App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowHandle, WindowKind, WindowOptions, div, prelude::FluentBuilder, px,
};

use crate::{
    settings::{Settings, SettingsGlobal, save_settings, storage::DEFAULT_SIDEBAR_WIDTH},
    ui::{
        components::{
            button::{ButtonIntent, ButtonStyle, button},
            icons::{ADJUSTMENTS, ALERT_CIRCLE, BOOKS, PLAY, RANKING, WORLD},
            label::{Label, label},
            scrollbar::{ScrollableHandle, floating_scrollbar},
            sidebar::{sidebar, sidebar_item},
            window_chrome::window_chrome,
            window_header::header,
        },
        settings::{
            about::AboutSettings, equalizer::EqualizerSettings, interface::InterfaceSettings,
            library::LibrarySettings, playback::PlaybackSettings, stats::StatsSettings,
        },
        theme::Theme,
    },
};

#[cfg(feature = "kugou")]
use crate::ui::settings::kugou::KugouSettings;

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

    let handle = cx
        .open_window(
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
    // 登记 `[frame]` 探针的按窗口归因标签。均衡器与统计页都在这扇窗里，而它此前只能以
    // `win-<id>` 出现（2026-10-17 真机实测：该窗 frames 186-214/30s、draw_p50 5235µs、
    // p99 24.8ms，比主窗的 2.5-3.2ms 贵近一倍），帧成本无法与主窗分开判读。
    // 同标签重开会覆盖，与 desktop-lyrics 同一套约定。
    if let Some(handle) = handle {
        crate::register_frame_window_label(handle.into(), "settings");
    }
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
    Stats,
    #[cfg(feature = "kugou")]
    Kugou,
    About,
}

impl SettingsSectionKind {
    fn id(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Library => "library",
            Self::Playback => "playback",
            Self::Equalizer => "equalizer",
            Self::Stats => "stats",
            #[cfg(feature = "kugou")]
            Self::Kugou => "kugou",
            Self::About => "about",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Interface => WORLD,
            Self::Library => BOOKS,
            Self::Playback => PLAY,
            Self::Equalizer => ADJUSTMENTS,
            Self::Stats => RANKING,
            #[cfg(feature = "kugou")]
            Self::Kugou => KUGOU,
            Self::About => ALERT_CIRCLE,
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Interface => tr!("INTERFACE", "Interface").into(),
            Self::Library => tr!("LIBRARY", "Library").into(),
            Self::Playback => tr!("PLAYBACK", "Playback").into(),
            Self::Equalizer => tr!("EQUALIZER", "Equalizer").into(),
            Self::Stats => tr!("STATS_SECTION", "Listening Stats").into(),
            #[cfg(feature = "kugou")]
            Self::Kugou => tr!("KUGOU_SECTION", "KuGou Music").into(),
            Self::About => tr!("ABOUT_SECTION", "About and Help").into(),
        }
    }

    // 在竖直方向填满内容区、而不是滚动的那些分区
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
    Stats(Entity<StatsSettings>),
    #[cfg(feature = "kugou")]
    Kugou(Entity<KugouSettings>),
    About(Entity<AboutSettings>),
}

impl SettingsSection {
    fn new(section: SettingsSectionKind, cx: &mut App) -> Self {
        match section {
            SettingsSectionKind::Interface => Self::Interface(InterfaceSettings::new(cx)),
            SettingsSectionKind::Library => Self::Library(LibrarySettings::new(cx)),
            SettingsSectionKind::Playback => Self::Playback(PlaybackSettings::new(cx)),
            SettingsSectionKind::Equalizer => Self::Equalizer(EqualizerSettings::new(cx)),
            SettingsSectionKind::Stats => Self::Stats(StatsSettings::new(cx)),
            #[cfg(feature = "kugou")]
            SettingsSectionKind::Kugou => Self::Kugou(KugouSettings::new(cx)),
            SettingsSectionKind::About => Self::About(AboutSettings::new(cx)),
        }
    }

    fn kind(&self) -> SettingsSectionKind {
        match self {
            Self::Interface(_) => SettingsSectionKind::Interface,
            Self::Library(_) => SettingsSectionKind::Library,
            Self::Playback(_) => SettingsSectionKind::Playback,
            Self::Equalizer(_) => SettingsSectionKind::Equalizer,
            Self::Stats(_) => SettingsSectionKind::Stats,
            #[cfg(feature = "kugou")]
            Self::Kugou(_) => SettingsSectionKind::Kugou,
            Self::About(_) => SettingsSectionKind::About,
        }
    }

    fn element(&self) -> gpui::AnyElement {
        match self {
            Self::Interface(interface) => interface.clone().into_any_element(),
            Self::Library(library) => library.clone().into_any_element(),
            Self::Playback(playback) => playback.clone().into_any_element(),
            Self::Equalizer(equalizer) => equalizer.clone().into_any_element(),
            Self::Stats(stats) => stats.clone().into_any_element(),
            #[cfg(feature = "kugou")]
            Self::Kugou(kugou) => kugou.clone().into_any_element(),
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

        // 强制重绘，确保滚动条与内边距被正确更新。
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

            // 首次绘制时 total_content_height 返回 0。这种情况下我们永远要画出
            // 内边距，以防出现明显的抖动。
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
                            // 16px 内边距 + 10px 余量
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
            .child(self.render_section_item(SettingsSectionKind::Equalizer, cx))
            .child(self.render_section_item(SettingsSectionKind::Stats, cx));

        #[cfg(feature = "kugou")]
        let sidebar = sidebar.child(self.render_section_item(SettingsSectionKind::Kugou, cx));

        let sidebar = sidebar.child(self.render_section_item(SettingsSectionKind::About, cx));

        window_chrome(
            div()
                .id("settings-window")
                .track_focus(&self.focus_handle)
                .role(gpui::Role::Region)
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

/// 播放与酷狗设置页共用：改动 `settings.playback`、持久化并重绘。
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

/// 设置页共用的一行动作：标题 + 副文本，加一个整宽的次级按钮，点击时分发 `action`
/// （延迟执行，好让它跑在点击处理之外）。泛型过视图类型，因此 about 与 library
/// 两页共用同一份——此前 about 页私有一份，library 页又手写了三遍同形展开。
pub(crate) fn action_row<V: 'static, A: Action + Clone>(
    cx: &Context<V>,
    label_id: &'static str,
    title: impl Into<SharedString>,
    subtext: impl Into<SharedString>,
    button_id: &'static str,
    button_label: impl Into<SharedString>,
    action: A,
) -> Label {
    label(label_id, title).subtext(subtext).w_full().child(
        button()
            .style(ButtonStyle::Regular)
            .intent(ButtonIntent::Secondary)
            .child(Into::<SharedString>::into(button_label))
            .id(button_id)
            .on_click(cx.listener(move |_, _, _, cx| {
                let action = action.clone();
                cx.defer(move |cx| cx.dispatch_action(&action));
            })),
    )
}
