use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use crate::ui::design::ICON_MD;
use cntp_i18n::tr;
use gpui::*;
use prelude::FluentBuilder;
use rustc_hash::FxHashMap;

use crate::{
    library::{
        db,
        types::{
            Album, DATE_PRECISION_FULL_DATE, DATE_PRECISION_YEAR, DATE_PRECISION_YEAR_MONTH,
            DBString, Track,
        },
    },
    playback::thread::PlaybackState,
    ui::{
        app::Pool,
        caching::meliora_cache,
        components::{
            button::{ButtonSize, button},
            icons::{DOTS_VERTICAL, STAR, STAR_FILLED, icon},
            playback_controls::playback_controls,
            popover::{PopoverPosition, popover},
            scrollbar::{ScrollableHandle, floating_scrollbar},
            table::table_data::capped_table_width,
            tooltip::build_tooltip,
        },
        library::{
            collection_summary::format_collection_summary,
            context_menus::{
                AlbumContextMenuContext, album::AlbumContextMenu, navigate_to_album_artists,
                queue_items_from_tracks,
            },
            detail_close_button,
            track_item::{ArtistNameVisibility, TrackItem, TrackItemLeftField, TrackItemOptions},
        },
        models::{
            LIKED_SONGS_PLAYLIST_ID, Models, PlaybackInfo, is_track_available_snapshot,
            toggle_album_like,
        },
        scroll_follow::SmoothScrollFollow,
        theme::Theme,
        util::{create_or_retrieve_view, prune_views, reduced_motion_enabled},
    },
};

const RELEASE_SCROLL_ANIMATION_DURATION: Duration = Duration::from_millis(250);

/// 发行页头部里专辑封面盒子及其图像的固定尺寸。
const COVER_SIZE_PX: f32 = 160.0;

/// 变高虚拟化列表的过绘带（视口上下额外渲染的像素）：约一行主行高
/// （主行 39px + 边界取整；先例 lyrics.rs 用 60px 行高/400px 过绘，本页行矮）。
const LIST_OVERDRAW_PX: f32 = 40.0;

/// 未测量条目的统一高度提示：滚动条在首次绘制收敛真实高度之前按它近似
/// （先例 lyrics.rs 的 `reset_with_uniform_height` 60px）。
const ROW_HEIGHT_HINT_PX: f32 = 40.0;

/// 列表条目总数：头部占 0、曲目 i 在条目 i+1、页脚在条目 n+1（空专辑即
/// 头部 + 页脚两条，与占位头部始终可见的既有行为一致）。
fn release_list_item_count(track_count: usize) -> usize {
    track_count + 2
}

/// 曲目索引 → 列表条目：头部占条目 0，所以曲目 i 在条目 i+1。
fn track_list_index(track_index: usize) -> usize {
    track_index + 1
}

/// 判定第 `index` 首曲目是否渲染组头：专辑首曲 / 每张碟的 1 号轨 / 碟号变化处。
/// 纯函数（原判定内联在行实体构造闭包里，提出来供测试与按需装配共用）。
fn track_is_group_start(tracks: &[Track], index: usize) -> bool {
    let track = &tracks[index];
    index == 0
        || track.track_number == Some(1)
        || tracks
            .get(index - 1)
            .is_some_and(|t| t.disc_number != track.disc_number)
}

/// 后台加载落地前显示的占位：此刻只知道 id，所以头部渲染为空、曲目列表保持空白，
/// 其余字段等 `reload` 完成时填入。在我们脚下被删除的专辑（例如清理扫描）同样永远
/// 拿不到真实数据——它持续渲染这张空发行页，而不是把 UI 线程 panic 掉。
fn placeholder_album(album_id: i64) -> Album {
    Album {
        id: album_id,
        title: DBString::from(""),
        artist_display_override: None,
        release_date: None,
        date_precision: None,
        label: None,
        catalog_number: None,
        isrc: None,
        vinyl_numbering: false,
    }
}

fn compute_all_liked(cx: &App, tracks: &[Track]) -> bool {
    if tracks.is_empty() {
        return false;
    }
    // 缓存集合是异步落地的、本视图观察它，所以点赞/取消赞不再花一次 UI 线程上的
    // `block_on` 查询。
    let Some(liked) = cx.global::<Models>().liked_ids.read(cx).clone() else {
        return false;
    };
    tracks.iter().all(|track| liked.contains(&track.id))
}

