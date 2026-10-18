use std::sync::OnceLock;

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, px,
};

use crate::{
    settings::{
        SettingsGlobal,
        interface::{
            DEFAULT_GRID_MIN_ITEM_WIDTH, MAX_GRID_MIN_ITEM_WIDTH, MIN_GRID_MIN_ITEM_WIDTH,
            StartupLibraryView, clamp_grid_min_item_width,
        },
        save_settings,
    },
    ui::command_palette::OpenPalette,
    ui::components::{
        button::{ButtonIntent, ButtonStyle, button},
        checkbox::checkbox,
        dropdown::dropdown,
        label::{Label, label},
        labeled_slider::labeled_slider,
        section_header::section_header,
    },
    ui::design::{SPACE_LG, SPACE_SM},
    ui::global_actions::OpenThemeFolder,
    ui::theme::{ThemeOption, ThemeOptionsGlobal},
};

use super::debounced_save::DebouncedSave;

#[derive(Clone)]
pub struct LanguageOption {
    pub code: &'static str,
    pub display_name: SharedString,
}

/// 静态语言列表，只构建一次。跟随系统那个标签在首次使用时解析；反正切换语言本来就
/// 需要重启应用，所以缓存它是安全的。
fn available_languages() -> &'static [LanguageOption] {
    static LANGUAGES: OnceLock<Vec<LanguageOption>> = OnceLock::new();
    LANGUAGES.get_or_init(|| {
        vec![
            LanguageOption {
                code: "",
                display_name: tr!("LANGUAGE_SYSTEM_DEFAULT", "System Default").into(),
            },
            LanguageOption {
                code: "cs",
                display_name: "Čeština".into(),
            },
            LanguageOption {
                code: "de",
                display_name: "Deutsch".into(),
            },
            LanguageOption {
                code: "el",
                display_name: "Ελληνικά".into(),
            },
            LanguageOption {
                code: "es",
                display_name: "Español".into(),
            },
            LanguageOption {
                code: "en",
                display_name: "English".into(),
            },
            LanguageOption {
                code: "ja",
                display_name: "日本語".into(),
            },
            LanguageOption {
                code: "zh-CN",
                display_name: "简体中文".into(),
            },
            LanguageOption {
                code: "sk",
                display_name: "Slovenčina".into(),
            },
            LanguageOption {
                code: "fi",
                display_name: "Suomi".into(),
            },
            LanguageOption {
                code: "vi",
                display_name: "Tiếng Việt".into(),
            },
        ]
    })
}

/// 把 `update` 应用到 `settings.interface`，随后持久化设置文件并 notify。开关和下拉
/// 写入都走这里；网格宽度滑块改为直接写入，这样拖拽时不会每个 tick 都保存或重绘
/// （见它的 `on_change`）。
fn update_interface_settings(
    settings: &Entity<crate::settings::Settings>,
    cx: &mut App,
    update: impl FnOnce(&mut crate::settings::interface::InterfaceSettings),
) {
    settings.update(cx, move |settings, cx| {
        update(&mut settings.interface);
        save_settings(cx, settings);
        cx.notify();
    });
}

/// 一个勾选行：点击标签即经 `update_interface` 切换 `InterfaceSettings` 的一个
/// `bool` 字段。勾选框 id 显式传入，好让一行的两个 id 都可 grep。
fn toggle_row(
    cx: &Context<InterfaceSettings>,
    label_id: &'static str,
    check_id: &'static str,
    title: impl Into<SharedString>,
    subtext: impl Into<SharedString>,
    checked: bool,
    toggle: fn(&mut crate::settings::interface::InterfaceSettings),
) -> Label {
    label(label_id, title)
        .subtext(subtext)
        .cursor_pointer()
        .w_full()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.update_interface(cx, toggle);
        }))
        .child(checkbox(check_id, checked))
}

pub struct InterfaceSettings {
    settings: Entity<crate::settings::Settings>,
    theme_options: Entity<Vec<ThemeOption>>,
    save_debounce: DebouncedSave,
}

