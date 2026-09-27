//! KuGou discovery page: the platform rank list with per-rank tracks and the
//! daily recommend playlist. Streams online tracks like the playlists page.
//! Gated behind the `kugou` cargo feature.

use std::{collections::HashSet, sync::Arc};

use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};

use crate::{
    kugou,
    ui::{
        components::{
            button::{ButtonIntent, button},
            managed_image::{ManagedImageKey, managed_image},
            scrollbar::floating_scrollbar,
        },
        kugou::{
            KugouRank, KugouTrackInfo, parse_rank_tracks, parse_ranks, parse_recommend_tracks,
        },
        library::{EscapeBack, view_header::view_header},
        theme::Theme,
    },
};

const TRACKS_PER_PAGE: i64 = 30;

/// uniform_list strides rows by one fixed height measured from the first
/// row, so track rows are pinned to their natural height: 8px vertical
/// padding × 2 + title line (text_sm) + 1px gap + subtitle line (text_xs)
/// + 1px bottom border. Same value as kugou_playlists.
const TRACK_ROW_HEIGHT: f32 = 60.0;

/// How long the like/unlike watcher waits for the shared helper to confirm
/// the outcome through the global liked-set before rolling the optimistic
/// update back.
const LIKE_WATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

/// Grid geometry for the virtualized rank-card grid. Five cards fill the
/// 900px content column (with the 16px side padding); `GRID_ROW_H` covers
/// the 148px cover plus the single-line name — `withsong: 0` means the
/// cards carry no song-preview line anymore.
const GRID_COLS: usize = 5;
const GRID_CARD_W: f32 = 148.0;
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
    /// Arc-shared so the track-row closures capture refcounts, not deep
    /// clones (every visible row re-clones its track every redraw).
    Ready(Vec<Arc<KugouTrackInfo>>),
}

pub struct KugouRanksView {
    tab: Tab,
    ranks: RanksState,
    selected: Option<KugouRank>,
    tracks: Vec<Arc<KugouTrackInfo>>,
    tracks_state: TracksState,
    track_page: i64,
    has_more_tracks: bool,
    /// Bumped on every track-list (re)load; in-flight page responses whose
    /// generation no longer matches are dropped, so quickly closing and
    /// reopening the same rank cannot append a stale page twice.
    track_generation: u64,
    recommend: RecommendState,
    /// hashes liked during this session (drives the star icon)
    liked: HashSet<String>,
    /// hashes with a like/unlike request in flight; extra clicks on those
    /// rows are ignored until the request settles
    like_in_flight: HashSet<String>,
    scroll_handle: ScrollHandle,
    /// Scroll position of the virtualized track list (open rank or daily
    /// recommend); reset whenever the visible list changes so it starts at
    /// the top.
    tracks_scroll_handle: UniformListScrollHandle,
}

