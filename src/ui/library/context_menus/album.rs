use std::rc::Rc;

use cntp_i18n::tr;
use gpui::{Entity, IntoElement, RenderOnce, Window};

use crate::{
    library::types::Album,
    ui::{
        components::{
            icons::{FOLDER_SEARCH, PLAY, PLAYLIST_ADD, PLUS, SHUFFLE, USERS},
            menu::{menu, menu_item, menu_separator},
        },
        library_access::LibraryAccess,
        models::Models,
    },
};

use super::{
    AlbumContextMenuContext, navigate_to_album_artists, play_album_next, play_album_now,
    queue_album, rescan_album, shuffle_album,
};

#[derive(IntoElement)]
pub struct AlbumContextMenu {
    album: Rc<Album>,
    context: AlbumContextMenuContext,
    show_add_to: Entity<bool>,
}

impl AlbumContextMenu {
    pub fn new(
        album: Rc<Album>,
        show_add_to: Entity<bool>,
        context: AlbumContextMenuContext,
    ) -> Self {
        Self {
            album,
            show_add_to,
            context,
        }
    }
}

impl RenderOnce for AlbumContextMenu {
    fn render(self, _: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let album = self.album.clone();
        let album_for_next = self.album.clone();
        let album_for_shuffle = self.album.clone();
        let album_for_queue = self.album.clone();
        let album_for_artist = self.album.clone();
        let album_for_rescan = self.album.clone();
        let show_add_to = self.show_add_to;
        let show_go_to_artist = self.context.show_go_to_artist;
        // 用缓存的可用性快照：这次 render 在菜单打开时运行，而旧的检查是每首曲目
        // 一次 block_on 查询加一次 stat
        let is_available = cx
            .global::<Models>()
            .available_albums
            .read(cx)
            .as_ref()
            .is_some_and(|set| set.contains(&album.id));
        let menu = menu()
            .item(
                menu_item("album_play", Some(PLAY), tr!("PLAY"), move |_, _, cx| {
                    play_album_now(cx, &album);
                })
                .disabled(!is_available),
            )
            .item(
                menu_item(
                    "album_play_next",
                    None::<gpui::SharedString>,
                    tr!("PLAY_NEXT"),
                    move |_, _, cx| {
                        play_album_next(cx, &album_for_next);
                    },
                )
                .disabled(!is_available),
            )
            .item(
                menu_item(
                    "album_shuffle",
                    Some(SHUFFLE),
                    tr!("SHUFFLE"),
                    move |_, _, cx| {
                        shuffle_album(cx, &album_for_shuffle);
                    },
                )
                .disabled(!is_available),
            )
            .item(
                menu_item(
                    "album_add_to_queue",
                    Some(PLUS),
                    tr!("ADD_TO_QUEUE"),
                    move |_, _, cx| {
                        queue_album(cx, &album_for_queue);
                    },
                )
                .disabled(!is_available),
            )
            .item(menu_item(
                "album_add_to_playlist",
                Some(PLAYLIST_ADD),
                tr!("ADD_TO_PLAYLIST"),
                move |_, _, cx| {
                    show_add_to.write(cx, true);
                },
            ))
            .item(menu_separator())
            .item(menu_item(
                "album_rescan",
                None::<gpui::SharedString>,
                tr!("RESCAN_ALBUM", "Rescan album"),
                move |_, _, cx| {
                    rescan_album(cx, &album_for_rescan);
                },
            ))
            .item(menu_separator());
        // 「在文件管理器中显示」（上游 447064d）：取专辑第一首可用曲目的
        // 位置让文件管理器定位。与曲目菜单同款 reveal 助手；查询走
        // LibraryAccess 的点击路径（UI 线程 block_on + 慢查询告警），专辑
        // 行数少、有 Arc 缓存的索引查询。
        let menu = if is_available {
            let album_for_reveal = self.album.clone();
            menu.item(menu_item(
                "album_show_in_file_manager",
                Some(FOLDER_SEARCH),
                crate::ui::library::context_menus::track_show_in_file_manager_label(),
                move |_, _, cx| {
                    let Ok(tracks) = cx.list_tracks_in_album(album_for_reveal.id) else {
                        return;
                    };
                    if let Some(track) = tracks
                        .iter()
                        .find(|t| crate::ui::models::is_track_available_snapshot(cx, t))
                    {
                        crate::ui::util::reveal_path_for_file_manager(track.location.as_path(), cx);
                    }
                },
            ))
        } else {
            menu
        };

        if show_go_to_artist {
            menu.item(menu_separator()).item(menu_item(
                "album_go_to_artist",
                Some(USERS),
                tr!("GO_TO_ARTIST"),
                move |ev, _, cx| {
                    navigate_to_album_artists(cx, album_for_artist.id, ev.position());
                },
            ))
        } else {
            menu
        }
    }
}
