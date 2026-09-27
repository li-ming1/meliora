use std::{rc::Rc, sync::Arc};

use cntp_i18n::{I18nString, tr};
use gpui::{AnyView, App, IntoElement, SharedString, Window, div};

use crate::{
    library::db::LibraryAccess,
    ui::{
        components::{
            context::ContextMenuBuilder,
            icons::{DISC, USERS},
            palette::{FinderItemLeft, PaletteItem},
        },
        library::context_menus::{
            AlbumContextMenuContext, TrackContextMenuContext, add_album_to_playlist_state,
            add_to_playlist_state, album::AlbumContextMenu, play_album_next, play_track_next,
            track::TrackContextMenu,
        },
        models::LIKED_SONGS_PLAYLIST_ID,
    },
};

#[derive(Debug, Clone, PartialEq)]
pub enum SearchPaletteItem {
    Album {
        id: i64,
        title: String,
        artist: String,
        artists: String,
        available: bool,
        /// Precomputed fuzzy-match text, built once at construction so the
        /// palette matcher never formats strings on the UI thread.
        search_text: Arc<str>,
    },
    Artist {
        id: i64,
        name: String,
        search_text: Arc<str>,
    },
    Track {
        id: i64,
        title: String,
        artists: String,
        album_id: Option<i64>,
        search_text: Arc<str>,
    },
    /// Online track streamed from KuGou (kugou feature only).
    #[cfg(feature = "kugou")]
    KugouTrack {
        track: crate::ui::kugou::KugouTrackInfo,
        search_text: Arc<str>,
    },
    /// Online track streamed from NetEase (netease feature only).
    #[cfg(feature = "netease")]
    NeteaseTrack {
        track: crate::ui::netease::NeteaseTrackInfo,
        search_text: Arc<str>,
    },
}

impl SearchPaletteItem {
    /// Image-cache URI for an album's thumbnail.
    fn thumbnail_path(album_id: i64) -> String {
        format!("!db://album/{}/thumb", album_id)
    }

    /// Online track titles fall back to the localized UNKNOWN_TRACK label
    /// when the source provides none.
    #[cfg(any(feature = "kugou", feature = "netease"))]
    fn online_title(title: &str) -> SharedString {
        if title.is_empty() {
            tr!("UNKNOWN_TRACK").into()
        } else {
            title.to_owned().into()
        }
    }

    /// Builds the local (library) items. The fuzzy-match text each item is
    /// found by is precomputed here — off the UI thread, once per item — so
    /// the palette matcher only clones it instead of re-formatting per item
    /// on every index injection.
    pub fn from_search_results(
        albums: Vec<(i64, String, Option<String>, String, bool)>,
        artists: Vec<(i64, String)>,
        tracks: Vec<(i64, String, String, Option<i64>)>,
    ) -> Vec<Arc<SearchPaletteItem>> {
        let mut items: Vec<Arc<SearchPaletteItem>> = Vec::new();

        for (id, name) in artists {
            let search_text: Arc<str> = name.clone().into();
            items.push(Arc::new(SearchPaletteItem::Artist {
                id,
                name,
                search_text,
            }));
        }

        for (id, title, artist_override, artists, available) in albums {
            let artist = artist_override.unwrap_or_else(|| artists.clone());
            let search_text: Arc<str> = format!("{} {} {}", title, artist, artists).into();
            items.push(Arc::new(SearchPaletteItem::Album {
                id,
                title,
                artist,
                artists,
                available,
                search_text,
            }));
        }

        for (id, title, artists, album_id) in tracks {
            let search_text: Arc<str> = format!("{} {}", title, artists).into();
            items.push(Arc::new(SearchPaletteItem::Track {
                id,
                title,
                artists,
                album_id,
                search_text,
            }));
        }

        items
    }
}

impl PaletteItem for SearchPaletteItem {
    fn left_content(&self, _cx: &mut App) -> Option<FinderItemLeft> {
        match self {
            SearchPaletteItem::Album { id, .. } => {
                Some(FinderItemLeft::Image(Self::thumbnail_path(*id).into()))
            }
            SearchPaletteItem::Artist { .. } => Some(FinderItemLeft::Icon(USERS.into())),
            SearchPaletteItem::Track { album_id, .. } => {
                if let Some(album_id) = album_id {
                    Some(FinderItemLeft::Image(
                        Self::thumbnail_path(*album_id).into(),
                    ))
                } else {
                    Some(FinderItemLeft::Icon(DISC.into()))
                }
            }
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { .. } => Some(FinderItemLeft::Icon(DISC.into())),
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { .. } => Some(FinderItemLeft::Icon(DISC.into())),
        }
    }

    fn middle_content(&self, _cx: &mut App) -> SharedString {
        match self {
            SearchPaletteItem::Album { title, .. } => title.clone().into(),
            SearchPaletteItem::Artist { name, .. } => name.clone().into(),
            SearchPaletteItem::Track { title, .. } => title.clone().into(),
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { track, .. } => Self::online_title(&track.title),
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { track, .. } => Self::online_title(&track.title),
        }
    }

