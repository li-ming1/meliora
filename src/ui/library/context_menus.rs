pub mod album;
pub mod info_section;
pub mod track;

use std::{path::PathBuf, rc::Rc, sync::Arc};

use camino::Utf8PathBuf;
use cntp_i18n::{I18nString, tr};
use gpui::{App, Entity, Pixels, Point, SharedString};

use crate::sync::RecoveredRwLock;
use crate::{
    library::{
        db::{self},
        playlist::announce_playlist_update,
        scan::ScanInterface,
        types::{Album, Track},
    },
    playback::{
        interface::{PlaybackInterface, replace_queue},
        queue::QueueItemData,
        thread::PlaybackState,
    },
    ui::{
        availability::is_track_available,
        components::{
            context::ContextMenuBuilder,
            icons::{STAR, STAR_FILLED},
        },
        library::{
            ViewSwitchMessage,
            add_to_playlist::{open_add_album_to_playlist, open_add_to_playlist},
            context_menus::{album::AlbumContextMenu, track::TrackContextMenu},
        },
        library_access::LibraryAccess,
        models::{Models, PlaybackInfo, PlaylistInfoTransfer, is_song_liked},
    },
};

#[derive(Clone, Copy)]
pub struct PlaylistMenuInfo {
    pub id: i64,
    pub item_id: i64,
}

type TrackPlayFromHereHandler = Rc<dyn Fn(&mut App, &Track) + 'static>;

/// 库曲目 → 队列项。不做逐曲目的 `Path::exists` 探测：调用方交来的是整张表/整个歌单
/// （大库下一次点击就是 10k 次 stat），而播放引擎会跳过已丢失的文件。
/// 行的可用性用户从行数据里本来就看得见。
pub fn queue_items_from_tracks(tracks: &[Track]) -> Vec<QueueItemData> {
    tracks
        .iter()
        .map(|track| {
            let item = QueueItemData::new(track.location.clone(), Some(track.id), track.album_id);
            item.set_known_duration(Some(track.duration));
            item
        })
        .collect()
}

/// 歌单行 → 队列项，`queue_items_from_tracks` 的 `PlaylistTrackRow` 姊妹。三处
/// 逐字重复同一条映射（右键菜单的"立即播放/下一首播放"、歌单页的"播放全部"、
/// `find_playlist_tracks`），而同样的"不做逐行 `exists()` 探测"理由此前只在菜单
/// 那一处写过。
pub fn queue_items_from_playlist_rows(rows: &[db::PlaylistTrackRow]) -> Vec<QueueItemData> {
    rows.iter()
        .map(|row| {
            QueueItemData::new(
                row.location.clone().into(),
                Some(row.track_id),
                Some(row.album_id),
            )
        })
        .collect()
}

fn queue_item_data(location: PathBuf, id: i64, album_id: Option<i64>) -> QueueItemData {
    QueueItemData::new(location, Some(id), album_id)
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

/// 为表格行构造曲目右键菜单。行以 `Arc` 引用计数进入构建器；完整的 `Track`
/// （含 PathBuf）只在菜单真正打开时才深拷贝，而不是表格渲染时每行每帧一次。
pub(crate) fn track_menu_for_table_shared(
    track: Arc<Track>,
    is_available: bool,
    context: &TrackContextMenuContext,
) -> ContextMenuBuilder {
    let context = context.clone();
    // 菜单打开期间构建器每次渲染都重跑，所以它的函数体只能读缓存/快照——
    // 不得有阻塞查询或系统调用。
    Rc::new(move |_, cx| {
        // 用缓存的已点赞集合：这里跑 DB 查询会阻塞 UI 线程（见 is_song_liked 的文档）
        let is_liked = is_song_liked(cx, track.id);
        // 深拷贝推迟到菜单打开时；每帧成本只是 Arc 引用计数
        let track = Rc::new((*track).clone());
        gpui::IntoElement::into_any_element(TrackContextMenu::new(
            track,
            is_available,
            is_liked,
            context.clone(),
            None,
        ))
    })
}

/// 为表格行构造专辑右键菜单；引用计数 vs 深拷贝的取舍见
/// [`track_menu_for_table_shared`]。
pub(crate) fn album_menu_for_table_shared(
    album: Arc<Album>,
    context: &AlbumContextMenuContext,
) -> ContextMenuBuilder {
    let context = *context;
    Rc::new(move |_, _| {
        let album = Rc::new((*album).clone());
        gpui::IntoElement::into_any_element(AlbumContextMenu::new(album, context))
    })
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
        queue_items_from_tracks(&tracks)
    } else if let Some(playlist_id) = playlist_id {
        let rows = cx.list_playlist_tracks(playlist_id).unwrap_or_else(|e| {
            tracing::warn!("track listing load failed: {e}");
            Arc::default()
        });
        queue_items_from_playlist_rows(&rows)
    } else if let Some(album_id) = track.album_id {
        let tracks = cx.list_tracks_in_album(album_id).unwrap_or_else(|e| {
            tracing::warn!("track listing load failed: {e}");
            Arc::default()
        });
        queue_items_from_tracks(&tracks)
    } else {
        vec![queue_item_data(
            track.location.clone(),
            track.id,
            track.album_id,
        )]
    };

    play_from_track(cx, track, queue_items);
}

