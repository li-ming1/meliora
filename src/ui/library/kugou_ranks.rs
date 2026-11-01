//! 酷狗发现页：平台榜单列表（含每个榜单的曲目）与每日推荐歌单。在线曲目的串流
//! 方式与云歌单页相同。受 `kugou` cargo feature 门控。

use std::{
    collections::HashSet,
    sync::{Arc, LazyLock, Mutex},
};

use crate::ui::online_track_row::{error_line, muted_line};
use cntp_i18n::tr;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ObjectFit, ParentElement, Render, Rgba, ScrollHandle, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, WeakEntity, Window, div,
    linear_color_stop, linear_gradient, prelude::FluentBuilder, px, uniform_list,
};

use crate::{
    kugou,
    ui::{
        components::{
            button::{ButtonIntent, ButtonStyle, button},
            icons::{HEART, HEART_FILLED, PLAY, icon},
            managed_image::{ManagedImageKey, managed_image},
            scrollbar::floating_scrollbar,
        },
        kugou::{
            KugouRank, KugouTrackInfo, parse_rank_tracks, parse_ranks, parse_recommend_tracks,
        },
        library::{EscapeBack, kugou_view_common, view_header::view_header},
        online_common::{
            CachedRoot, LoadMoreSpec, cached_root, clear_root, online_list_container,
            page_scroll_container, store_root,
        },
        theme::Theme,
    },
};

const TRACKS_PER_PAGE: i64 = 30;

/// 两个标签页根数据的会话缓存（TTL 与辅助函数在 `online_common`）：视图实体每次
/// 切标签都会重建，缓存新鲜时整轮根列表重取直接跳过。
type CachedTabRoot<T> = LazyLock<Mutex<Option<CachedRoot<Vec<T>>>>>;
static CACHED_RANKS: CachedTabRoot<KugouRank> = LazyLock::new(|| Mutex::new(None));
static CACHED_RECOMMEND: CachedTabRoot<Arc<KugouTrackInfo>> = LazyLock::new(|| Mutex::new(None));

