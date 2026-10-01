pub mod album;
pub mod info_section;
pub mod track;

use std::{path::PathBuf, rc::Rc, sync::Arc};

use camino::Utf8PathBuf;
use cntp_i18n::{I18nString, tr};
use gpui::{AnyElement, App, AppContext, Entity, IntoElement, Pixels, Point, SharedString, Window};

use crate::{
    library::{
        db::{self, LibraryAccess},
        scan::ScanInterface,
        types::{Album, Track},
    },
    playback::{
        interface::{PlaybackInterface, replace_queue},
        queue::QueueItemData,
    },
    ui::app::Pool,
    ui::{
        availability::is_track_available,
        components::{
            context::ContextMenuBuilder,
            icons::{STAR, STAR_FILLED},
        },
        library::{
            ViewSwitchMessage,
            add_to_playlist::AddToPlaylist,
            context_menus::{album::AlbumContextMenu, track::TrackContextMenu},
        },
        models::{Models, PlaybackInfo, PlaylistEvent, PlaylistInfoTransfer, is_song_liked},
    },
};

#[derive(Clone, Copy)]
pub struct PlaylistMenuInfo {
    pub id: i64,
    pub item_id: i64,
}

type TrackPlayFromHereHandler = Rc<dyn Fn(&mut App, &Track) + 'static>;

/// Library tracks -> queue items. No per-track `Path::exists` probe: the
/// callers hand over whole tables/playlists (10k stats per click on a large
/// library) and the playback engine skips files that went missing. Row
/// availability is already visible to the user from the row data.
pub fn queue_items_from_tracks(cx: &mut App, tracks: &[Track]) -> Vec<QueueItemData> {
    tracks
        .iter()
        .map(|track| {
            let item =
                QueueItemData::new(cx, track.location.clone(), Some(track.id), track.album_id);
            item.set_known_duration(Some(track.duration));
            item
        })
        .collect()
}

fn queue_item_data(
    cx: &mut App,
    location: PathBuf,
    id: i64,
    album_id: Option<i64>,
) -> QueueItemData {
    QueueItemData::new(cx, location, Some(id), album_id)
}

#[derive(Clone, Default)]
pub struct TrackContextMenuContext {
    pub show_go_to_album: bool,
    pub show_go_to_artist: bool,
    pub play_from_here: Option<TrackPlayFromHereHandler>,
}

#[derive(Clone, Copy)]
pub struct AlbumContextMenuContext {
    pub show_go_to_artist: bool,
}

impl Default for AlbumContextMenuContext {
    fn default() -> Self {
        Self {
            show_go_to_artist: true,
        }
    }
}

pub(crate) struct AddToPlaylistState {
    pub show: Entity<bool>,
    pub add_to: Entity<AddToPlaylist>,
}

/// Creates or retrieves the `AddToPlaylist` keyed state for the given track,
/// returning the show toggle and the playlist entity.
pub(crate) fn add_to_playlist_state(
    key: &'static str,
    track_id: i64,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<bool>, Entity<AddToPlaylist>) {
    let menu_state = window.use_keyed_state((key, track_id as usize), cx, |_, cx| {
        let show = cx.new(|_| false);
        let add_to = AddToPlaylist::new(cx, show.clone(), vec![track_id]);
        AddToPlaylistState { show, add_to }
    });
    let state = menu_state.read(cx);
    (state.show.clone(), state.add_to.clone())
}