/// 曲目行的按需装配参数：行实体不再在加载完成时全量预建——render 的
/// `gpui::list()` 变高虚拟化（先例 lyrics.rs / finder.rs）只对「可见 + 过绘」
/// 条目运行 `render_item`，行实体按需建进 `ReleaseView::row_views`，滚出
/// VIEW_KEEP_AROUND 带之外由 `prune_views` 修剪。本结构只存构建行所需的定型
/// 参数，render 每帧克隆进条目闭包的只是 Arc 引用计数。不用 `uniform_list`：
/// 行高不恒定（组头行在 39px 主行上方追加 ~45px 组头 div），而本仓 gpui
/// checkout 无 ItemHeight 枚举，uniform_list 会把单个条目的测量高度强钳到全
/// 列表（条目 0 恒为 ~84px 组头行 → 全列表按 84px 计，行间出空洞）——
/// GPUI_HARDCORE_PERFORMANCE.md §38「未虚拟化」的旧口径随本次迁移作废。
/// `show_go_to_album`/`show_go_to_artist` 原是构造参数，但两个调用点的实值
/// 恒为 false/true，直接定死，免得行装配守卫还得比对它们。
struct TrackListing {
    tracks: Arc<Vec<Track>>,
    artist_name_visibility: ArtistNameVisibility,
    vinyl_numbering: bool,
    /// 找出最大的曲目号并交给 track item 用于测量宽度。
    max_track_num_str: Option<SharedString>,
}

impl TrackListing {
    fn new(
        tracks: Arc<Vec<Track>>,
        artist_name_visibility: ArtistNameVisibility,
        vinyl_numbering: bool,
    ) -> Self {
        let max_track_num_str = tracks
            .iter()
            .filter_map(|t| t.track_number)
            .max()
            .map(|n| format!("{}", n).into());

        Self {
            tracks,
            artist_name_visibility,
            vinyl_numbering,
            max_track_num_str,
        }
    }

    /// 按需构建第 `index` 首曲目的行实体（只在 `render_item` 为可见/过绘条目
    /// 装配时调用；行实体的建造成本是一次快照集合查找 + 元素树，无 DB/stat）。
    fn build_row(&self, index: usize, cx: &mut App) -> Entity<TrackItem> {
        TrackItem::new(
            cx,
            self.tracks[index].clone(),
            self.artist_name_visibility.clone(),
            TrackItemLeftField::TrackNum,
            None,
            self.max_track_num_str.clone(),
            None,
            TrackItemOptions {
                is_start: track_is_group_start(&self.tracks, index),
                vinyl_numbering: self.vinyl_numbering,
                show_go_to_album: false,
                show_go_to_artist: true,
            },
        )
    }
}

pub struct ReleaseView {
    album: Arc<Album>,
    artist_name: Option<DBString>,
    tracks: Arc<Vec<Track>>,
    /// 逐曲目的可用性，加载时算一次，而不是每帧 N 次 stat。
    tracks_available: Arc<Vec<bool>>,
    track_listing: Arc<TrackListing>,
    /// 按列表条目缓存（键 = 条目 idx，曲目 i 在条目 i+1）的行实体：虚拟化后只在
    /// `render_item` 装配可见/过绘条目时创建，滚出 VIEW_KEEP_AROUND 带即被
    /// `prune_views` 修剪（先例 finder.rs 的 views_model / artist_detail_view 的
    /// grid_views）。
    row_views: Entity<FxHashMap<usize, Entity<TrackItem>>>,
    /// 上一次装配到达的条目，供 `prune_views` 计算修剪带（先例 finder.rs）。
    render_counter: Entity<usize>,
    /// 变高虚拟化列表状态（`gpui::list`；先例 lyrics.rs）。条目映射见
    /// [`release_list_item_count`]；条目内容变化时在 `apply_loaded` 里
    /// `reset_with_uniform_height`，行装配守卫判定未变时保留滚动位置与已测行高。
    list_state: ListState,
    /// 每次加载都自增，使晚于新一轮完成的慢加载不能覆盖它（与 `Table::reload_rows`
    /// 同一个守卫）。
    load_generation: u64,
    collection_summary: SharedString,
    release_info: Option<SharedString>,
    img_path: SharedString,
    /// 抽象滚动句柄（List 变体）：offset 语义与 `Regular` 相同（负 y，0 = 顶部），
    /// 导航历史的原始 offset 原样往返。
    scroll_handle: ScrollableHandle,
    pending_scroll: Option<usize>,
    /// 导航历史要恢复的原始 offset y：列表首次布局前 `set_offset_from_scrollbar`
    /// 会因 `last_layout_bounds` 为 None 静默早退（list.rs 的实现），故存到这里，
    /// 借 PendingLayout 重试环等 viewport 就绪的帧再写入。
    pending_restore: Option<f32>,
    scroll_follow: SmoothScrollFollow,
    scroll_frame_scheduled: bool,
    all_liked: bool,
    /// 加载时算好并缓存：`tracks_available` 一旦建就不可变，于是头部不再每次渲染
    /// 都重扫一遍。
    has_available_tracks: bool,
    /// 当前播放的曲目是否属于本专辑。由 `PlaybackInfo::current_track` 推导，在
    /// 曲目变更时（下面的观察者）与加载时重算——字段本身非每帧重算。它只驱动
    /// 头部播放控件的高亮；行内高亮的承重墙是 track_item.rs 里每行每帧对
    /// current_track 的读取（唤醒窗口并随整帧重渲生效），观察者的 `!=` 守卫
    /// 拦不住、也不需要拦换曲重渲帧。
    current_track_in_album: bool,
    menu_open: bool,
    track_ids: std::rc::Rc<[i64]>,
    /// 只解析一次；否则页脚每帧都要重新解析它。
    release_date_utc: Option<chrono::DateTime<chrono::Utc>>,
}

