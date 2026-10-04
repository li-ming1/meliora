use std::{path::PathBuf, rc::Rc};

use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{Entity, IntoElement, RenderOnce, SharedString, Window};

use crate::{
    library::types::Track,
    ui::{
        availability::{is_online_path, is_track_path_available},
        components::{
            icons::{DISC, DOWNLOAD, FOLDER_SEARCH, PLAYLIST_ADD, USERS},
            menu::{menu, menu_item, menu_separator},
        },
        models::{is_track_available_snapshot, toggle_like_by_id},
        util::reveal_path_for_file_manager,
    },
};

use super::{
    like_toggle_icon_and_label, navigate_to_track_album, navigate_to_track_artist,
    track_show_in_file_manager_label,
};

#[derive(IntoElement)]
pub struct InfoSectionContextMenu {
    current_path: Option<PathBuf>,
    track: Option<Rc<Track>>,
    /// Artist navigability of `track`, resolved by the parent's background
    /// job and handed over at construction: menu construction must not run
    /// the DB's `block_on` query per frame the menu stays open.
    can_go_to_artist: bool,
    is_liked: Option<i64>,
    show_add_to: Option<Entity<bool>>,
    /// Present only for online (KuGou) tracks: label plus a fire-and-forget
    /// download action. Takes the place of "show in file manager", which is
    /// meaningless for a stream URL.
    online_download: Option<OnlineDownloadAction>,
}

/// Online download menu entry: `(localized label, fire-and-forget action)`.
type OnlineDownloadAction = (SharedString, Rc<dyn Fn(&mut gpui::App)>);

impl InfoSectionContextMenu {
    pub fn new(
        current_path: Option<PathBuf>,
        track: Option<Rc<Track>>,
        can_go_to_artist: bool,
        is_liked: Option<i64>,
        show_add_to: Option<Entity<bool>>,
        online_download: Option<OnlineDownloadAction>,
    ) -> Self {
        Self {
            current_path,
            track,
            can_go_to_artist,
            is_liked,
            show_add_to,
            online_download,
        }
    }
}

impl RenderOnce for InfoSectionContextMenu {
    fn render(self, _window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let reveal_path = self.current_path;
        let is_online = reveal_path.as_deref().is_some_and(is_online_path);
        let track = self.track;
        // availability snapshot read: a per-render `path.exists()` syscall here
        // would block the UI thread every frame the menu stays open. Without a
        // library row yet (resolve window, non-library file) the fresh stat
        // stays, so a path that exists is not read as missing until the next
        // snapshot reload lands.
        let can_reveal_track = match track.as_deref() {
            Some(t) => is_track_available_snapshot(cx, t),
            None => !is_online && reveal_path.as_deref().is_some_and(is_track_path_available),
        };

        menu()
            .when_some(track.clone(), |menu, track_for_artist| {
                let can_go_to_artist = self.can_go_to_artist;
                menu.item(
                    menu_item(
                        "info_section_go_to_artist",
                        Some(USERS),
                        tr!("GO_TO_ARTIST"),
                        move |ev, _, cx| {
                            navigate_to_track_artist(cx, &track_for_artist, ev.position());
                        },
                    )
                    .disabled(!can_go_to_artist),
                )
            })
            .when_some(track.clone(), |menu, track_for_album| {
                let can_go_to_album = track_for_album.album_id.is_some();
                menu.item(
                    menu_item(
                        "info_section_go_to_album",
                        Some(DISC),
                        tr!("GO_TO_ALBUM"),
                        move |_, _, cx| {
                            navigate_to_track_album(cx, &track_for_album);
                        },
                    )
                    .disabled(!can_go_to_album),
                )
            })
            .when(can_reveal_track, |menu| {
                menu.item(menu_item(
                    "info_section_show_in_file_manager",
                    Some(FOLDER_SEARCH),
                    track_show_in_file_manager_label(),
                    move |_, _, cx| {
                        if let Some(path) = reveal_path.as_ref() {
                            reveal_path_for_file_manager(path, cx);
                        }
                    },
                ))
            })
            .when_some(self.online_download, |menu, online_download| {
                let (label, download) = online_download;
                menu.item(menu_item(
                    "info_section_download",
                    Some(DOWNLOAD),
                    label,
                    move |_, _, cx| download(cx),
                ))
            })
            .when_some(track.clone(), |menu, track_for_like| {
                let is_liked = self.is_liked;
                let track_id = track_for_like.id;
                let (like_icon, like_label) = like_toggle_icon_and_label(is_liked);
                menu.item(menu_separator()).item(menu_item(
                    "info_section_toggle_like",
                    Some(like_icon),
                    like_label,
                    move |_, _, cx| {
                        toggle_like_by_id(track_id, is_liked, cx);
                    },
                ))
            })
            .when_some(self.show_add_to, |menu, show_add_to| {
                menu.item(menu_separator()).item(menu_item(
                    "info_section_add_to_playlist",
                    Some(PLAYLIST_ADD),
                    tr!("ADD_TO_PLAYLIST"),
                    move |_, _, cx| {
                        show_add_to.write(cx, true);
                    },
                ))
            })
    }
}
