use std::{
    cell::Cell,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, Mutex},
};

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, DragMoveEvent, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, actions, div, prelude::FluentBuilder, px, rems, rgba,
    uniform_list,
};
use rustc_hash::FxHashMap;
use tracing::error;

use crate::{
    library::{
        db::{self, LibraryAccess, PlaylistTrackRow, PlaylistTrackSortMethod},
        playlist::export_playlist,
        types::{DBString, Playlist, PlaylistType, Track},
    },
    playback::queue::QueueItemData,
    ui::{
        app::Pool,
        caching::meliora_cache,
        command_palette::{CommandCategory, CommandManager, CommandSpec},
        components::{
            button::{ButtonSize, button},
            drag_drop::{
                AlbumDragData, DragDropItemState, DragDropListConfig, DragDropListManager,
                DragPreview, DropIndicator, DropPosition, TrackDragData, check_drag_cancelled,
                handle_external_drag_move, handle_track_drag_move, handle_track_drop,
                request_edge_scroll,
            },
            dropdown::dropdown,
            icons::{PLAYLIST, SORT_ASCENDING, SORT_DESCENDING, STAR, icon},
            playback_controls::playback_controls,
            scrollbar::{ScrollableHandle, floating_scrollbar},
            table::table_data::TABLE_MAX_WIDTH,
            tooltip::build_tooltip,
        },
        library::collection_summary::format_collection_summary,
        library::track_item::{ArtistNameVisibility, TrackItem, TrackItemLeftField},
        models::{LIKED_SONGS_PLAYLIST_ID, Models, PlaylistEvent},
        theme::Theme,
        util::{create_or_retrieve_view, prune_views},
    },
};

use super::track_item::TrackPlaylistInfo;
use crate::ui::design::ICON_LG;

actions!(playlist, [Export, Import]);

// height + border
const PLAYLIST_ITEM_HEIGHT: f32 = 40.0;

/// Prefetch band: when the visible-row center moves more than half this many
/// rows, the view batch-prefetches full rows for `center ± ROW_PREFETCH_PAD`
/// on the async runtime into the view-local row cache, so newly built rows hit
/// the cache instead of one UI-thread `block_on` DB hit per row.
const ROW_PREFETCH_PAD: usize = 256;

/// Upper bound of the row prefetch cache (simple FIFO, no LRU dependency): two
/// full ±256-row prefetch windows plus slack.
const ROW_CACHE_CAPACITY: usize = 1024;

/// Prefetched playlist rows. `generation` invalidates in-flight prefetch
/// tasks: `reload` clears the cache and bumps it, so a task started before the
/// reload can never write pre-reload rows back. Same shape as the table's row
/// cache in `library::types::table`, but view-local on purpose.
struct PrefetchedRows {
    generation: u64,
    order: VecDeque<i64>,
    rows: FxHashMap<i64, Arc<Track>>,
}

type PrefetchedRowsHandle = Arc<Mutex<PrefetchedRows>>;

/// Cached row lookup; a short `Mutex` critical section.
fn prefetched_track(cache: &PrefetchedRowsHandle, track_id: i64) -> Option<Arc<Track>> {
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rows
        .get(&track_id)
        .cloned()
}

/// FIFO-bounded insert; returns false when the cache generation changed
/// mid-prefetch (a reload cleared it), which tells the prefetch task to stop
/// writing stale rows.
fn insert_prefetched(cache: &PrefetchedRowsHandle, track: Arc<Track>, generation: u64) -> bool {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.generation != generation {
        return false;
    }

    if cache.rows.contains_key(&track.id) {
        return true;
    }

    if cache.order.len() >= ROW_CACHE_CAPACITY
        && let Some(oldest) = cache.order.pop_front()
    {
        cache.rows.remove(&oldest);
    }
    cache.order.push_back(track.id);
    cache.rows.insert(track.id, track);
    true
}

/// Same insert from the UI-thread fallback path: rendering and event handling
/// run on the same thread and never interleave with a reload, so reading the
/// current generation and inserting with it can never race the cache being
/// cleared.
fn insert_prefetched_current(cache: &PrefetchedRowsHandle, track: Arc<Track>) {
    let generation = cache.lock().unwrap_or_else(|e| e.into_inner()).generation;
    insert_prefetched(cache, track, generation);
}

/// Drops the prefetched rows and invalidates in-flight prefetch tasks. Runs on
/// every reload: the track list (and with it every track_id) can change
/// underneath the cached rows.
fn clear_prefetched_rows(cache: &PrefetchedRowsHandle) {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.rows.clear();
    cache.order.clear();
    cache.generation = cache.generation.wrapping_add(1);
}

