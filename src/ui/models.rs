use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, Global, Pixels, Point, RenderImage,
    SharedString, Size,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::{
    library::{
        db::{self, LibraryAccess, LikedTrackSortMethod, PlaylistTrackSortMethod},
        scan::ScanEvent,
        types::Album,
    },
    media::metadata::Metadata,
    playback::{
        events::RepeatState,
        queue::QueueItemData,
        thread::PlaybackState,
    },
    settings::{
        SettingsGlobal,
        interface::StartupLibraryView,
        storage::{
            DEFAULT_LYRICS_FRACTION, DEFAULT_QUEUE_WIDTH, DEFAULT_SIDEBAR_WIDTH, StorageData,
            TableSettings,
        },
    },
    ui::{app::Pool, availability::compute_available_albums, library::{NavigationHistory, ViewSwitchMessage}},
};

// yes this looks a little silly
impl EventEmitter<Metadata> for Metadata {}

#[derive(Debug, PartialEq, Clone)]
pub struct ImageEvent(pub Arc<[u8]>);

impl EventEmitter<ImageEvent> for Option<Arc<RenderImage>> {}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct WindowInformation {
    pub maximized: bool,
    pub size: Size<Pixels>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsHealth {
    Ok,
    Corrupt { path: PathBuf },
}

// Click position and artist choices for the artist picker overlay
pub type ArtistPickerState = Option<(Point<Pixels>, Vec<(i64, SharedString)>)>;

pub struct Models {
    pub metadata: Entity<Metadata>,
    pub albumart: Entity<Option<Arc<RenderImage>>>,
    pub albumart_original: Entity<Option<Arc<RenderImage>>>,
    /// Cached id set of the "Liked Songs" playlist, loaded once at startup
    /// and re-loaded whenever that playlist changes. Lets track rows check
    /// like state without one DB query per row.
    pub liked_ids: Entity<Option<Arc<HashSet<i64>>>>,
    /// Cached set of album ids that still have at least one track on disk,
    /// reloaded at startup and on scan completion. Lets album rows, grid
    /// tiles and album context menus test availability without a per-row
    /// `block_on` query plus a stat per track.
    pub available_albums: Entity<Option<Arc<FxHashSet<i64>>>>,
    /// Album metadata for row construction: rebuilding a track row needed up
    /// to three `get_album_by_id` block_on queries (vinyl numbering, album
    /// title, artist override). A scan is the only writer of album metadata,
    /// so the cache is cleared there and on process-wide startup.
    pub album_cache: Entity<FxHashMap<i64, Arc<Album>>>,
    pub queue: Entity<Queue>,
    pub scan_state: Entity<ScanEvent>,
    pub settings_health: Entity<SettingsHealth>,
    pub switcher_model: Entity<NavigationHistory>,
    pub artist_picker_model: Entity<ArtistPickerState>,
    pub show_about: Entity<bool>,
    pub playlist_tracker: Entity<PlaylistInfoTransfer>,
    pub sidebar_width: Entity<Pixels>,
    pub animated_sidebar_width: Entity<Pixels>,
    pub queue_width: Entity<Pixels>,
    pub show_queue: Entity<bool>,
    pub show_lyrics: Entity<bool>,
    pub split_widths: std::collections::HashMap<String, Entity<Pixels>>,
    pub table_settings: Entity<std::collections::HashMap<String, TableSettings>>,
    pub liked_tracks_sort_method: Entity<LikedTrackSortMethod>,
    pub playlist_sort_methods: Entity<std::collections::HashMap<i64, PlaylistTrackSortMethod>>,
    pub sidebar_collapsed: Entity<bool>,
    pub lyrics_height: Entity<Pixels>,
    pub controls_left_width: Entity<Pixels>,
    pub controls_right_width: Entity<Pixels>,
    pub window_information: Entity<Option<WindowInformation>>,
}

impl Global for Models {}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct CurrentTrack(PathBuf);

impl CurrentTrack {
    pub fn new(path: PathBuf) -> Self {
        CurrentTrack(path)
    }

