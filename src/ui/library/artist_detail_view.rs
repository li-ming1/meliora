use std::{path::Path, rc::Rc, sync::Arc};

use cntp_i18n::tr;
use gpui::*;
use prelude::FluentBuilder;
use rustc_hash::FxHashMap;

use crate::{
    library::{
        db::{self, LikedTrackSortMethod},
        types::{Album, DBString, Track, table::AlbumColumn},
    },
    playback::thread::PlaybackState,
    ui::{
        app::Pool,
        caching::meliora_cache,
        components::{
            button::{ButtonSize, button},
            dropdown::dropdown,
            icons::{SORT_ASCENDING, SORT_DESCENDING, icon},
            playback_controls::playback_controls,
            scrollbar::floating_scrollbar,
            table::{
                grid_item::GridItem,
                table_data::{GridContext, capped_table_width},
            },
            tooltip::build_tooltip,
            uniform_grid::{DEFAULT_ITEM_EXTRA_HEIGHT, uniform_grid},
        },
        library::{
            context_menus::{AlbumContextMenuContext, queue_items_from_tracks},
            detail_close_button,
            track_item::{ArtistNameVisibility, TrackItem, TrackItemLeftField, TrackItemOptions},
        },
        library_access::LibraryAccess,
        models::{
            LIKED_SONGS_PLAYLIST_ID, Models, PlaybackInfo, PlaylistEvent,
            is_track_available_snapshot,
        },
        theme::Theme,
        util::prune_views,
    },
};

use super::ViewSwitchMessage;
use crate::ui::design::ICON_MD;

type GridHandler = dyn Fn(&mut App, &(u32, String)) + 'static;

/// 逐曲目的可用性，每次加载曲目列表时按后台快照算一次：每帧都对所有文件重新 stat
/// 代价高得离谱，而加载时每行一次 `path.exists()` 又是 UI 线程上的系统调用——
/// 两者都由 `reload_availability` 在线程外维护的 `available_tracks` 集合取代。
fn availability_map(cx: &App, tracks: &[Track]) -> Arc<Vec<bool>> {
    Arc::new(
        tracks
            .iter()
            .map(|track| is_track_available_snapshot(cx, track))
            .collect(),
    )
}

/// 每首曲目构造一个 `TrackItem` 行。每行都携带完整的来源列表（供队列上下文使用），
/// 艺术家名与曲目自身相同时隐藏；其余标志位对两个艺术家曲目列表完全一致。
fn track_items(
    cx: &mut App,
    tracks: &Arc<Vec<Track>>,
    artist_name: Option<&DBString>,
) -> Vec<Entity<TrackItem>> {
    tracks
        .iter()
        .map(|track| {
            TrackItem::new(
                cx,
                track.clone(),
                ArtistNameVisibility::OnlyIfDifferent(artist_name.cloned()),
                TrackItemLeftField::Art,
                None,
                None,
                Some(tracks.clone()),
                TrackItemOptions {
                    is_start: false,
                    vinyl_numbering: false,
                    show_go_to_album: false,
                    show_go_to_artist: false,
                },
            )
        })
        .collect()
}

/// 当前播放路径属于 `tracks` 时为真（可用性快照判定为不可用的行不算在播）。
fn contains_current_track(
    current_path: Option<&Path>,
    tracks: &[Track],
    available: &[bool],
) -> bool {
    current_path.is_some_and(|current| {
        tracks
            .iter()
            .zip(available.iter())
            .any(|(track, &is_available)| is_available && current == track.location.as_path())
    })
}

/// uniform_list 需要固定行高；40px 正是非虚拟化的包装（`div().h(px(40.0))`）
/// 原本就把每个 TrackItem 钉住的高度。
const TRACK_ROW_HEIGHT: f32 = 40.0;

/// uniform_list 只有在自身视口小于内容时才裁剪行，所以列表在这个行数以内保持自然
/// 高度，超出后内部滚动，而不是把页面滚动撑长。
const MAX_VISIBLE_TRACK_ROWS: usize = 12;

/// 专辑网格在这个条目数以内保持自然高度、随页面滚动（旧行为）；超过后改为
/// 有界高度 + 自身滚动句柄的内部滚动。auto_height 模式下 uniform_grid 每次
/// 渲染都要 layout **全部**瓦片——合辑 / Various Artists 动辄数百张专辑，
/// 页面每滚动一帧就是数百次瓦片 layout，且 prune 在 auto_height 下剪不掉
/// 任何行（`render_counter` 被最后一格顶到末尾），GridItem 实体同样全量常驻。
/// 阈值以下两种模式视觉无差，故取一个普通艺人远达不到的值，把行为变化限制
/// 在真正昂贵的合辑场景。
const ALBUM_GRID_MAX_INLINE_ITEMS: usize = 24;