/// [`ReleaseView::render_header`] 的头部渲染标志位，打包成一个结构体，
/// 好让该方法不必收一堆位置参数 bool。
struct HeaderState {
    has_available_tracks: bool,
    current_track_in_album: bool,
    is_playing: bool,
    show_close_button: bool,
}

impl ReleaseView {
    pub(super) fn new(
        cx: &mut App,
        album_id: i64,
        target_track_id: Option<i64>,
        initial_scroll_offset: Option<f32>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            // 观察的是集合本身而不是歌单追踪器：追踪器在异步 reload 落地前就触发，
            // 那时读缓存会是陈旧的。
            let liked_ids = cx.global::<Models>().liked_ids.clone();
            cx.observe(&liked_ids, |this: &mut Self, _, cx| {
                let new_all_liked = compute_all_liked(cx, &this.tracks);
                if new_all_liked != this.all_liked {
                    this.all_liked = new_all_liked;
                    cx.notify();
                }
            })
            .detach();

            // 头部播放控件在当前曲目属于本专辑时高亮。就在这里于曲目变更时重算
            // （带 `!=` 守卫），字段本身不必每帧重算。行内高亮不依赖这个守卫——
            // 它由 TrackItem::render 每帧读 current_track 唤醒窗口并随整帧重渲
            // 生效（track_item.rs 的 is_current 读取），行实体虚拟化后该机制在活
            // 行上自动收敛；删除那个每帧读取就必须同时为行引入 current_track 观察，
            // 否则同专辑切歌时行内高亮停更。
            let current_track = cx.global::<PlaybackInfo>().current_track.clone();
            cx.observe(&current_track, |this: &mut Self, _, cx| {
                let in_album = this.compute_current_track_in_album(cx);
                if in_album != this.current_track_in_album {
                    this.current_track_in_album = in_album;
                    cx.notify();
                }
            })
            .detach();

            cx.on_release(|this: &mut Self, cx: &mut App| {
                ImageSource::Resource(Resource::Embedded(this.img_path.clone())).remove_asset(cx);
            })
            .detach();

            let placeholder = Arc::new(placeholder_album(album_id));
            let collection_summary = format_collection_summary(0, 0);

            // 占位期条目数就是 2（头部 + 页脚）：头部（空盒封面 + 空标题）必须
            // 在后台加载落地前可见，与既有占位行为一致。
            let list_state = ListState::new(2, ListAlignment::Top, px(LIST_OVERDRAW_PX));

            let mut this = Self {
                album: placeholder,
                artist_name: None,
                tracks: Arc::new(Vec::new()),
                tracks_available: Arc::new(Vec::new()),
                track_listing: Arc::new(TrackListing::new(
                    Arc::new(Vec::new()),
                    ArtistNameVisibility::OnlyIfDifferent(None),
                    false,
                )),
                row_views: cx.new(|_| FxHashMap::default()),
                render_counter: cx.new(|_| 0),
                list_state: list_state.clone(),
                collection_summary,
                release_info: None,
                img_path: SharedString::from(format!("!db://album/{album_id}/full")),
                scroll_handle: ScrollableHandle::List { state: list_state },
                pending_scroll: None,
                pending_restore: None,
                scroll_follow: SmoothScrollFollow::new(RELEASE_SCROLL_ANIMATION_DURATION),
                scroll_frame_scheduled: false,
                all_liked: false,
                has_available_tracks: false,
                current_track_in_album: false,
                menu_open: false,
                track_ids: std::rc::Rc::from(Vec::new()),
                release_date_utc: None,
                load_generation: 0,
            };
            // 打开视图时在运行时上加载专辑头部与其曲目列表（见 reload）；
            // 落地前视图显示占位头部与空列表。
            this.reload(cx, target_track_id);

            // 重扫完成即刷新打开中的详情（上游 bcea976）：文件监视器自动重扫
            // 增删曲目后，停留中的视图不得继续展示旧曲目列表
            let scan_state = cx.global::<crate::ui::models::Models>().scan_state.clone();
            cx.observe(&scan_state, |this: &mut Self, e, cx| {
                if e.read(cx).is_complete_with_changes() {
                    this.reload(cx, None);
                }
            })
            .detach();

            // 恢复导航历史记忆的滚动位置（上游 e15e8ba）；「跳转到曲目」的
            // 目标滚动优先，带 target 的入口不恢复。存取两侧都是原始 offset
            // 原样往返，不做符号归一。虚拟化列表在首次布局前 set_offset 会静默
            // 早退（last_layout_bounds 为 None），故存进 pending_restore，借
            // PendingLayout 重试环等 viewport 就绪后再写入。
            if target_track_id.is_none() {
                this.pending_restore = initial_scroll_offset;
            }

            this
        })
    }

    /// 导航历史用的滚动记忆：主内容 handle 的纵向原始偏移。
    pub(super) fn get_scroll_offset(&self) -> f32 {
        self.scroll_handle.offset().y.into()
    }

    /// 滚动历史的键（抹掉「跳转到曲目」分量，见 library::scroll_storage_key）。
    pub(super) fn scroll_storage_key(&self) -> super::ViewSwitchMessage {
        super::ViewSwitchMessage::Release(self.album.id, None)
    }

    /// 在后台运行时上加载专辑头部及其曲目列表：打开视图过去会在「专辑行 + 完整曲目列表
    /// 查询（外加已点赞专辑归属检查）」上阻塞 UI 线程。加载落地前视图继续显示占位；
    /// 世代守卫丢弃被更新一轮加载取代的结果。
    fn reload(&mut self, cx: &mut Context<Self>, target_track_id: Option<i64>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let album_id = self.album.id;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let loaded = crate::RUNTIME
                .spawn(async move {
                    // 查询失败按原样降级（保留占位头部/空列表/未全赞），但不静默：
                    // 照 sidebar 先例记 warn，使 Err 与正常空结果在日志里可分辨。
                    let album = db::get_album_by_id(&pool, album_id)
                        .await
                        .inspect_err(
                            |err| tracing::warn!(error = %err, "album header query failed"),
                        )
                        .ok();
                    let tracks = db::list_tracks_in_album(&pool, album_id)
                        .await
                        .unwrap_or_else(|err| {
                            tracing::warn!(error = %err, "album tracks query failed");
                            Arc::new(Vec::new())
                        });
                    // 与 `compute_all_liked` 完全相同的已点赞专辑检查（含它的空列表
                    // 提前返回），但挪出 UI 线程：这些 id 只在曲目列表加载完后才存在，
                    // 所以检查就搭在这里跑。
                    let all_liked = if tracks.is_empty() {
                        false
                    } else {
                        let ids: Vec<i64> = tracks.iter().map(|track| track.id).collect();
                        db::playlist_contains_all_tracks(&pool, LIKED_SONGS_PLAYLIST_ID, &ids)
                            .await
                            .unwrap_or_else(|err| {
                                tracing::warn!(error = %err, "album liked check query failed");
                                false
                            })
                    };
                    (album, tracks, all_liked)
                })
                .await
                .inspect_err(|err| tracing::warn!(error = ?err, "album reload task failed"))
                .unwrap_or((None, Arc::new(Vec::new()), false));

            let _ = this.update(cx, |this, cx| {
                // 更新的加载已取代这一轮：本结果陈旧。
                if this.load_generation != generation {
                    return;
                }
                let (album, tracks, all_liked) = loaded;
                this.apply_loaded(album, tracks, all_liked, target_track_id, cx);
            });
        })
        .detach();
    }

    /// 用一次后台加载的结果填掉占位。派生状态（行、可用性、滚动目标）在这里的
    /// UI 线程上重建——只有查询本身在线程外跑。
    fn apply_loaded(
        &mut self,
        album: Option<Arc<Album>>,
        tracks: Arc<Vec<Track>>,
        all_liked: bool,
        target_track_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        // 专辑取回失败时保留占位：在我们脚下被删除的专辑（例如清理扫描）会渲染一张
        // 空发行页，而不是把 UI 线程 panic 掉。
        let prev_artist_name = self.artist_name.clone();
        let prev_vinyl_numbering = self.album.vinyl_numbering;
        if let Some(album) = album {
            self.artist_name = album.artist_display_override.clone();
            self.release_info = match (&album.label, &album.catalog_number) {
                (Some(label), Some(catalog_number)) => Some(format!(
                    "{} • {}",
                    label.0.as_str(),
                    catalog_number.0.as_str()
                )),
                (Some(label), None) => Some(label.0.to_string()),
                (None, Some(catalog_number)) => Some(catalog_number.0.to_string()),
                (None, None) => None,
            }
            // 结果为空（例如厂牌为空白）时不渲染页脚行
            .filter(|info| !info.is_empty())
            .map(SharedString::from);
            self.release_date_utc = album.release_date.as_ref().and_then(|date| {
                chrono::NaiveDate::parse_from_str(date.0.as_str(), "%Y-%m-%d")
                    .ok()
                    .map(|nd| {
                        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                            nd.and_hms_opt(0, 0, 0).expect("date has zero time"),
                            chrono::Utc,
                        )
                    })
            });
            self.album = album;
        }
        self.track_ids = tracks.iter().map(|track| track.id).collect();
        let new_available = Arc::new(
            tracks
                .iter()
                .map(|track| is_track_available_snapshot(cx, track))
                .collect(),
        );
        // 行装配守卫（album_ids 式，先例 artist_detail_view.rs / Table 的
        // clear_row_views）：曲目内容、可用性、艺人名可见性与 vinyl 模式全部未变
        // 时保留行实体缓存与列表状态——`reset_with_uniform_height` 会丢掉滚动
        // 位置与已测行高，重扫无变更触发的 reload 不得把读者弹回顶部。头部/页脚
        // 不在守卫范围内：它们每次渲染都从 self 现场构建，没有缓存可陈旧。
        let listing_changed = self.tracks != tracks
            || self.tracks_available != new_available
            || prev_artist_name != self.artist_name
            || prev_vinyl_numbering != self.album.vinyl_numbering;
        self.tracks = tracks.clone();
        self.tracks_available = new_available;
        if listing_changed {
            self.track_listing = Arc::new(TrackListing::new(
                tracks.clone(),
                ArtistNameVisibility::OnlyIfDifferent(self.artist_name.clone()),
                self.album.vinyl_numbering,
            ));
            self.row_views.update(cx, |views, _| views.clear());
            self.render_counter.update(cx, |counter, _| *counter = 0);
            // 曲目集合变化等于一张新列表：用统一行高提示重建，滚动条在首次绘制
            // 收敛真实高度之前大致正确（先例 lyrics.rs 的 reset_with_uniform_height）。
            // reset 同时把视图送回顶部——「跳转到曲目」的 pending_scroll 随后接管。
            self.list_state.reset_with_uniform_height(
                release_list_item_count(tracks.len()),
                px(ROW_HEIGHT_HINT_PX),
            );
        }
        self.collection_summary = format_collection_summary(
            tracks.len() as i64,
            tracks.iter().map(|track| track.duration).sum(),
        );
        self.pending_scroll = target_track_id.and_then(|track_id| {
            tracks
                .iter()
                .position(|track| track.id == track_id && is_track_available_snapshot(cx, track))
        });
        self.all_liked = all_liked;
        // 派生的头部状态，每次加载算一次而不是每帧算。
        self.has_available_tracks = self.tracks_available.iter().any(|&a| a);
        self.current_track_in_album = self.compute_current_track_in_album(cx);

        cx.notify();
    }

    /// 当前播放曲目是否属于本专辑可用曲目之一：借用当前路径（不 clone），
    /// 并把每首曲目与它预先算好的可用性配对——没有每帧 stat，而且这次扫描只在
    /// 曲目变更 / 加载时跑，不在每次渲染跑。
    fn compute_current_track_in_album(&self, cx: &App) -> bool {
        cx.global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .map(|current| current.get_path().as_path())
            .is_some_and(|current| {
                self.tracks
                    .iter()
                    .zip(self.tracks_available.iter())
                    .any(|(track, &available)| available && current == track.location.as_path())
            })
    }

    fn render_header(
        &self,
        theme: &Theme,
        state: HeaderState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let HeaderState {
            has_available_tracks,
            current_track_in_album,
            is_playing,
            show_close_button,
        } = state;

        div()
            .pt(px(52.0))
            .flex_shrink(1.0)
            .flex()
            .overflow_x_hidden()
            .px(px(18.0))
            .w_full()
            .relative()
            .when(show_close_button, |this| {
                this.child(detail_close_button("release_close"))
            })
            .child(
                div()
                    .rounded(px(theme.radius_lg))
                    .bg(theme.album_art_background)
                    .border_1()
                    .border_color(theme.elevated_border_color)
                    .shadow_lg()
                    .w(px(COVER_SIZE_PX))
                    .h(px(COVER_SIZE_PX))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .child(
                        img(self.img_path.clone())
                            .min_w(px(COVER_SIZE_PX))
                            .min_h(px(COVER_SIZE_PX))
                            .max_w(px(COVER_SIZE_PX))
                            .max_h(px(COVER_SIZE_PX))
                            .overflow_hidden()
                            .flex()
                            // TODO: 理想情况这里应是 ObjectFit::Cover，但那样会破坏圆角
                            // FIXME: 这是一个 GPUI bug
                            .object_fit(ObjectFit::Fill)
                            .rounded(px(theme.radius_lg)),
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
                            .id(("release_view_artist", self.album.id as usize))
                            .text_ellipsis()
                            .overflow_x_hidden()
                            .cursor_pointer()
                            .on_click({
                                let album_id = self.album.id;
                                move |ev, _, cx| {
                                    navigate_to_album_artists(cx, album_id, ev.position());
                                }
                            })
                            .when_some(self.artist_name.clone(), |this, artist| this.child(artist)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_size(rems(2.5))
                            .line_height(rems(2.75))
                            .overflow_x_hidden()
                            .pb(px(10.0))
                            .w_full()
                            .text_ellipsis()
                            .child(self.album.title.clone()),
                    )
                    .child(
                        div()
                            .pb(px(10.0))
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .child(self.collection_summary.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                playback_controls(
                                    "release",
                                    has_available_tracks,
                                    current_track_in_album,
                                    is_playing,
                                    {
                                        let tracks = self.track_listing.tracks.clone();
                                        move |_cx| queue_items_from_tracks(&tracks)
                                    },
                                )
                                .show_add_to_queue(false)
                                .trailing(self.render_menu_button(cx)),
                            )
                            .child(self.render_like_button(theme)),
                    ),
            )
    }

    fn render_like_button(&self, theme: &Theme) -> impl IntoElement {
        let all_liked = self.all_liked;
        let has_tracks = !self.tracks.is_empty();
        let track_ids = self.track_ids.clone();

        div()
            .id("release-like")
            .rounded_sm()
            .p(px(8.0))
            .when(has_tracks, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(theme.button_secondary_hover))
                    .active(|this| this.bg(theme.button_secondary_active))
                    .tooltip(build_tooltip(if all_liked {
                        tr!("UNLIKE_ALBUM", "Unlike Album")
                    } else {
                        tr!("LIKE_ALBUM", "Like Album")
                    }))
                    .on_click(move |_, _, cx| {
                        toggle_album_like(track_ids.to_vec(), all_liked, cx);
                    })
            })
            .when(!has_tracks, |this| this.opacity(0.5))
            .child(
                icon(if all_liked { STAR_FILLED } else { STAR })
                    .size(ICON_MD)
                    .text_color(if all_liked {
                        theme.liked_song
                    } else {
                        theme.text_secondary
                    }),
            )
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu_open = false;
        cx.notify();
    }

    fn render_menu_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = self.menu_open;

        div()
            .relative()
            .flex()
            .child(
                button()
                    .id("release-menu-button")
                    .size(ButtonSize::Large)
                    .flex_none()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();

                            this.menu_open = !menu_open;
                            cx.notify();
                        }),
                    )
                    .child(icon(DOTS_VERTICAL).size(ICON_MD).my_auto()),
            )
            .when(menu_open, |this| {
                let album = Rc::new((*self.album).clone());
                let weak_self = cx.entity().downgrade();
                let close = {
                    move |cx: &mut App| {
                        weak_self.update(cx, |this, cx| this.close_menu(cx)).ok();
                    }
                };
                let close_for_dismiss = close.clone();
                let close_for_out = close.clone();
                this.child(
                    popover()
                        .position(PopoverPosition::BottomRight)
                        .edge_offset(px(4.0))
                        .p(px(0.0))
                        .on_dismiss(move |_, cx| close_for_dismiss(cx))
                        .on_mouse_down_out(move |_, _, cx| close_for_out(cx))
                        .child(
                            div()
                                .id("release-menu-container")
                                .on_click(move |_, _, cx| close(cx))
                                .child(AlbumContextMenu::new(
                                    album,
                                    AlbumContextMenuContext::default(),
                                )),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_footer(&self, theme: &Theme) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .text_sm()
            .ml(px(18.0))
            .pt(px(12.0))
            .pb(px(12.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_secondary)
            .when_some(self.release_info.clone(), |this, release_info| {
                this.child(div().child(release_info))
            })
            .when_some(
                self.album.date_precision,
                |this, precision| match precision {
                    DATE_PRECISION_FULL_DATE | DATE_PRECISION_YEAR_MONTH => {
                        if let Some(utc) = self.release_date_utc {
                            this.child(if precision == DATE_PRECISION_FULL_DATE {
                                tr!(
                                    "RELEASED_DATE",
                                    "Released {{date}}",
                                    date:date("YMD", length="long")=utc
                                )
                            } else {
                                tr!(
                                    "RELEASED_DATE",
                                    date:date("YM", length="long")=utc
                                )
                            })
                        } else {
                            this
                        }
                    }
                    DATE_PRECISION_YEAR => {
                        // release_date 缺失或短于 4 字符年份前缀（元数据畸形）时：
                        // 跳过发行年这一行，而不是让渲染过程 panic
                        let year = self
                            .album
                            .release_date
                            .as_ref()
                            .and_then(|date| date.0.get(..4));
                        match year {
                            Some(year) => {
                                this.child(tr!("RELEASED_YEAR", "Released {{year}}", year = year))
                            }
                            None => this,
                        }
                    }
                    _ => this,
                },
            )
            .when_some(self.album.isrc.as_ref(), |this, isrc| {
                this.child(div().child(isrc.clone()))
            })
    }

    fn schedule_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_frame_scheduled {
            return;
        }

        self.scroll_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.scroll_frame_scheduled = false;
            let reduced_motion = reduced_motion_enabled(cx);
            this.advance_scroll_animation(window, cx, reduced_motion);
        });
    }

    fn advance_scroll_animation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) {
        // 导航历史的原始 offset 恢复：viewport 尚未布局时排下一帧再试——
        // 恢复不能死等外部 notify（首帧布局后未必再有渲染帧）。
        if let Some(y) = self.pending_restore {
            let viewport = self.scroll_handle.bounds();
            if viewport.size.height > px(0.0) {
                self.scroll_handle
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(y)));
                self.pending_restore = None;
            } else {
                self.schedule_scroll_frame(window, cx);
            }
        }

        if let Some(pending_scroll) = self.pending_scroll {
            match self.compute_follow_target(pending_scroll) {
                FollowTarget::PendingLayout => {
                    self.schedule_scroll_frame(window, cx);
                    return;
                }
                FollowTarget::NoScrollNeeded => {
                    self.pending_scroll = None;
                }
                FollowTarget::Target(target_scroll_top) => {
                    self.scroll_follow.follow_to(
                        &self.scroll_handle,
                        target_scroll_top,
                        reduced_motion,
                    );
                    self.pending_scroll = None;
                }
            }
        }

        let changed = self
            .scroll_follow
            .advance_or_snap(&self.scroll_handle, reduced_motion);

        if self.scroll_follow.is_active() {
            self.schedule_scroll_frame(window, cx);
        }

        if changed {
            cx.notify();
        }
    }

    fn compute_follow_target(&self, track_index: usize) -> FollowTarget {
        let viewport = self.scroll_handle.bounds();
        if viewport.size.height <= px(0.0) {
            return FollowTarget::PendingLayout;
        }

        // 列表条目映射：头部占 0，曲目 i 在条目 i+1。
        let list_ix = track_list_index(track_index);
        let Some(item_bounds) = self.list_state.bounds_for_item(list_ix) else {
            // 虚拟化后目标行可能尚未测量（滚出可见带的 Unmeasured 条目，
            // `bounds_for_item` 只对已测量条目有值）：先 reveal 粗定位把它拉进
            // 渲染窗口（sum_tree 游标按行高提示可跨越未测量条目），下一帧行被
            // 测量后再走下面的平滑路径精确落位（先例 lyrics.rs 的跟随路径）。
            self.list_state.scroll_to_reveal_item(list_ix);
            return FollowTarget::PendingLayout;
        };

        let max_scroll_top = self.scroll_handle.max_offset().y.max(px(0.0));
        let raw_offset_y = viewport.origin.y - item_bounds.origin.y;
        let target_scroll_top = (-raw_offset_y).max(px(0.0)).min(max_scroll_top);
        let current_scroll_top = -self.scroll_handle.offset().y;

        if (target_scroll_top - current_scroll_top).abs() <= px(0.1) {
            FollowTarget::NoScrollNeeded
        } else {
            FollowTarget::Target(target_scroll_top)
        }
    }
}

