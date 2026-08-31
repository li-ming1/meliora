use std::sync::Arc;
#[cfg(feature = "online_sources")]
use std::time::Duration;

use gpui::{App, AppContext, Context, Entity, EventEmitter, IntoElement, Render, Window};
use nucleo::Utf32String;
use tracing::debug;

use crate::{
    library::{db::LibraryAccess, scan::ScanEvent},
    ui::{
        availability::album_has_available_tracks,
        components::palette::Palette,
        library::ViewSwitchMessage,
        models::Models,
    },
};

use super::search_item::SearchPaletteItem;

type MatcherFunc = Box<dyn Fn(&Arc<SearchPaletteItem>, &mut App) -> Utf32String + 'static>;
type OnAccept = Box<dyn Fn(&Arc<SearchPaletteItem>, &mut App) + 'static>;

pub struct SearchModel {
    palette: Entity<Palette<SearchPaletteItem, MatcherFunc, OnAccept>>,
    /// Local (library) items, kept around so online results can be merged in.
    #[cfg(feature = "online_sources")]
    local_items: Vec<Arc<SearchPaletteItem>>,
    #[cfg(feature = "kugou")]
    kugou_items: Vec<Arc<SearchPaletteItem>>,
    /// Bumped on every query change; in-flight online searches compare
    /// against it so stale results are dropped.
    #[cfg(feature = "kugou")]
    kugou_query_generation: u64,
    #[cfg(feature = "netease")]
    netease_items: Vec<Arc<SearchPaletteItem>>,
    #[cfg(feature = "netease")]
    netease_query_generation: u64,
}

fn load_search_items(cx: &mut App) -> Vec<Arc<SearchPaletteItem>> {
    let albums = match cx.list_albums_search() {
        Ok(album_data) => album_data
            .into_iter()
            .map(|(id, title, artist_override, artists)| {
                (
                    id,
                    title,
                    artist_override,
                    artists,
                    album_has_available_tracks(cx, id),
                )
            })
            .collect(),
        Err(e) => {
            debug!("Failed to load albums for search: {:?}", e);
            Vec::new()
        }
    };

    let artists = match cx.list_artists_search() {
        Ok(data) => data,
        Err(e) => {
            debug!("Failed to load artists for search: {:?}", e);
            Vec::new()
        }
    };

    let tracks = match cx.list_tracks_search() {
        Ok(data) => data,
        Err(e) => {
            debug!("Failed to load tracks for search: {:?}", e);
            Vec::new()
        }
    };

    SearchPaletteItem::from_search_results(albums, artists, tracks)
}