/// 有界模式下的可见行数。容器高度按 `行数 × (min_item_width + 行高附加)`
/// 估算：窗口更宽时单元被拉伸、行更高，可见行数只会变少（多滚一点），不会
/// 出现空白区；窗口更窄时单元缩到 min_item_width 以下则反之。
const ALBUM_GRID_MAX_VISIBLE_ROWS: usize = 2;

pub struct ArtistDetailView {
    artist_id: i64,
    artist_name: Option<DBString>,
    album_ids: Arc<Vec<(u32, String)>>,
    liked_track_items: Vec<Entity<TrackItem>>,
    standalone_track_items: Vec<Entity<TrackItem>>,
    all_tracks: Arc<Vec<Track>>,
    all_tracks_available: Arc<Vec<bool>>,
    liked_tracks: Arc<Vec<Track>>,
    liked_tracks_available: Arc<Vec<bool>>,
    standalone_tracks: Arc<Vec<Track>>,
    standalone_tracks_available: Arc<Vec<bool>>,
    scroll_handle: ScrollHandle,
    /// 两个虚拟化曲目列表的滚动状态；与视图同生命周期保留，因此重新排序时读者的
    /// 位置不丢，做法同 PlaylistView。
    liked_scroll_handle: UniformListScrollHandle,
    standalone_scroll_handle: UniformListScrollHandle,
    /// 专辑网格超过 `ALBUM_GRID_MAX_INLINE_ITEMS` 后改用内部滚动时的滚动状态。
    /// 详情页不缓存（每次导航新建实体，见 `library.rs` 的 Artist 分支），故无需
    /// 在换艺人时重置。
    album_scroll_handle: UniformListScrollHandle,
    grid_views: Entity<FxHashMap<usize, Entity<GridItem<Album, AlbumColumn>>>>,
    grid_render_counter: Entity<usize>,
    nav_model: Entity<super::NavigationHistory>,
    liked_sort: LikedTrackSortMethod,
    standalone_sort: LikedTrackSortMethod,
    /// 每次重载「已点赞曲目」都自增，使晚于新一轮完成的慢加载不能覆盖它
    /// （与 `Table::reload_rows` 同一个守卫）。
    liked_load_generation: u64,
    /// 整视图加载时自增，使晚于新一轮完成的慢加载不能覆盖它
    /// （与 `Table::reload_rows` 同一个守卫）。
    load_generation: u64,
}

impl ArtistDetailView {
    pub(super) fn new(
        cx: &mut App,
        artist_id: i64,
        nav_model: Entity<super::NavigationHistory>,
    ) -> Entity<Self> {
        let view: Entity<Self> = cx.new(|cx| {
            let liked_sort = *cx.global::<Models>().liked_tracks_sort_method.read(cx);

            let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

            cx.subscribe(&playlist_tracker, move |this: &mut Self, _, ev, cx| {
                if let PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID) = ev {
                    this.reload_liked_tracks(cx);
                }
            })
            .detach();

            let grid_views = cx.new(|_| FxHashMap::default());
            let grid_render_counter = cx.new(|_| 0usize);

            let mut this = Self {
                artist_id,
                artist_name: None,
                album_ids: Arc::new(Vec::new()),
                liked_track_items: Vec::new(),
                standalone_track_items: Vec::new(),
                all_tracks: Arc::new(Vec::new()),
                all_tracks_available: Arc::new(Vec::new()),
                liked_tracks: Arc::new(Vec::new()),
                liked_tracks_available: Arc::new(Vec::new()),
                standalone_tracks: Arc::new(Vec::new()),
                standalone_tracks_available: Arc::new(Vec::new()),
                scroll_handle: ScrollHandle::new(),
                liked_scroll_handle: UniformListScrollHandle::new(),
                standalone_scroll_handle: UniformListScrollHandle::new(),
                album_scroll_handle: UniformListScrollHandle::new(),
                grid_views,
                grid_render_counter,
                nav_model: nav_model.clone(),
                liked_sort,
                standalone_sort: LikedTrackSortMethod::ReleaseOrder,
                liked_load_generation: 0,
                load_generation: 0,
            };
            // 打开视图时在运行时上加载艺术家头部、专辑网格与三个曲目列表（见 reload）；
            // 落地前视图显示空页。
            this.reload(cx);
            this
        });