/// 每日推荐是按已登录 userid 抓取的；账号身份一变就丢弃，做法与歌单总览相同。
pub(crate) fn invalidate_cached_recommend() {
    clear_root(&CACHED_RECOMMEND);
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tab {
    Ranks,
    DailyRecommend,
}

enum RanksState {
    Loading,
    Failed(SharedString),
    Ready(Vec<KugouRank>),
}

/// 虚拟化榜单卡片网格的几何。5 张卡片填满 900px 内容列（算上左右各 16px padding）；
/// `GRID_ROW_H` 覆盖 148px 封面加单行名称——`withsong: 0` 意味着卡片不再有歌曲预览行。
const GRID_COLS: usize = 5;
use crate::ui::online_track_row::RANK_CARD_W as GRID_CARD_W;
const GRID_GAP: f32 = 18.0;
const GRID_ROW_H: f32 = 180.0;

enum TracksState {
    Idle,
    Loading,
    Failed(SharedString),
}

enum RecommendState {
    Idle,
    Loading,
    Failed(SharedString),
    /// 用 Arc 共享，好让曲目行闭包捕获的是引用计数而非深拷贝
    /// （每个可见行每次重绘都会重新 clone 自己那条曲目）。
    Ready(Vec<Arc<KugouTrackInfo>>),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum RecommendViewMode {
    #[default]
    List,
    Stage,
}

pub struct KugouRanksView {
    tab: Tab,
    recommend_view_mode: RecommendViewMode,
    stage_focus_index: usize,
    ranks: RanksState,
    selected: Option<KugouRank>,
    tracks: Vec<Arc<KugouTrackInfo>>,
    tracks_state: TracksState,
    track_page: i64,
    has_more_tracks: bool,
    /// 每次（重）加载曲目列表都自增；世代号不再匹配的在途分页响应会被丢弃，
    /// 于是快速关闭再打开同一榜单也不可能把同一陈旧页追加两次。
    track_generation: u64,
    recommend: RecommendState,
    /// 本次会话内点过赞的 hash（驱动星标图标）
    liked: HashSet<String>,
    /// 有赞/取消赞请求已在途的 hash；这些行上的额外点击在请求落地前一律忽略
    like_in_flight: HashSet<String>,
    scroll_handle: ScrollHandle,
    /// 虚拟化曲目列表（打开的榜单或每日推荐）的滚动位置；可见列表一变就重置，
    /// 使其从顶部开始。
    tracks_scroll_handle: UniformListScrollHandle,
}

impl KugouRanksView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut view = Self {
                tab: Tab::Ranks,
                recommend_view_mode: RecommendViewMode::List,
                stage_focus_index: 0,
                ranks: RanksState::Loading,
                selected: None,
                tracks: Vec::new(),
                tracks_state: TracksState::Idle,
                track_page: 0,
                has_more_tracks: false,
                track_generation: 0,
                recommend: RecommendState::Idle,
                liked: HashSet::new(),
                like_in_flight: HashSet::new(),
                scroll_handle: ScrollHandle::new(),
                tracks_scroll_handle: UniformListScrollHandle::new(),
            };
            view.load_ranks(cx);
            view
        })
    }

    fn load_ranks(&mut self, cx: &mut Context<Self>) {
        // 会话缓存命中：视图每次切标签都会重建，所以新鲜的缓存榜单列表整段跳过网络。
        if let Some(ranks) = cached_root(&CACHED_RANKS) {
            self.ranks = RanksState::Ready(ranks);
            cx.notify();
            return;
        }

        self.ranks = RanksState::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.rank_list().await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    this.ranks = match request {
                        Ok(Ok(response)) => {
                            let ranks = parse_ranks(&response.body);
                            if ranks.is_empty() {
                                RanksState::Failed(tr!("KUGOU_NO_RANKS", "No ranks found").into())
                            } else {
                                store_root(&CACHED_RANKS, ranks.clone());
                                RanksState::Ready(ranks)
                            }
                        }
                        Ok(Err(err)) => RanksState::Failed(err.to_string().into()),
                        Err(err) => RanksState::Failed(err.to_string().into()),
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn switch_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.scroll_handle = ScrollHandle::new();
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        if tab == Tab::DailyRecommend && matches!(self.recommend, RecommendState::Idle) {
            self.load_recommend(cx);
        }
        cx.notify();
    }

    fn load_recommend(&mut self, cx: &mut Context<Self>) {
        if let Some(tracks) = cached_root(&CACHED_RECOMMEND) {
            self.recommend = RecommendState::Ready(tracks);
            cx.notify();
            return;
        }

        self.recommend = RecommendState::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.everyday_recommend().await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    this.recommend = match request {
                        Ok(Ok(response)) => {
                            let tracks = parse_recommend_tracks(&response.body);
                            if tracks.is_empty() {
                                RecommendState::Failed(
                                    tr!("KUGOU_NO_RECOMMEND", "No recommendations today").into(),
                                )
                            } else {
                                let tracks: Vec<_> = tracks.into_iter().map(Arc::new).collect();
                                store_root(&CACHED_RECOMMEND, tracks.clone());
                                RecommendState::Ready(tracks)
                            }
                        }
                        Ok(Err(err)) => RecommendState::Failed(err.to_string().into()),
                        Err(err) => RecommendState::Failed(err.to_string().into()),
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn open_rank(&mut self, rank: KugouRank, cx: &mut Context<Self>) {
        self.selected = Some(rank);
        self.tracks.clear();
        self.tracks_state = TracksState::Loading;
        self.track_page = 0;
        self.has_more_tracks = false;
        self.scroll_handle = ScrollHandle::new();
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();
        self.load_tracks_page(1, cx);
    }

    fn load_tracks_page(&mut self, page: i64, cx: &mut Context<Self>) {
        let Some(rank) = self.selected.clone() else {
            return;
        };

        self.tracks_state = TracksState::Loading;
        self.track_generation += 1;
        let generation = self.track_generation;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let rankid = rank.rankid;
            let request = crate::RUNTIME
                .spawn(async move { client.rank_audio(rankid, page, TRACKS_PER_PAGE).await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    // 请求已在途期间榜单可能被关闭后重开；陈旧页不得追加两次
                    let still_current = this
                        .selected
                        .as_ref()
                        .is_some_and(|r| r.rankid == rank.rankid)
                        && this.track_generation == generation;

                    match request {
                        Ok(Ok(response)) if still_current => {
                            let total = response
                                .body
                                .pointer("/data/total")
                                .and_then(serde_json::Value::as_i64)
                                .unwrap_or(0);
                            let page_tracks = parse_rank_tracks(&response.body);
                            this.tracks.extend(page_tracks.into_iter().map(Arc::new));
                            this.track_page = page;
                            this.has_more_tracks = this.tracks.len() < total as usize;
                            this.tracks_state = TracksState::Idle;
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(err)) if still_current => {
                            this.tracks_state = TracksState::Failed(err.to_string().into());
                        }
                        Ok(Err(_)) => {}
                        Err(err) if still_current => {
                            this.tracks_state = TracksState::Failed(err.to_string().into());
                        }
                        Err(_) => {}
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        // 一次只让一页在途：从点击到响应落地 tracks_state 一直是 Loading，
        // 所以多余的点击被忽略
        if self.has_more_tracks && !matches!(self.tracks_state, TracksState::Loading) {
            let next_page = self.track_page + 1;
            self.load_tracks_page(next_page, cx);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.tracks.clear();
        self.tracks_state = TracksState::Idle;
        self.track_page = 0;
        self.has_more_tracks = false;
        self.track_generation += 1;
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();
    }

    fn is_liked(&self, hash: &str) -> bool {
        self.liked.contains(hash) || crate::ui::kugou::liked_set_contains(hash)
    }

    fn toggle_like(&mut self, track: &KugouTrackInfo, cx: &mut Context<Self>) {
        let hash = track.hash.clone();
        // 每首曲目同时只有一个赞/取消赞请求；请求已在途期间的点击被忽略
        if !self.like_in_flight.insert(hash.clone()) {
            return;
        }
        let want_liked = !self.is_liked(&hash);
        // 供回滚监视器使用的快照
        let was_liked = self.liked.contains(&hash);
        // 乐观更新
        if want_liked {
            self.liked.insert(hash.clone());
            crate::ui::kugou::like_track(cx, track);
        } else {
            self.liked.remove(&hash);
            crate::ui::kugou::unlike_track(cx, track);
        }
        cx.notify();
        self.watch_like_outcome(hash, want_liked, was_liked, cx);
    }

    /// 共享的赞/取消赞辅助函数成功时翻转进程级已点赞集合、失败时原样不动，
    /// 因此结果可由 `crate::ui::kugou::liked_set_contains` 观测。监视它（有界），
    /// 请求未落地时回滚乐观的本地更新，于是失败的点赞不会留下错误的星标。
    fn watch_like_outcome(
        &mut self,
        hash: String,
        want_liked: bool,
        was_liked: bool,
        cx: &mut Context<Self>,
    ) {
        kugou_view_common::spawn_like_watcher(
            cx,
            hash,
            want_liked,
            |view, hash| {
                view.like_in_flight.remove(hash);
            },
            move |view, hash| {
                if was_liked {
                    view.liked.insert(hash.to_string());
                } else {
                    view.liked.remove(hash);
                }
            },
        );
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.tab == Tab::DailyRecommend {
            tr!("KUGOU_DAILY_RECOMMEND", "Daily Recommend")
        } else {
            tr!("KUGOU_RANKS")
        };

        let mut header = view_header(title.to_string());

        if self.selected.is_none() {
            let is_stage = self.recommend_view_mode == RecommendViewMode::Stage;
            let is_recommend = self.tab == Tab::DailyRecommend;
            header = header.right(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(is_recommend, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(2.0))
                                .child(
                                    button()
                                        .id("recommend-list-mode")
                                        .style(if !is_stage { ButtonStyle::Regular } else { ButtonStyle::Minimal })
                                        .intent(if !is_stage { ButtonIntent::Primary } else { ButtonIntent::Secondary })
                                        .child("列表")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.recommend_view_mode = RecommendViewMode::List;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    button()
                                        .id("recommend-stage-mode")
                                        .style(if is_stage { ButtonStyle::Regular } else { ButtonStyle::Minimal })
                                        .intent(if is_stage { ButtonIntent::Primary } else { ButtonIntent::Secondary })
                                        .child("舞台")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.recommend_view_mode = RecommendViewMode::Stage;
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .child(self.tab_button(Tab::Ranks, tr!("KUGOU_RANKS").to_string(), cx))
                    .child(self.tab_button(
                        Tab::DailyRecommend,
                        tr!("KUGOU_DAILY_RECOMMEND").to_string(),
                        cx,
                    )),
            );
        }

        header
    }

    fn tab_button(
        &self,
        tab: Tab,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.tab == tab;
        button()
            .id(format!("kugou-rank-tab-{:?}", tab))
            .style(if active {
                ButtonStyle::Regular
            } else {
                ButtonStyle::Minimal
            })
            .intent(if active {
                ButtonIntent::Primary
            } else {
                ButtonIntent::Secondary
            })
            .child(label.into())
            .on_click(cx.listener(move |this, _, _, cx| this.switch_tab(tab, cx)))
    }

    fn render_ranks_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = match &self.ranks {
            RanksState::Ready(ranks) => ranks.len().div_ceil(GRID_COLS),
            _ => 0,
        };
        let entity = cx.entity().downgrade();
        div()
            .id("kugou-ranks-grid-container")
            .w_full()
            .max_w(px(900.0))
            .min_w(px(GRID_CARD_W * GRID_COLS as f32
                + GRID_GAP * (GRID_COLS - 1) as f32
                + 32.0))
            .mr_auto()
            .ml_auto()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_h(px(0.0))
            .px(px(16.0))
            .pt(px(4.0))
            .pb(px(24.0))
            .child(
                div()
                    .relative()
                    .w_full()
                    .flex_grow(1.0)
                    .min_h(px(0.0))
                    .child(
                        uniform_list("kugou-ranks-grid", rows, move |range, _, cx| {
                            let Some(view) = entity.upgrade() else {
                                return Vec::new();
                            };
                            let ranks: &[KugouRank] = match &view.read(cx).ranks {
                                RanksState::Ready(ranks) => ranks,
                                _ => &[],
                            };
                            range
                                .map(|row| {
                                    let start = row * GRID_COLS;
                                    let theme = cx.global::<Theme>();
                                    let cards: Vec<_> = ranks
                                        .iter()
                                        .skip(start)
                                        .take(GRID_COLS)
                                        .enumerate()
                                        .map(|(i, rank)| {
                                            Self::render_rank_card(rank, start + i, theme, &entity)
                                                .into_any_element()
                                        })
                                        .collect();
                                    div()
                                        .h(px(GRID_ROW_H))
                                        .overflow_hidden()
                                        .flex()
                                        .gap(px(GRID_GAP))
                                        .children(cards)
                                        .into_any_element()
                                })
                                .collect()
                        })
                        .w_full()
                        .h_full(),
                    ),
            )
            .into_any_element()
    }

    /// 单张榜单卡片。它在 uniform_list 闭包内部构造，那里只有 `&App`，
    /// 所以点击经弱实体句柄抵达视图（与下面的曲目行同一套模式）。
    fn render_rank_card(
        rank: &KugouRank,
        index: usize,
        theme: &Theme,
        entity: &WeakEntity<Self>,
    ) -> impl IntoElement {
        crate::ui::online_track_row::rank_card(
            "kugou-rank",
            index,
            theme,
            &rank.cover_url,
            &rank.name,
            None,
            {
                let entity = entity.clone();
                let rank = rank.clone();
                move |_, _, cx| {
                    if let Some(view) = entity.upgrade() {
                        view.update(cx, |this, cx| this.open_rank(rank.clone(), cx));
                    }
                }
            },
        )
    }

    /// 虚拟化容器要渲染的那些行：打开的榜单的曲目，或该标签显示时的每日推荐列表。
    /// 两个列表过去各自从自己的状态渲染；容器只读一个切片，所以由这里挑出正确的那个。
    fn visible_tracks(&self) -> &[Arc<KugouTrackInfo>] {
        if self.selected.is_some() || self.tab != Tab::DailyRecommend {
            &self.tracks
        } else if let RecommendState::Ready(tracks) = &self.recommend {
            tracks
        } else {
            &self.tracks
        }
    }

    /// 行是在 uniform_list 的渲染闭包内部构造的，那里只有 `&App`，因此这里没有
    /// `cx.listener` 可用：点赞处理器改经弱句柄抵达视图（与 kugou_playlists 同一套模式）。
    fn render_track_row(
        &self,
        track: &Arc<KugouTrackInfo>,
        index: usize,
        entity: &Entity<Self>,
        cx: &App,
    ) -> AnyElement {
        kugou_view_common::kugou_track_row_view(
            self,
            entity,
            track,
            index,
            cx,
            "kugou-rank-track",
            false,
            |view, hash| view.is_liked(hash),
            |view, track, cx| view.toggle_like(track, cx),
        )
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.global::<Theme>();

        match self.tab {
            Tab::Ranks => match &self.ranks {
                RanksState::Loading => muted_line(tr!("KUGOU_LOADING"), theme).into_any_element(),
                RanksState::Failed(message) => div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .py(px(12.0))
                    .child(error_line(message.clone(), theme))
                    .child(
                        div().flex().justify_center().child(
                            button()
                                .id("kugou-ranks-retry")
                                .child(tr!("KUGOU_RETRY"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.load_ranks(cx);
                                })),
                        ),
                    )
                    .into_any_element(),
                // 实践中不可达：Render::render 会把 Ready 的榜单网格交给下面的虚拟化
                // 容器；这个 arm 只是让 match 保持穷尽
                RanksState::Ready(_) => div().into_any_element(),
            },
            Tab::DailyRecommend => match &self.recommend {
                RecommendState::Idle | RecommendState::Loading => {
                    muted_line(tr!("KUGOU_LOADING"), theme).into_any_element()
                }
                RecommendState::Failed(message) => div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .py(px(12.0))
                    .child(error_line(message.clone(), theme))
                    .child(
                        div().flex().justify_center().child(
                            button()
                                .id("kugou-recommend-retry")
                                .child(tr!("KUGOU_RETRY"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.load_recommend(cx);
                                })),
                        ),
                    )
                    .into_any_element(),
                // 实践中不可达：Render::render 会把 Ready 的每日推荐交给下面的虚拟化
                // 容器；这个 arm 只是让 match 保持穷尽
                RecommendState::Ready(_) => div().into_any_element(),
            },
        }
    }

    fn render_stage_carousel(
        &self,
        tracks: &[Arc<KugouTrackInfo>],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if tracks.is_empty() {
            return div().into_any_element();
        }

        let theme = cx.global::<Theme>();
        let focus = self.stage_focus_index.min(tracks.len().saturating_sub(1));
        let focus_track = &tracks[focus];
        let liked = self.is_liked(&focus_track.hash);

        // 左右轮播槽位：展示 [-2, -1, 0, 1, 2] 视差卡片
        let offsets: [isize; 5] = [-2, -1, 0, 1, 2];
        let cards = offsets.into_iter().filter_map(|d| {
            let idx = (focus as isize + d) as usize;
            if (focus as isize + d) < 0 || idx >= tracks.len() {
                return None;
            }
            let track = &tracks[idx];
            let is_center = d == 0;
            let (w, h, shadow_level, opacity_val) = match d.abs() {
                0 => (px(260.0), px(260.0), true, 1.0f32),
                1 => (px(205.0), px(205.0), true, 0.70f32),
                _ => (px(160.0), px(160.0), false, 0.45f32),
            };

            let card = div()
                .id(("stage_card_wrap", idx))
                .relative()
                .w(w)
                .h(h)
                .rounded(px(16.0))
                .overflow_hidden()
                .border_1()
                .border_color(if is_center {
                    theme.button_primary
                } else {
                    theme.border_color
                })
                .bg(theme.album_art_background)
                .opacity(opacity_val)
                .cursor_pointer()
                .when(is_center, |s| {
                    s.shadow_lg()
                        .child(
                            // 底部镜面倒影渐变遮罩 (Mirror Floor Reflection)
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(70.0))
                                .bg(linear_gradient(
                                    0.0,
                                    linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.85), 0.0),
                                    linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.0), 1.0),
                                )),
                        )
                })
                .when(!is_center && shadow_level, |s| s.shadow_md())
                .child(
                    managed_image(
                        ("stage_card", idx),
                        ManagedImageKey::HttpCover(track.cover_url.clone()),
                    )
                    .thumb_max(256)
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Cover),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.stage_focus_index = idx;
                    cx.notify();
                }));

            Some(card)
        });

        let play_track = (*focus_track).clone();
        let like_track = (*focus_track).clone();

        div()
            .id("kugou-stage-carousel")
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(24.0))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let dy = f32::from(ev.delta.pixel_delta(px(1.0)).y);
                if dy.abs() > 4.0 {
                    if dy < 0.0 {
                        this.stage_focus_index = (this.stage_focus_index + 1).min(this.visible_tracks().len().saturating_sub(1));
                    } else {
                        this.stage_focus_index = this.stage_focus_index.saturating_sub(1);
                    }
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(16.0))
                    .w_full()
                    .children(cards),
            )
            // 悬浮磨砂胶囊控制岛 (Floating Action Capsule Island)
            .child(
                div()
                    .id("stage-focus-island")
                    .px(px(28.0))
                    .py(px(16.0))
                    .rounded(px(20.0))
                    .bg(theme.elevated_background)
                    .border_1()
                    .border_color(theme.elevated_border_color)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.text)
                                    .child(focus_track.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text_secondary)
                                    .child(focus_track.artist.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(16.0))
                            .child(
                                div()
                                    .id("stage-prev-btn")
                                    .size(px(34.0))
                                    .rounded_full()
                                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.06))
                                    .border_1()
                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.12))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.nav_button_hover))
                                    .text_color(theme.text)
                                    .child("‹")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.stage_focus_index = this.stage_focus_index.saturating_sub(1);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("stage-play-btn")
                                    .size(px(46.0))
                                    .rounded_full()
                                    .bg(theme.button_primary)
                                    .text_color(theme.button_primary_text)
                                    .shadow_md()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.button_primary_hover))
                                    .child(icon(PLAY).size(px(22.0)).text_color(theme.button_primary_text))
                                    .on_click(move |_, _, cx| {
                                        crate::ui::kugou::play_track_now(cx, &play_track);
                                    }),
                            )
                            .child(
                                div()
                                    .id("stage-like-btn")
                                    .size(px(34.0))
                                    .rounded_full()
                                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.06))
                                    .border_1()
                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.12))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.nav_button_hover))
                                    .child(icon(if liked { HEART_FILLED } else { HEART }).size(px(18.0)).text_color(if liked { theme.liked_song } else { theme.text_secondary }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_like(&like_track, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("stage-next-btn")
                                    .size(px(34.0))
                                    .rounded_full()
                                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.06))
                                    .border_1()
                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.12))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.nav_button_hover))
                                    .text_color(theme.text)
                                    .child("›")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let max = this.visible_tracks().len().saturating_sub(1);
                                        this.stage_focus_index = (this.stage_focus_index + 1).min(max);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// 榜单网格与曲目列表两者的加载态、空态所用的静默占位行。
