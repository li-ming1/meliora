use std::sync::Arc;
#[cfg(feature = "online_sources")]
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Window,
};
use nucleo::Utf32String;
use tracing::debug;

use crate::{
    library::{db, scan::ScanEvent},
    ui::{
        app::Pool, availability::compute_available_albums, components::palette::Palette,
        library::ViewSwitchMessage, models::Models,
    },
};

use super::search_item::SearchPaletteItem;

type MatcherFunc = Box<dyn Fn(&Arc<SearchPaletteItem>, &mut App) -> Utf32String + 'static>;
type OnAccept = Box<dyn Fn(&Arc<SearchPaletteItem>, &mut App) + 'static>;

pub struct SearchModel {
    palette: Entity<Palette<SearchPaletteItem, MatcherFunc, OnAccept>>,
    /// Local (library) items, kept around so online results can be merged in.
    local_items: Vec<Arc<SearchPaletteItem>>,
    /// Bumped when the local index is invalidated; an in-flight background
    /// load compares against it so a stale load can't overwrite a newer one.
    load_generation: u64,
    /// Whether a (completed) local index exists. Distinct from
    /// `local_items.is_empty()` so the palette-open observer doesn't stack a
    /// new load on every open while one is still in flight.
    local_items_loaded: bool,
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

/// Loads the whole-library search index on the background runtime: three
/// full-table queries plus one `exists()` stat per distinct track path take
/// seconds on a 100k library, and this used to run synchronously inside the
/// first palette-open frame.
async fn load_search_items_off_thread(pool: sqlx::SqlitePool) -> Vec<Arc<SearchPaletteItem>> {
    let availability_rows = db::list_album_availability(&pool).await.unwrap_or_default();
    let available_albums =
        tokio::task::spawn_blocking(move || compute_available_albums(availability_rows))
            .await
            .unwrap_or_default();

    let albums = db::list_albums_search(&pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|(id, title, artist_override, artists)| {
                    (
                        id,
                        title,
                        artist_override,
                        artists,
                        available_albums.contains(&id),
                    )
                })
                .collect()
        })
        .unwrap_or_else(|e| {
            debug!("Failed to load albums for search: {:?}", e);
            Vec::new()
        });

    let artists = db::list_artists_search(&pool).await.unwrap_or_else(|e| {
        debug!("Failed to load artists for search: {:?}", e);
        Vec::new()
    });

    let tracks = db::list_tracks_search(&pool).await.unwrap_or_else(|e| {
        debug!("Failed to load tracks for search: {:?}", e);
        Vec::new()
    });

    SearchPaletteItem::from_search_results(albums, artists, tracks)
}