/// Schedules a background row prefetch when the visible center has moved half
/// a prefetch band since the last one. `state` tracks (cache generation,
/// center) so a stationary list costs one `Cell` read and one uncontended
/// lock per frame. On schedule the full rows for `center ± ROW_PREFETCH_PAD`
/// are fetched on the async runtime into the view's row cache (cleared on
/// every reload), turning the per-row `cx.get_track_by_id` UI-thread
/// `block_on` into a cache hit for rows past the keep-around band. Same shape
/// as the table's `schedule_row_prefetch`.
fn schedule_row_prefetch(
    state: &Rc<Cell<(u64, usize)>>,
    rows_cache: &PrefetchedRowsHandle,
    items: &[PlaylistTrackRow],
    center: usize,
    cx: &App,
) {
    let generation = rows_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .generation;
    let (scheduled_generation, scheduled_center) = state.get();
    if scheduled_generation == generation
        && scheduled_center.abs_diff(center) <= ROW_PREFETCH_PAD / 2
    {
        return;
    }
    state.set((generation, center));

    let start = center.saturating_sub(ROW_PREFETCH_PAD);
    let end = (center + ROW_PREFETCH_PAD + 1).min(items.len());
    if start >= end {
        return;
    }

    let pool = cx.global::<Pool>().0.clone();
    let track_ids: Vec<i64> = items[start..end].iter().map(|row| row.track_id).collect();
    let rows_cache = rows_cache.clone();

    // dropping the JoinHandle detaches the task
    let _ = crate::RUNTIME.spawn(async move {
        for track_id in track_ids {
            // skip rows the cache already holds: overlapping windows stay cheap
            if prefetched_track(&rows_cache, track_id).is_some() {
                continue;
            }
            match db::get_track_by_id(&pool, track_id).await {
                Ok(track) => {
                    if !insert_prefetched(&rows_cache, track, generation) {
                        // cache was cleared (reload): stop writing stale rows
                        break;
                    }
                }
                Err(err) => {
                    tracing::debug!(track_id, error = %err, "playlist row prefetch missed");
                }
            }
        }
    });
}

fn sort_method_label(method: PlaylistTrackSortMethod) -> SharedString {
    match method {
        PlaylistTrackSortMethod::Custom => tr!("SORT_CUSTOM", "Custom Order").into(),
        PlaylistTrackSortMethod::TitleAsc | PlaylistTrackSortMethod::TitleDesc => {
            tr!("SORT_TITLE").into()
        }
        PlaylistTrackSortMethod::ArtistAsc | PlaylistTrackSortMethod::ArtistDesc => {
            tr!("SORT_ARTIST", "Artist").into()
        }
        PlaylistTrackSortMethod::AlbumAsc | PlaylistTrackSortMethod::AlbumDesc => {
            tr!("SORT_ALBUM", "Album").into()
        }
        PlaylistTrackSortMethod::DurationAsc | PlaylistTrackSortMethod::DurationDesc => {
            tr!("SORT_DURATION", "Duration").into()
        }
        PlaylistTrackSortMethod::RecentlyAdded | PlaylistTrackSortMethod::RecentlyAddedAsc => {
            tr!("SORT_RECENTLY_ADDED").into()
        }
    }
}

const BASE_SORT_METHODS: [PlaylistTrackSortMethod; 6] = [
    PlaylistTrackSortMethod::Custom,
    PlaylistTrackSortMethod::TitleAsc,
    PlaylistTrackSortMethod::ArtistAsc,
    PlaylistTrackSortMethod::AlbumAsc,
    PlaylistTrackSortMethod::DurationAsc,
    PlaylistTrackSortMethod::RecentlyAdded,
];

/// Wrapper component for playlist track items that adds drag-and-drop support
pub struct PlaylistTrackItem {
    track_item: Entity<TrackItem>,
    idx: usize,
    playlist_item_id: i64,
    track_title: SharedString,
    drag_drop_manager: Entity<DragDropListManager>,
    /// Drag payload prebuilt once; `on_drag` takes it by value every frame, so
    /// render clones this small struct instead of re-running `from_track` (a
    /// per-frame `PathBuf` allocation). `Some` only when reordering is enabled.
    drag_data: Option<TrackDragData>,
}

impl PlaylistTrackItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut App,
        track_item: Entity<TrackItem>,
        idx: usize,
        playlist_item_id: i64,
        track_title: SharedString,
        drag_drop_manager: Entity<DragDropListManager>,
        list_id: gpui::ElementId,
        track_id: i64,
        album_id: Option<i64>,
        track_path: std::path::PathBuf,
        drag_enabled: bool,
    ) -> Entity<Self> {
        cx.new(|cx| {
            cx.observe(&drag_drop_manager, |_, _, cx| {
                cx.notify();
            })
            .detach();

            // drag payload prebuilt once: `on_drag` takes it by value every
            // frame, so render clones this small struct instead of re-running
            // `from_track` (a per-frame `PathBuf` allocation)
            let drag_data = if drag_enabled {
                Some(
                    TrackDragData::from_track(track_id, album_id, track_path, track_title.clone())
                        .with_reorder_info(list_id, idx),
                )
            } else {
                None
            };

            Self {
                track_item,
                idx,
                playlist_item_id,
                track_title,
                drag_drop_manager,
                drag_data,
            }
        })
    }
}

impl Render for PlaylistTrackItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let item_state = DragDropItemState::for_index(self.drag_drop_manager.read(cx), self.idx);

        let track_title = self.track_title.clone();

        let mut element = div()
            .id(("playlist-track-item", self.playlist_item_id as u64))
            .w_full()
            .h(px(PLAYLIST_ITEM_HEIGHT))
            .relative()
            .when(item_state.is_being_dragged, |d| d.opacity(0.5))
            .drag_over::<TrackDragData>(move |style, _, _, _| style.bg(rgba(0x88888822)))
            .child(DropIndicator::with_state(
                item_state.is_drop_target_before,
                item_state.is_drop_target_after,
                theme.button_primary,
            ));

        // payload prebuilt at construction; `on_drag` takes it by value every
        // frame, so render clones the small struct
        if let Some(drag_data) = self.drag_data.clone() {
            element = element.on_drag(drag_data, move |_, _, _, cx| {
                DragPreview::new(cx, track_title.clone())
            });
        }

        element.child(self.track_item.clone())
    }
}