        view
    }

    /// 在后台运行时上加载艺术家头部、专辑网格与三个曲目列表：打开视图过去会连着
    /// 五条查询阻塞 UI 线程。加载落地前视图保持空白；世代守卫丢弃被更新一轮加载
    /// 取代的结果，且已点赞列表额外受 `liked_load_generation` 把关，因此与本轮赛跑的
    /// 点赞/取消赞或已点赞排序重载，绝不会是这个更慢的整视图加载覆盖掉的。
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let liked_generation = self.liked_load_generation;
        let artist_id = self.artist_id;
        let liked_sort = self.liked_sort;
        let standalone_sort = self.standalone_sort;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let loaded = crate::RUNTIME
                .spawn(async move {
                    let artist = db::get_artist_by_id(&pool, artist_id).await.ok();
                    let album_ids = db::list_albums_by_artist(&pool, artist_id)
                        .await
                        .unwrap_or_default();
                    let all_tracks = db::list_all_tracks_by_artist(&pool, artist_id)
                        .await
                        .unwrap_or_else(|_| Arc::new(Vec::new()));
                    let liked_tracks =
                        db::list_liked_tracks_by_artist(&pool, artist_id, liked_sort)
                            .await
                            .unwrap_or_else(|_| Arc::new(Vec::new()));
                    let standalone_tracks =
                        db::list_standalone_tracks_by_artist(&pool, artist_id, standalone_sort)
                            .await
                            .unwrap_or_else(|_| Arc::new(Vec::new()));
                    (
                        artist,
                        album_ids,
                        all_tracks,
                        liked_tracks,
                        standalone_tracks,
                    )
                })
                .await
                .unwrap_or((
                    None,
                    Vec::new(),
                    Arc::new(Vec::new()),
                    Arc::new(Vec::new()),
                    Arc::new(Vec::new()),
                ));

            let _ = this.update(cx, |this, cx| {
                // 更新的加载已取代这一轮：本结果陈旧。
                if this.load_generation != generation {
                    return;
                }

                let (artist, album_ids, all_tracks, liked_tracks, standalone_tracks) = loaded;
                this.artist_name = artist.and_then(|a| a.name.clone());
                this.album_ids = Arc::new(album_ids);
                this.all_tracks = all_tracks;
                this.all_tracks_available = availability_map(cx, &this.all_tracks);
                this.set_standalone_tracks(standalone_tracks, cx);
                // 在本轮整视图加载在途期间启动的已点赞重载才拥有已点赞列表：这里跳过
                // 陈旧快照，让它自己的世代守卫交付更新的那一份。
                if this.liked_load_generation == liked_generation {
                    this.set_liked_tracks(liked_tracks, cx);
                }

                cx.notify();
            });
        })
        .detach();
    }

    /// 在后台运行时上重载艺术家的已点赞曲目列表：每一次点赞/取消赞（`PlaylistUpdated`）
    /// 与已点赞排序变更过去都在 UI 线程上同步跑这条查询。内存里的 liked-ids 缓存这里
    /// 不够用——已点赞的顺序（发行顺序、最近添加）活在 SQL 里——所以查询本身必须移出线程。
    /// 世代守卫丢弃被更新一次重载取代的结果。
    fn reload_liked_tracks(&mut self, cx: &mut Context<Self>) {
        self.liked_load_generation = self.liked_load_generation.wrapping_add(1);
        let generation = self.liked_load_generation;
        let artist_id = self.artist_id;
        let sort = self.liked_sort;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let liked_tracks = crate::RUNTIME
                .spawn(async move {
                    db::list_liked_tracks_by_artist(&pool, artist_id, sort)
                        .await
                        .unwrap_or_else(|_| Arc::new(Vec::new()))
                })
                .await
                .unwrap_or_else(|_| Arc::new(Vec::new()));

            let _ = this.update(cx, |this, cx| {
                // 更新的一次重载已取代这一轮：本结果陈旧。
                if this.liked_load_generation != generation {
                    return;
                }
                this.set_liked_tracks(liked_tracks, cx);
            });
        })
        .detach();
    }

    pub fn update_liked_sort(&mut self, sort_method: LikedTrackSortMethod, cx: &mut Context<Self>) {
        let current_descending = Self::is_descending(self.liked_sort);
        let next_sort = Self::apply_direction(Self::base_sort(sort_method), current_descending);

        if self.liked_sort == next_sort {
            return;
        }

        self.liked_sort = next_sort;
        self.sync_sort_with_model(cx);
        // 重排后的列表在运行时上加载（见 reload_liked_tracks）。
        self.reload_liked_tracks(cx);
    }

    fn set_liked_tracks(&mut self, liked_tracks: Arc<Vec<Track>>, cx: &mut Context<Self>) {
        self.liked_tracks = liked_tracks;
        self.liked_tracks_available = availability_map(cx, &self.liked_tracks);
        self.liked_track_items = track_items(cx, &self.liked_tracks, self.artist_name.as_ref());

        cx.notify();
    }

    fn toggle_liked_sort_order(&mut self, cx: &mut Context<Self>) {
        self.liked_sort = Self::toggled_sort(self.liked_sort);
        self.sync_sort_with_model(cx);
        self.reload_liked_tracks(cx);
    }

    fn update_standalone_sort(
        &mut self,
        sort_method: LikedTrackSortMethod,
        cx: &mut Context<Self>,
    ) {
        let current_descending = Self::is_descending(self.standalone_sort);
        let next_sort = Self::apply_direction(Self::base_sort(sort_method), current_descending);

        if self.standalone_sort == next_sort {
            return;
        }

        self.standalone_sort = next_sort;

        let standalone_tracks = cx
            .list_standalone_tracks_by_artist(self.artist_id, self.standalone_sort)
            .unwrap_or_else(|_| Arc::new(Vec::new()));

        self.set_standalone_tracks(standalone_tracks, cx);
    }

    fn set_standalone_tracks(
        &mut self,
        standalone_tracks: Arc<Vec<Track>>,
        cx: &mut Context<Self>,
    ) {
        self.standalone_tracks = standalone_tracks;
        self.standalone_tracks_available = availability_map(cx, &self.standalone_tracks);
        self.standalone_track_items =
            track_items(cx, &self.standalone_tracks, self.artist_name.as_ref());

        cx.notify();
    }

    fn toggle_standalone_sort_order(&mut self, cx: &mut Context<Self>) {
        self.standalone_sort = Self::toggled_sort(self.standalone_sort);
        let standalone_tracks = cx
            .list_standalone_tracks_by_artist(self.artist_id, self.standalone_sort)
            .unwrap_or_else(|_| Arc::new(Vec::new()));
        self.set_standalone_tracks(standalone_tracks, cx);
    }

    fn base_sort(sort_method: LikedTrackSortMethod) -> LikedTrackSortMethod {
        match sort_method {
            LikedTrackSortMethod::TitleAsc | LikedTrackSortMethod::TitleDesc => {
                LikedTrackSortMethod::TitleAsc
            }
            LikedTrackSortMethod::ReleaseOrder | LikedTrackSortMethod::ReleaseOrderDesc => {
                LikedTrackSortMethod::ReleaseOrder
            }
            LikedTrackSortMethod::RecentlyAdded | LikedTrackSortMethod::RecentlyAddedAsc => {
                LikedTrackSortMethod::RecentlyAdded
            }
        }
    }

    fn apply_direction(
        base_sort_method: LikedTrackSortMethod,
        descending: bool,
    ) -> LikedTrackSortMethod {
        match base_sort_method {
            LikedTrackSortMethod::TitleAsc | LikedTrackSortMethod::TitleDesc => {
                if descending {
                    LikedTrackSortMethod::TitleDesc
                } else {
                    LikedTrackSortMethod::TitleAsc
                }
            }
            LikedTrackSortMethod::ReleaseOrder | LikedTrackSortMethod::ReleaseOrderDesc => {
                if descending {
                    LikedTrackSortMethod::ReleaseOrderDesc
                } else {
                    LikedTrackSortMethod::ReleaseOrder
                }
            }
            LikedTrackSortMethod::RecentlyAdded | LikedTrackSortMethod::RecentlyAddedAsc => {
                if descending {
                    LikedTrackSortMethod::RecentlyAdded
                } else {
                    LikedTrackSortMethod::RecentlyAddedAsc
                }
            }
        }
    }

    fn is_descending(sort_method: LikedTrackSortMethod) -> bool {
        matches!(
            sort_method,
            LikedTrackSortMethod::TitleDesc
                | LikedTrackSortMethod::ReleaseOrderDesc
                | LikedTrackSortMethod::RecentlyAdded
        )
    }

    fn toggled_sort(sort_method: LikedTrackSortMethod) -> LikedTrackSortMethod {
        match sort_method {
            LikedTrackSortMethod::TitleAsc => LikedTrackSortMethod::TitleDesc,
            LikedTrackSortMethod::TitleDesc => LikedTrackSortMethod::TitleAsc,
            LikedTrackSortMethod::ReleaseOrder => LikedTrackSortMethod::ReleaseOrderDesc,
            LikedTrackSortMethod::ReleaseOrderDesc => LikedTrackSortMethod::ReleaseOrder,
            LikedTrackSortMethod::RecentlyAdded => LikedTrackSortMethod::RecentlyAddedAsc,
            LikedTrackSortMethod::RecentlyAddedAsc => LikedTrackSortMethod::RecentlyAdded,
        }
    }

    fn sync_sort_with_model(&self, cx: &mut Context<Self>) {
        let liked_tracks_sort_method = cx.global::<Models>().liked_tracks_sort_method.clone();
        liked_tracks_sort_method.update(cx, |value, _| *value = self.liked_sort);
    }
}

