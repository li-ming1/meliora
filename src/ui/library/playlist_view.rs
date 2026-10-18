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
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::error;

use crate::sync::RecoveredMutex;
use crate::{
    library::{
        db::{self, PlaylistTrackRow, PlaylistTrackSortMethod},
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
        library::track_item::{
            ArtistNameVisibility, TrackItem, TrackItemLeftField, TrackItemOptions,
        },
        library_access::LibraryAccess,
        models::{LIKED_SONGS_PLAYLIST_ID, Models, PlaylistEvent},
        theme::Theme,
        util::{create_or_retrieve_view, prune_views, reduced_motion_enabled},
    },
};

use super::track_item::TrackPlaylistInfo;
use crate::ui::design::ICON_LG;

actions!(playlist, [Export, Import]);

// 行高 + 边框
const PLAYLIST_ITEM_HEIGHT: f32 = 40.0;

/// 预取带宽：当可见行中心移动超过该行数的一半时，视图就在异步运行时上批量预取
/// `center ± ROW_PREFETCH_PAD` 的整行，写入视图本地的行缓存，于是新建的行命中
/// 缓存，而不是每行一次 UI 线程 `block_on` DB 查询。
const ROW_PREFETCH_PAD: usize = 256;

/// 行预取缓存的上界（简单 FIFO，不依赖 LRU）：两个 ±256 行的完整预取窗口再加余量。
const ROW_CACHE_CAPACITY: usize = 1024;

/// 预取到的歌单行。`generation` 用于使在途预取任务失效：`reload` 清空缓存并自增
/// 世代号，因此 reload 之前启动的任务绝不可能把 reload 之前的行写回。
/// 形状与 `library::types::table` 里的表格行缓存相同，但刻意做成视图本地的。
struct PrefetchedRows {
    generation: u64,
    order: VecDeque<i64>,
    rows: FxHashMap<i64, Arc<Track>>,
}

type PrefetchedRowsHandle = Arc<Mutex<PrefetchedRows>>;

/// 缓存行查找；`Mutex` 只持有极短的临界区。
fn prefetched_track(cache: &PrefetchedRowsHandle, track_id: i64) -> Option<Arc<Track>> {
    cache.lock_recovered().rows.get(&track_id).cloned()
}

