use std::{path::PathBuf, sync::OnceLock};

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
    ui::theme::{ThemeOption, ThemeOptionsGlobal, resolve_theme_relative_path},
};

use super::debounced_save::DebouncedSave;

#[derive(Clone)]
pub struct LanguageOption {
    pub code: &'static str,
    pub display_name: SharedString,
}

/// Static language list, built once. The system-default label resolves at
/// first use; language changes require an app restart anyway, so caching it
/// is safe.
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

/// Applies `update` to `settings.interface`, then persists the settings
/// file and notifies. Toggles and dropdown writes go through here; the
/// grid-width slider writes directly instead so a drag does not save or
/// repaint per tick (see its `on_change`).
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

/// One checkbox row: clicking the label toggles one `bool` field of
/// `InterfaceSettings` via `update_interface`. The checkbox id is passed
/// explicitly so both ids of the row pair stay greppable.
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
    data_dir: PathBuf,
    theme_options: Entity<Vec<ThemeOption>>,
    save_debounce: DebouncedSave,
}

impl InterfaceSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let settings_global = cx.global::<SettingsGlobal>();
        let settings = settings_global.model.clone();
        let data_dir = settings_global
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let theme_options = cx.global::<ThemeOptionsGlobal>().model.clone();

        cx.new(|cx| {
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();
            cx.observe(&theme_options, |_, _, cx| cx.notify()).detach();

            Self {
                settings,
                data_dir,
                theme_options,
                save_debounce: DebouncedSave::new(),
            }
        })
    }

    /// Applies `update` to the interface section, re-clamps the grid item
    /// width, then persists and notifies. Toggles route through here; the
    /// dropdowns that never touch the grid go through
    /// [`update_interface_settings`] directly.
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
            let resolved = resolve_theme_relative_path(&self.data_dir, interface.theme.as_deref());
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
                |interface| interface.two_column_library = !interface.two_column_library,
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
                                // live value lands in the model WITHOUT notifying the settings
                                // model: a per-tick model notify cascades into the app-wide
                                // refresh_windows observer (app.rs) - a full repaint of every
                                // window per mouse move while dragging. Only this page entity
                                // is notified so the slider and its readout track the drag;
                                // the rest of the UI refreshes once the debounced save lands
                                // (plus the settings file watcher's real refresh).
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