pub struct PlaylistView {
    playlist: Arc<Playlist>,
    playlist_track_ids: Arc<Vec<PlaylistTrackRow>>,
    /// Bumped on every load so a slow load that finishes after a newer one
    /// cannot overwrite it (same guard as `Table::reload_rows`).
    load_generation: u64,
    views: Entity<FxHashMap<usize, Entity<PlaylistTrackItem>>>,
    render_counter: Entity<usize>,
    focus_handle: FocusHandle,
    first_render: bool,
    scroll_handle: UniformListScrollHandle,
    drag_drop_manager: Entity<DragDropListManager>,
    list_id: gpui::ElementId,
    sort_method: PlaylistTrackSortMethod,
    /// Prefetched full rows filled on the async runtime (see
    /// `schedule_row_prefetch`); row creation reads this before falling back
    /// to the per-row UI-thread `block_on` query. Cleared on every reload.
    track_row_cache: PrefetchedRowsHandle,
    /// Last scheduled row prefetch: (cache generation, visible center). See
    /// `schedule_row_prefetch`.
    prefetch_state: Rc<Cell<(u64, usize)>>,
}

/// Placeholder shown until the background load lands: the id is already
/// known, and the Liked Songs playlist is a fixed system playlist (models::
/// LIKED_SONGS_PLAYLIST_ID), so its header can render correctly right away.
/// Everything else fills in when `reload` completes.
fn placeholder_playlist(playlist_id: i64) -> Playlist {
    let is_liked_songs = playlist_id == LIKED_SONGS_PLAYLIST_ID;
    Playlist {
        id: playlist_id,
        name: DBString::from(if is_liked_songs { "Liked Songs" } else { "" }),
        created_at: chrono::Utc::now(),
        playlist_type: if is_liked_songs {
            PlaylistType::System
        } else {
            PlaylistType::User
        },
        position: 0,
        track_count: 0,
        total_duration: 0,
    }
}

impl PlaylistView {
    pub(super) fn new(cx: &mut App, playlist_id: i64) -> Entity<Self> {
        cx.new(|cx| {
            let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

            let list_id: gpui::ElementId = format!("playlist-{}", playlist_id).into();
            let config = DragDropListConfig::new(list_id.clone(), px(PLAYLIST_ITEM_HEIGHT));
            let drag_drop_manager = DragDropListManager::new(cx, config);

            let sort_method = cx
                .global::<Models>()
                .playlist_sort_methods
                .read(cx)
                .get(&playlist_id)
                .copied()
                .unwrap_or(PlaylistTrackSortMethod::Custom);

            cx.subscribe(
                &playlist_tracker,
                move |this: &mut Self, _, ev: &PlaylistEvent, cx| {
                    if let PlaylistEvent::PlaylistUpdated(id) = ev
                        && *id == this.playlist.id
                    {
                        this.reload(cx);
                    }
                },
            )
            .detach();

            cx.observe(&drag_drop_manager, |_, _, cx| {
                cx.notify();
            })
            .detach();

            let focus_handle = cx.focus_handle();

            cx.register_command(
                CommandSpec::new(
                    ("playlist::export", playlist_id),
                    Some(CommandCategory::Playlist),
                    tr!("EXPORT_PLAYLIST_TO_M3U", "Export Playlist to M3U"),
                    Export,
                )
                .focus_handle(focus_handle.clone()),
            );

            cx.on_release(move |_, cx| {
                cx.unregister_command(("playlist::export", playlist_id));
            })
            .detach();

            let views = cx.new(|_| FxHashMap::default());
            let render_counter = cx.new(|_| 0);
            let scroll_handle = UniformListScrollHandle::new();
            let track_row_cache: PrefetchedRowsHandle = Arc::new(Mutex::new(PrefetchedRows {
                generation: 0,
                order: VecDeque::new(),
                rows: FxHashMap::default(),
            }));
            let prefetch_state = Rc::new(Cell::new((0, 0)));

            let mut this = Self {
                playlist: Arc::new(placeholder_playlist(playlist_id)),
                playlist_track_ids: Arc::new(Vec::new()),
                load_generation: 0,
                views,
                render_counter,
                focus_handle,
                first_render: true,
                scroll_handle,
                drag_drop_manager,
                list_id,
                sort_method,
                track_row_cache,
                prefetch_state,
            };
            // Opening the view loads the playlist header + its sorted track
            // list on the runtime (see reload); until it lands the view shows
            // the placeholder header and an empty list.
            this.reload(cx);
            this
        })
    }

    /// Loads the playlist header and its sorted track list on the background
    /// runtime: opening the view (and every PlaylistUpdated reload / sort
    /// change) used to block the UI thread on the playlist aggregate plus a
    /// full-table JOIN. The view keeps showing its previous data until the
    /// new rows land; a generation guard drops results superseded by a newer
    /// load.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let playlist_id = self.playlist.id;
        let sort_method = self.sort_method;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let loaded = crate::RUNTIME
                .spawn(async move {
                    let playlist = db::get_playlist(&pool, playlist_id).await.ok();
                    let tracks = db::get_playlist_tracks_sorted(&pool, playlist_id, sort_method)
                        .await
                        .ok();
                    (playlist, tracks)
                })
                .await
                .unwrap_or((None, None));

