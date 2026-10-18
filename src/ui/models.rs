use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, Global, Pixels, Point, RenderImage,
    SharedString, Size, px,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::{
    library::{
        db::{self, LikedTrackSortMethod, PlaylistTrackSortMethod},
        scan::ScanEvent,
        types::{Album, Track},
    },
    media::metadata::Metadata,
    playback::{events::RepeatState, queue::QueueItemData, thread::PlaybackState},
    settings::{
        SettingsGlobal,
        interface::StartupLibraryView,
        storage::{
            DEFAULT_CONTROLS_LEFT_WIDTH, DEFAULT_CONTROLS_RIGHT_WIDTH, DEFAULT_LYRICS_FRACTION,
            DEFAULT_QUEUE_WIDTH, DEFAULT_SIDEBAR_WIDTH, StorageData, TableSettings,
        },
    },
    ui::{
        app::Pool,
        assets::db::clear_shrunk_thumb_cache,
        availability::{compute_availability_from_tracks, is_track_available},
        library::{NavigationHistory, ViewSwitchMessage},
        library_access::LibraryAccess,
    },
};

// SettingsHealth 已搬到 settings 层（它描述的是 settings.json 的状态）；
// 这里再导出，供现有的 `ui::models::SettingsHealth` 使用者继续用。
pub use crate::settings::SettingsHealth;

// 是，这看着有点傻
impl EventEmitter<Metadata> for Metadata {}

#[derive(Debug, PartialEq, Clone)]
pub struct ImageEvent(pub Arc<[u8]>);

impl EventEmitter<ImageEvent> for Option<Arc<RenderImage>> {}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct WindowInformation {
    pub maximized: bool,
    pub size: Size<Pixels>,
}

// 艺人选择浮层的点击位置与候选艺人
pub type ArtistPickerState = Option<(Point<Pixels>, Vec<(i64, SharedString)>)>;

/// 全局状态袋（2026-09-26 审计 A-5 起的演进约定）：字段按六个域归组，
/// 新增字段必须落到所属域内、插在该域现有字段的后面，六个域依次为——
/// 播放镜像（metadata/albumart）、库缓存（liked_ids/available_*/album_cache）、
/// 队列与扫描（queue/scan_state/settings_health）、导航（switcher_model/
/// artist_picker_model）、UI 布局态（sidebar/queue/lyrics/controls 宽高与
/// 开关、split_widths、window_information）、持久化镜像（table_settings/
/// 排序方法/show_about/playlist_tracker）。现有声明顺序里跨域交错的历史
/// 字段不强行重排；新字段不得造成新的跨域交错。
pub struct Models {
    pub metadata: Entity<Metadata>,
    /// 当前曲目的封面图。存进去的*值*始终是 `None`——这个实体只当作 SMTC
    /// 控制器的 `ImageEvent` 总线用（见
    /// `controllers::register_pbc_event_handlers`）；绝不要去读它的值。
    pub albumart: Entity<Option<Arc<RenderImage>>>,
    /// 「我收藏」歌单的缓存 id 集合：启动时加载一次、该歌单每次变动时重载。
    /// 让曲目行不必每行一次 DB 查询就能检查收藏状态。
    pub liked_ids: Entity<Option<Arc<HashSet<i64>>>>,
    /// 缓存的「至少还有一首曲目在盘上」的专辑 id 集合，启动时与扫描完成时
    /// 重载。让专辑行、网格瓦片与专辑右键菜单不必每行做一次 `block_on` 查询
    /// 外加每曲一次 stat 就能测可用性。
    pub available_albums: Entity<Option<Arc<FxHashSet<i64>>>>,
    /// 缓存的「至少还有一首署名曲目在盘上」的艺人 id 集合，启动时与扫描完成时
    /// 重载。让艺人行不必每行一次 `list_all_tracks_by_artist` 查询外加每曲
    /// 一次 stat 就能测可用性。
    pub available_artists: Entity<Option<Arc<FxHashSet<i64>>>>,
    /// 缓存的「文件仍在盘上」的曲目 id 集合，与专辑/艺人集合在同一次可用性
    /// 扫描里算出（启动时与扫描完成时）。让曲目行不必每个新建行在 UI 线程上
    /// 做一次 `path.exists()` stat 就能测可用性。在线（http/https）曲目在
    /// 查这个集合之前就短路为可用。
    pub available_tracks: Entity<Option<Arc<FxHashSet<i64>>>>,
    /// 供行构建使用的专辑元数据：重建一个曲目行过去要多达三次
    /// `get_album_by_id` block_on 查询（碟片编号、专辑标题、艺人覆盖）。
    /// 扫描是专辑元数据的唯一写入方，所以缓存就在扫描处与进程启动时清空。
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
    /// 全屏沉浸聆听视图（`ui::immersive`）：为 `true` 时主窗口渲染沉浸页而不是
    /// 库布局，并把窗口切到全屏。
    pub immersive: Entity<bool>,
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
    /// 输出流的采样率，单位 Hz；首个流建立之前为 0。
    pub sample_rate: Entity<u32>,
}