impl SearchModel {
    pub fn new(cx: &mut App, show: &Entity<bool>) -> Entity<SearchModel> {
        cx.new(|cx| {
            let items = load_search_items(cx);

            let weak_self = cx.weak_entity();

            let matcher: MatcherFunc = Box::new(|item, _| match item.as_ref() {
                SearchPaletteItem::Album {
                    title,
                    artist,
                    artists,
                    ..
                } => Utf32String::from(format!("{} {} {}", title, artist, artists)),
                SearchPaletteItem::Artist { name, .. } => Utf32String::from(name.as_str()),
                SearchPaletteItem::Track { title, artists, .. } => {
                    Utf32String::from(format!("{} {}", title, artists))
                }
                #[cfg(feature = "kugou")]
                SearchPaletteItem::KugouTrack(track) => {
                    Utf32String::from(format!("{} {}", track.title, track.artist))
                }
                #[cfg(feature = "netease")]
                SearchPaletteItem::NeteaseTrack(track) => {
                    Utf32String::from(format!("{} {}", track.title, track.artist))
                }
            });

            let on_accept: OnAccept = Box::new(move |item, cx| {
                let event = match item.as_ref() {
                    SearchPaletteItem::Album { id, .. } => {
                        Some(ViewSwitchMessage::Release(*id, None))
                    }
                    SearchPaletteItem::Artist { id, .. } => Some(ViewSwitchMessage::Artist(*id)),
                    SearchPaletteItem::Track { id, album_id, .. } => {
                        album_id.as_ref().map(|album_id| ViewSwitchMessage::Release(*album_id, Some(*id)))
                    }
                    #[cfg(feature = "kugou")]
                    SearchPaletteItem::KugouTrack(track) => {
                        crate::ui::kugou::play_track_now(cx, track);
                        None
                    }
                    #[cfg(feature = "netease")]
                    SearchPaletteItem::NeteaseTrack(track) => {
                        crate::ui::netease::play_track_now(cx, track);
                        None
                    }
                };

                let Some(event) = event else {
                    return;
                };

                if let Some(search_model) = weak_self.upgrade() {
                    search_model.update(cx, |_: &mut SearchModel, cx| {
                        cx.emit(event);
                    });
                }
            });

            #[cfg(feature = "online_sources")]
            let local_items = items.clone();

            let palette = Palette::new(cx, items, matcher, on_accept, show);

            let search_model = SearchModel {
                palette,
                #[cfg(feature = "online_sources")]
                local_items,
                #[cfg(feature = "kugou")]
                kugou_items: Vec::new(),
                #[cfg(feature = "kugou")]
                kugou_query_generation: 0,
                #[cfg(feature = "netease")]
                netease_items: Vec::new(),
                #[cfg(feature = "netease")]
                netease_query_generation: 0,
            };

            #[cfg(feature = "online_sources")]
            {
                // the palette emits the raw query string; use it to fetch
                // online results and merge them behind the local ones
                let palette = search_model.palette.clone();
                cx.subscribe(
                    &palette,
                    |this: &mut SearchModel, _, query: &String, cx| {
                        #[cfg(feature = "kugou")]
                        this.on_kugou_query(query, cx);
                        #[cfg(feature = "netease")]
                        this.on_netease_query(query, cx);
                    },
                )
                .detach();
            }

            // 首次打开搜索面板时加载本地索引并推给调色板（按需构建，避免启动即常驻）
            let palette_for_load = search_model.palette.clone();
            let show_for_load = show.clone();
            cx.observe(&show_for_load, move |_this, show, cx| {
                // online 分支需要读写本地索引，无在线源时该参数保持未用
                #[cfg(feature = "online_sources")]
                let this = _this;
                if *show.read(cx) {
                    #[cfg(feature = "online_sources")]
                    if !this.local_items.is_empty() {
                        return;
                    }

                    let new_items = load_search_items(cx);

                    #[cfg(feature = "online_sources")]
                    {
                        this.local_items = new_items.clone();
                    }

                    palette_for_load.update(cx, |_, cx| {
                        cx.emit(new_items);
                    });
                }
            })
            .detach();

            let scan_status = cx.global::<Models>().scan_state.clone();
            let palette_weak = search_model.palette.downgrade();

            cx.observe(&scan_status, move |_this, scan_event, cx| {
                let state = scan_event.read(cx);

                #[cfg(feature = "online_sources")]
                let this = _this;

                if *state == ScanEvent::ScanCompleteIdle
                    || *state == ScanEvent::ScanCompleteWatching
                    || *state == ScanEvent::TargetedRescanComplete
                {
                    debug!("Scan complete, refreshing search items");

                    let new_items = load_search_items(cx);

                    if let Some(palette) = palette_weak.upgrade() {
                        #[cfg(feature = "online_sources")]
                        let emitted = {
                            this.local_items = new_items.clone();
                            this.merged_items()
                        };

                        palette.update(cx, |_, cx| {
                            #[cfg(feature = "online_sources")]
                            cx.emit(emitted);
                            #[cfg(not(feature = "online_sources"))]
                            cx.emit(new_items);
                        });
                    }
                }
            })
            .detach();

            search_model
        })
    }

    /// Local items followed by online (KuGou, then NetEase) items.
    #[cfg(feature = "online_sources")]
    fn merged_items(&self) -> Vec<Arc<SearchPaletteItem>> {
        self.local_items
            .iter()
            .chain(self.kugou_iter())
            .chain(self.netease_iter())
            .cloned()
            .collect()
    }

    #[cfg(feature = "kugou")]
    fn kugou_iter(&self) -> impl Iterator<Item = &Arc<SearchPaletteItem>> {
        self.kugou_items.iter()
    }

    #[cfg(not(feature = "kugou"))]
    #[allow(dead_code)]
    fn kugou_iter(&self) -> std::iter::Empty<&Arc<SearchPaletteItem>> {
        std::iter::empty()
    }