/// 曲目点赞/取消赞切换用的星标图标与本地化标签，按当前已点赞状态取键
/// （已点赞行的 id，未点赞时为 `None`）。
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

        announce_playlist_update(&playlist_tracker, cx, playlist_id);
    })
    .detach();
}

/// 此刻播放队列的长度；容忍队列锁毒化。
fn current_queue_length(cx: &App) -> usize {
    cx.global::<Models>()
        .queue
        .read(cx)
        .data
        .read_recovered()
        .len()
}

/// 「下一首播放」插入点占据的队列槽位：当前播放项之后一位；停止态（无当前
/// 曲目）插队头。queue.position 的 0 是双义哨兵——停止态下它表示「没有当前
/// 曲目」（thread.rs 的撤销路径置 0，stop() 后也可能残留旧值），此时 +1 会
/// 把「下一首」插到第 1 位、跳过队头；播放态的 0 则真的是「队头在播」，
/// 照旧 +1。所有「下一首播放」消费点必须经它取插入位（契约：queue.position
/// 的 0 同为双义值，消费侧一律加播放态守卫）。
pub(crate) fn next_queue_position(cx: &App) -> usize {
    let position = cx.global::<Models>().queue.read(cx).position;
    let stopped = *cx.global::<PlaybackInfo>().playback_state.read(cx) == PlaybackState::Stopped;
    if stopped { 0 } else { position + 1 }
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

/// 把 `items` 追加到队列并跳转到其中第一条。
pub(crate) fn play_items_now(cx: &mut App, items: impl IntoIterator<Item = QueueItemData>) {
    // 空选集早退必须保留：否则 queue_list 空转后仍会 jump 到队尾
    let items: Vec<_> = items.into_iter().collect();
    if items.is_empty() {
        return;
    }
    let queue_length = current_queue_length(cx);
    let playback_interface = cx.global::<PlaybackInterface>();
    playback_interface.queue_list(items);
    playback_interface.jump(queue_length);
}

/// 把 `items` 按顺序插到当前队列位置之后。
pub(crate) fn play_items_next(cx: &mut App, items: impl IntoIterator<Item = QueueItemData>) {
    let queue_position = next_queue_position(cx);
    cx.global::<PlaybackInterface>()
        .insert_list_at(items.into_iter().collect(), queue_position);
}

pub(crate) fn queue_items(cx: &mut App, items: impl IntoIterator<Item = QueueItemData>) {
    cx.global::<PlaybackInterface>()
        .queue_list(items.into_iter().collect());
}

fn play_track_now(cx: &mut App, track: &Track) {
    let data = queue_item_data(track.location.clone(), track.id, track.album_id);
    play_now(cx, data);
}

pub fn play_track_next(cx: &mut App, track: &Track) {
    let data = queue_item_data(track.location.clone(), track.id, track.album_id);
    play_next(cx, data);
}

fn queue_track(cx: &mut App, track: &Track) {
    let data = queue_item_data(track.location.clone(), track.id, track.album_id);
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
    let tracks = cx.list_tracks_in_album(album.id).unwrap_or_else(|e| {
        tracing::warn!("album tracks load failed: {e}");
        Arc::new(Vec::new())
    });
    queue_items_from_tracks(&tracks)
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