impl Global for PlaybackInfo {}

/// 进度显示的重绘量化粒度。position 以 33ms/250ms 双档广播，但进度条每
/// 拍实际移动亚像素（4 分钟曲目 250ms ≈ 0.3px@300px 宽）、时间标签整秒
/// 缓存——position 观察者按此粒度守卫 cx.notify()，把"播放中主窗口
/// 30fps 持续重绘"压到 4Hz，视觉无差（2026-10-07 桌面歌词专项实测项）。
pub const POSITION_DISPLAY_QUANTUM_MS: u64 = 250;

#[derive(Debug, Clone)]
pub struct Queue {
    pub data: Arc<RwLock<Arc<Vec<QueueItemData>>>>,
    pub position: usize,
    /// 由 UI 侧事件桥（`ui::playback_bridge`）在每次 `QueueUpdated`——播放线程
    /// 的「队列内容变了」广播——自增的单调计数；`QueuePositionChanged` 只写
    /// `position`、不碰这里。队列面板的观察者把它用作内容变化判别器：仅位置的
    /// 移动会保留活 uid 集合与每个条目的时长状态，所以面板那次 O(n) 的视图/
    /// 摘要重扫可以整个跳过（见 `ui::queue`）。不伴随 `QueueUpdated` 的内容变动
    /// （`replace_current_path` / `replace_next_path` 那两处流 URL 刷新）过去
    /// 同样不通知 UI，也不改任何 uid 与时长，所以这个 epoch 恰好只跟踪重扫过去
    /// 会响应的那批通知。
    pub content_epoch: u64,
}

// PlaylistInfoTransfer / PlaylistEvent 已搬到 library 层（它们是歌单域的状态；
// library/scan 不再为了发出它们而反向 import ui::models）；这里再导出，供现有的
// `ui::models::{PlaylistEvent, PlaylistInfoTransfer}` 使用者继续用。
pub use crate::library::playlist::{PlaylistEvent, PlaylistInfoTransfer};

fn resolve_startup_view(startup_view: StartupLibraryView) -> ViewSwitchMessage {
    match startup_view {
        StartupLibraryView::Albums => ViewSwitchMessage::Albums,
        StartupLibraryView::Artists => ViewSwitchMessage::Artists,
        StartupLibraryView::Tracks => ViewSwitchMessage::Tracks,
        StartupLibraryView::Files => ViewSwitchMessage::Files,
        // 「我收藏」需要 list_all_playlists 聚合才能解析出它的歌单 id；那个查询
        // 现在跑在运行时上（见 build_models），所以先从 Albums 兜底视图起步，
        // 等结果落地再导航。
        StartupLibraryView::LikedSongs => ViewSwitchMessage::Albums,
    }
}