            let _ = this.update(cx, |this, cx| {
                // A newer load superseded this one: this result is stale.
                if this.load_generation != generation {
                    return;
                }

                if let Some(playlist) = loaded.0 {
                    this.playlist = playlist;
                }
                if let Some(tracks) = loaded.1 {
                    this.playlist_track_ids = tracks;
                }
                this.views = cx.new(|_| FxHashMap::default());
                this.render_counter = cx.new(|_| 0);
                // cached rows must never outlive a reload: the track list (and
                // with it every track_id) can change underneath them; the
                // generation bump also invalidates in-flight prefetch tasks
                clear_prefetched_rows(&this.track_row_cache);
                cx.notify();
            });
        })
        .detach();
    }

    fn update_sort_method(&mut self, sort_method: PlaylistTrackSortMethod, cx: &mut Context<Self>) {
        let current_descending = Self::is_descending(self.sort_method);
        let next_sort = Self::apply_direction(Self::base_sort(sort_method), current_descending);

        self.set_sort_method(next_sort, cx);
    }

    fn toggle_sort_order(&mut self, cx: &mut Context<Self>) {
        if self.is_custom_sort() {
            return;
        }

        self.set_sort_method(Self::toggled_sort(self.sort_method), cx);
    }

    fn set_sort_method(&mut self, method: PlaylistTrackSortMethod, cx: &mut Context<Self>) {
        if self.sort_method == method {
            return;
        }
        self.sort_method = method;
        // The re-sorted rows load on the runtime (see reload); the view keeps
        // the previous order until they land.
        self.reload(cx);

        let playlist_sort_methods = cx.global::<Models>().playlist_sort_methods.clone();
        playlist_sort_methods.update(cx, |map, _| {
            map.insert(self.playlist.id, method);
        });

        cx.notify();
    }

    fn base_sort(sort_method: PlaylistTrackSortMethod) -> PlaylistTrackSortMethod {
        match sort_method {
            PlaylistTrackSortMethod::Custom => PlaylistTrackSortMethod::Custom,
            PlaylistTrackSortMethod::TitleAsc | PlaylistTrackSortMethod::TitleDesc => {
                PlaylistTrackSortMethod::TitleAsc
            }
            PlaylistTrackSortMethod::ArtistAsc | PlaylistTrackSortMethod::ArtistDesc => {
                PlaylistTrackSortMethod::ArtistAsc
            }
            PlaylistTrackSortMethod::AlbumAsc | PlaylistTrackSortMethod::AlbumDesc => {
                PlaylistTrackSortMethod::AlbumAsc
            }
            PlaylistTrackSortMethod::DurationAsc | PlaylistTrackSortMethod::DurationDesc => {
                PlaylistTrackSortMethod::DurationAsc
            }
            PlaylistTrackSortMethod::RecentlyAdded | PlaylistTrackSortMethod::RecentlyAddedAsc => {
                PlaylistTrackSortMethod::RecentlyAdded
            }
        }
    }

    fn apply_direction(
        base_sort_method: PlaylistTrackSortMethod,
        descending: bool,
    ) -> PlaylistTrackSortMethod {
        match base_sort_method {
            PlaylistTrackSortMethod::Custom => PlaylistTrackSortMethod::Custom,
            PlaylistTrackSortMethod::TitleAsc | PlaylistTrackSortMethod::TitleDesc => {
                if descending {
                    PlaylistTrackSortMethod::TitleDesc
                } else {
                    PlaylistTrackSortMethod::TitleAsc
                }
            }
            PlaylistTrackSortMethod::ArtistAsc | PlaylistTrackSortMethod::ArtistDesc => {
                if descending {
                    PlaylistTrackSortMethod::ArtistDesc
                } else {
                    PlaylistTrackSortMethod::ArtistAsc
                }
            }
            PlaylistTrackSortMethod::AlbumAsc | PlaylistTrackSortMethod::AlbumDesc => {
                if descending {
                    PlaylistTrackSortMethod::AlbumDesc
                } else {
                    PlaylistTrackSortMethod::AlbumAsc
                }
            }
            PlaylistTrackSortMethod::DurationAsc | PlaylistTrackSortMethod::DurationDesc => {
                if descending {
                    PlaylistTrackSortMethod::DurationDesc
                } else {
                    PlaylistTrackSortMethod::DurationAsc
                }
            }
            PlaylistTrackSortMethod::RecentlyAdded | PlaylistTrackSortMethod::RecentlyAddedAsc => {
                if descending {
                    PlaylistTrackSortMethod::RecentlyAdded
                } else {
                    PlaylistTrackSortMethod::RecentlyAddedAsc
                }
            }
        }
    }

    fn is_descending(sort_method: PlaylistTrackSortMethod) -> bool {
        matches!(
            sort_method,
            PlaylistTrackSortMethod::TitleDesc
                | PlaylistTrackSortMethod::ArtistDesc
                | PlaylistTrackSortMethod::AlbumDesc
                | PlaylistTrackSortMethod::DurationDesc
                | PlaylistTrackSortMethod::RecentlyAdded
        )
    }

    fn toggled_sort(sort_method: PlaylistTrackSortMethod) -> PlaylistTrackSortMethod {
        match sort_method {
            PlaylistTrackSortMethod::Custom => PlaylistTrackSortMethod::Custom,
            PlaylistTrackSortMethod::TitleAsc => PlaylistTrackSortMethod::TitleDesc,
            PlaylistTrackSortMethod::TitleDesc => PlaylistTrackSortMethod::TitleAsc,
            PlaylistTrackSortMethod::ArtistAsc => PlaylistTrackSortMethod::ArtistDesc,
            PlaylistTrackSortMethod::ArtistDesc => PlaylistTrackSortMethod::ArtistAsc,
            PlaylistTrackSortMethod::AlbumAsc => PlaylistTrackSortMethod::AlbumDesc,
            PlaylistTrackSortMethod::AlbumDesc => PlaylistTrackSortMethod::AlbumAsc,
            PlaylistTrackSortMethod::DurationAsc => PlaylistTrackSortMethod::DurationDesc,
            PlaylistTrackSortMethod::DurationDesc => PlaylistTrackSortMethod::DurationAsc,
            PlaylistTrackSortMethod::RecentlyAdded => PlaylistTrackSortMethod::RecentlyAddedAsc,
            PlaylistTrackSortMethod::RecentlyAddedAsc => PlaylistTrackSortMethod::RecentlyAdded,
        }
    }

    fn is_custom_sort(&self) -> bool {
        matches!(self.sort_method, PlaylistTrackSortMethod::Custom)
    }

    fn resolve_target_position(
        &self,
        drop_target: Option<(usize, DropPosition)>,
        cx: &mut Context<Self>,
    ) -> Option<i64> {
        let playlist_track_ids = self.playlist_track_ids.clone();
        drop_target.and_then(|(target_index, position)| {
            if playlist_track_ids.is_empty() {
                return None;
            }
            if target_index < playlist_track_ids.len() {
                let target_item_id = playlist_track_ids[target_index].playlist_item_id;
                let target_item = cx.get_playlist_item(target_item_id).ok()?;
                Some(match position {
                    DropPosition::Before => target_item.position,
                    DropPosition::After => target_item.position + 1,
                })
            } else {
                let last_item_id = playlist_track_ids.last()?.playlist_item_id;
                let last_item = cx.get_playlist_item(last_item_id).ok()?;
                Some(last_item.position + 1)
            }
        })
    }

    fn add_tracks_to_playlist(
        &mut self,
        track_ids: Vec<i64>,
        target_position: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let playlist_id = self.playlist.id;
        let pool = cx.global::<Pool>().0.clone();
        let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

        cx.spawn(async move |_, cx| {
            let pool_for_add = pool.clone();
            let task = crate::RUNTIME.spawn(async move {
                let mut new_item_ids: Vec<i64> = Vec::new();
                for track_id in track_ids {
                    let item_id =
                        db::add_playlist_item(&pool_for_add, playlist_id, track_id).await?;
                    new_item_ids.push(item_id);
                }
                Ok::<Vec<i64>, sqlx::Error>(new_item_ids)
            });

            let new_item_ids = match task.await {
                Ok(Ok(ids)) => ids,
                Ok(Err(err)) => {
                    error!("could not add tracks to playlist: {err:?}");
                    return;
                }
                Err(err) => {
                    error!("add tracks to playlist task panicked: {err:?}");
                    return;
                }
            };

            if let Some(pos) = target_position {
                for &item_id in new_item_ids.iter().rev() {
                    let pool_for_move = pool.clone();
                    let _ =
                        crate::RUNTIME
                            .spawn(async move {
                                db::move_playlist_item(&pool_for_move, item_id, pos).await
                            })
                            .await;
                }
            }

            playlist_tracker.update(cx, |_, cx| {
                cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
            });
        })
        .detach();
    }
}

