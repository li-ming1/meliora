use std::sync::atomic::{AtomicUsize, Ordering};

use cntp_i18n::tr;
use gpui::{Action, App, AppContext, Menu, MenuItem, SharedString, actions};
use tracing::{debug, error, info, warn};

use crate::{
    library::{db, scan::ScanInterface},
    playback::{interface::PlaybackInterface, queue::QueueItemData, thread::PlaybackState},
    toasts::{Toast, emit_toast},
    ui::{
        app::Pool,
        settings::{SettingsSectionKind, open_settings_window, open_settings_window_with_section},
        troubleshooting::{copy_troubleshooting_info, open_log},
    },
};

use super::models::{Models, PlaybackInfo};

actions!(
    meliora,
    [Quit, About, CloseWindow, Search, Settings, OpenEqualizer]
);
actions!(meliora, [OpenThemeFolder]);
actions!(
    player,
    [PlayPause, Next, Previous, ShuffleAll, StopAfterCurrent]
);
actions!(scan, [ForceScan, Scan]);
actions!(debug, [TestToast]);
actions!(meliora, [HideSelf, HideOthers, ShowAll]);
actions!(help, [Issues]);
actions!(queue, [Undo]);

pub fn register_actions(cx: &mut App) {
    debug!("registering actions");
    cx.on_action(quit);
    cx.on_action(close_window);
    cx.on_action(play_pause);
    cx.on_action(next);
    cx.on_action(previous);
    cx.on_action(hide_self);
    cx.on_action(hide_others);
    cx.on_action(show_all);
    cx.on_action(about);
    cx.on_action(force_scan);
    cx.on_action(open_settings);
    cx.on_action(open_equalizer);
    cx.on_action(undo);
    cx.on_action(issues);
    cx.on_action(shuffle_all);
    cx.on_action(stop_after_current);
    cx.on_action(scan);
    cx.on_action(open_log);
    cx.on_action(copy_troubleshooting_info);
    cx.on_action(open_theme_folder);
    cx.on_action(test_toast);

    debug!("actions: {:?}", cx.all_action_names());
    debug!("action available: {:?}", cx.is_action_available(&Quit));

    let mut app_menu = MenuBuilder::new(tr!("APP_NAME"))
        .add_item(menu_item(
            tr!("ABOUT", "About Meliora"),
            About,
            MenuPlatform::All,
        ))
        .add_item(menu_separator(MenuPlatform::All))
        .add_item(menu_item(tr!("SETTINGS"), Settings, MenuPlatform::All));

    app_menu = app_menu
        .platform(MenuPlatform::MacOS)
        .add_item(menu_separator(MenuPlatform::MacOS))
        .add_item(Some(MenuItem::os_submenu(
            "Services",
            gpui::SystemMenuType::Services,
        )))
        .add_item(menu_separator(MenuPlatform::MacOS))
        .add_item(menu_item(
            tr!("HIDE", "Hide Meliora"),
            HideSelf,
            MenuPlatform::MacOS,
        ))
        .add_item(menu_item(
            tr!("HIDE_OTHERS", "Hide Others"),
            HideOthers,
            MenuPlatform::MacOS,
        ))
        .add_item(menu_item(
            tr!("SHOW_ALL", "Show All"),
            ShowAll,
            MenuPlatform::MacOS,
        ))
        .add_item(menu_separator(MenuPlatform::All))
        .add_item(menu_item(
            tr!("QUIT", "Quit Meliora"),
            Quit,
            MenuPlatform::All,
        ));

    let window_menu = MenuBuilder::new(tr!(
        "WINDOW",
        "Window",
        #description = "The Window menu. Must *exactly* match the text required by macOS."
    ))
    .platform(MenuPlatform::MacOS)
    .build();

    let menus = [app_menu.build(), window_menu]
        .into_iter()
        .flatten()
        .collect::<Vec<Menu>>();
    cx.set_menus(menus);
}

fn quit(_: &Quit, cx: &mut App) {
    info!("Quitting...");
    cx.quit();
}

fn close_window(_: &CloseWindow, cx: &mut App) {
    cx.defer(|cx| {
        let Some(window_id) = cx.active_window() else {
            warn!("No active window to close");
            return;
        };
        _ = cx.update_window(window_id, |_, window, _| {
            window.remove_window();
        })
    });
}

fn play_pause(_: &PlayPause, cx: &mut App) {
    let state = cx.global::<PlaybackInfo>().playback_state.read(cx);
    let interface = cx.global::<PlaybackInterface>();
    match state {
        PlaybackState::Stopped => {
            interface.play();
        }
        PlaybackState::Playing => {
            interface.pause();
        }
        PlaybackState::Paused => {
            interface.play();
        }
    }
}

fn next(_: &Next, cx: &mut App) {
    let interface = cx.global::<PlaybackInterface>();
    interface.next();
}

fn previous(_: &Previous, cx: &mut App) {
    let interface = cx.global::<PlaybackInterface>();
    interface.previous();
}

fn hide_self(_: &HideSelf, cx: &mut App) {
    cx.hide();
}

fn hide_others(_: &HideOthers, cx: &mut App) {
    cx.hide_other_apps();
}

fn show_all(_: &ShowAll, cx: &mut App) {
    cx.unhide_other_apps();
}

fn about(_: &About, cx: &mut App) {
    let show_about = cx.global::<Models>().show_about.clone();
    show_about.write(cx, true);
}