impl KugouRanksView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut view = Self {
                tab: Tab::Ranks,
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
                                RecommendState::Ready(tracks.into_iter().map(Arc::new).collect())
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
                    // the rank may have been closed and reopened while this
                    // request was in flight; a stale page must not append twice
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
        // one page in flight at a time: tracks_state stays Loading from the
        // click until the response lands, so extra clicks are ignored
        if self.has_more_tracks && !matches!(self.tracks_state, TracksState::Loading) {
            self.load_tracks_page(self.track_page + 1, cx);
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
        // one like/unlike request per track at a time; clicks while a request
        // is in flight are ignored
        if !self.like_in_flight.insert(hash.clone()) {
            return;
        }
        let want_liked = !self.is_liked(&hash);
        // snapshot for the rollback watcher
        let was_liked = self.liked.contains(&hash);
        // optimistic update
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

    /// The shared like/unlike helpers flip the process-wide liked-set on
    /// success and leave it untouched on failure, so the outcome is
    /// observable via `crate::ui::kugou::liked_set_contains`. Watch it
    /// (bounded) and roll the optimistic local update back when the request
    /// did not land, so a failed like doesn't leave a wrong star.
    fn watch_like_outcome(
        &mut self,
        hash: String,
        want_liked: bool,
        was_liked: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let deadline = std::time::Instant::now() + LIKE_WATCH_TIMEOUT;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                if crate::ui::kugou::liked_set_contains(&hash) == want_liked {
                    // landed: release the dedup marker, nothing to roll back
                    this.update(cx, |this, _| {
                        this.like_in_flight.remove(&hash);
                    })
                    .ok();
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
            }
            // never confirmed: undo the optimistic update
            this.update(cx, |this, cx| {
                this.like_in_flight.remove(&hash);
                if was_liked {
                    this.liked.insert(hash);
                } else {
                    this.liked.remove(&hash);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.tab == Tab::DailyRecommend {
            tr!("KUGOU_DAILY_RECOMMEND", "Daily Recommend")
        } else {
            tr!("KUGOU_RANKS")
        };

        let mut header = view_header(title.to_string());

        if self.selected.is_none() {
            header = header.right(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
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
        let label = label.into();
        let active = self.tab == tab;
        button()
            .id(format!("kugou-rank-tab-{:?}", tab))
            .intent(if active {
                ButtonIntent::Primary
            } else {
                ButtonIntent::Secondary
            })
            .child(label)
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
                                    let theme = cx.global::<Theme>().clone();
                                    let cards: Vec<_> = ranks
                                        .iter()
                                        .skip(start)
                                        .take(GRID_COLS)
                                        .enumerate()
                                        .map(|(i, rank)| {
                                            Self::render_rank_card(rank, start + i, &theme, &entity)
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

    /// One rank card. Built from inside the uniform_list closure where only
    /// `&App` is available, so the click reaches the view through the weak
    /// entity handle (same pattern as the track rows below).
    fn render_rank_card(
        rank: &KugouRank,
        index: usize,
        theme: &Theme,
        entity: &WeakEntity<Self>,
    ) -> impl IntoElement {
        let cover = managed_image(
            ("kugou-rank-cover", index),
            ManagedImageKey::HttpCover(rank.cover_url.clone()),
        )
        // cards paint at 148 CSS px; 192 device px keeps 1.25-1.5 DPR sharp
        // while shrinking every tile the atlas packs
        .thumb_max(192)
        .w(px(GRID_CARD_W))
        .h(px(GRID_CARD_W))
        .rounded(px(theme.radius_md));
        let name = div()
            .mt(px(6.0))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .overflow_x_hidden()
            .text_ellipsis()
            .child(rank.name.clone());
        div()
            .id(("kugou-rank", index))
            .flex()
            .flex_col()
            .w(px(GRID_CARD_W))
            .cursor_pointer()
            .on_click({
                let entity = entity.clone();
                let rank = rank.clone();
                move |_, _, cx| {
                    if let Some(view) = entity.upgrade() {
                        view.update(cx, |this, cx| this.open_rank(rank.clone(), cx));
                    }
                }
            })
            .child(cover)
            .child(name)
    }

    /// The rows the virtualized container renders: the open rank's tracks, or
    /// the daily-recommend list while that tab is showing. Both lists used to
    /// render from their own state; the container reads a single slice, so
    /// this picks the right one.
    fn visible_tracks(&self) -> &[Arc<KugouTrackInfo>] {
        if self.selected.is_some() || self.tab != Tab::DailyRecommend {
            &self.tracks
        } else if let RecommendState::Ready(tracks) = &self.recommend {
            tracks
        } else {
            &self.tracks
        }
    }

    /// Rows are built from inside the uniform_list render closure where only
    /// `&App` is available, so there is no `cx.listener` here: the like
    /// handler reaches the view through a weak handle instead (same pattern
    /// as kugou_playlists).
    fn render_track_row(
        &self,
        track: &Arc<KugouTrackInfo>,
        index: usize,
        entity: &Entity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let liked = self.is_liked(&track.hash);
        // The row closures must own their data ('static): capture Arc clones
        // (refcount bump only) instead of three deep clones per visible row
        // per redraw — a full clone re-allocates `hash: String` each time.
        let play = track.clone();
        let like = track.clone();
        let download = track.clone();
        let weak = entity.downgrade();

        crate::ui::kugou::kugou_track_row(
            theme,
            track,
            index,
            "kugou-rank-track",
            true,
            false,
            liked,
            move |_, _, cx| {
                crate::ui::kugou::play_track_now(cx, &play);
            },
            move |_, _, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| this.toggle_like(&like, cx));
            },
            move |_, _, cx| {
                crate::ui::kugou::download_track_ui(cx, (*download).clone());
            },
        )
    }

    /// The "Load More" pill below the virtualized rank track list. Also used
    /// by the degenerate no-rows state so the escape hatch stays reachable.
    fn render_load_more_button(cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().justify_center().pt(px(12.0)).child(
            button()
                .id("kugou-rank-load-more")
                .child(tr!("KUGOU_LOAD_MORE"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.load_more(cx);
                })),
        )
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.global::<Theme>().clone();

        match self.tab {
            Tab::Ranks => match &self.ranks {
                RanksState::Loading => muted_line(tr!("KUGOU_LOADING"), &theme).into_any_element(),
                RanksState::Failed(message) => {
                    error_line(message.clone(), &theme).into_any_element()
                }
                // unreachable in practice: Render::render routes a Ready rank
                // grid to the virtualized container below; this arm only
                // keeps the match exhaustive
                RanksState::Ready(_) => div().into_any_element(),
            },
            Tab::DailyRecommend => match &self.recommend {
                RecommendState::Idle | RecommendState::Loading => {
                    muted_line(tr!("KUGOU_LOADING"), &theme).into_any_element()
                }
                RecommendState::Failed(message) => {
                    error_line(message.clone(), &theme).into_any_element()
                }
                RecommendState::Ready(tracks) => {
                    // unreachable in practice: Render::render routes a Ready
                    // daily-recommend to the virtualized container below; this
                    // arm only keeps the match exhaustive
                    let entity = cx.entity();
                    div()
                        .flex()
                        .flex_col()
                        .children(tracks.iter().enumerate().map(|(index, track)| {
                            self.render_track_row(track, index, &entity, cx)
                                .into_any_element()
                        }))
                        .into_any_element()
                }
            },
        }
    }
}

/// Muted placeholder line for the loading and empty states of both the rank
/// grid and the track lists.
fn muted_line(text: impl IntoElement, theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.text_secondary)
        .py(px(24.0))
        .child(text)
}

/// Error placeholder line; the caller pairs it with a retry button.
fn error_line(message: impl IntoElement, theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.status_error)
        .py(px(12.0))
        .child(message)
}

impl Render for KugouRanksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let scroll_handle = self.scroll_handle.clone();

        // Mirror kugou_playlists: while a track list is on screen it is its
        // own scroll container and replaces the page-level scroller wholesale,
        // so the accumulated rows stay virtualized regardless of page count.
        let tracks_ready = if self.selected.is_some() {
            !(matches!(
                &self.tracks_state,
                TracksState::Loading | TracksState::Failed(_)
            ) && self.tracks.is_empty())
        } else {
            self.tab == Tab::DailyRecommend && matches!(self.recommend, RecommendState::Ready(_))
        };
        // the rank-card grid is a uniform_list too: like the track lists it
        // must be the scroll container itself, not a child of the page
        // scroller, or the virtualization culls nothing
        let grid_ready = self.selected.is_none()
            && self.tab == Tab::Ranks
            && matches!(self.ranks, RanksState::Ready(_));

        let mut root = div()
            .id("kugou-ranks-view")
            .key_context("KugouRanks")
            .on_action(cx.listener(|this, _: &EscapeBack, _, cx| {
                // Escape leaves the rank detail back to the discovery list
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
            let track_count = self.visible_tracks().len();
            let has_more_tracks = self.selected.is_some() && self.has_more_tracks;
            let list_entity = cx.entity();
            let tracks_scroll_handle = self.tracks_scroll_handle.clone();
            let title = self.selected.as_ref().map(|rank| rank.name.clone());

            root = root.child(
                div()
                    .id("kugou-ranks-tracks-container")
                    .w_full()
                    .max_w(px(900.0))
                    .mr_auto()
                    .ml_auto()
                    .flex()
                    .flex_col()
                    .flex_grow(1.0)
                    .min_h(px(0.0))
                    .px(px(16.0))
                    .pt(px(4.0))
                    .when_some(title, |this, name| {
                        this.child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .pb(px(8.0))
                                .child(name),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .flex_grow(1.0)
                            .min_h(px(0.0))
                            .child(
                                uniform_list(
                                    "kugou-rank-tracks",
                                    track_count,
                                    move |range, _, cx| {
                                        let start = range.start;
                                        let view = list_entity.read(cx);
                                        view.visible_tracks()[range]
                                            .iter()
                                            .enumerate()
                                            .map(|(i, track)| {
                                                div().h(px(TRACK_ROW_HEIGHT)).child(
                                                    view.render_track_row(
                                                        track,
                                                        start + i,
                                                        &list_entity,
                                                        cx,
                                                    ),
                                                )
                                            })
                                            .collect()
                                    },
                                )
                                .w_full()
                                .h_full()
                                .track_scroll(&tracks_scroll_handle),
                            ),
                    )
                    .when(has_more_tracks, |this| {
                        this.child(Self::render_load_more_button(cx))
                    })
                    .child(floating_scrollbar(
                        "kugou-ranks-tracks-scrollbar",
                        tracks_scroll_handle,
                    )),
            );
        } else if grid_ready {
            root = root.child(self.render_ranks_grid(cx));
        } else {
            let content: AnyElement = if self.selected.is_some() {
                match &self.tracks_state {
                    TracksState::Loading if self.tracks.is_empty() => {
                        muted_line(tr!("KUGOU_LOADING"), &theme).into_any_element()
                    }
                    TracksState::Failed(message) if self.tracks.is_empty() => {
                        error_line(message.clone(), &theme).into_any_element()
                    }
                    // unreachable while tracks_ready routes non-empty lists to
                    // the virtualized container; kept exhaustive for the compiler
                    _ => div().into_any_element(),
                }
            } else {
                self.render_content(cx)
            };

            root = root
                .child(
                    div()
                        .id("kugou-ranks-scroll")
                        .w_full()
                        .max_w(px(900.0))
                        .mr_auto()
                        .ml_auto()
                        .flex()
                        .flex_col()
                        .flex_grow(1.0)
                        .min_h(px(0.0))
                        .px(px(16.0))
                        .pt(px(4.0))
                        .overflow_y_scroll()
                        .track_scroll(&scroll_handle)
                        .child(content),
                )
                .child(floating_scrollbar("kugou-ranks-scrollbar", scroll_handle));
        }

        root
    }
}