/// 存进去的宽/高为 0 表示「从未保存过」（历史存储契约）：回退到编译期默认值，
/// 而不是把面板压扁。每个从 `StorageData` 重建的尺寸实体都走这里。
fn stored_dimension(stored: Pixels, default: Pixels) -> Pixels {
    if stored > px(0.0) { stored } else { default }
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
    let available_artists: Entity<Option<Arc<FxHashSet<i64>>>> = cx.new(|_| None);
    let available_tracks: Entity<Option<Arc<FxHashSet<i64>>>> = cx.new(|_| None);
    let album_cache: Entity<FxHashMap<i64, Arc<Album>>> = cx.new(|_| FxHashMap::default());

    let startup_setting = cx
        .global::<SettingsGlobal>()
        .model
        .read(cx)
        .interface
        .startup_library_view;
    let startup_view = resolve_startup_view(startup_setting);

    let switcher_model = cx.new(|_| NavigationHistory::new(startup_view));
    // 保留给下面的「我收藏」启动视图解析使用（该实体会移入 Models 全局）。
    let switcher_for_startup = switcher_model.clone();
    let artist_picker_model = cx.new(|_| None);

    let sidebar_width: Entity<Pixels> =
        cx.new(|_| stored_dimension(storage_data.sidebar_width(), DEFAULT_SIDEBAR_WIDTH));
    // 实际渲染出的侧栏宽度，由侧栏视图朝 `sidebar_width` 补间或在收起时压到 0。
    let animated_sidebar_width: Entity<Pixels> =
        cx.new(|_| stored_dimension(storage_data.sidebar_width(), DEFAULT_SIDEBAR_WIDTH));
    let queue_width: Entity<Pixels> =
        cx.new(|_| stored_dimension(storage_data.queue_width(), DEFAULT_QUEUE_WIDTH));
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
    let lyrics_height: Entity<Pixels> =
        cx.new(|_| stored_dimension(storage_data.lyrics_fraction(), DEFAULT_LYRICS_FRACTION));
    let controls_left_width: Entity<Pixels> = cx.new(|_| {
        stored_dimension(
            storage_data.controls_left_width(),
            DEFAULT_CONTROLS_LEFT_WIDTH,
        )
    });
    let controls_right_width: Entity<Pixels> = cx.new(|_| {
        stored_dimension(
            storage_data.controls_right_width(),
            DEFAULT_CONTROLS_RIGHT_WIDTH,
        )
    });

    let window_information = cx.new(|_| None);
    let immersive: Entity<bool> = cx.new(|_| false);

    cx.set_global(Models {
        metadata,
        albumart,
        liked_ids,
        available_albums,
        available_artists,
        available_tracks,
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
        immersive,
    });

    // 启动视图 = 「我收藏」：解析它的歌单 id 需要 list_all_playlists 聚合，
    // 而那过去会让启动阻塞在 UI 线程上。现在在运行时上解析、落地后导航；
    // 若届时用户已经导航到别处，就保持当前视图不动。
    if matches!(startup_setting, StartupLibraryView::LikedSongs) {
        let switcher_model = switcher_for_startup;
        let pool = cx.global::<Pool>().0.clone();
        cx.spawn(async move |cx| {
            let playlists = crate::RUNTIME
                .spawn(async move { db::list_all_playlists(&pool).await })
                .await
                .ok()
                .and_then(|result| result.ok());

            let Some(liked_id) = playlists.and_then(|playlists| {
                playlists
                    .iter()
                    .find(|playlist| playlist.is_liked_songs())
                    .map(|playlist| playlist.id)
            }) else {
                warn!(
                    "Liked Songs startup view selected but playlist could not be resolved, defaulting to Albums"
                );
                return;
            };

            cx.update(|cx| {
                if switcher_model.read(cx).current() == ViewSwitchMessage::Albums {
                    switcher_model.update(cx, |_, cx| {
                        cx.emit(ViewSwitchMessage::Playlist(liked_id));
                    });
                }
            });
        })
        .detach();
    }

    // 启动时填充一次收藏 id 集合（Models 已在上面注册），此后每当收藏歌单
    // 变化再填一次——所有点赞/取消点赞路径都会发出
    // PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID)。
    reload_liked_ids(cx);
    let tracker = cx.global::<Models>().playlist_tracker.clone();
    cx.subscribe(&tracker, |_, ev, cx| {
        if *ev == PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID) {
            reload_liked_ids(cx);
        }
    })
    .detach();

    // 专辑/艺人/曲目的可用性快照：启动时加载，每次扫描完成时刷新。扫描同样是
    // 专辑元数据的唯一写入方，所以供行构建用的专辑缓存也在同一点丢弃。
    reload_availability(cx);
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
            // 扫描同样是缩略图资产的唯一写入方：丢弃缩小后的缩略图缓存，让新增
            // /重扫到的封面重新进入有界 FIFO 缓存，而不是此后每次缓存未命中都要
            // 付一次 DB 加载 + 解码 + 缩小。
            clear_shrunk_thumb_cache();
            reload_availability(cx);
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