/// Creates or retrieves the `AddToPlaylist` keyed state for the given album,
/// returning the show toggle and the playlist entity. The album's track ids
/// load in the background the first time the submenu opens instead of here —
/// this initializer runs per newly-seen album row while the grid scrolls,
/// where a `block_on` query per album stalled the scroll.
pub(crate) fn add_album_to_playlist_state(
    key: &'static str,
    album_id: i64,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<bool>, Entity<AddToPlaylist>) {
    let menu_state = window.use_keyed_state((key, album_id as usize), cx, |_, cx| {
        let show = cx.new(|_| false);
        let add_to = AddToPlaylist::new(cx, show.clone(), Vec::new());

        let loaded = Rc::new(std::cell::Cell::new(false));
        let loaded_for_load = loaded.clone();
        let add_to_for_load = add_to.clone();
        cx.observe(&show, move |_, show, cx| {
            if !*show.read(cx) || loaded_for_load.get() {
                return;
            }
            loaded_for_load.set(true);
            let pool = cx.global::<Pool>().0.clone();
            let add_to_for_task = add_to_for_load.clone();
            cx.spawn(async move |_this, cx| {
                let tracks = crate::RUNTIME
                    .spawn(async move { db::list_tracks_in_album(&pool, album_id).await })
                    .await
                    .ok()
                    .and_then(|result| result.ok())
                    .map(|tracks| tracks.iter().map(|track| track.id).collect::<Vec<i64>>())
                    .unwrap_or_default();
                add_to_for_task.update(cx, |add_to, _| add_to.set_track_ids(tracks));
            })
            .detach();
        })
        .detach();

        AddToPlaylistState { show, add_to }
    });
    let state = menu_state.read(cx);
    (state.show.clone(), state.add_to.clone())
}

/// Builds the track context menu for a table row. The row enters the builder
/// as an `Arc` refcount; the full `Track` (PathBuf included) is deep-cloned
/// only when the menu actually opens, not once per row per frame while the
/// table renders.
pub(crate) fn track_menu_for_table_shared(
    track: Arc<Track>,
    is_available: bool,
    context: &TrackContextMenuContext,
    window: &mut Window,
    cx: &mut App,
) -> (ContextMenuBuilder, Option<AnyElement>) {
    let (show_add_to, add_to) = add_to_playlist_state("track-menu-state", track.id, window, cx);

    let context = context.clone();
    // the menu tree — including everything TrackContextMenu::render touches
    // (artist lookup query, path stat, translations) — is built only when the
    // menu opens, keeping it off the per-row repaint path
    let builder: ContextMenuBuilder = Rc::new(move |_, cx| {
        // cached liked set: a DB query here would block the UI thread (see is_song_liked docs)
        let is_liked = is_song_liked(cx, track.id);
        // deep clone deferred to menu-open; per-frame cost is the Arc refcount
        let track = Rc::new((*track).clone());
        TrackContextMenu::new(
            track,
            is_available,
            is_liked,
            context.clone(),
            None,
            show_add_to.clone(),
        )
        .into_any_element()
    });

    (builder, Some(add_to.into_any_element()))
}

/// Builds the album context menu for a table row; see
/// [`track_menu_for_table_shared`] for the refcount-vs-deep-clone tradeoff.
pub(crate) fn album_menu_for_table_shared(
    album: Arc<Album>,
    context: &AlbumContextMenuContext,
    window: &mut Window,
    cx: &mut App,
) -> (ContextMenuBuilder, Option<AnyElement>) {
    let (show_add_to, add_to) =
        add_album_to_playlist_state("album-menu-state", album.id, window, cx);

    let context = *context;
    let builder: ContextMenuBuilder = Rc::new(move |_, _| {
        let album = Rc::new((*album).clone());
        AlbumContextMenu::new(album, show_add_to.clone(), context).into_any_element()
    });

    (builder, Some(add_to.into_any_element()))
}

pub fn play_from_track(cx: &mut App, track: &Track, queue_items: Vec<QueueItemData>) {
    if !is_track_available(track) || queue_items.is_empty() {
        return;
    }

    let playback_interface = cx.global::<PlaybackInterface>();
    if let Some(index) = queue_items
        .iter()
        .position(|item| item.get_path() == &track.location)
    {
        playback_interface.replace_queue_with_index(queue_items, index);
    } else {
        playback_interface.replace_queue(queue_items);
    }
}

pub fn play_from_track_listing(
    cx: &mut App,
    track: &Track,
    playlist_id: Option<i64>,
    queue_context: Option<Arc<Vec<Track>>>,
) {
    let queue_items = if let Some(tracks) = queue_context {
        queue_items_from_tracks(cx, &tracks)
    } else if let Some(playlist_id) = playlist_id {
        // no per-row exists() probe: playlists can be thousands of rows and
        // the playback engine skips missing files
        cx.get_playlist_tracks(playlist_id)
            .unwrap_or_default()
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
    } else if let Some(album_id) = track.album_id {
        let tracks = cx.list_tracks_in_album(album_id).unwrap_or_default();
        queue_items_from_tracks(cx, &tracks)
    } else {
        vec![queue_item_data(
            cx,
            track.location.clone(),
            track.id,
            track.album_id,
        )]
    };

    play_from_track(cx, track, queue_items);
}