    pub fn get_path(&self) -> &PathBuf {
        &self.0
    }
}

impl PartialEq<std::path::PathBuf> for CurrentTrack {
    fn eq(&self, other: &std::path::PathBuf) -> bool {
        &self.0 == other
    }
}

#[derive(Clone)]
pub struct PlaybackInfo {
    pub position: Entity<u64>,
    pub duration: Entity<u64>,
    pub playback_state: Entity<PlaybackState>,
    pub current_track: Entity<Option<CurrentTrack>>,
    pub shuffling: Entity<bool>,
    pub repeating: Entity<RepeatState>,
    pub stop_after_current: Entity<bool>,
    pub volume: Entity<f64>,
    pub prev_volume: Entity<f64>,
    /// Output stream rate in Hz, 0 until the first stream exists.
    pub sample_rate: Entity<u32>,
}

impl Global for PlaybackInfo {}

#[derive(Debug, Clone)]
pub struct Queue {
    pub data: Arc<RwLock<Vec<QueueItemData>>>,
    pub position: usize,
}

pub struct PlaylistInfoTransfer;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlaylistEvent {
    PlaylistUpdated(i64),
    PlaylistDeleted(i64),
}

impl EventEmitter<PlaylistEvent> for PlaylistInfoTransfer {}

fn resolve_startup_view(cx: &App, startup_view: StartupLibraryView) -> ViewSwitchMessage {
    match startup_view {
        StartupLibraryView::Albums => ViewSwitchMessage::Albums,
        StartupLibraryView::Artists => ViewSwitchMessage::Artists,
        StartupLibraryView::Tracks => ViewSwitchMessage::Tracks,
        StartupLibraryView::Files => ViewSwitchMessage::Files,
        StartupLibraryView::LikedSongs => match cx.get_all_playlists() {
            Ok(playlists) => playlists
                .iter()
                .find(|playlist| playlist.is_liked_songs())
                .map(|playlist| ViewSwitchMessage::Playlist(playlist.id))
                .unwrap_or_else(|| {
                    warn!(
                        "Liked Songs startup view selected but playlist was not found, defaulting to Albums"
                    );
                    ViewSwitchMessage::Albums
                }),
            Err(error) => {
                warn!(
                    ?error,
                    "Liked Songs startup view selected but playlists could not be loaded, defaulting to Albums"
                );
                ViewSwitchMessage::Albums
            }
        },
    }
}

pub fn build_models(
    cx: &mut App,
    queue: Queue,
    storage_data: &StorageData,
    initial_track: Option<CurrentTrack>,
    initial_shuffle: bool,
    initial_repeat: RepeatState,
) {
    debug!("Building models");
    let metadata: Entity<Metadata> = cx.new(|_| Metadata::default());
    let albumart: Entity<Option<Arc<RenderImage>>> = cx.new(|_| None);
    let albumart_original: Entity<Option<Arc<RenderImage>>> = cx.new(|_| None);
    let queue: Entity<Queue> = cx.new(move |_| queue);
    let scan_state: Entity<ScanEvent> = cx.new(|_| ScanEvent::ScanCompleteIdle);
    let initial_corrupt_path = cx.global::<SettingsGlobal>().initial_corrupt_path.clone();
    let settings_health: Entity<SettingsHealth> = cx.new(|_| match initial_corrupt_path {
        Some(path) => SettingsHealth::Corrupt { path },
        None => SettingsHealth::Ok,
    });
    let show_about: Entity<bool> = cx.new(|_| false);

    let playlist_tracker: Entity<PlaylistInfoTransfer> = cx.new(|_| PlaylistInfoTransfer);

    let liked_ids: Entity<Option<Arc<HashSet<i64>>>> = cx.new(|_| None);
    let available_albums: Entity<Option<Arc<FxHashSet<i64>>>> = cx.new(|_| None);
    let album_cache: Entity<FxHashMap<i64, Arc<Album>>> = cx.new(|_| FxHashMap::default());

    let startup_view = resolve_startup_view(
        cx,
        cx.global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .startup_library_view,
    );

    let switcher_model = cx.new(|_| NavigationHistory::new(startup_view));
    let artist_picker_model = cx.new(|_| None);

    let sidebar_width: Entity<Pixels> = cx.new(|_| {
        if storage_data.sidebar_width > 0.0 {
            storage_data.sidebar_width()
        } else {
            DEFAULT_SIDEBAR_WIDTH
        }
    });
    // Rendered sidebar width, tweened toward `sidebar_width` / collapsed by the sidebar view.
    let animated_sidebar_width: Entity<Pixels> = cx.new(|_| {
        if storage_data.sidebar_width > 0.0 {
            storage_data.sidebar_width()
        } else {
            DEFAULT_SIDEBAR_WIDTH
        }
    });
    let queue_width: Entity<Pixels> = cx.new(|_| {
        if storage_data.queue_width > 0.0 {
            storage_data.queue_width()
        } else {
            DEFAULT_QUEUE_WIDTH
        }
    });
    let show_queue: Entity<bool> = cx.new(|_| storage_data.show_queue);
    let show_lyrics: Entity<bool> = cx.new(|_| storage_data.show_lyrics);
    let split_widths: std::collections::HashMap<String, Entity<Pixels>> = {
        use crate::settings::storage::SPLIT_FRACTION_KEYS;
        SPLIT_FRACTION_KEYS
            .iter()
            .map(|key| {
                let value = cx.new(|_| storage_data.split_fraction_for(key));
                (key.to_string(), value)
            })
            .collect()
    };

    let table_settings = cx.new(|_| storage_data.table_settings.clone());
    let liked_tracks_sort_method = cx.new(|_| storage_data.liked_tracks_sort_method);
    let playlist_sort_methods = cx.new(|_| storage_data.playlist_sort_methods.clone());
    let sidebar_collapsed: Entity<bool> = cx.new(|_| storage_data.sidebar_collapsed);
    let lyrics_height: Entity<Pixels> = cx.new(|_| {
        if storage_data.lyrics_fraction > 0.0 {
            storage_data.lyrics_fraction()
        } else {
            DEFAULT_LYRICS_FRACTION
        }
    });
    let controls_left_width: Entity<Pixels> = cx.new(|_| {
        if storage_data.controls_left_width > 0.0 {
            storage_data.controls_left_width()
        } else {
            crate::settings::storage::DEFAULT_CONTROLS_LEFT_WIDTH
        }
    });
    let controls_right_width: Entity<Pixels> = cx.new(|_| {
        if storage_data.controls_right_width > 0.0 {
            storage_data.controls_right_width()
        } else {
            crate::settings::storage::DEFAULT_CONTROLS_RIGHT_WIDTH
        }
    });

    let window_information = cx.new(|_| None);

    cx.set_global(Models {
        metadata,
        albumart,
        albumart_original,
        liked_ids,
        available_albums,
        album_cache,
        queue,
        scan_state,
        settings_health,
        switcher_model,
        artist_picker_model,
        show_about,
        playlist_tracker,
        sidebar_width,
        animated_sidebar_width,
        queue_width,
        show_queue,
        show_lyrics,
        split_widths,
        table_settings,
        liked_tracks_sort_method,
        playlist_sort_methods,
        sidebar_collapsed,
        lyrics_height,
        controls_left_width,
        controls_right_width,
        window_information,
    });

    // Populate the liked-songs id set once at startup (Models is registered
    // above), then again whenever the liked playlist changes — all like /
    // unlike paths emit PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID).
    reload_liked_ids(cx);
    let tracker = cx.global::<Models>().playlist_tracker.clone();
    cx.subscribe(&tracker, |_, ev, cx| {
        if *ev == PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID) {
            reload_liked_ids(cx);
        }
    })
    .detach();