/// FIFO 有界插入；缓存世代号在预取中途发生变化（reload 清空了缓存）时返回 false，
/// 据此告诉预取任务别再写陈旧行。
fn insert_prefetched(cache: &PrefetchedRowsHandle, track: Arc<Track>, generation: u64) -> bool {
    let mut cache = cache.lock_recovered();
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

/// 从 UI 线程兜底路径做同一次插入：渲染与事件处理跑在同一线程，不会与 reload
/// 交错，因此读当前世代号再按它插入，绝不可能与缓存被清空构成竞态。
fn insert_prefetched_current(cache: &PrefetchedRowsHandle, track: Arc<Track>) {
    let generation = cache.lock_recovered().generation;
    insert_prefetched(cache, track, generation);
}

/// 丢弃预取到的行并使在途预取任务失效。每次 reload 都执行：曲目列表
/// （连同每一个 track_id）可能在缓存行的脚下发生变化。
fn clear_prefetched_rows(cache: &PrefetchedRowsHandle) {
    let mut cache = cache.lock_recovered();
    cache.rows.clear();
    cache.order.clear();
    cache.generation = cache.generation.wrapping_add(1);
}

/// 可见中心相比上次调度移动了半个预取带宽时，安排一次后台行预取。`state` 记录
/// （缓存世代号，可见中心），因此静止的列表每帧只花一次 `Cell` 读 + 一次无竞争的
/// 锁。一旦安排，就在异步运行时上取回 `center ± ROW_PREFETCH_PAD` 的整行写入视图的
/// 行缓存（每次 reload 清空），把逐行的 `cx.get_track_by_id` UI 线程 `block_on`
/// 变成超出 keep-around 带的那些行的缓存命中。形状与表格的 `schedule_row_prefetch` 相同。
fn schedule_row_prefetch(
    state: &Rc<Cell<(u64, usize)>>,
    rows_cache: &PrefetchedRowsHandle,
    items: &[PlaylistTrackRow],
    center: usize,
    cx: &App,
) {
    let generation = rows_cache.lock_recovered().generation;
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

    // 丢弃 JoinHandle 即让任务脱离托管
    #[allow(clippy::let_underscore_future)]
    let _ = crate::RUNTIME.spawn(async move {
        // 跳过缓存已持有的行：重叠窗口保持廉价；重复 id 去重，
        // 让同一首曲目列两次也只付一次取回
        let mut seen = FxHashSet::default();
        let mut to_fetch = Vec::new();
        for track_id in track_ids {
            if seen.insert(track_id) && prefetched_track(&rows_cache, track_id).is_none() {
                to_fetch.push(track_id);
            }
        }

        // 位置参数分块绑定，使任何单条语句都不超过后端的宿主参数上限。
        const SQL_BIND_CHUNK: usize = 900;
        for chunk in to_fetch.chunks(SQL_BIND_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                include_str!("../../../queries/library/find_tracks_by_ids.sql"),
                placeholders
            );
            let mut query = sqlx::query_as::<_, Track>(sqlx::AssertSqlSafe(sql));
            for track_id in chunk {
                query = query.bind(track_id);
            }

            match query.fetch_all(&pool).await {
                Ok(fetched) => {
                    let mut fetched: FxHashMap<i64, Track> =
                        fetched.into_iter().map(|track| (track.id, track)).collect();
                    // 按窗口顺序写回，让 FIFO 缓存保持旧的逐行预取的逐出顺序
                    for track_id in chunk {
                        let Some(track) = fetched.remove(track_id) else {
                            continue;
                        };
                        if !insert_prefetched(&rows_cache, Arc::new(track), generation) {
                            // 缓存已被清空（reload）：停止写陈旧行
                            return;
                        }
                    }
                    if !fetched.is_empty() {
                        tracing::debug!(count = fetched.len(), "playlist row prefetch missed");
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        count = chunk.len(),
                        error = %err,
                        "playlist row prefetch failed"
                    );
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

/// 歌单曲目行的包装组件，外加拖放支持
pub struct PlaylistTrackItem {
    track_item: Entity<TrackItem>,
    idx: usize,
    playlist_item_id: i64,
    track_title: SharedString,
    drag_drop_manager: Entity<DragDropListManager>,
    /// 拖拽负载预先构造一次；`on_drag` 每帧按值取走它，所以 render 只 clone 这个小
    /// 结构体，不再重跑 `from_track`（那是每帧一次 `PathBuf` 分配）。
    /// 仅在允许重排时为 `Some`。
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
        cx.new(|_| {
            // 拖拽负载在构造时预先建好：`on_drag` 每帧按值取走它，所以 render 只
            // clone 这个小结构体，不再重跑 `from_track`（每帧一次 `PathBuf` 分配）
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

        // 负载在构造时预先建好；`on_drag` 每帧按值取走它，所以 render 只 clone 小结构体
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
    /// 汇总行在构造与 reload 时由 `playlist` 推导；render 只 clone 它
    /// （预先算好的字段，与 `ReleaseView` 同法）。
    collection_summary: SharedString,
    playlist_track_ids: Arc<Vec<PlaylistTrackRow>>,
    /// 每次加载都自增，使晚于新一轮完成的慢加载不能把它覆盖（与
    /// `Table::reload_rows` 同一个守卫）。
    load_generation: u64,
    views: Entity<FxHashMap<usize, Entity<PlaylistTrackItem>>>,
    render_counter: Entity<usize>,
    focus_handle: FocusHandle,
    first_render: bool,
    scroll_handle: UniformListScrollHandle,
    drag_drop_manager: Entity<DragDropListManager>,
    list_id: gpui::ElementId,
    sort_method: PlaylistTrackSortMethod,
    /// 在异步运行时上填充的预取整行（见 `schedule_row_prefetch`）；构造行时先读它，
    /// 再退回到逐行的 UI 线程 `block_on` 查询。每次 reload 清空。
    track_row_cache: PrefetchedRowsHandle,
    /// 上一次安排的行预取：（缓存世代号，可见中心）。见 `schedule_row_prefetch`。
    prefetch_state: Rc<Cell<(u64, usize)>>,
}

/// 后台加载落地前显示的占位：id 此刻已知，而「我喜欢的」歌单是固定的系统歌单
/// （models::LIKED_SONGS_PLAYLIST_ID），所以它的头部可以立刻正确渲染。
/// 其余字段等 `reload` 完成后填入。
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

            let placeholder = Arc::new(placeholder_playlist(playlist_id));
            let collection_summary =
                format_collection_summary(placeholder.track_count, placeholder.total_duration);

            let mut this = Self {
                playlist: placeholder,
                collection_summary,
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
            // 打开视图时在运行时上加载歌单头部 + 其排序后的曲目列表（见 reload）；
            // 结果落地前视图显示占位头部与空列表。
            this.reload(cx);
            this
        })
    }

    /// 在后台运行时上加载歌单头部及其排序后的曲目列表：打开视图（以及每一次
    /// PlaylistUpdated 触发的 reload / 排序变更）过去会在「歌单聚合 + 全表 JOIN」上
    /// 阻塞 UI 线程。新行落地前视图继续显示原有数据；世代守卫会丢弃被更新一轮
    /// 加载取代的结果。
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
                    let tracks = db::list_playlist_tracks_sorted(&pool, playlist_id, sort_method)
                        .await
                        .ok();
                    (playlist, tracks)
                })
                .await
                .unwrap_or((None, None));

            let _ = this.update(cx, |this, cx| {
                // 更新的加载已取代这一轮：本结果陈旧。
                if this.load_generation != generation {
                    return;
                }

                if let Some(playlist) = loaded.0 {
                    this.collection_summary =
                        format_collection_summary(playlist.track_count, playlist.total_duration);
                    this.playlist = playlist;
                }
                if let Some(tracks) = loaded.1 {
                    this.playlist_track_ids = tracks;
                }
                this.views = cx.new(|_| FxHashMap::default());
                this.render_counter = cx.new(|_| 0);
                // 缓存行绝不得比 reload 活得更久：曲目列表（连同每一个 track_id）
                // 可能在它们脚下变化；世代号自增同时使在途预取任务失效
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
        // 重排后的行在运行时上加载（见 reload）；落地前视图保持原有顺序。
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
            // 一次批量调用完成追加与可选搬移（单事务），取代逐首
            // add_playlist_item + 逐首 move_playlist_item 的 N 次事务
            let task = crate::RUNTIME.spawn(async move {
                db::add_playlist_items(&pool, playlist_id, &track_ids, target_position).await
            });

            match task.await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    error!("could not add tracks to playlist: {err:?}");
                    return;
                }
                Err(err) => {
                    error!("add tracks to playlist task panicked: {err:?}");
                    return;
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
        let collection_summary = self.collection_summary.clone();

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
                                                {
                                                    // 点击时按屏上快照建队列（所见即所得）：
                                                    // 播放的顺序正是用户看到的顺序，而不是在
                                                    // UI 线程 block_on 下重跑排序聚合查询。
                                                    // PlaylistUpdated 时的 reload() 把快照的
                                                    // 陈旧窗口压得很小；下面的阻塞兜底纯属防御
                                                    // （空列表时按钮不带 on_click 渲染，无法触发）。
                                                    let snapshot = items_clone.clone();
                                                    move |cx| {
                                                        let playlist_tracks =
                                                            if snapshot.is_empty() {
                                                                cx.list_playlist_tracks_sorted(
                                                                    playlist_id,
                                                                    current_sort,
                                                                )
                                                                .unwrap_or_default()
                                                            } else {
                                                                snapshot.clone()
                                                            };

                                                        playlist_tracks
                                                            .iter()
                                                            .map(|row| {
                                                                QueueItemData::new(
                                                                    row.location.clone().into(),
                                                                    Some(row.track_id),
                                                                    Some(row.album_id),
                                                                )
                                                            })
                                                            .collect()
                                                    }
                                                }
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

                                    let reduced_motion = reduced_motion_enabled(cx);
                                    let scrolled = handle_track_drag_move(
                                        this.drag_drop_manager.clone(),
                                        scroll_handle,
                                        event,
                                        item_count,
                                        cx,
                                        reduced_motion,
                                    );

                                    if scrolled {
                                        // 有守卫，最多挂起一条帧链
                                        request_edge_scroll(
                                            this.drag_drop_manager.clone(),
                                            this.scroll_handle.clone().into(),
                                            window,
                                            cx,
                                        );
                                    }

                                    // 只在可见内容真的移动时重绘：drag move 以鼠标
                                    // 上报频率触发，无条件 notify 会让每次移动都把整
                                    // 个视图重渲染一遍
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

                                    let reduced_motion = reduced_motion_enabled(cx);
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

                                    // 与上面的曲目处理器采用同一套门控
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
                                        .is_some_and(|id| *id == this.list_id);

                                    if is_internal && this.is_custom_sort() {
                                        let playlist_track_ids = this.playlist_track_ids.clone();
                                        let playlist_id = this.playlist.id;

                                        handle_track_drop(
                                            this.drag_drop_manager.clone(),
                                            drag_data,
                                            cx,
                                            |from_idx, to_idx, cx| {
                                                // 拖拽起点到落点之间列表可能已 reload
                                                // （异步 PlaylistUpdated）：陈旧的源索
                                                // 引在这里绝不能 panic。
                                                let Some(source_row) =
                                                    playlist_track_ids.get(from_idx)
                                                else {
                                                    return;
                                                };
                                                let item_id = source_row.playlist_item_id;

                                                let new_position = if to_idx < playlist_track_ids.len() {
                                                    let target_item_id = playlist_track_ids[to_idx].playlist_item_id;
                                                    // 条目缺失（并发移除）时跳过这次移动，
                                                    // 而不是让处理器 panic
                                                    let target_item = match cx.get_playlist_item(target_item_id) {
                                                        Ok(item) => item,
                                                        Err(e) => {
                                                            error!("Failed to move playlist item {}: {}", item_id, e);
                                                            return;
                                                        }
                                                    };
                                                    target_item.position
                                                } else if let Some(last_row) = playlist_track_ids.last() {
                                                    // 最后一行（to_idx 越过末尾）
                                                    let last_item = match cx.get_playlist_item(last_row.playlist_item_id) {
                                                        Ok(item) => item,
                                                        Err(e) => {
                                                            error!("Failed to move playlist item {}: {}", last_row.playlist_item_id, e);
                                                            return;
                                                        }
                                                    };
                                                    last_item.position + 1
                                                } else {
                                                    // 快照为空：没有可参照的移动目标
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

                                    // 让行预取保持在可见窗口前一个带宽，新行即可命中行缓存
                                    schedule_row_prefetch(
                                        &prefetch_state,
                                        &track_row_cache,
                                        &items_clone,
                                        center,
                                        cx,
                                    );

                                    // 每次窗口重建只做一次 prune（与 table.rs:827 同一
                                    // 修法）：逐行调用是 O(可见行数×缓存键数) 的键迭代，
                                    // 且每次调用各自分配 `to_remove` Vec。
                                    if !is_templ_render {
                                        prune_views(&views_model, &render_counter, center, cx);
                                    }

                                    let items = &items_clone[range];

                                    items
                                        .iter()
                                        .enumerate()
                                        .map(|(idx, item)| {
                                            let idx = idx + start;

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
                                                        // 下面两条行路径装配的都是同一对
                                                        // TrackItem/PlaylistTrackItem，
                                                        // 只是携带的数据不同
                                                        let make_row = |cx: &mut App,
                                                                        track: Track,
                                                                        track_title: SharedString,
                                                                        album_id: Option<i64>,
                                                                        track_path: std::path::PathBuf| {
                                                            let track_item = TrackItem::new(
                                                                cx,
                                                                track,
                                                                ArtistNameVisibility::Always,
                                                                TrackItemLeftField::Art,
                                                                Some(TrackPlaylistInfo {
                                                                    id: pl_id,
                                                                    item_id: playlist_item_id,
                                                                }),
                                                                None, // max_track_num：Art 左栏字段用不到
                                                                None, // queue_context：歌单改用 pl_id
                                                                TrackItemOptions {
                                                                    is_start: false,
                                                                    // vinyl_numbering：歌单不适用
                                                                    vinyl_numbering: false,
                                                                    show_go_to_album: true,
                                                                    show_go_to_artist: true,
                                                                },
                                                            );

                                                            PlaylistTrackItem::new(
                                                                cx,
                                                                track_item,
                                                                idx,
                                                                playlist_item_id,
                                                                track_title,
                                                                drag_drop_manager.clone(),
                                                                list_id.clone(),
                                                                track_id,
                                                                album_id,
                                                                track_path,
                                                                is_custom_sort,
                                                            )
                                                        };

                                                        // 先查预取行：命中就避免下面的 UI 线程
                                                        // `block_on`（超出 keep-around 带的每个
                                                        // 新建行都要付一次）
                                                        let track = match prefetched_track(
                                                            &track_row_cache,
                                                            track_id,
                                                        ) {
                                                            Some(track) => track,
                                                            None => {
                                                                match cx.get_track_by_id(track_id) {
                                                                    Ok(track) => {
                                                                        // 把兜底查询的结果也写入缓存，
                                                                        // 于是重新进入的行命中缓存，
                                                                        // 不再重跑那个阻塞查询
                                                                        insert_prefetched_current(
                                                                            &track_row_cache,
                                                                            track.clone(),
                                                                        );
                                                                        track
                                                                    }
                                                                    // 曲目在歌单快照与本视图构造之间
                                                                    // 从库里消失了：渲染一个等高的空
                                                                    // 占位行，而不是 panic。
                                                                    Err(_) => {
                                                                        return make_row(
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
                                                                            SharedString::default(),
                                                                            None,
                                                                            std::path::PathBuf::new(),
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        };
                                                        let track_title: SharedString =
                                                            track.title.clone().0;
                                                        let track_path = track.location.clone();
                                                        let album_id = track.album_id;

                                                        make_row(
                                                            cx,
                                                            (*track).clone(),
                                                            track_title,
                                                            album_id,
                                                            track_path,
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
    let playlist_tracks = cx.list_playlist_tracks(playlist_id).unwrap_or_default();

    playlist_tracks
        .iter()
        .map(|row| {
            QueueItemData::new(
                row.location.clone().into(),
                Some(row.track_id),
                Some(row.album_id),
            )
        })
        .collect()
}