/// Star icon and localized label for a track's like/unlike toggle, keyed on
/// its current liked state (the liked row id, or `None` when unliked).
pub(crate) fn like_toggle_icon_and_label(is_liked: Option<i64>) -> (&'static str, I18nString) {
    if is_liked.is_some() {
        (STAR_FILLED, tr!("UNLIKE"))
    } else {
        (STAR, tr!("LIKE"))
    }
}

pub fn track_show_in_file_manager_label() -> SharedString {
    if cfg!(target_os = "macos") {
        tr!("SHOW_IN_FINDER", "Show in Finder").into()
    } else if cfg!(target_os = "windows") {
        tr!("SHOW_IN_FILE_EXPLORER", "Show in File Explorer").into()
    } else {
        tr!("SHOW_IN_FILE_MANAGER", "Show in File Manager").into()
    }
}

pub fn remove_from_playlist(
    item_id: i64,
    playlist_id: i64,
    pool: sqlx::SqlitePool,
    playlist_tracker: Entity<PlaylistInfoTransfer>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let task =
            crate::RUNTIME.spawn(async move { db::remove_playlist_item(&pool, item_id).await });

        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::error!("could not remove track from playlist: {err:?}");
                return;
            }
            Err(err) => {
                tracing::error!("remove-from-playlist task panicked: {err:?}");
                return;
            }
        }

        playlist_tracker.update(cx, |_, cx| {
            cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
        });
    })
    .detach();
}