    // Album availability snapshot: loaded at startup and refreshed whenever
    // a scan completes. A scan is also the only writer of album metadata, so
    // the row-construction album cache is dropped at the same point.
    reload_available_albums(cx);
    let scan_state = cx.global::<Models>().scan_state.clone();
    cx.observe(&scan_state, |scan_event, cx| {
        if matches!(
            scan_event.read(cx),
            ScanEvent::ScanCompleteIdle
                | ScanEvent::ScanCompleteWatching
                | ScanEvent::TargetedRescanComplete
        ) {
            let album_cache = cx.global::<Models>().album_cache.clone();
            album_cache.update(cx, |cache, _| cache.clear());
            reload_available_albums(cx);
        }
    })
    .detach();

    let position: Entity<u64> = cx.new(|_| 0);
    let duration: Entity<u64> = cx.new(|_| 0);
    let default_playback_state = if initial_track.is_some() {
        PlaybackState::Paused
    } else {
        PlaybackState::Stopped
    };
    let playback_state: Entity<PlaybackState> = cx.new(|_| default_playback_state);
    let current_track: Entity<Option<CurrentTrack>> = cx.new(|_| initial_track);
    let shuffling: Entity<bool> = cx.new(|_| initial_shuffle);
    let repeating: Entity<RepeatState> = cx.new(|_| initial_repeat);
    let stop_after_current: Entity<bool> = cx.new(|_| false);
    let volume: Entity<f64> = cx.new(|_| storage_data.volume);
    let prev_volume: Entity<f64> = cx.new(|_| storage_data.volume);
    let sample_rate: Entity<u32> = cx.new(|_| 0);