fn force_scan(_: &ForceScan, cx: &mut App) {
    let scanner = cx.global::<ScanInterface>();
    scanner.force_scan();
}

fn scan(_: &Scan, cx: &mut App) {
    let scanner = cx.global::<ScanInterface>();
    scanner.scan();
}

fn test_toast(_: &TestToast, _cx: &mut App) {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    match n % 5 {
        0 => emit_toast(Toast::info(tr!(
            "TEST_TOAST_INFO",
            "Info toast #{{n}}",
            n = n
        ))),
        1 => emit_toast(Toast::success(tr!(
            "TEST_TOAST_SUCCESS",
            "Success toast #{{n}}",
            n = n
        ))),
        2 => emit_toast(Toast::warning(tr!(
            "TEST_TOAST_WARNING",
            "Warning toast #{{n}}",
            n = n
        ))),
        3 => emit_toast(
            Toast::error(tr!(
                "TEST_TOAST_ERROR",
                "Error toast #{{n}} (with action)",
                n = n
            ))
            .with_action(tr!("TEST_TOAST_RETRY", "Retry"), |_cx| {
                info!("test toast action clicked");
            }),
        ),
        // was: define_error!(TestError{..}) + emit_error
        _ => {
            error!("test emit_error, n={n}");
            emit_toast(Toast::error(tr!(
                "TEST_TOAST_EMIT_ERROR",
                "Error via emit_error (n={{n}})",
                n = n
            )));
        }
    }
}

fn open_settings(_: &Settings, cx: &mut App) {
    open_settings_window(cx);
}

fn open_equalizer(_: &OpenEqualizer, cx: &mut App) {
    open_settings_window_with_section(cx, SettingsSectionKind::Equalizer);
}

fn issues(_: &Issues, cx: &mut App) {
    cx.open_url("https://github.com/li-ming1/meliora/issues");
}

fn shuffle_all(_: &ShuffleAll, cx: &mut App) {
    // the whole-library query and the per-track item construction used to run
    // on the UI thread; on a 100k library that froze the main thread for
    // hundreds of ms. Items are built with no metadata entity — `get_data`
    // creates one lazily on first use — so nothing GPUI-owned is constructed
    // off the main thread.
    let pool = cx.global::<Pool>().0.clone();
    cx.spawn(async move |cx| {
        let tracks = crate::RUNTIME
            .spawn(async move { db::get_all_tracks(&pool).await })
            .await
            .map(|result| result.unwrap_or_default())
            .unwrap_or_default();

        let items: Vec<QueueItemData> = tracks
            .into_iter()
            .map(|(path, id, album_id)| QueueItemData::lazy(path.into(), Some(id), Some(album_id)))
            .collect();

        cx.update(|cx| {
            let interface = cx.global::<PlaybackInterface>();

            if !(*cx.global::<PlaybackInfo>().shuffling.read(cx)) {
                interface.toggle_shuffle();
            }
            interface.replace_queue(items);
        });
    })
    .detach();
}

fn undo(_: &Undo, cx: &mut App) {
    let interface = cx.global::<PlaybackInterface>();
    interface.undo();
}

fn stop_after_current(_: &StopAfterCurrent, cx: &mut App) {
    let interface = cx.global::<PlaybackInterface>();
    interface.toggle_stop_after_current();
}

fn open_theme_folder(_: &OpenThemeFolder, cx: &mut App) {
    let themes_dir = crate::paths::data_dir().join(crate::ui::theme::THEMES_DIR_NAME);
    let _ = std::fs::create_dir_all(&themes_dir);
    cx.open_with_system(&themes_dir);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuPlatform {
    MacOS,
    All,
}

impl MenuPlatform {
    fn is_active(self) -> bool {
        match self {
            Self::MacOS => cfg!(target_os = "macos"),
            Self::All => true,
        }
    }
}

/// Builds an individual GPUI `Menu` for the application menu bar.
pub struct MenuBuilder {
    name: SharedString,
    items: Vec<MenuItem>,
    platform: MenuPlatform,
    disabled: bool,
}

impl MenuBuilder {
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self {
            name: name.into(),
            items: Vec::new(),
            platform: MenuPlatform::All,
            disabled: false,
        }
    }

    pub fn platform(mut self, platform: MenuPlatform) -> Self {
        self.platform = platform;
        self
    }

    pub fn add_item(mut self, item: impl Into<Option<MenuItem>>) -> Self {
        if let Some(item) = item.into() {
            self.items.push(item);
        }
        self
    }

    pub fn build(self) -> Option<Menu> {
        if !self.platform.is_active() {
            return None;
        }

        Some(Menu {
            name: self.name,
            items: self.items,
            disabled: self.disabled,
        })
    }
}

/// Creates a single GPUI `MenuItem`, unless the platform check fails.
pub fn menu_item<A: Action>(
    name: impl Into<SharedString>,
    action: A,
    platform: MenuPlatform,
) -> Option<MenuItem> {
    if !platform.is_active() {
        return None;
    }

    Some(MenuItem::action(name, action))
}

/// Creates a single GPUI `MenuItem` separator, unless the platform check fails.
pub fn menu_separator(platform: MenuPlatform) -> Option<MenuItem> {
    if !platform.is_active() {
        return None;
    }

    Some(MenuItem::separator())
}