impl SearchModel {
    pub fn new(cx: &mut App, show: &Entity<bool>) -> Entity<SearchModel> {
        cx.new(|cx| {
            let weak_self = cx.weak_entity();

            // Search text is precomputed on every item at construction (see
            // `SearchPaletteItem::from_search_results` and the online query
            // handlers below), so index injection never formats per item.
            let matcher: MatcherFunc = Box::new(|item, _| {
                Utf32String::from(match item.as_ref() {
                    SearchPaletteItem::Album { search_text, .. }
                    | SearchPaletteItem::Artist { search_text, .. }
                    | SearchPaletteItem::Track { search_text, .. } => search_text.as_ref(),
                    #[cfg(feature = "kugou")]
                    SearchPaletteItem::KugouTrack { search_text, .. } => search_text.as_ref(),
                    #[cfg(feature = "netease")]
                    SearchPaletteItem::NeteaseTrack { search_text, .. } => search_text.as_ref(),
                })
            });

            let on_accept: OnAccept = Box::new(move |item, cx| {
                let event = match item.as_ref() {
                    SearchPaletteItem::Album { id, .. } => {
                        Some(ViewSwitchMessage::Release(*id, None))
                    }
                    SearchPaletteItem::Artist { id, .. } => Some(ViewSwitchMessage::Artist(*id)),
                    SearchPaletteItem::Track { id, album_id, .. } => album_id
                        .as_ref()
                        .map(|album_id| ViewSwitchMessage::Release(*album_id, Some(*id))),
                    #[cfg(feature = "kugou")]
                    SearchPaletteItem::KugouTrack { track, .. } => {
                        crate::ui::kugou::play_track_now(cx, track);
                        None
                    }
                    #[cfg(feature = "netease")]
                    SearchPaletteItem::NeteaseTrack { track, .. } => {
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

            // Do not build the whole-library index here: SearchView is created
            // with the main window, and three full-table queries plus an
            // availability pass would block the first frame for a large
            // library. The first palette open (observer below) loads it, and
            // scan completions refresh it.
            let palette = Palette::new(cx, Vec::new(), matcher, on_accept, show);

            let search_model = SearchModel {
                palette,
                local_items: Vec::new(),
                load_generation: 0,
                local_items_loaded: false,
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
                    |this: &mut SearchModel, _, query: &SharedString, cx| {
                        #[cfg(feature = "kugou")]
                        this.on_kugou_query(query, cx);
                        #[cfg(feature = "netease")]
                        this.on_netease_query(query, cx);
                    },
                )
                .detach();
            }

            // 首次打开搜索面板时在后台构建本地索引并推给调色板（按需构建，避免
            // 启动即常驻，也避免打开面板那一帧被三个全表查询 + 逐路径 stat 卡死）
            let show_for_load = show.clone();
            cx.observe(&show_for_load, move |this, show, cx| {
                if *show.read(cx) && !this.local_items_loaded {
                    this.load_local_items_async(cx);
                }
            })
            .detach();

            let scan_status = cx.global::<Models>().scan_state.clone();
            let show_for_scan = show.clone();

            cx.observe(&scan_status, move |this, scan_event, cx| {
                let state = scan_event.read(cx);

                if matches!(
                    *state,
                    ScanEvent::ScanCompleteIdle
                        | ScanEvent::ScanCompleteWatching
                        | ScanEvent::TargetedRescanComplete
                ) {
                    debug!("Scan complete, refreshing search items");

                    // File-watcher rescans fire even while the palette is
                    // closed: rebuilding the whole index then would query the
                    // whole library for results nobody is looking at.
                    // Invalidate (dropping any in-flight background load) and
                    // only reload now if the panel is actually open.
                    this.local_items.clear();
                    this.local_items_loaded = false;
                    this.load_generation += 1;
                    if !*show_for_scan.read(cx) {
                        return;
                    }

                    this.load_local_items_async(cx);
                }
            })
            .detach();

            search_model
        })
    }

    /// Local items followed by online (KuGou, then NetEase) items.
    fn merged_items(&self) -> Vec<Arc<SearchPaletteItem>> {
        self.local_items
            .iter()
            .chain(self.kugou_iter())
            .chain(self.netease_iter())
            .cloned()
            .collect()
    }

    /// Loads the local index on the background runtime and pushes the merged
    /// item set to the palette when it lands. Online results may have arrived
    /// while the load was in flight (the palette can be queried immediately),
    /// so the apply emits the full merge — emitting local-only would drop
    /// them. A newer load generation discards this one's result.
    fn load_local_items_async(&mut self, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let generation = self.load_generation;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let new_items = crate::RUNTIME
                .spawn(async move { load_search_items_off_thread(pool).await })
                .await
                .unwrap_or_default();

            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.local_items = new_items;
                this.local_items_loaded = true;
                let emitted = this.merged_items();
                this.palette.update(cx, |_, cx| {
                    cx.emit(emitted);
                });
            })
            .ok();
        })
        .detach();
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
            // only re-merge when something actually went away, or every
            // keystroke below two chars re-emits the whole library to the finder
            if !self.kugou_items.is_empty() {
                self.kugou_items.clear();
                self.emit_merged_items(cx);
            }
            return;
        }

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;

            // a newer keystroke superseded this one; bail before hitting the
            // network so fast typing doesn't fire one request per character
            if this
                .update(cx, |this, _| this.kugou_query_generation != generation)
                .unwrap_or(true)
            {
                return;
            }

            let client = crate::kugou::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.search(&query, 1, 15).await })
                .await;

            this.update(cx, |this, cx| {
                if this.kugou_query_generation != generation {
                    return;
                }

                match request {
                    Ok(Ok(response)) => {
                        this.kugou_items =
                            crate::ui::kugou::parse_tracks(&response.body, "/data/lists")
                                .into_iter()
                                .map(|track| {
                                    let search_text: Arc<str> =
                                        format!("{} {}", track.title, track.artist).into();
                                    Arc::new(SearchPaletteItem::KugouTrack { track, search_text })
                                })
                                .collect();
                        this.emit_merged_items(cx);
                    }
                    // a failed search keeps the previous results: wiping them
                    // makes the palette flash empty on a transient network
                    // error, and the stale items are dropped on the next
                    // generation change / reset anyway
                    Ok(Err(err)) => {
                        warn!("kugou online search failed: {err}");
                    }
                    Err(err) => {
                        warn!("kugou online search task failed: {err}");
                    }
                }
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
            // only re-merge when something actually went away, or every
            // keystroke below two chars re-emits the whole library to the finder
            if !self.netease_items.is_empty() {
                self.netease_items.clear();
                self.emit_merged_items(cx);
            }
            return;
        }

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;

            // a newer keystroke superseded this one; bail before hitting the
            // network so fast typing doesn't fire one request per character
            if this
                .update(cx, |this, _| this.netease_query_generation != generation)
                .unwrap_or(true)
            {
                return;
            }

            let client = crate::netease::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.cloudsearch(&query, 1, 15, 0).await })
                .await;

            this.update(cx, |this, cx| {
                if this.netease_query_generation != generation {
                    return;
                }

                match request {
                    Ok(Ok(response)) => {
                        this.netease_items =
                            crate::ui::netease::parse_tracks(&response.body, "/result/songs")
                                .into_iter()
                                .map(|track| {
                                    let search_text: Arc<str> =
                                        format!("{} {}", track.title, track.artist).into();
                                    Arc::new(SearchPaletteItem::NeteaseTrack { track, search_text })
                                })
                                .collect();
                        this.emit_merged_items(cx);
                    }
                    // a failed search keeps the previous results: wiping them
                    // makes the palette flash empty on a transient network
                    // error, and the stale items are dropped on the next
                    // generation change / reset anyway
                    Ok(Err(err)) => {
                        warn!("netease online search failed: {err}");
                    }
                    Err(err) => {
                        warn!("netease online search task failed: {err}");
                    }
                }
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