/// UI 行携带的收藏状态。存进去的 id 恒为*曲目 id*：`Some(track_id)` 表示这首
/// 歌在「我收藏」歌单里。它只是显示状态——取消收藏的动作会从曲目 id 解析出
/// playlist_item 行（见 `unlike_track` / `toggle_like_by_id`），所以谁都不许把
/// 这个值当成 playlist_item 的行 id 来解释。
pub(crate) trait HasLikedState {
    fn is_liked(&self) -> Option<i64>;
    fn set_liked(&mut self, item_id: Option<i64>);
}

/// 后台点赞/取消点赞 DB 任务的共用结果处理：成功返回 true；查询失败或任务
/// panic 时打出调用方给的消息并返回 false（调用方随后跳过它的后续 notify）。
fn like_task_succeeded<T, E: std::fmt::Debug>(
    result: Result<sqlx::Result<T>, E>,
    failure_log: &str,
    panic_log: &str,
) -> bool {
    match result {
        Ok(Ok(_)) => true,
        Ok(Err(err)) => {
            tracing::error!("{failure_log}: {err:?}");
            false
        }
        Err(err) => {
            tracing::error!("{panic_log}: {err:?}");
            false
        }
    }
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

    if !like_task_succeeded(task.await, "could not like song", "like task panicked") {
        return;
    }

    entity.update(cx, |this, cx| {
        this.set_liked(Some(track_id));
        cx.notify();
    });

    playlist_tracker.update(cx, |_, cx| {
        cx.emit(PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID));
    });
}