    cx.set_global(PlaybackInfo {
        position,
        duration,
        playback_state,
        current_track,
        shuffling,
        repeating,
        stop_after_current,
        volume,
        prev_volume,
        sample_rate,
    });
}

pub(crate) const LIKED_SONGS_PLAYLIST_ID: i64 = 1;

pub(crate) trait HasLikedState {
    fn is_liked(&self) -> Option<i64>;
    fn set_liked(&mut self, item_id: Option<i64>);
}

pub(crate) async fn like_track<E: HasLikedState + 'static>(
    track_id: i64,
    entity: Entity<E>,
    playlist_tracker: Entity<PlaylistInfoTransfer>,
    pool: sqlx::SqlitePool,
    cx: &mut AsyncApp,
) {
    let task = crate::RUNTIME.spawn(async move {
        db::add_playlist_item(&pool, LIKED_SONGS_PLAYLIST_ID, track_id).await
    });

    let new_id = match task.await {
        Ok(Ok(id)) => id,
        Ok(Err(err)) => {
            tracing::error!("could not like song: {err:?}");
            return;
        }
        Err(err) => {
            tracing::error!("like task panicked: {err:?}");
            return;
        }
    };

    entity.update(cx, |this, cx| {
        this.set_liked(Some(new_id));
        cx.notify();
    });

    playlist_tracker.update(cx, |_, cx| {
        cx.emit(PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID));
    });
}

pub(crate) async fn unlike_track<E: HasLikedState + 'static>(
    item_id: i64,
    entity: Entity<E>,
    playlist_tracker: Entity<PlaylistInfoTransfer>,
    pool: sqlx::SqlitePool,
    cx: &mut AsyncApp,
) {
    let task = crate::RUNTIME.spawn(async move { db::remove_playlist_item(&pool, item_id).await });

    match task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::error!("could not unlike song: {err:?}");
            entity.update(cx, |this, cx| {
                this.set_liked(Some(item_id));
                cx.notify();
            });
            return;
        }
        Err(err) => {
            tracing::error!("unlike task panicked: {err:?}");
            return;
        }
    }

    playlist_tracker.update(cx, |_, cx| {
        cx.emit(PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID));
    });
}

pub(crate) fn toggle_like<E: HasLikedState + 'static>(
    track_id: i64,
    entity: Entity<E>,
    cx: &mut App,
) {
    let pool = cx.global::<Pool>().0.clone();
    let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

    // Defer so this is safe to call from inside a listener, where the entity
    // is already leased and synchronous read/update would re-enter and panic.
    cx.defer(move |cx| {
        let is_liked = entity.read(cx).is_liked();
        if let Some(item_id) = is_liked {
            entity.update(cx, |this, cx| {
                this.set_liked(None);
                cx.notify();
            });
            cx.spawn(async move |cx| {
                unlike_track(item_id, entity, playlist_tracker, pool, cx).await;
            })
            .detach();
        } else {
            cx.spawn(async move |cx| {
                like_track(track_id, entity, playlist_tracker, pool, cx).await;
            })
            .detach();
        }
    });
}

pub(crate) fn toggle_like_by_id(track_id: i64, is_liked: Option<i64>, cx: &mut App) {
    let pool = cx.global::<Pool>().0.clone();
    let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

    cx.spawn(async move |cx| {
        let task = crate::RUNTIME.spawn(async move {
            match is_liked {
                Some(item_id) => db::remove_playlist_item(&pool, item_id).await,
                None => db::add_playlist_item(&pool, LIKED_SONGS_PLAYLIST_ID, track_id)
                    .await
                    .map(|_| ()),
            }
        });

        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::error!("could not toggle like: {err:?}");
                return;
            }
            Err(err) => {
                tracing::error!("like/unlike task panicked: {err:?}");
                return;
            }
        }

        playlist_tracker.update(cx, |_, cx| {
            cx.emit(PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID));
        });
    })
    .detach();
}

pub(crate) fn toggle_album_like(track_ids: Vec<i64>, all_liked: bool, cx: &mut App) {
    if track_ids.is_empty() {
        return;
    }

    let pool = cx.global::<Pool>().0.clone();
    let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

    cx.spawn(async move |cx| {
        let task = crate::RUNTIME.spawn(async move {
            if all_liked {
                db::remove_tracks_from_playlist(&pool, LIKED_SONGS_PLAYLIST_ID, &track_ids).await
            } else {
                db::add_tracks_to_playlist_if_missing(&pool, LIKED_SONGS_PLAYLIST_ID, &track_ids)
                    .await
            }
        });

        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::error!("could not toggle album like: {err:?}");
                return;
            }
            Err(err) => {
                tracing::error!("album like task panicked: {err:?}");
                return;
            }
        }

        playlist_tracker.update(cx, |_, cx| {
            cx.emit(PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID));
        });
    })
    .detach();
}