impl Render for ArtistDetailView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let entity = cx.entity();
        let standalone_entity = entity.clone();
        let liked_list_entity = entity.clone();
        let standalone_list_entity = entity.clone();

        let scroll_handle = self.scroll_handle.clone();
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();
        let two_column = settings.interface.two_column_library;
        let grid_min_item_width = crate::settings::interface::clamp_grid_min_item_width(
            settings.interface.grid_min_item_width,
        );

        let album_count = self.album_ids.len();
        let album_ids = self.album_ids.clone();
        // 专辑网格的两种形态（见 ALBUM_GRID_MAX_INLINE_ITEMS）：条目少时自然
        // 高度随页面滚动；超过阈值后有界高度 + 自身滚动句柄。高度估算用与
        // uniform_grid 同一套常量，避免两边漂移。
        let album_grid_bounded = album_count > ALBUM_GRID_MAX_INLINE_ITEMS;
        let album_grid_height =
            px(ALBUM_GRID_MAX_VISIBLE_ROWS as f32
                * (grid_min_item_width + DEFAULT_ITEM_EXTRA_HEIGHT));
        let album_grid_scroll_handle = self.album_scroll_handle.clone();
        let grid_views_model = self.grid_views.clone();
        let grid_render_counter = self.grid_render_counter.clone();
        // 网格逐单元回调里只在**本帧第一个单元**做 prune（`ui/components/table.rs`
        // 与 `palette/finder.rs` 的 `render` 里同一修法）：uniform_grid 的闭包按单元逐个调用，无 range 可算 center，
        // 所以用门控；逐单元 prune 是 O(可见单元×缓存键数) 的键迭代。
        let prune_gate = std::cell::Cell::new(false);
        let nav_model = self.nav_model.clone();

        let is_playing =
            cx.global::<PlaybackInfo>().playback_state.read(cx) == &PlaybackState::Playing;

        // 当前曲目的路径只借用一次：下面三次归属判断只拿每行自身的 location 比较
        // （不 clone），可用性则来自预先算好的逐曲目映射表。
        let current_path = cx
            .global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .map(|current| current.get_path().as_path());

        let current_track_in_artist =
            contains_current_track(current_path, &self.all_tracks, &self.all_tracks_available);
        let has_available_artist_tracks = self.all_tracks_available.iter().any(|&a| a);

        let current_track_in_liked = contains_current_track(
            current_path,
            &self.liked_tracks,
            &self.liked_tracks_available,
        );
        let has_available_liked_tracks = self.liked_tracks_available.iter().any(|&a| a);

        let current_track_in_standalone = contains_current_track(
            current_path,
            &self.standalone_tracks,
            &self.standalone_tracks_available,
        );
        let has_available_standalone_tracks = self.standalone_tracks_available.iter().any(|&a| a);

        let liked_track_header =
            if !self.liked_track_items.is_empty() {
                Some(
                    div()
                        .border_t_1()
                        .border_color(theme.border_color)
                        .px(px(18.0))
                        .pt(px(10.0))
                        .pb(px(5.0))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .text_size(px(18.0))
                                .my_auto()
                                .child(tr!("ARTIST_LIKED_TRACKS", "Liked Tracks")),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_between()
                                .pb(px(13.0))
                                .child(playback_controls(
                                    "artist-liked",
                                    has_available_liked_tracks,
                                    current_track_in_liked,
                                    is_playing,
                                    {
                                        let liked_tracks = self.liked_tracks.clone();
                                        move |_cx| queue_items_from_tracks(&liked_tracks)
                                    },
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(12.0))
                                        .items_stretch()
                                        .child(
                                            button()
                                                .id("artist-liked-sort-direction-button")
                                                .size(ButtonSize::Large)
                                                .on_click(cx.listener(
                                                    |this: &mut ArtistDetailView, _, _, cx| {
                                                        this.toggle_liked_sort_order(cx);
                                                    },
                                                ))
                                                .child(
                                                    icon(if Self::is_descending(self.liked_sort) {
                                                        SORT_DESCENDING
                                                    } else {
                                                        SORT_ASCENDING
                                                    })
                                                    .text_color(theme.text_secondary)
                                                    .size(ICON_MD),
                                                )
                                                .tooltip(if Self::is_descending(self.liked_sort) {
                                                    build_tooltip(tr!(
                                                        "SORT_ASCENDING",
                                                        "Sort Ascending"
                                                    ))
                                                } else {
                                                    build_tooltip(tr!(
                                                        "SORT_DESCENDING",
                                                        "Sort Descending"
                                                    ))
                                                }),
                                        )
                                        .child(
                                            dropdown::<LikedTrackSortMethod>(
                                                "artist-liked-sort-dropdown",
                                            )
                                            .option(
                                                LikedTrackSortMethod::RecentlyAdded,
                                                tr!("SORT_RECENTLY_ADDED", "Recently Added"),
                                            )
                                            .option(
                                                LikedTrackSortMethod::TitleAsc,
                                                tr!("SORT_TITLE", "Title"),
                                            )
                                            .option(
                                                LikedTrackSortMethod::ReleaseOrder,
                                                tr!("SORT_RELEASE_ORDER", "Release Order"),
                                            )
                                            .selected(Self::base_sort(self.liked_sort))
                                            .w(px(200.0))
                                            .on_change(move |sort_method, _, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.update_liked_sort(*sort_method, cx);
                                                });
                                            }),
                                        ),
                                ),
                        ),
                )
            } else {
                None
            };

        let standalone_track_header = if !self.standalone_track_items.is_empty() {
            Some(
                div()
                    .border_t_1()
                    .border_color(theme.border_color)
                    .px(px(18.0))
                    .pt(px(10.0))
                    .pb(px(5.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(18.0))
                            .my_auto()
                            .child(tr!("TRACKS")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .pb(px(13.0))
                            .child(playback_controls(
                                "artist-standalone",
                                has_available_standalone_tracks,
                                current_track_in_standalone,
                                is_playing,
                                {
                                    let standalone_tracks = self.standalone_tracks.clone();
                                    move |_cx| queue_items_from_tracks(&standalone_tracks)
                                },
                            ))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(12.0))
                                    .items_stretch()
                                    .child(
                                        button()
                                            .id("artist-standalone-sort-direction-button")
                                            .size(ButtonSize::Large)
                                            .on_click(cx.listener(
                                                |this: &mut ArtistDetailView, _, _, cx| {
                                                    this.toggle_standalone_sort_order(cx);
                                                },
                                            ))
                                            .child(
                                                icon(
                                                    if Self::is_descending(self.standalone_sort) {
                                                        SORT_DESCENDING
                                                    } else {
                                                        SORT_ASCENDING
                                                    },
                                                )
                                                .text_color(theme.text_secondary)
                                                .size(ICON_MD),
                                            )
                                            .tooltip(
                                                if Self::is_descending(self.standalone_sort) {
                                                    build_tooltip(tr!("SORT_ASCENDING"))
                                                } else {
                                                    build_tooltip(tr!("SORT_DESCENDING"))
                                                },
                                            ),
                                    )
                                    .child(
                                        dropdown::<LikedTrackSortMethod>(
                                            "artist-standalone-sort-dropdown",
                                        )
                                        .option(
                                            LikedTrackSortMethod::RecentlyAdded,
                                            tr!("SORT_RECENTLY_ADDED"),
                                        )
                                        .option(LikedTrackSortMethod::TitleAsc, tr!("SORT_TITLE"))
                                        .option(
                                            LikedTrackSortMethod::ReleaseOrder,
                                            tr!("SORT_RELEASE_ORDER"),
                                        )
                                        .selected(Self::base_sort(self.standalone_sort))
                                        .w(px(200.0))
                                        .on_change(
                                            move |sort_method, _, cx| {
                                                standalone_entity.update(cx, |this, cx| {
                                                    this.update_standalone_sort(*sort_method, cx);
                                                });
                                            },
                                        ),
                                    ),
                            ),
                    ),
            )
        } else {
            None
        };

        // 虚拟化的曲目列表：高度设了上限，所以长列表内部滚动而不是把页面滚动撑长；
        // 行在 uniform_list 闭包内经实体读取（闭包比 render 的借用活得更久）。
        let liked_row_count = self.liked_track_items.len();
        let liked_list_height =
            px(TRACK_ROW_HEIGHT * liked_row_count.min(MAX_VISIBLE_TRACK_ROWS) as f32);
        let liked_scroll_handle = self.liked_scroll_handle.clone();

        let standalone_row_count = self.standalone_track_items.len();
        let standalone_list_height =
            px(TRACK_ROW_HEIGHT * standalone_row_count.min(MAX_VISIBLE_TRACK_ROWS) as f32);
        let standalone_scroll_handle = self.standalone_scroll_handle.clone();

        div()
            .flex()
            .flex_col()
            .w_full()
            .max_h_full()
            .relative()
            .overflow_hidden()
            .when(!full_width, capped_table_width)
            .child(
                div()
                    .flex()
                    .w_full()
                    .max_h_full()
                    .relative()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("artist-detail-view")
                            .overflow_y_scroll()
                            .track_scroll(&scroll_handle)
                            .pb(px(18.0))
                            .w_full()
                            .flex_shrink(1.0)
                            .overflow_x_hidden()
                            .child(
                                div()
                                    .pt(px(52.0))
                                    .px(px(18.0))
                                    .w_full()
                                    .relative()
                                    .when(two_column, |this| {
                                        this.child(detail_close_button("artist_detail_close"))
                                    })
                                    .child(
                                        div()
                                            .font_weight(FontWeight::EXTRA_BOLD)
                                            .text_size(rems(2.5))
                                            .line_height(rems(2.75))
                                            .overflow_x_hidden()
                                            .pb(px(10.0))
                                            .w_full()
                                            .text_ellipsis()
                                            .when_some(self.artist_name.clone(), |this, name| {
                                                this.child(name)
                                            }),
                                    )
                                    .when(!self.all_tracks.is_empty(), |this| {
                                        this.child(div().pb(px(18.0)).child(playback_controls(
                                            "artist",
                                            has_available_artist_tracks,
                                            current_track_in_artist,
                                            is_playing,
                                            {
                                                let all_tracks = self.all_tracks.clone();
                                                move |_cx| queue_items_from_tracks(&all_tracks)
                                            },
                                        )))
                                    }),
                            )
                            .when(album_count > 0, |this| {
                                let handler: Option<Rc<GridHandler>> =
                                    Some(Rc::new(move |cx, id| {
                                        nav_model.update(cx, |_, cx| {
                                            cx.emit(ViewSwitchMessage::Release(id.0 as i64, None));
                                        });
                                    }));

                                this.child(
                                    div()
                                        .border_t_1()
                                        .border_color(theme.border_color)
                                        .px(px(18.0))
                                        .pt(px(10.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_size(px(18.0))
                                        .child(tr!("ARTIST_ALBUMS", "Albums")),
                                )
                                .child({
                                    // 单元渲染闭包：自然高度与有界滚动两种形态共用一份，
                                    // 故先 hoist 成局部绑定（分支各自 move 它，互斥分支
                                    // 编译期不会构成双重 move）。
                                    let render_album_tile = move |idx: usize,
                                                                _: &mut Window,
                                                                cx: &mut App|
                                                                 -> AnyElement {
                                        if prune_gate.replace(true) {
                                            grid_render_counter.update(cx, |m, _| *m = idx);
                                        } else {
                                            prune_views(
                                                &grid_views_model,
                                                &grid_render_counter,
                                                idx,
                                                cx,
                                            );
                                        }

                                        // `create_or_retrieve_view` 的可失败等价物：
                                        // 专辑可能在 list_albums_by_artist 与本帧之间
                                        // 消失（清理轮删了它，或其查询失败），而这里的
                                        // 旧 `.unwrap` 曾在陈旧的 album_ids 快照上把
                                        // 整个 app panic 掉。
                                        let cached_view =
                                            grid_views_model.read(cx).get(&idx).cloned();

                                        match cached_view {
                                            Some(view) => div()
                                                .size_full()
                                                .child(view)
                                                .into_any_element(),
                                            None => {
                                                let item_id = album_ids[idx].clone();
                                                match GridItem::<Album, AlbumColumn>::new(
                                                    cx,
                                                    item_id,
                                                    handler.clone(),
                                                    AlbumContextMenuContext {
                                                        show_go_to_artist: false,
                                                    },
                                                    GridContext::Standalone,
                                                ) {
                                                    Some(view) => {
                                                        grid_views_model.update(cx, |m, _| {
                                                            m.insert(idx, view.clone());
                                                        });
                                                        div()
                                                            .size_full()
                                                            .child(view)
                                                            .into_any_element()
                                                    }
                                                    // 专辑已消失：本帧渲染一个空白格；
                                                    // 已在途的那次 reload 会替换掉
                                                    // 陈旧快照。
                                                    None => div().into_any_element(),
                                                }
                                            }
                                        }
                                    };

                                    let base = div().px(px(10.0)).pt(px(2.0)).pb(px(10.0)).w_full();
                                    if album_grid_bounded {
                                        // 有界形态：容器限高，网格填满容器并内部滚动
                                        // （非 auto_height 模式按 relative(1.) + flex_grow
                                        // 撑满父高，故高度由外层容器决定），滚动条与
                                        // 主库网格同一摆法（relative 容器内的浮层）。
                                        base.relative()
                                            .h(album_grid_height)
                                            .child(
                                                uniform_grid(
                                                    "artist-albums-grid",
                                                    album_count,
                                                    Some(album_grid_scroll_handle.clone()),
                                                    render_album_tile,
                                                )
                                                .min_item_width(px(grid_min_item_width))
                                                .gap(px(0.0)),
                                            )
                                            .child(
                                                floating_scrollbar(
                                                    "artist-albums-grid-scrollbar",
                                                    album_grid_scroll_handle,
                                                )
                                                .right(px(4.0)),
                                            )
                                    } else {
                                        base.child(
                                            uniform_grid(
                                                "artist-albums-grid",
                                                album_count,
                                                None::<UniformListScrollHandle>,
                                                render_album_tile,
                                            )
                                            .min_item_width(px(grid_min_item_width))
                                            .gap(px(0.0))
                                            .auto_height(),
                                        )
                                    }
                                })
                            })
                            .when_some(liked_track_header, move |this, header| {
                                this.child(header).child(
                                    div()
                                        .w_full()
                                        .border_t_1()
                                        .border_color(theme.border_color)
                                        // 用有界 LRU 而非全量保留：已点赞列表可能挂着数百张行封面，
                                        // 128×72px 缩略图（~3 MB）仍足以覆盖视口加滚动邻域且不必重解码
                                        .image_cache(meliora_cache(
                                            "artist_liked_tracks_cache",
                                            128,
                                        ))
                                        .child(
                                            uniform_list(
                                                "artist-liked-tracks",
                                                liked_row_count,
                                                move |range, _, cx| {
                                                    let view = liked_list_entity.read(cx);
                                                    view.liked_track_items[range]
                                                        .iter()
                                                        .map(|item| {
                                                            div()
                                                                .h(px(TRACK_ROW_HEIGHT))
                                                                .child(item.clone())
                                                        })
                                                        .collect()
                                                },
                                            )
                                            .h(liked_list_height)
                                            .w_full()
                                            .track_scroll(&liked_scroll_handle),
                                        ),
                                )
                            })
                            .when_some(standalone_track_header, move |this, header| {
                                this.child(header).child(
                                    div()
                                        .w_full()
                                        .border_t_1()
                                        .border_color(theme.border_color)
                                        .image_cache(meliora_cache(
                                            "artist_standalone_tracks_cache",
                                            128,
                                        ))
                                        .child(
                                            uniform_list(
                                                "artist-standalone-tracks",
                                                standalone_row_count,
                                                move |range, _, cx| {
                                                    let view = standalone_list_entity.read(cx);
                                                    view.standalone_track_items[range]
                                                        .iter()
                                                        .map(|item| {
                                                            div()
                                                                .h(px(TRACK_ROW_HEIGHT))
                                                                .child(item.clone())
                                                        })
                                                        .collect()
                                                },
                                            )
                                            .h(standalone_list_height)
                                            .w_full()
                                            .track_scroll(&standalone_scroll_handle),
                                        ),
                                )
                            }),
                    )
                    .child(
                        floating_scrollbar("artist_detail_scrollbar", scroll_handle).right(px(4.0)),
                    ),
            )
    }
}