/// 把 `track_id` 从「我收藏」歌单里移除。歌单条目行由曲目 id 解析
/// （`remove_tracks_from_playlist` 在一个事务里完成查找与删除），所以这里
/// 从不依赖调用方的那一行当初存的是哪个 id。
pub(crate) async fn unlike_track<E: HasLikedState + 'static>(
    track_id: i64,
    entity: Entity<E>,
    playlist_tracker: Entity<PlaylistInfoTransfer>,
    pool: sqlx::SqlitePool,
    cx: &mut AsyncApp,
) {
    let task = crate::RUNTIME.spawn(async move {
        let track_ids = [track_id];
        db::remove_tracks_from_playlist(&pool, LIKED_SONGS_PLAYLIST_ID, &track_ids).await
    });

    match task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::error!("could not unlike song: {err:?}");
            entity.update(cx, |this, cx| {
                this.set_liked(Some(track_id));
                cx.notify();
            });
            return;
        }
        Err(err) => {
            tracing::error!("unlike task panicked: {err:?}");
            // 取消收藏其实没执行（事务随 panic 一起回滚），但调用方已经清掉了
            // 显示状态——把它恢复，让该行仍显示为已收藏，与 DB 一致。
            entity.update(cx, |this, cx| {
                this.set_liked(Some(track_id));
                cx.notify();
            });
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

    // 延迟执行，好让这个函数可以安全地从监听器内部调用：那时实体已被租走，
    // 同步 read/update 会重入并 panic。
    cx.defer(move |cx| {
        // 存进去的 id 只是显示状态（见 HasLikedState）；取消收藏永远针对本函数的
        // `track_id`，绝不用存着的值。
        let is_liked = entity.read(cx).is_liked().is_some();
        if is_liked {
            entity.update(cx, |this, cx| {
                this.set_liked(None);
                cx.notify();
            });
            cx.spawn(async move |cx| {
                unlike_track(track_id, entity, playlist_tracker, pool, cx).await;
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

/// 不依赖行实体的点赞/取消点赞。`is_liked` 只携带「是否已收藏」这个标记——
/// `Some(_)` 从「我收藏」歌单里移除 `track_id`，`None` 则加入。`Some` 里的 id
/// 刻意忽略：调用方历史上要么传曲目 id、要么传 playlist_item 行 id，两者都表示
/// 「已收藏」。
pub(crate) fn toggle_like_by_id(track_id: i64, is_liked: Option<i64>, cx: &mut App) {
    let pool = cx.global::<Pool>().0.clone();
    let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

    cx.spawn(async move |cx| {
        let task = crate::RUNTIME.spawn(async move {
            match is_liked {
                Some(_) => {
                    let track_ids = [track_id];
                    db::remove_tracks_from_playlist(&pool, LIKED_SONGS_PLAYLIST_ID, &track_ids)
                        .await
                }
                None => db::add_playlist_item(&pool, LIKED_SONGS_PLAYLIST_ID, track_id)
                    .await
                    .map(|_| ()),
            }
        });

        if !like_task_succeeded(
            task.await,
            "could not toggle like",
            "like/unlike task panicked",
        ) {
            return;
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

        if !like_task_succeeded(
            task.await,
            "could not toggle album like",
            "album like task panicked",
        ) {
            return;
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
    // 观察缓存的收藏 id 集合而不是歌单追踪器：追踪器触发时，反映这次变化的重载
    // 还已在途，在那里读缓存会读到过期值。集合实体恰好在新鲜数据落地时通知
    // （启动、点赞、取消点赞、批量收藏），且通知零 IO——旧的追踪器订阅在每次
    // 收藏歌单变化时都会为每条活行跑一次同步 DB 查询。
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

/// 在异步运行时上把收藏曲目的 id 集合（重）加载进 `Models.liked_ids`。启动时
/// 调用一次、收藏歌单每次变化时再调一次，于是曲目行可以对着缓存集合测收藏
/// 状态，而不必每行一次 DB 查询。尽力而为：加载失败时集合保持原样。
pub(crate) fn reload_liked_ids(cx: &mut App) {
    let pool = cx.global::<Pool>().0.clone();
    let liked_ids = cx.global::<Models>().liked_ids.clone();
    cx.spawn(async move |cx| {
        let result = crate::RUNTIME
            .spawn(async move {
                const QUERY: &str = "SELECT track_id FROM playlist_item WHERE playlist_id = ?";
                sqlx::query_as::<_, (i64,)>(QUERY)
                    .bind(LIKED_SONGS_PLAYLIST_ID)
                    .fetch_all(&pool)
                    .await
                    .map(|rows| rows.into_iter().map(|(id,)| id).collect::<HashSet<i64>>())
            })
            .await;
        // 真正的尽力而为：查询失败（例如 SQLITE_BUSY）或任务 panic 都必须让
        // 上一次的集合原地保留。用空集合覆盖会把 UI 里每一行都变成未收藏，
        // 并让下一次点赞插入一条重复的 playlist_item 行。
        let Ok(Ok(ids)) = result else {
            warn!("could not reload liked songs track ids, keeping previous set");
            return;
        };
        liked_ids.update(cx, |set, cx| {
            *set = Some(Arc::new(ids));
            cx.notify();
        });
    })
    .detach();
}

/// 对着缓存集合查 `track_id` 的收藏状态：曲目在「我收藏」歌单里时返回
/// `Some(track_id)`，否则 `None`——正是每行该存进 `is_liked` 字段的值
/// （见 `HasLikedState`）。首次重载落地之前返回 `None`（未知）；调用方观察
/// `Models.liked_ids`，所以答案会在下一次通知时自我纠正，全程零 DB 访问。
pub(crate) fn is_song_liked(cx: &App, track_id: i64) -> Option<i64> {
    let cached = cx.global::<Models>().liked_ids.read(cx).clone();
    cached.and_then(|set| set.contains(&track_id).then_some(track_id))
}

/// 从后台快照取的曲目可用性（启动与扫描完成时重载，与专辑/艺人集合同一次
/// 扫描）：让视图构造能回答「这首曲目的文件还在不在」，而不必在 UI 线程上
/// 每曲一次 `path.exists()` 系统调用。快照尚未加载时返回 `None` 并回退到
/// 快照机制之前的确切行为，所以启动窗口绝不会读成「不可用」——与表格行
/// （`types/table.rs`）同一份契约。
pub(crate) fn is_track_available_snapshot(cx: &App, track: &Track) -> bool {
    match cx.global::<Models>().available_tracks.read(cx).clone() {
        Some(set) => set.contains(&track.id),
        None => is_track_available(track),
    }
}

/// 在异步运行时上（重）加载专辑、艺人与曲目三份可用性快照：一次全量
/// `(location, id, album_id)` 曲目扫描加一次纯整数的 `(track_id, artist_id)`
/// 署名扫描，随后一趟阻塞处理，对每个不同的曲目路径只 stat 一次就得出全部
/// 三个集合。基础行（未按专辑过滤的行）是唯一完整的来源——
/// `find_album_availability` 会把没有专辑的曲目滤掉，而那些行绝不能被置灰——
/// 且署名关联在 Rust 里对着基础行完成，所以没有任何查询需要再搬运一次路径
/// 字符串。只在启动与扫描完成时调用。与 `reload_liked_ids` 同样是全有或全无：
/// 查询或任务失败时三份旧快照原地保留，绝不发布空集合。
pub(crate) fn reload_availability(cx: &mut App) {
    // 只有派生的查询完全成功时才交出这些行；每次失败都会被记录，好让调用方
    // 保留上一次的快照。
    async fn query_rows<T>(
        what: &'static str,
        task: tokio::task::JoinHandle<sqlx::Result<T>>,
    ) -> Option<T> {
        match task.await {
            Ok(Ok(rows)) => Some(rows),
            Ok(Err(err)) => {
                warn!("could not load {what}: {err}, keeping previous availability snapshots");
                None
            }
            Err(err) => {
                warn!("{what} task failed: {err}, keeping previous availability snapshots");
                None
            }
        }
    }

    let pool = cx.global::<Pool>().0.clone();
    let albums = cx.global::<Models>().available_albums.clone();
    let artists = cx.global::<Models>().available_artists.clone();
    let tracks = cx.global::<Models>().available_tracks.clone();
    cx.spawn(async move |cx| {
        let Some(track_rows) = query_rows(
            "track rows",
            crate::RUNTIME.spawn({
                let pool = pool.clone();
                async move { db::list_all_tracks(&pool).await }
            }),
        )
        .await
        else {
            return;
        };
        let Some(artist_credits) = query_rows(
            "track artist credits",
            crate::RUNTIME.spawn({
                let pool = pool.clone();
                async move { db::list_track_artist_credits(&pool).await }
            }),
        )
        .await
        else {
            return;
        };
        let (album_set, artist_set, track_set) = match crate::RUNTIME
            .spawn_blocking(move || {
                compute_availability_from_tracks(track_rows, artist_credits)
            })
            .await
        {
            Ok(sets) => sets,
            Err(err) => {
                warn!(
                    "availability stat pass task failed: {err}, keeping previous availability snapshots"
                );
                return;
            }
        };
        albums.update(cx, |slot, cx| {
            *slot = Some(Arc::new(album_set));
            cx.notify();
        });
        artists.update(cx, |slot, cx| {
            *slot = Some(Arc::new(artist_set));
            cx.notify();
        });
        tracks.update(cx, |slot, cx| {
            *slot = Some(Arc::new(track_set));
            cx.notify();
        });
    })
    .detach();
}

/// 供行构建使用的专辑元数据：缓存热的时候走缓存（只有扫描——它的唯一写入方——
/// 完成时才清空），冷未命中时一次阻塞查询——所以每张专辑第一次被见到之后，
/// 重建行就零次 DB 往返。
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