    fn right_content(&self, _cx: &mut App) -> Option<SharedString> {
        match self {
            SearchPaletteItem::Album { artist, .. } => Some(artist.clone().into()),
            SearchPaletteItem::Track { artists, .. } => Some(artists.clone().into()),
            SearchPaletteItem::Artist { .. } => None,
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { track, .. } => Some(track.detail_label()),
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { track, .. } => Some(track.detail_label()),
        }
    }

    fn is_enabled(&self, _cx: &App) -> bool {
        match self {
            SearchPaletteItem::Album { available, .. } => *available,
            SearchPaletteItem::Artist { .. } => true,
            SearchPaletteItem::Track { album_id, .. } => album_id.is_some(),
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { .. } => true,
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { .. } => true,
        }
    }

    /// Online results are replaced wholesale on every query change; keeping
    /// them out of the finder's nucleo index (see `PaletteItem::is_volatile`)
    /// means refreshing them never rebuilds the whole-library index.
    fn is_volatile(&self) -> bool {
        match self {
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { .. } => true,
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { .. } => true,
            _ => false,
        }
    }

    fn category(&self) -> Option<I18nString> {
        Some(match self {
            SearchPaletteItem::Artist { .. } => tr!("ARTISTS"),
            SearchPaletteItem::Album { .. } => tr!("ALBUMS"),
            SearchPaletteItem::Track { .. } => tr!("TRACKS"),
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { .. } => tr!("KUGOU_ONLINE_RESULTS", "Online · KuGou"),
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { .. } => {
                tr!("NETEASE_ONLINE_RESULTS", "Online · NetEase")
            }
        })
    }

    fn on_middle_click(&self, cx: &mut App) {
        match self {
            SearchPaletteItem::Artist { .. } => {}
            SearchPaletteItem::Album { id, .. } => {
                if let Ok(album) = cx.get_album_by_id(*id) {
                    play_album_next(cx, &album);
                }
            }
            SearchPaletteItem::Track { id, .. } => {
                if let Ok(track) = cx.get_track_by_id(*id) {
                    play_track_next(cx, &track);
                }
            }
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { track, .. } => {
                crate::ui::kugou::queue_track(cx, track);
            }
            #[cfg(feature = "netease")]
            SearchPaletteItem::NeteaseTrack { track, .. } => {
                crate::ui::netease::queue_track(cx, track);
            }
        }
    }

    fn context_menu(&self, _window: &mut Window, _cx: &mut App) -> Option<ContextMenuBuilder> {
        match self {
            SearchPaletteItem::Album { id, .. } => {
                let id = *id;
                Some(Rc::new(move |window, cx| {
                    let (show_add_to, _) =
                        add_album_to_playlist_state("pi_context_album_add_to", id, window, cx);
                    let album =
                        window.use_keyed_state(("pi_context_album", id as usize), cx, |_, cx| {
                            cx.get_album_by_id(id)
                        });

                    if let Ok(album) = album.read(cx) {
                        AlbumContextMenu::new(
                            Rc::new((**album).clone()),
                            show_add_to,
                            AlbumContextMenuContext {
                                show_go_to_artist: true,
                            },
                        )
                        .into_any_element()
                    } else {
                        div().into_any_element()
                    }
                }))
            }
            SearchPaletteItem::Track { id, .. } => {
                let id = *id;
                Some(Rc::new(move |window, cx| {
                    let (show_add_to, _) =
                        add_to_playlist_state("pi_context_add_to", id, window, cx);
                    let track =
                        window.use_keyed_state(("pi_context_track", id as usize), cx, |_, cx| {
                            cx.get_track_by_id(id)
                        });

                    if let Ok(track) = track.read(cx) {
                        let is_liked = cx
                            .playlist_has_track(LIKED_SONGS_PLAYLIST_ID, track.id)
                            .unwrap_or_default();
                        TrackContextMenu::new(
                            Rc::new((**track).clone()),
                            true,
                            is_liked,
                            TrackContextMenuContext {
                                show_go_to_album: true,
                                show_go_to_artist: true,
                                play_from_here: None,
                            },
                            None,
                            show_add_to,
                        )
                        .into_any_element()
                    } else {
                        div().into_any_element()
                    }
                }))
            }
            _ => None,
        }
    }

    /// Built only when the context menu opens (see `PaletteItem::context_menu_overlay`);
    /// the entity is keyed window state, so repeated opens reuse it.
    fn context_menu_overlay(&self, window: &mut Window, cx: &mut App) -> Option<AnyView> {
        match self {
            SearchPaletteItem::Track { id, .. } => {
                let (_, add_to) = add_to_playlist_state("pi_context_add_to", *id, window, cx);
                Some(add_to.into())
            }
            SearchPaletteItem::Album { id, .. } => {
                let (_, add_to) =
                    add_album_to_playlist_state("pi_context_album_add_to", *id, window, cx);
                Some(add_to.into())
            }
            _ => None,
        }
    }
}
