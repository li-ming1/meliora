use std::{rc::Rc, sync::Arc};

use cntp_i18n::{I18nString, tr};
use gpui::{AnyView, App, IntoElement, SharedString, Window, div};

use crate::ui::{
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
    library_access::LibraryAccess,
    models::is_song_liked,
};
// 与 ui.rs 的 `mod online_track_row` 同 gate：trait 仅服务下方 kugou
// 匹配臂的 `detail_label()`（trait 方法，需在作用域内）。use 树嵌套段上不能
// 挂 `#[cfg]`（rustc 判为解析错误），故按全仓惯例拆为独立 use 语句。
#[cfg(feature = "kugou")]
use crate::ui::online_track_row::OnlineTrackDisplay;

#[derive(Debug, Clone, PartialEq)]
pub enum SearchPaletteItem {
    Album {
        id: i64,
        title: String,
        artist: String,
        artists: String,
        available: bool,
        /// 预计算的模糊匹配文本，构造时算一次，好让面板匹配器永不在 UI 线程上
        /// 格式化字符串。
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
    /// 从酷狗串流的在线曲目（仅 kugou feature）。
    #[cfg(feature = "kugou")]
    KugouTrack {
        track: crate::ui::kugou::KugouTrackInfo,
        search_text: Arc<str>,
    },
}

impl SearchPaletteItem {
    /// 某专辑缩略图的图片缓存 URI。
    fn thumbnail_path(album_id: i64) -> String {
        format!("!db://album/{}/thumb", album_id)
    }

    /// 在线曲目的标题在音源没给时退回本地化的 UNKNOWN_TRACK 标签。
    #[cfg(feature = "kugou")]
    fn online_title(title: &str) -> SharedString {
        if title.is_empty() {
            tr!("UNKNOWN_TRACK").into()
        } else {
            title.to_owned().into()
        }
    }

    /// 构建本地（音乐库）条目。每个条目用来被匹配到的模糊文本在这里预计算——
    /// 在 UI 线程之外、每条目一次——于是面板匹配器只需克隆它，而不是每次注入索引时
    /// 逐条目重新格式化。
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
        }
    }

    fn middle_content(&self, _cx: &mut App) -> SharedString {
        match self {
            SearchPaletteItem::Album { title, .. } => title.clone().into(),
            SearchPaletteItem::Artist { name, .. } => name.clone().into(),
            SearchPaletteItem::Track { title, .. } => title.clone().into(),
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { track, .. } => Self::online_title(&track.title),
        }
    }

    fn right_content(&self, _cx: &mut App) -> Option<SharedString> {
        match self {
            SearchPaletteItem::Album { artist, .. } => Some(artist.clone().into()),
            SearchPaletteItem::Track { artists, .. } => Some(artists.clone().into()),
            SearchPaletteItem::Artist { .. } => None,
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { track, .. } => Some(track.detail_label()),
        }
    }

    fn is_enabled(&self, _cx: &App) -> bool {
        match self {
            SearchPaletteItem::Album { available, .. } => *available,
            SearchPaletteItem::Artist { .. } => true,
            SearchPaletteItem::Track { album_id, .. } => album_id.is_some(),
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { .. } => true,
        }
    }

    /// 在线结果在每次查询变化时整体替换；把它们挡在 finder 的 nucleo 索引之外
    /// （见 `PaletteItem::is_volatile`），意味着刷新它们永远不会重建全库索引。
    fn is_volatile(&self) -> bool {
        match self {
            #[cfg(feature = "kugou")]
            SearchPaletteItem::KugouTrack { .. } => true,
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
                        // 已缓存的喜欢集合：在这里查 DB 会阻塞 UI 线程（见 is_song_liked 的文档）
                        let is_liked = is_song_liked(cx, track.id);
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

    /// 只在上下文菜单打开时构建（见
    /// `PaletteItem::context_menu_overlay`）；该实体是按 key 存放的窗口状态，
    /// 所以反复打开会复用它。
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

/// 搜索索引条目构建的耗时探针——面板打开重建中，DB 侧 `scale_probe`（它的
/// "3) 搜索索引四查询"一节）没有覆盖的那一半：把三个查询结果变成带预计算匹配文本的
/// `SearchPaletteItem`。它在打开时跑在 `spawn_blocking` 上，所以这里的退化就是打开延迟。
///
/// ```text
/// cargo test --release search_item::bench -- --ignored --nocapture
/// ```
#[cfg(test)]
mod bench {
    use super::SearchPaletteItem;
    use crate::test_support::bench_summarize_n;

    const ALBUMS: usize = 2_000;
    const ARTISTS: usize = 2_000;
    const TRACKS: usize = 10_000;

    fn synthetic_results() -> SyntheticResults {
        let albums = (0..ALBUMS as i64)
            .map(|id| {
                (
                    id,
                    format!("Album {id:05}"),
                    Some(format!("Artist {:05}", id % ARTISTS as i64)),
                    format!("Artist {:05} · Album {id:05}", id % ARTISTS as i64),
                    id % 7 != 0,
                )
            })
            .collect();
        let artists = (0..ARTISTS as i64)
            .map(|id| (id, format!("Artist {id:05}")))
            .collect();
        let tracks = (0..TRACKS as i64)
            .map(|id| {
                (
                    id,
                    format!("Track {id:05}"),
                    format!("Artist {:05} · Track {id:05}", id % ARTISTS as i64),
                    Some(id % ALBUMS as i64),
                )
            })
            .collect();
        SyntheticResults {
            albums,
            artists,
            tracks,
        }
    }

    struct SyntheticResults {
        albums: Vec<(i64, String, Option<String>, String, bool)>,
        artists: Vec<(i64, String)>,
        tracks: Vec<(i64, String, String, Option<i64>)>,
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn report_search_item_build_costs() {
        let data = synthetic_results();
        println!("search index: {ALBUMS} albums, {ARTISTS} artists, {TRACKS} tracks");
        // 每次运行都会克隆查询结果（from_search_results 按值消费它们）；14k 条目时
        // 这个克隆是每次运行的固定开销，读构建本身的数字时要把它扣掉。
        bench_summarize_n("from_search_results (14k items)", 2, 50, || {
            let albums = data.albums.clone();
            let artists = data.artists.clone();
            let tracks = data.tracks.clone();
            let items = SearchPaletteItem::from_search_results(albums, artists, tracks);
            assert_eq!(items.len(), ALBUMS + ARTISTS + TRACKS);
        });
    }
}