impl InterfaceSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let settings_global = cx.global::<SettingsGlobal>();
        let settings = settings_global.model.clone();
        let theme_options = cx.global::<ThemeOptionsGlobal>().model.clone();

        cx.new(|cx| {
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();
            cx.observe(&theme_options, |_, _, cx| cx.notify()).detach();

            Self {
                settings,
                theme_options,
                save_debounce: DebouncedSave::new(),
            }
        })
    }

    /// 把 `update` 应用到 interface 分区，重新钳位网格条目宽度，然后持久化并 notify。
    /// 开关走这里；从不触碰网格的下拉直接走 [`update_interface_settings`]。
    fn update_interface(
        &self,
        cx: &mut App,
        update: impl FnOnce(&mut crate::settings::interface::InterfaceSettings),
    ) {
        update_interface_settings(&self.settings, cx, |interface| {
            update(interface);
            interface.grid_min_item_width =
                clamp_grid_min_item_width(interface.grid_min_item_width);
        });
    }
}

impl Render for InterfaceSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let interface = self.settings.read(cx).interface.clone();
        let settings = self.settings.clone();

        let language_dropdown = {
            let settings_c = settings.clone();
            let mut dd = dropdown::<String>("language-dropdown")
                .w(px(250.0))
                .selected(interface.language.clone())
                .on_change(move |code, _, cx| {
                    update_interface_settings(&settings_c, cx, |s| s.language = code.clone());
                });
            for lang in available_languages() {
                dd = dd.option(lang.code.to_string(), lang.display_name.clone());
            }
            dd
        };

        let theme_dropdown = {
            let settings_c = settings.clone();
            // 有效性改查 fs watcher 维护的 theme_options 列表，替代逐 render
            // 的 is_file() stat；内置 Light 无对应文件，列表命中才能正确回显。
            let resolved = interface.theme.clone().filter(|selected| {
                self.theme_options
                    .read(cx)
                    .iter()
                    .any(|t| t.id.as_deref() == Some(selected.as_str()))
            });
            let mut dd = dropdown::<Option<String>>("theme-dropdown")
                .w(px(250.0))
                .selected(resolved)
                .on_change(move |id, _, cx| {
                    update_interface_settings(&settings_c, cx, |s| s.theme = id.clone());
                });
            for theme in self.theme_options.read(cx).iter() {
                let label: SharedString = if theme.id.is_none() {
                    tr!("THEME_DEFAULT", "Default").into()
                } else {
                    theme.label.clone().into()
                };
                dd = dd.option(theme.id.clone(), label);
            }
            dd
        };

        let startup_view_dropdown = {
            let settings_c = settings.clone();
            dropdown::<StartupLibraryView>("startup-library-view-dropdown")
                .w(px(250.0))
                .selected(interface.startup_library_view)
                .option(StartupLibraryView::Albums, tr!("ALBUMS"))
                .option(StartupLibraryView::Artists, tr!("ARTISTS"))
                .option(StartupLibraryView::Tracks, tr!("TRACKS"))
                .option(StartupLibraryView::LikedSongs, tr!("LIKED_SONGS"))
                .option(StartupLibraryView::Files, tr!("FILES"))
                .on_change(move |view, _, cx| {
                    update_interface_settings(&settings_c, cx, |s| s.startup_library_view = *view);
                })
        };

        let body = div()
            .flex()
            .flex_col()
            .gap(SPACE_LG)
            .child(section_header(tr!("INTERFACE")))
            .child(
                label("language-selector", tr!("LANGUAGE", "Language"))
                    .subtext(tr!(
                        "LANGUAGE_SUBTEXT",
                        "Select your preferred language for the application. Changes to the \
                        language will take effect after restarting the application."
                    ))
                    .w_full()
                    .child(language_dropdown),
            )
            .child(
                label("theme-selector", tr!("INTERFACE_THEME", "Theme"))
                    .subtext(tr!(
                        "INTERFACE_THEME_SUBTEXT",
                        "Choose a built-in theme or add your own. Place custom theme files in the \
                        themes folder. Changes apply immediately."
                    ))
                    .w_full()
                    .child(
                        div().flex().flex_col().gap(SPACE_SM).child(theme_dropdown).child(
                            button()
                                .style(ButtonStyle::Regular)
                                .intent(ButtonIntent::Secondary)
                                .child(tr!("OPEN_THEMES_FOLDER", "Open Themes Folder"))
                                .id("open-themes-folder-button")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.defer(move |cx| {
                                        cx.dispatch_action(&OpenThemeFolder);
                                    });
                                })),
                        ),
                    ),
            )
            .child(
                label(
                    "startup-library-view-selector",
                    tr!("INTERFACE_STARTUP_LIBRARY_VIEW", "Default startup view"),
                )
                .subtext(tr!(
                    "INTERFACE_STARTUP_LIBRARY_VIEW_SUBTEXT",
                    "Choose which library page opens when Meliora launches."
                ))
                .w_full()
                .child(startup_view_dropdown),
            )
            .child({
                let full_width_label = label(
                    "interface-full-width-library",
                    tr!("INTERFACE_FULL_WIDTH_LIBRARY", "Full-width library"),
                )
                .subtext(tr!(
                    "INTERFACE_FULL_WIDTH_LIBRARY_SUBTEXT",
                    "Allows the library to take up the full width of the screen."
                ))
                .cursor_pointer()
                .w_full()
                .child(checkbox(
                    "interface-full-width-library-check",
                    interface.full_width_library || interface.two_column_library,
                ));

                if interface.two_column_library {
                    full_width_label.opacity(0.5)
                } else {
                    full_width_label.on_click(cx.listener(move |this, _, _, cx| {
                        this.update_interface(cx, |interface| {
                            interface.full_width_library = !interface.full_width_library;
                        });
                    }))
                }
            })
            .child(toggle_row(
                cx,
                "interface-two-column-library",
                "interface-two-column-library-check",
                tr!("INTERFACE_TWO_COLUMN_LIBRARY", "Two-column library"),
                tr!(
                    "INTERFACE_TWO_COLUMN_LIBRARY_SUBTEXT",
                    "Show navigation pages (like Artists) and content pages (like an album) side by side."
                ),
                interface.two_column_library,
                |interface| {
                    interface.two_column_library = !interface.two_column_library;
                    // 保持冻结的整宽槽位同步：two_column 会强制把生效值置为开启，
                    // 少了这一步，之后关掉 two_column 就会静默复活该槽位之前的过期值。
                    if interface.two_column_library {
                        interface.full_width_library = true;
                    }
                },
            ))
            .child(
                label(
                    "interface-grid-min-item-width",
                    tr!("INTERFACE_GRID_MIN_ITEM_WIDTH", "Grid item width"),
                )
                .subtext(tr!(
                    "INTERFACE_GRID_MIN_ITEM_WIDTH_SUBTEXT",
                    "Adjusts the minimum width of items in grid view."
                ))
                .w_full()
                .child(
                    labeled_slider("interface-grid-min-item-width-slider")
                        .slider_id("interface-grid-min-item-width-slider-track")
                        .w(px(250.0))
                        .min(MIN_GRID_MIN_ITEM_WIDTH)
                        .max(MAX_GRID_MIN_ITEM_WIDTH)
                        .default_value(DEFAULT_GRID_MIN_ITEM_WIDTH)
                        .value(interface.normalized_grid_min_item_width())
                        .format_value(|v| format!("{v:.0} px").into())
                        .on_change({
                            let weak_self = cx.weak_entity();
                            move |value, _, cx| {
                                // 实时值写入模型，但**不** notify 设置模型：每个 tick 都
                                // notify 模型会级联到应用级的 refresh_windows 观察者
                                // （app.rs）——拖拽时每次鼠标移动都重绘所有窗口。这里只
                                // notify 本页面实体，让滑块与读数跟随拖拽；其余 UI 等防抖
                                // 保存落地时刷一次（外加设置文件监视器的那次真实刷新）。
                                settings.update(cx, |settings, _| {
                                    settings.interface.grid_min_item_width =
                                        clamp_grid_min_item_width(value);
                                });
                                if let Some(this) = weak_self.upgrade() {
                                    this.update(cx, |this, cx| {
                                        cx.notify();
                                        this.save_debounce.schedule(&this.settings, cx);
                                    });
                                }
                            }
                        }),
                ),
            )
            .child(toggle_row(
                cx,
                "interface-reduced-motion",
                "interface-reduced-motion-check",
                tr!("INTERFACE_REDUCED_MOTION", "Reduced motion"),
                tr!(
                    "INTERFACE_REDUCED_MOTION_SUBTEXT",
                    "Disables smooth scrolling, fades, and other motion-heavy UI animations."
                ),
                interface.reduced_motion,
                |interface| interface.reduced_motion = !interface.reduced_motion,
            ))
            .child(toggle_row(
                cx,
                "interface-always-show-scrollbars",
                "interface-always-show-scrollbars-check",
                tr!("INTERFACE_ALWAYS_SHOW_SCROLLBARS", "Always show scrollbars"),
                tr!(
                    "INTERFACE_ALWAYS_SHOW_SCROLLBARS_SUBTEXT",
                    "Keeps scrollbars visible instead of hiding them automatically."
                ),
                interface.always_show_scrollbars,
                |interface| interface.always_show_scrollbars = !interface.always_show_scrollbars,
            ))
            .child(toggle_row(
                cx,
                "interface-slim-scrollbars",
                "interface-slim-scrollbars-check",
                tr!("INTERFACE_SLIM_SCROLLBARS", "Slim scrollbars"),
                tr!(
                    "INTERFACE_SLIM_SCROLLBARS_SUBTEXT",
                    "Use slimmer scrollbars for a cleaner visual style."
                ),
                interface.slim_scrollbars,
                |interface| interface.slim_scrollbars = !interface.slim_scrollbars,
            ))
            .child(toggle_row(
                cx,
                "interface-queue-select-on-click",
                "interface-queue-select-on-click-check",
                tr!("INTERFACE_QUEUE_SELECT_ON_CLICK", "Clicking on queue selects tracks"),
                tr!(
                    "INTERFACE_QUEUE_SELECT_ON_CLICK_SUBTEXT",
                    "Clicking on a queue item selects it, double clicking plays it."
                ),
                interface.queue_select_on_click,
                |interface| interface.queue_select_on_click = !interface.queue_select_on_click,
            ))
            .child(toggle_row(
                cx,
                "interface-close-to-tray",
                "interface-close-to-tray-check",
                tr!("INTERFACE_CLOSE_TO_TRAY", "Closing the window hides to tray"),
                tr!(
                    "INTERFACE_CLOSE_TO_TRAY_SUBTEXT",
                    "On: closing the main window hides it to the tray and music keeps playing. Off: closing the main window quits Meliora."
                ),
                interface.close_to_tray,
                |interface| interface.close_to_tray = !interface.close_to_tray,
            ))
            .child(toggle_row(
                cx,
                "interface-start-minimized",
                "interface-start-minimized-check",
                tr!("INTERFACE_START_MINIMIZED", "Start minimized to tray"),
                tr!(
                    "INTERFACE_START_MINIMIZED_SUBTEXT",
                    "Start hidden in the tray; click the tray icon to show the window."
                ),
                interface.start_minimized,
                |interface| interface.start_minimized = !interface.start_minimized,
            ));

        body.child(
            label(
                "interface-command-palette",
                tr!("COMMAND_PALETTE", "Command Palette"),
            )
            .subtext(tr!(
                "INTERFACE_COMMAND_PALETTE_SUBTEXT",
                "Open the command palette to quickly access actions, shortcuts, and search."
            ))
            .w_full()
            .child(
                div().mt(SPACE_SM).child(
                    button()
                        .style(ButtonStyle::Regular)
                        .intent(ButtonIntent::Secondary)
                        .child(tr!("COMMAND_PALETTE_OPEN", "Open Command Palette"))
                        .id("open-command-palette-button")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&OpenPalette);
                            });
                        })),
                ),
            ),
        )
    }
}