impl Render for KugouRanksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scroll_handle = self.scroll_handle.clone();

        // 对齐 kugou_playlists：曲目列表在屏时它本身就是滚动容器，并整体替换页面级
        // 滚动器，所以无论翻了多少页，累计的行都保持虚拟化。
        let tracks_ready = if self.selected.is_some() {
            !(matches!(
                &self.tracks_state,
                TracksState::Loading | TracksState::Failed(_)
            ) && self.tracks.is_empty())
        } else {
            self.tab == Tab::DailyRecommend && matches!(self.recommend, RecommendState::Ready(_))
        };
        // 榜单卡片网格同样是 uniform_list：和曲目列表一样，它必须自己就是滚动容器，
        // 而不是页面级滚动器的子节点，否则虚拟化裁剪不掉任何东西
        let grid_ready = self.selected.is_none()
            && self.tab == Tab::Ranks
            && matches!(self.ranks, RanksState::Ready(_));

        let mut root = div()
            .id("kugou-ranks-view")
            .key_context("KugouRanks")
            .on_action(cx.listener(|this, _: &EscapeBack, _, cx| {
                // Escape 从榜单详情退回发现列表
                if this.selected.is_some() {
                    this.close(cx);
                }
            }))
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(self.render_header(cx));

        if tracks_ready {
            if self.selected.is_none()
                && self.tab == Tab::DailyRecommend
                && self.recommend_view_mode == RecommendViewMode::Stage
            {
                let tracks = self.visible_tracks().to_vec();
                root = root.child(self.render_stage_carousel(&tracks, cx));
            } else {
                let has_more_tracks = self.selected.is_some() && self.has_more_tracks;
                let tracks_scroll_handle = self.tracks_scroll_handle.clone();
                let title = self.selected.as_ref().map(|rank| rank.name.clone());

                root = root.child(online_list_container(
                    cx,
                    "kugou-ranks-tracks-container",
                    "kugou-rank-tracks",
                    "kugou-ranks-tracks-scrollbar",
                    &tracks_scroll_handle,
                    kugou_view_common::ONLINE_TRACK_ROW_HEIGHT,
                    self.visible_tracks().len(),
                    0.0,
                    false,
                    title,
                    |view| view.visible_tracks(),
                    |view, index, track, entity, cx| view.render_track_row(track, index, entity, cx),
                    has_more_tracks.then(|| LoadMoreSpec {
                        id: "kugou-rank-load-more",
                        label: tr!("KUGOU_LOAD_MORE").into_any_element(),
                        on_click: Box::new(|view: &mut Self, cx: &mut Context<Self>| {
                            view.load_more(cx)
                        }),
                    }),
                ));
            }
        } else if grid_ready {
            root = root.child(self.render_ranks_grid(cx));
        } else {
            // 只在这个兜底分支里借用：上面所有 `&mut cx` 的用法
            // （头部、网格、加载更多）都已经结束。
            let theme = cx.global::<Theme>();
            let content: AnyElement = if self.selected.is_some() {
                match &self.tracks_state {
                    TracksState::Loading if self.tracks.is_empty() => {
                        muted_line(tr!("KUGOU_LOADING"), theme).into_any_element()
                    }
                    TracksState::Failed(message) if self.tracks.is_empty() => {
                        error_line(message.clone(), theme).into_any_element()
                    }
                    // tracks_ready 已把非空列表路由到虚拟化容器，此处不可达；
                    // 保留它是为了让编译器满足穷尽性
                    _ => div().into_any_element(),
                }
            } else {
                self.render_content(cx)
            };

            root = root
                .child(page_scroll_container(
                    "kugou-ranks-scroll",
                    &scroll_handle,
                    content,
                ))
                .child(floating_scrollbar("kugou-ranks-scrollbar", scroll_handle));
        }

        root
    }
}