impl Render for PlaylistView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        check_drag_cancelled(self.drag_drop_manager.clone(), cx);

        let items_clone = self.playlist_track_ids.clone();
        let views_model = self.views.clone();
        let render_counter = self.render_counter.clone();
        let pl_id = self.playlist.id;
        let playlist_name = self.playlist.name.0.clone();
        let scroll_handle = self.scroll_handle.clone();
        let drag_drop_manager = self.drag_drop_manager.clone();
        let list_id = self.list_id.clone();
        let prefetch_state = self.prefetch_state.clone();
        let track_row_cache = self.track_row_cache.clone();
        let item_count = items_clone.len();
        let playlist_id = self.playlist.id;
        let is_custom_sort = self.is_custom_sort();
        let current_sort = self.sort_method;
        let collection_summary =
            format_collection_summary(self.playlist.track_count, self.playlist.total_duration);

        if self.first_render {
            self.first_render = false;
            self.focus_handle.focus(window, cx);
        }

        let theme = cx.global::<Theme>();
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();

        let entity = cx.entity();
        let mut sort_dropdown = dropdown("playlist-sort-dropdown")
            .w(px(220.0))
            .flex_shrink_0()
            .selected(Self::base_sort(current_sort))
            .on_change(move |method: &PlaylistTrackSortMethod, _, cx| {
                entity.update(cx, |this, cx| {
                    this.update_sort_method(*method, cx);
                });
            });
        for method in BASE_SORT_METHODS {
            sort_dropdown = sort_dropdown.option(method, sort_method_label(method));
        }

        div()
            .image_cache(meliora_cache(
                ("playlist", self.playlist.id as u64),
                100,
            ))
            .id("playlist-view")
            .track_focus(&self.focus_handle)
            .key_context("Library")
            .on_action(move |_: &Export, _, cx| {
                if let Err(err) = export_playlist(cx, pl_id, &playlist_name) {
                    error!("Failed to export playlist: {}", err);
                }
            })
            .flex()
            .flex_col()
            .flex_shrink(1.0)
            .overflow_x_hidden()
            .when(!full_width, |this| this.max_w(px(TABLE_MAX_WIDTH)))
            .h_full()
            .child(
                div()
                    .pt(px(52.0))
                    .flex()
                    .overflow_x_hidden()
                    .flex_shrink(1.0)
                    .flex_col()
                    .h_full()
                    .child(
                        div()
                            .flex()
                            .overflow_x_hidden()
                            .flex_shrink(1.0)
                            .px(px(18.0))
                            .w_full()
                            .child(
                                div()
                                    .bg(theme.album_art_background)
                                    .shadow_sm()
                                    .w(px(160.0))
                                    .h(px(160.0))
                                    .flex_shrink_0()
                                    .rounded(px(theme.radius_sm))
                                    .overflow_hidden()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(if self.playlist.playlist_type == PlaylistType::System {
                                            STAR
                                        } else {
                                            PLAYLIST
                                        })
                                        .size(px(100.0)),
                                    ),
                            )
                            .child(
                                div()
                                    .ml(px(18.0))
                                    .mt_auto()
                                    .flex_shrink(1.0)
                                    .flex()
                                    .flex_col()
                                    .w_full()
                                    .overflow_x_hidden()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::EXTRA_BOLD)
                                            .text_size(rems(2.5))
                                            .line_height(rems(2.75))
                                            .overflow_x_hidden()
                                            .pb(px(10.0))
                                            .w_full()
                                            .text_ellipsis()
                                            .child(if self.playlist.is_liked_songs() {
                                                div().child(tr!("LIKED_SONGS"))
                                            } else {
                                                div().child(self.playlist.name.clone())
                                            }),
                                    )
                                    .child(
                                        div()
                                            .pb(px(10.0))
                                            .text_sm()
                                            .text_color(theme.text_secondary)
                                            .child(collection_summary),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_end()
                                            .justify_between()
                                            .gap(px(12.0))
                                            .w_full()
                                            .child(playback_controls(
                                                "playlist",
                                                !self.playlist_track_ids.is_empty(),
                                                false,
                                                false,
                                                move |cx| {
                                                    let playlist_tracks = cx
                                                        .get_playlist_tracks_sorted(
                                                            playlist_id,
                                                            current_sort,
                                                        )
                                                        .unwrap_or_default();

                                                     playlist_tracks
                                                        .iter()
                                                        .map(|row| {
                                                            QueueItemData::new(
                                                                cx,
                                                                row.location.clone().into(),
                                                                Some(row.track_id),
                                                                Some(row.album_id),
                                                            )
                                                        })
                                                        .collect()
                                                },
                                            ))
                                            .child(
                                                div()
                                                    .flex()
                                                    .gap(px(12.0))
                                                    .items_stretch()
                                                    .when(!is_custom_sort, |this| {
                                                        this.child(
                                                            button()
                                                                .id("playlist-sort-direction-button")
                                                                .size(ButtonSize::Large)
                                                                .on_click(cx.listener(
                                                                    |this: &mut PlaylistView, _, _, cx| {
                                                                        this.toggle_sort_order(cx);
                                                                    },
                                                                ))
                                                                .child(
                                                                    icon(
                                                                        if Self::is_descending(
                                                                            self.sort_method,
                                                                        ) {
                                                                            SORT_DESCENDING
                                                                        } else {
                                                                            SORT_ASCENDING
                                                                        },
                                                                    )
                                                                    .text_color(theme.text_secondary)
                                                                    .size(ICON_LG),
                                                                )
                                                                .tooltip(
                                                                    if Self::is_descending(self.sort_method)
                                                                    {
                                                                        build_tooltip(tr!("SORT_ASCENDING"))
                                                                    } else {
                                                                        build_tooltip(tr!(
                                                                            "SORT_DESCENDING"
                                                                        ))
                                                                    },
                                                                ),
                                                        )
                                                    })
                                                    .child(sort_dropdown),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("playlist-list-container")
                            .flex()
                            .w_full()
                            .h_full()
                            .relative()
                            .mt(px(18.0))
                            .on_drag_move::<TrackDragData>(cx.listener(
                                move |this: &mut PlaylistView,
                                      event: &DragMoveEvent<TrackDragData>,
                                      window,
                                      cx| {
                                    let before = {
                                        let manager = this.drag_drop_manager.read(cx);
                                        (manager.state.is_dragging, manager.state.drop_target)
                                    };
                                    let scroll_handle: ScrollableHandle =
                                        this.scroll_handle.clone().into();

                                    let reduced_motion = cx
                                        .global::<crate::settings::SettingsGlobal>()
                                        .model
                                        .read(cx)
                                        .interface
                                        .reduced_motion;
                                    let scrolled = handle_track_drag_move(
                                        this.drag_drop_manager.clone(),
                                        scroll_handle,
                                        event,
                                        item_count,
                                        cx,
                                        reduced_motion,
                                    );

                                    if scrolled {
                                        // guarded, at most one pending frame chain
                                        request_edge_scroll(
                                            this.drag_drop_manager.clone(),
                                            this.scroll_handle.clone().into(),
                                            window,
                                            cx,
                                        );
                                    }

                                    // repaint only when something visible moved:
                                    // drag move fires at mouse report rate and an
                                    // unconditional notify re-rendered the whole
                                    // view on every move
                                    let changed = {
                                        let manager = this.drag_drop_manager.read(cx);
                                        scrolled
                                            || (manager.state.is_dragging, manager.state.drop_target)
                                                != before
                                    };
                                    if changed {
                                        cx.notify();
                                    }
                                },
                            ))
                            .on_drag_move::<AlbumDragData>(cx.listener(
                                move |this: &mut PlaylistView,
                                      event: &DragMoveEvent<AlbumDragData>,
                                      window,
                                      cx| {
                                    let before = {
                                        let manager = this.drag_drop_manager.read(cx);
                                        (manager.state.is_dragging, manager.state.drop_target)
                                    };
                                    let scroll_handle: ScrollableHandle =
                                        this.scroll_handle.clone().into();
                                    let mouse_pos = event.event.position;
                                    let container_bounds = event.bounds;

                                    let reduced_motion = cx
                                        .global::<crate::settings::SettingsGlobal>()
                                        .model
                                        .read(cx)
                                        .interface
                                        .reduced_motion;
                                    let scrolled = handle_external_drag_move(
                                        this.drag_drop_manager.clone(),
                                        scroll_handle,
                                        mouse_pos,
                                        container_bounds,
                                        item_count,
                                        cx,
                                        reduced_motion,
                                    );

                                    if scrolled {
                                        request_edge_scroll(
                                            this.drag_drop_manager.clone(),
                                            this.scroll_handle.clone().into(),
                                            window,
                                            cx,
                                        );
                                    }

                                    // same gating as the track handler above
                                    let changed = {
                                        let manager = this.drag_drop_manager.read(cx);
                                        scrolled
                                            || (manager.state.is_dragging, manager.state.drop_target)
                                                != before
                                    };
                                    if changed {
                                        cx.notify();
                                    }
                                },
                            ))
                            .on_drop(cx.listener(
                                move |this: &mut PlaylistView, drag_data: &TrackDragData, _, cx| {
                                    let is_internal = drag_data
                                        .source_list_id
                                        .as_ref()
                                        .map(|id| *id == this.list_id)
                                        .unwrap_or(false);

                                    if is_internal && this.is_custom_sort() {
                                        let playlist_track_ids = this.playlist_track_ids.clone();
                                        let playlist_id = this.playlist.id;

                                        handle_track_drop(
                                            this.drag_drop_manager.clone(),
                                            drag_data,
                                            cx,
                                            |from_idx, to_idx, cx| {
                                                // The list can reload between drag start
                                                // and drop (async PlaylistUpdated): a
                                                // stale source index must not panic here.
                                                let Some(source_row) =
                                                    playlist_track_ids.get(from_idx)
                                                else {
                                                    return;
                                                };
                                                let item_id = source_row.playlist_item_id;

                                                let new_position = if to_idx < playlist_track_ids.len() {
                                                    let target_item_id = playlist_track_ids[to_idx].playlist_item_id;
                                                    // a missing item (concurrent removal) skips
                                                    // this move instead of panicking the handler
                                                    let target_item = match cx.get_playlist_item(target_item_id) {
                                                        Ok(item) => item,
                                                        Err(e) => {
                                                            error!("Failed to move playlist item {}: {}", item_id, e);
                                                            return;
                                                        }
                                                    };
                                                    target_item.position
                                                } else if let Some(last_row) = playlist_track_ids.last() {
                                                    // last row (to_idx beyond the end)
                                                    let last_item = match cx.get_playlist_item(last_row.playlist_item_id) {
                                                        Ok(item) => item,
                                                        Err(e) => {
                                                            error!("Failed to move playlist item {}: {}", last_row.playlist_item_id, e);
                                                            return;
                                                        }
                                                    };
                                                    last_item.position + 1
                                                } else {
                                                    // empty snapshot: nothing to move against
                                                    return;
                                                };

                                                if let Err(e) = cx.move_playlist_item(item_id, new_position)
                                                {
                                                    error!("Failed to move playlist item: {}", e);
                                                    return;
                                                }

                                                let tracker =
                                                    cx.global::<Models>().playlist_tracker.clone();
                                                tracker.update(cx, |_, cx| {
                                                    cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
                                                });
                                            },
                                        );
                                    } else if let Some(track_id) = drag_data.track_id {
                                        let drop_target = this.drag_drop_manager.read(cx).state.drop_target;
                                        let target_position = this.resolve_target_position(drop_target, cx);
                                        this.add_tracks_to_playlist(vec![track_id], target_position, cx);
                                        this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                                    } else {
                                        this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                                    }
                                    cx.notify();
                                },
                            ))
                            .on_drop(cx.listener(
                                move |this: &mut PlaylistView, drag_data: &AlbumDragData, _, cx| {
                                    let drop_target = this.drag_drop_manager.read(cx).state.drop_target;
                                    let target_position = this.resolve_target_position(drop_target, cx);

                                    if let Ok(tracks) = cx.list_tracks_in_album(drag_data.album_id) {
                                        let track_ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
                                        this.add_tracks_to_playlist(track_ids, target_position, cx);
                                    }

                                    this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                                    cx.notify();
                                },
                            ))
                            .child(
                                uniform_list("playlist-list", items_clone.len(), move |range, _, cx| {
                                    let start = range.start;
                                    let is_templ_render = range.start == 0 && range.end == 1;
                                    let center = start + (range.end - range.start) / 2;

                                    // keep the row prefetch one band ahead of the
                                    // visible window so new rows hit the row cache
                                    schedule_row_prefetch(
                                        &prefetch_state,
                                        &track_row_cache,
                                        &items_clone,
                                        center,
                                        cx,
                                    );

                                    let items = &items_clone[range];

                                    items
                                        .iter()
                                        .enumerate()
                                        .map(|(idx, item)| {
                                            let idx = idx + start;

                                            if !is_templ_render {
                                                prune_views(&views_model, &render_counter, idx, cx);
                                            }

                                            let drag_drop_manager = drag_drop_manager.clone();
                                            let list_id = list_id.clone();
                                            let track_row_cache = track_row_cache.clone();
                                            let playlist_item_id = item.playlist_item_id;
                                            let track_id = item.track_id;

                                            div().h(px(PLAYLIST_ITEM_HEIGHT)).child(
                                                create_or_retrieve_view(
                                                    &views_model,
                                                    idx,
                                                    move |cx| {
                                                        // prefetched rows first: a hit avoids the
                                                        // UI-thread `block_on` below (one per newly
                                                        // built row past the keep-around band)
                                                        let track = match prefetched_track(
                                                            &track_row_cache,
                                                            track_id,
                                                        ) {
                                                            Some(track) => track,
                                                            None => {
                                                                match cx.get_track_by_id(track_id) {
                                                                    Ok(track) => {
                                                                        // cache the fallback's
                                                                        // result so re-entering rows
                                                                        // hit the cache instead of
                                                                        // re-running the blocking query
                                                                        insert_prefetched_current(
                                                                            &track_row_cache,
                                                                            track.clone(),
                                                                        );
                                                                        track
                                                                    }
                                                                    // The track vanished from the
                                                                    // library between the playlist
                                                                    // snapshot and this view's
                                                                    // creation: render an empty
                                                                    // placeholder row (same height)
                                                                    // instead of panicking.
                                                                    Err(_) => {
                                                                        let track_item = TrackItem::new(
                                                                            cx,
                                                                            Track {
                                                                                id: 0,
                                                                                title: DBString::default(),
                                                                                album_id: None,
                                                                                track_number: None,
                                                                                disc_number: None,
                                                                                duration: 0,
                                                                                location: std::path::PathBuf::new(),
                                                                                artist_names: None,
                                                                                disc_subtitle: None,
                                                                            },
                                                                            false,
                                                                            ArtistNameVisibility::Always,
                                                                            TrackItemLeftField::Art,
                                                                            Some(TrackPlaylistInfo {
                                                                                id: pl_id,
                                                                                item_id: playlist_item_id,
                                                                            }),
                                                                            false, // vinyl_numbering - not applicable for playlists
                                                                            None, // max_track_num - not needed for Art left field
                                                                            None, // queue_context - playlist uses pl_id instead
                                                                            true, // show_go_to_album
                                                                            true, // show_go_to_artist
                                                                        );
                                                                        return PlaylistTrackItem::new(
                                                                            cx,
                                                                            track_item,
                                                                            idx,
                                                                            playlist_item_id,
                                                                            SharedString::default(),
                                                                            drag_drop_manager,
                                                                            list_id,
                                                                            track_id,
                                                                            None,
                                                                            std::path::PathBuf::new(),
                                                                            is_custom_sort,
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        };
                                                        let track_title: SharedString =
                                                            track.title.clone().0;
                                                        let track_path = track.location.clone();
                                                        let album_id = track.album_id;

                                                        let track_item = TrackItem::new(
                                                            cx,
                                                            (*track).clone(),
                                                            false,
                                                            ArtistNameVisibility::Always,
                                                            TrackItemLeftField::Art,
                                                            Some(TrackPlaylistInfo {
                                                                id: pl_id,
                                                                item_id: playlist_item_id,
                                                            }),
                                                            false, // vinyl_numbering - not applicable for playlists
                                                            None, // max_track_num - not needed for Art left field
                                                            None, // queue_context - playlist uses pl_id instead
                                                            true, // show_go_to_album
                                                            true, // show_go_to_artist
                                                        );

                                                        PlaylistTrackItem::new(
                                                            cx,
                                                            track_item,
                                                            idx,
                                                            playlist_item_id,
                                                            track_title,
                                                            drag_drop_manager,
                                                            list_id,
                                                            track_id,
                                                            album_id,
                                                            track_path,
                                                            is_custom_sort,
                                                        )
                                                    },
                                                    cx,
                                                ),
                                            )
                                        })
                                        .collect()
                                })
                                .w_full()
                                .h_full()
                                .flex()
                                .flex_col()
                                .track_scroll(&scroll_handle),
                            )
                            .child(
                                floating_scrollbar("playlist", scroll_handle)
                                    .right(px(4.0)),
                            ),
                    ),
            )
    }
}

pub fn find_playlist_tracks(cx: &mut App, playlist_id: i64) -> Vec<QueueItemData> {
    let playlist_tracks = cx.get_playlist_tracks(playlist_id).unwrap_or_default();

    playlist_tracks
        .iter()
        .map(|row| {
            QueueItemData::new(
                cx,
                row.location.clone().into(),
                Some(row.track_id),
                Some(row.album_id),
            )
        })
        .collect()
}