#[derive(Clone, Copy)]
enum FollowTarget {
    PendingLayout,
    NoScrollNeeded,
    Target(Pixels),
}

impl Render for ReleaseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let reduced_motion = settings.interface.reduced_motion;
        let full_width = settings.interface.effective_full_width();
        if self.pending_scroll.is_some()
            || self.pending_restore.is_some()
            || self.scroll_follow.is_active()
        {
            if reduced_motion {
                // 减弱动效下仍需一趟来敲定待定布局，并把在途的滚动动画吸附到终值；
                // 只是之后不再安排新的动画帧。
                self.advance_scroll_animation(window, cx, reduced_motion);
            } else {
                self.schedule_scroll_frame(window, cx);
            }
        }

        let list_state = self.list_state.clone();
        let this_entity = cx.entity();
        let row_views = self.row_views.clone();
        let render_counter = self.render_counter.clone();
        let listing = self.track_listing.clone();
        let footer_ix = release_list_item_count(self.track_listing.tracks.len()) - 1;
        let show_footer = self.release_info.is_some()
            || self.album.release_date.is_some()
            || self.album.isrc.is_some();
        // 每次渲染只 prune 一次（首条目触发；闭包随渲染每帧重建，Cell 天然
        // 逐帧复位），render_counter 仍逐条目推进，band 演化与 finder.rs 一致。
        let prune_gate = Cell::new(false);

        div()
            .image_cache(meliora_cache(("release", self.album.id as u64), 1))
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .relative()
            .overflow_hidden()
            .when(!full_width, capped_table_width)
            .child(
                div()
                    .id("release-view")
                    .w_full()
                    .flex_grow_1()
                    .flex_shrink(1.0)
                    .overflow_x_hidden()
                    // 变高虚拟化：滚动由 List 自身处理（paint 阶段注册滚轮事件），
                    // 外层不再 overflow_y_scroll / track_scroll；高度链必须
                    // definite——外层 h_full（库容器本就 h_full），本层 flex_grow
                    // 拿满剩余空间，列表 h_full 解析为本层高度。
                    .child(
                        list(list_state, move |idx, _, cx| {
                            if prune_gate.replace(true) {
                                render_counter.update(cx, |m, _| *m = idx);
                            } else {
                                prune_views(&row_views, &render_counter, idx, cx);
                            }
                            if idx == 0 {
                                // 头部：render_menu_button 的 listener 需要
                                // &mut Context，经实体 update 取——此时本视图的
                                // render lease 早已释放（先例 lyrics.rs 条目闭包
                                // 的 read/update 时序注释）。
                                return this_entity.update(cx, |this, cx| {
                                    let theme = cx.global::<Theme>().clone();
                                    let is_playing =
                                        cx.global::<PlaybackInfo>().playback_state.read(cx)
                                            == &PlaybackState::Playing;
                                    let two_column = cx
                                        .global::<crate::settings::SettingsGlobal>()
                                        .model
                                        .read(cx)
                                        .interface
                                        .two_column_library;
                                    this.render_header(
                                        &theme,
                                        HeaderState {
                                            has_available_tracks: this.has_available_tracks,
                                            current_track_in_album: this.current_track_in_album,
                                            is_playing,
                                            show_close_button: two_column,
                                        },
                                        cx,
                                    )
                                    .into_any_element()
                                });
                            }
                            if idx == footer_ix {
                                return if show_footer {
                                    // 页脚是只读渲染，read 足够（与头部不同，
                                    // 它不装 listener）。
                                    let this = this_entity.read(cx);
                                    let theme = cx.global::<Theme>().clone();
                                    this.render_footer(&theme).into_any_element()
                                } else {
                                    // 页脚字段全空：零高度占位条目，保持条目映射
                                    // 恒定（头部=0、页脚=n+1）不随内容漂移。
                                    div().into_any_element()
                                };
                            }
                            // 曲目行：条目 idx ↔ 曲目索引 idx-1；行实体按需建进
                            // row_views（键 = 条目 idx），带外由 prune_views 修剪。
                            let track_ix = idx - 1;
                            create_or_retrieve_view(
                                &row_views,
                                idx,
                                |cx| listing.build_row(track_ix, cx),
                                cx,
                            )
                            .into_any_element()
                        })
                        .w_full()
                        .h_full(),
                    ),
            )
            .child(
                floating_scrollbar("release_scrollbar", self.scroll_handle.clone()).right(px(4.0)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    // 不用 `use super::*`：顶层 `use gpui::*` 会把 gpui re-export 的 `test`
    // 属性宏（gpui.rs 经 gpui_macros 无条件再导出）一起带进来，`#[test]` 就
    // 解析成它而不是 builtin，同步测试函数展开即递归爆栈。
    use super::{Track, release_list_item_count, track_is_group_start, track_list_index};
    use crate::library::types::DBString;

    fn track(id: i64, track_number: Option<i32>, disc_number: Option<i32>) -> Track {
        Track {
            id,
            title: DBString::from(""),
            album_id: None,
            track_number,
            disc_number,
            duration: 0,
            location: PathBuf::from(format!("/library/{id}.flac")),
            artist_names: None,
            disc_subtitle: None,
        }
    }

    #[test]
    fn group_start_invariants() {
        // 专辑首曲恒为组头，即使曲号/碟号都缺；无碟号的后续曲目不触发
        let tracks = vec![track(1, None, None), track(2, None, None)];
        assert!(track_is_group_start(&tracks, 0));
        assert!(!track_is_group_start(&tracks, 1));

        // 每张碟的 1 号轨触发——同碟畸形重复的 1 号轨也触发（与旧内联判定一致）
        let tracks = vec![
            track(1, Some(1), Some(1)),
            track(2, Some(2), Some(1)),
            track(3, Some(1), Some(1)),
        ];
        assert!(track_is_group_start(&tracks, 2));

        // 碟号变化处触发，碟内推进不触发
        let tracks = vec![
            track(1, Some(9), Some(1)),
            track(2, Some(10), Some(1)),
            track(3, Some(1), Some(2)),
            track(4, Some(2), Some(2)),
        ];
        assert!(!track_is_group_start(&tracks, 1));
        assert!(track_is_group_start(&tracks, 2));
        assert!(!track_is_group_start(&tracks, 3));
    }

    #[test]
    fn list_item_mapping_invariants() {
        // 条目映射不变量：头部=0、曲目 i 在条目 i+1、页脚=n+1，总数 = n + 2。
        // 空专辑（含占位期）恒为 2 条——头部在后台加载落地前必须可见。
        assert_eq!(release_list_item_count(0), 2);
        let n = 69;
        assert_eq!(release_list_item_count(n), n + 2);
        assert_eq!(track_list_index(0), 1);
        assert_eq!(track_list_index(n - 1), n);
        // 页脚紧跟最后一曲且是最后一个条目
        assert_eq!(track_list_index(n - 1) + 1, release_list_item_count(n) - 1);
    }
}
