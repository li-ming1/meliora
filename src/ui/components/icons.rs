// Paths to icons from Tabler Icons, for use with icon()
// See assets/icons/LICENSE
// The filter-* icons are original works, see assets/icons/LICENSE-meliora-icons

use gpui::{IntoElement, RenderOnce, SharedString, StyleRefinement, Styled, Svg, svg};
use palette::IntoColor;

use crate::ui::theme::Theme;

#[derive(IntoElement)]
pub struct Icon {
    svg: Svg,
    icon: SharedString,
}

impl Styled for Icon {
    fn style(&mut self) -> &mut StyleRefinement {
        self.svg.style()
    }
}

impl RenderOnce for Icon {
    fn render(mut self, _: &mut gpui::Window, cx: &mut gpui::App) -> impl gpui::IntoElement {
        let theme = cx.global::<Theme>();

        let color_ref = *self
            .svg
            .style()
            .text
            .color
            .get_or_insert(theme.text.into_color());

        self.svg.path(self.icon).text_color(color_ref)
    }
}

pub fn icon(icon: impl Into<SharedString>) -> Icon {
    Icon {
        svg: svg(),
        icon: icon.into(),
    }
}

pub const ADJUSTMENTS: &str = "!bundled:icons/adjustments.svg";
pub const ARROW_LEFT: &str = "!bundled:icons/arrow-left.svg";
pub const ARROW_RIGHT: &str = "!bundled:icons/arrow-right.svg";
pub const SHUFFLE: &str = "!bundled:icons/arrows-shuffle.svg";
pub const CIRCLE_PLUS: &str = "!bundled:icons/circle-plus.svg";
pub const FOLDER_CHECK: &str = "!bundled:icons/folder-check.svg";
pub const FOLDER_SEARCH: &str = "!bundled:icons/folder-search.svg";
pub const FOLDER_BOLT: &str = "!bundled:icons/folder-bolt.svg";
pub const MAXIMIZE: &str = "!bundled:icons/maximize.svg";
pub const MINIMIZE: &str = "!bundled:icons/minimize.svg";
pub const MINUS: &str = "!bundled:icons/minus.svg";
pub const PAUSE: &str = "!bundled:icons/player-pause.svg";
pub const PLAY: &str = "!bundled:icons/player-play.svg";
pub const NEXT_TRACK: &str = "!bundled:icons/player-track-next.svg";
pub const PREV_TRACK: &str = "!bundled:icons/player-track-prev.svg";
pub const PLUS: &str = "!bundled:icons/plus.svg";
pub const REPEAT: &str = "!bundled:icons/repeat.svg";
pub const REPEAT_ONCE: &str = "!bundled:icons/repeat-once.svg";
pub const REPEAT_OFF: &str = "!bundled:icons/repeat-off.svg";
pub const TRASH: &str = "!bundled:icons/trash.svg";
pub const CROSS: &str = "!bundled:icons/x.svg";
pub const VOLUME: &str = "!bundled:icons/volume.svg";
pub const VOLUME_OFF: &str = "!bundled:icons/volume-off.svg";
pub const MENU: &str = "!bundled:icons/menu-2.svg";
pub const DOTS_VERTICAL: &str = "!bundled:icons/dots-vertical.svg";
pub const CHEVRON_UP: &str = "!bundled:icons/chevron-up.svg";
pub const CHEVRON_DOWN: &str = "!bundled:icons/chevron-down.svg";
pub const DISC: &str = "!bundled:icons/disc.svg";
pub const PLAYLIST: &str = "!bundled:icons/playlist.svg";
pub const PLAYLIST_ADD: &str = "!bundled:icons/playlist-add.svg";
pub const PLAYLIST_REMOVE: &str = "!bundled:icons/playlist-x.svg";
pub const STAR: &str = "!bundled:icons/star.svg";
pub const STAR_FILLED: &str = "!bundled:icons/star-filled.svg";
pub const SIDEBAR: &str = "!bundled:icons/layout-sidebar.svg";
pub const SIDEBAR_INACTIVE: &str = "!bundled:icons/layout-sidebar-inactive.svg";
pub const SEARCH: &str = "!bundled:icons/search.svg";
pub const CHECK: &str = "!bundled:icons/check.svg";
pub const LOCK: &str = "!bundled:icons/lock.svg";
pub const BOOKS: &str = "!bundled:icons/books.svg";
pub const ALERT_CIRCLE: &str = "!bundled:icons/alert-circle.svg";
pub const WORLD: &str = "!bundled:icons/world.svg";
#[cfg(feature = "kugou")]
pub const KUGOU: &str = "!bundled:icons/kugou.svg";
#[cfg(feature = "netease")]
pub const NETEASE: &str = "!bundled:icons/netease.svg";
pub const GRID: &str = "!bundled:icons/layout-grid.svg";
pub const GRID_INACTIVE: &str = "!bundled:icons/layout-grid-inactive.svg";
pub const LIST: &str = "!bundled:icons/layout-list.svg";
pub const LIST_INACTIVE: &str = "!bundled:icons/layout-list-inactive.svg";
pub const USERS: &str = "!bundled:icons/users.svg";
pub const RANKING: &str = "!bundled:icons/ranking.svg";
pub const SORT_DESCENDING: &str = "!bundled:icons/sort-descending.svg";
pub const SORT_ASCENDING: &str = "!bundled:icons/sort-ascending.svg";
pub const FOLDER_X: &str = "!bundled:icons/folder-x.svg";
pub const MICROPHONE: &str = "!bundled:icons/microphone-2.svg";
pub const PENCIL: &str = "!bundled:icons/pencil.svg";
pub const FILE_EXPORT: &str = "!bundled:icons/file-export.svg";
pub const DOWNLOAD: &str = "!bundled:icons/download.svg";
pub const MUSIC: &str = "!bundled:icons/music.svg";
pub const POWER: &str = "!bundled:icons/power.svg";
pub const FOLDER: &str = "!bundled:icons/folder.svg";
pub const FOLDER_OPEN: &str = "!bundled:icons/folder-open.svg";
pub const FILE: &str = "!bundled:icons/file.svg";
pub const CHEVRON_RIGHT: &str = "!bundled:icons/chevron-right.svg";
pub const REFRESH: &str = "!bundled:icons/refresh.svg";
pub const FILTER_BELL: &str = "!bundled:icons/filter-bell.svg";
pub const FILTER_LOW_PASS: &str = "!bundled:icons/filter-low-pass.svg";
pub const FILTER_HIGH_PASS: &str = "!bundled:icons/filter-high-pass.svg";
pub const FILTER_BAND_PASS: &str = "!bundled:icons/filter-band-pass.svg";
pub const FILTER_NOTCH: &str = "!bundled:icons/filter-notch.svg";
pub const SETTINGS: &str = "!bundled:icons/settings.svg";
pub const SUN: &str = "!bundled:icons/sun.svg";
pub const MOON: &str = "!bundled:icons/moon.svg";