    #[cfg(feature = "netease")]
    fn netease_iter(&self) -> impl Iterator<Item = &Arc<SearchPaletteItem>> {
        self.netease_items.iter()
    }

    #[cfg(not(feature = "netease"))]
    #[allow(dead_code)]
    fn netease_iter(&self) -> std::iter::Empty<&Arc<SearchPaletteItem>> {
        std::iter::empty()
    }

    #[cfg(feature = "online_sources")]
    fn emit_merged_items(&self, cx: &mut Context<Self>) {
        let merged = self.merged_items();
        self.palette.update(cx, |_, cx| {
            cx.emit(merged);
        });
    }

    /// Debounced online search: 350ms after the last keystroke, query KuGou
    /// and merge the results behind the local ones.
    #[cfg(feature = "kugou")]
    fn on_kugou_query(&mut self, query: &str, cx: &mut Context<Self>) {
        use tracing::warn;

        self.kugou_query_generation += 1;
        let generation = self.kugou_query_generation;

        let query = query.trim().to_string();
        if query.chars().count() < 2 {
            self.kugou_items.clear();
            self.emit_merged_items(cx);
            return;
        }

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;

            let client = crate::kugou::shared_client();
            let request =
                crate::RUNTIME.spawn(async move { client.search(&query, 1, 15).await }).await;

            this.update(cx, |this, cx| {
                if this.kugou_query_generation != generation {
                    return;
                }

                match request {
                    Ok(Ok(response)) => {
                        this.kugou_items = crate::ui::kugou::parse_tracks(
                            &response.body,
                            "/data/lists",
                        )
                        .into_iter()
                        .map(|track| Arc::new(SearchPaletteItem::KugouTrack(track)))
                        .collect();
                    }
                    Ok(Err(err)) => {
                        warn!("kugou online search failed: {err}");
                        this.kugou_items.clear();
                    }
                    Err(err) => {
                        warn!("kugou online search task failed: {err}");
                        this.kugou_items.clear();
                    }
                }

                this.emit_merged_items(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Debounced online search: 350ms after the last keystroke, query NetEase
    /// and merge the results behind the local and KuGou ones.
    #[cfg(feature = "netease")]
    fn on_netease_query(&mut self, query: &str, cx: &mut Context<Self>) {
        use tracing::warn;

        self.netease_query_generation += 1;
        let generation = self.netease_query_generation;

        let query = query.trim().to_string();
        if query.chars().count() < 2 {
            self.netease_items.clear();
            self.emit_merged_items(cx);
            return;
        }

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;

            let client = crate::netease::shared_client();
            let request = crate::RUNTIME
                .spawn(async move {
                    client
                        .cloudsearch(&query, 1, 15, 0)
                        .await
                })
                .await;

            this.update(cx, |this, cx| {
                if this.netease_query_generation != generation {
                    return;
                }

                match request {
                    Ok(Ok(response)) => {
                        this.netease_items = crate::ui::netease::parse_tracks(
                            &response.body,
                            "/result/songs",
                        )
                        .into_iter()
                        .map(|track| Arc::new(SearchPaletteItem::NeteaseTrack(track)))
                        .collect();
                    }
                    Ok(Err(err)) => {
                        warn!("netease online search failed: {err}");
                        this.netease_items.clear();
                    }
                    Err(err) => {
                        warn!("netease online search task failed: {err}");
                        this.netease_items.clear();
                    }
                }

                this.emit_merged_items(cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        cx.update_entity(&self.palette, |palette, cx| {
            palette.reset(cx);
        });

        // drop online results so they don't reappear with an empty query
        #[cfg(feature = "kugou")]
        {
            self.kugou_items.clear();
            self.kugou_query_generation += 1;
        }
        #[cfg(feature = "netease")]
        {
            self.netease_items.clear();
            self.netease_query_generation += 1;
        }
        #[cfg(feature = "online_sources")]
        self.emit_merged_items(cx);
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.update(cx, |palette, cx| {
            palette.focus(window, cx);
        });
    }
}

impl EventEmitter<String> for SearchModel {}
impl EventEmitter<ViewSwitchMessage> for SearchModel {}

impl Render for SearchModel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.palette.clone()
    }
}