pub(crate) fn subscribe_liked_updates<E>(
    cx: &mut Context<E>,
    get_track_id: impl Fn(&E) -> Option<i64> + 'static,
) where
    E: HasLikedState + 'static,
{
    // Observe the cached liked-ids set instead of the playlist tracker: the
    // tracker fires while the reload that reflects the change is still in
    // flight, so a cache read there would be stale. The set entity notifies
    // exactly when fresh data lands (startup, like, unlike, batch like), and
    // the notification costs no IO — the old tracker subscription ran one
    // synchronous DB query per live row on every liked-playlist change.
    let liked_ids = cx.global::<Models>().liked_ids.clone();
    cx.observe(&liked_ids, move |this, _, cx| {
        let new_liked = get_track_id(this).and_then(|id| is_song_liked(cx, id));
        if new_liked != this.is_liked() {
            this.set_liked(new_liked);
            cx.notify();
        }
    })
    .detach();
}

/// (Re)loads the liked-songs track-id set into `Models.liked_ids` on the
/// async runtime. Called once at startup and on every liked-playlist change,
/// so track rows can test like state against the cached set instead of one
/// DB query per row. Best-effort: a failed load leaves the set unchanged.
pub(crate) fn reload_liked_ids(cx: &mut App) {
    let pool = cx.global::<Pool>().0.clone();
    let liked_ids = cx.global::<Models>().liked_ids.clone();
    cx.spawn(async move |cx| {
        let ids = crate::RUNTIME
            .spawn(async move {
                const QUERY: &str = "SELECT track_id FROM playlist_item WHERE playlist_id = ?";
                sqlx::query_as::<_, (i64,)>(QUERY)
                    .bind(LIKED_SONGS_PLAYLIST_ID)
                    .fetch_all(&pool)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id,)| id)
                    .collect::<HashSet<i64>>()
            })
            .await
            .unwrap_or_default();
        liked_ids.update(cx, |set, cx| {
            *set = Some(Arc::new(ids));
            cx.notify();
        });
    })
    .detach();
}

/// Like-state of `track_id` against the cached set. Mirrors
/// `playlist_has_track`'s `Option<i64>` shape so it can replace
/// row-construction queries directly. Returns `None` (unknown) until the
/// first reload lands — callers observe `Models.liked_ids`, so the answer
/// self-corrects on the next notification without any DB access.
pub(crate) fn is_song_liked(cx: &App, track_id: i64) -> Option<i64> {
    let cached = cx.global::<Models>().liked_ids.read(cx).clone();
    cached.and_then(|set| set.contains(&track_id).then_some(track_id))
}

/// (Re)loads the album-availability set on the async runtime: one query for
/// all `(album, location)` pairs, then one `exists()` stat per distinct path
/// on a blocking thread. Startup and scan completion only.
pub(crate) fn reload_available_albums(cx: &mut App) {
    let pool = cx.global::<Pool>().0.clone();
    let available = cx.global::<Models>().available_albums.clone();
    cx.spawn(async move |cx| {
        let rows = crate::RUNTIME
            .spawn(async move { db::list_album_availability(&pool).await })
            .await
            .map(|result| result.unwrap_or_default())
            .unwrap_or(vec![]);
        let set = crate::RUNTIME
            .spawn_blocking(move || compute_available_albums(rows))
            .await
            .unwrap_or_default();
        available.update(cx, |slot, cx| {
            *slot = Some(Arc::new(set));
            cx.notify();
        });
    })
    .detach();
}

/// Album metadata for row construction, from the cache when warm (cleared
/// only when a scan, its sole writer, completes) and one blocking query on a
/// cold miss — so a row rebuild costs zero DB round trips after the first
/// time each album is seen.
pub(crate) fn cached_album(cx: &mut App, album_id: i64) -> Option<Arc<Album>> {
    let cache = cx.global::<Models>().album_cache.clone();
    if let Some(album) = cache.read(cx).get(&album_id).cloned() {
        return Some(album);
    }
    let album = cx.get_album_by_id(album_id).ok()?;
    cache.update(cx, |cache, _| {
        cache.insert(album_id, album.clone());
    });
    Some(album)
}