/// Length of the playback queue right now; tolerates a poisoned queue lock.
fn current_queue_length(cx: &App) -> usize {
    cx.global::<Models>()
        .queue
        .read(cx)
        .data
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// Queue slot the "play next" insertion point occupies: one past the
/// currently playing item.
fn next_queue_position(cx: &App) -> usize {
    cx.global::<Models>().queue.read(cx).position + 1
}

pub(crate) fn play_now(cx: &mut App, data: QueueItemData) {
    let queue_length = current_queue_length(cx);
    let playback_interface = cx.global::<PlaybackInterface>();
    playback_interface.queue(data);
    playback_interface.jump(queue_length);
}

pub(crate) fn play_next(cx: &mut App, data: QueueItemData) {
    let queue_position = next_queue_position(cx);
    cx.global::<PlaybackInterface>()
        .insert_at(data, queue_position);
}

pub(crate) fn queue_item(cx: &mut App, data: QueueItemData) {
    cx.global::<PlaybackInterface>().queue(data);
}

/// Append `items` to the queue and jump to the first of them.
pub(crate) fn play_items_now(cx: &mut App, items: impl IntoIterator<Item = QueueItemData>) {
    let mut items = items.into_iter().peekable();
    if items.peek().is_none() {
        return;
    }
    let queue_length = current_queue_length(cx);
    let playback_interface = cx.global::<PlaybackInterface>();
    for item in items {
        playback_interface.queue(item);
    }
    playback_interface.jump(queue_length);
}

/// Insert `items` directly after the current queue position, in order.
pub(crate) fn play_items_next(cx: &mut App, items: impl IntoIterator<Item = QueueItemData>) {
    let queue_position = next_queue_position(cx);
    for (offset, item) in items.into_iter().enumerate() {
        cx.global::<PlaybackInterface>()
            .insert_at(item, queue_position + offset);
    }
}

pub(crate) fn queue_items(cx: &mut App, items: impl IntoIterator<Item = QueueItemData>) {
    let playback_interface = cx.global::<PlaybackInterface>();
    for item in items {
        playback_interface.queue(item);
    }
}

fn play_track_now(cx: &mut App, track: &Track) {
    let data = queue_item_data(cx, track.location.clone(), track.id, track.album_id);
    play_now(cx, data);
}

pub fn play_track_next(cx: &mut App, track: &Track) {
    let data = queue_item_data(cx, track.location.clone(), track.id, track.album_id);
    play_next(cx, data);
}

fn queue_track(cx: &mut App, track: &Track) {
    let data = queue_item_data(cx, track.location.clone(), track.id, track.album_id);
    queue_item(cx, data);
}

pub(crate) fn navigate_to_track_artist(cx: &mut App, track: &Track, position: Point<Pixels>) {
    let Ok(artists) = cx.artist_ids_for_track(track.id) else {
        return;
    };

    navigate_to_artists(cx, artists, position);
}

pub(crate) fn navigate_to_track_album(cx: &mut App, track: &Track) {
    navigate_to_album(cx, track, None);
}

pub(crate) fn navigate_to_track_album_and_reveal(cx: &mut App, track: &Track) {
    navigate_to_album(cx, track, Some(track.id));
}

fn navigate_to_album(cx: &mut App, track: &Track, target_track_id: Option<i64>) {
    let Some(album_id) = track.album_id else {
        return;
    };

    let switcher = cx.global::<Models>().switcher_model.clone();
    switcher.update(cx, |_, cx| {
        cx.emit(ViewSwitchMessage::Release(album_id, target_track_id));
    });
}

pub(crate) fn navigate_to_album_artists(cx: &mut App, album_id: i64, position: Point<Pixels>) {
    let Ok(artists) = cx.artist_ids_for_album(album_id) else {
        return;
    };

    navigate_to_artists(cx, artists, position);
}

pub(crate) fn navigate_to_artists(
    cx: &mut App,
    artists: Vec<(i64, String)>,
    position: Point<Pixels>,
) {
    match artists.as_slice() {
        [] => {}
        [(id, _)] => navigate_to_artist(cx, *id),
        _ => {
            let model = cx.global::<Models>().artist_picker_model.clone();
            model.update(cx, |m, cx| {
                *m = Some((
                    position,
                    artists
                        .into_iter()
                        .map(|(id, name)| (id, name.into()))
                        .collect(),
                ));
                cx.notify();
            });
        }
    }
}

pub(crate) fn navigate_to_artist(cx: &mut App, artist_id: i64) {
    let switcher = cx.global::<Models>().switcher_model.clone();
    switcher.update(cx, |_, cx| {
        cx.emit(ViewSwitchMessage::Artist(artist_id));
    });
}

fn available_album_queue_items(cx: &mut App, album: &Album) -> Vec<QueueItemData> {
    let tracks = cx
        .list_tracks_in_album(album.id)
        .unwrap_or_else(|_| Arc::new(Vec::new()));
    queue_items_from_tracks(cx, &tracks)
}

fn play_album_now(cx: &mut App, album: &Album) {
    let queue_items = available_album_queue_items(cx, album);
    if queue_items.is_empty() {
        return;
    }

    replace_queue(queue_items, cx);
}

pub fn play_album_next(cx: &mut App, album: &Album) {
    let queue_items = available_album_queue_items(cx, album);
    play_items_next(cx, queue_items);
}

fn shuffle_album(cx: &mut App, album: &Album) {
    let queue_items = available_album_queue_items(cx, album);
    if queue_items.is_empty() {
        return;
    }

    let interface = cx.global::<PlaybackInterface>();
    if !(*cx.global::<PlaybackInfo>().shuffling.read(cx)) {
        interface.toggle_shuffle();
    }
    replace_queue(queue_items, cx);
}

fn queue_album(cx: &mut App, album: &Album) {
    let items = available_album_queue_items(cx, album);
    queue_items(cx, items);
}

pub(crate) fn rescan_album(cx: &App, album: &Album) {
    let paths = match cx.list_album_paths(album.id) {
        Ok(paths) => paths,
        Err(err) => {
            tracing::error!("could not list paths for album rescan: {err:?}");
            return;
        }
    };

    let utf8_paths: Vec<Utf8PathBuf> = paths.into_iter().map(Utf8PathBuf::from).collect();
    cx.global::<ScanInterface>().rescan_paths(utf8_paths);
}

pub(crate) fn rescan_track(cx: &App, track: &Track) {
    let path = match Utf8PathBuf::from_path_buf(track.location.clone()) {
        Ok(path) => path,
        Err(path) => {
            tracing::error!("cannot rescan track with non-UTF-8 path: {:?}", path);
            return;
        }
    };

    cx.global::<ScanInterface>().rescan_paths(vec![path]);
}
