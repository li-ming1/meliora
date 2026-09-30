//! NetEase discovery page: Top Charts (toplist) and Daily Recommend tabs.
//! Gated behind the `netease` cargo feature (via the parent module).

use std::collections::HashSet;
use std::sync::Arc;

use crate::ui::online_track_row::{error_line, muted_line};
use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};

use crate::{
    netease,
    ui::{
        components::{
            button::{ButtonIntent, button},
            scrollbar::floating_scrollbar,
        },
        library::{EscapeBack, view_header::view_header},
        netease::{NeteaseRank, NeteaseTrackInfo, parse_ranks, parse_tracks},
        settings::{SettingsSectionKind, open_settings_window_with_section},
        theme::Theme,
    },
};

const TRACKS_PER_PAGE: i64 = 30;

/// uniform_list strides rows by one fixed height measured from the first
/// row, so track rows are pinned to their natural height: 8px vertical
/// padding × 2 + title line (text_sm) + 1px gap + subtitle line (text_xs)
/// + 1px bottom border. Same value as netease_playlists / kugou_ranks.
const TRACK_ROW_HEIGHT: f32 = 60.0;

/// How long the like/unlike watcher waits for the shared helper to confirm
/// the outcome through the global liked-set before rolling the optimistic
/// update back.
const LIKE_WATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How often the like/unlike watcher polls the global liked-set for the
/// outcome while waiting.
const LIKE_WATCH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tab {
    Ranks,
    DailyRecommend,
}

enum RanksState {
    Loading,
    Failed(SharedString),
    Ready(Vec<NeteaseRank>),
}

/// Grid geometry for the virtualized rank-card grid. Five cards fill the
/// 900px content column (with the 16px side padding); `GRID_ROW_H` covers
/// the 148px cover plus the name and update-frequency lines.
const GRID_COLS: usize = 5;
use crate::ui::online_track_row::RANK_CARD_W as GRID_CARD_W;
const GRID_GAP: f32 = 18.0;
const GRID_ROW_H: f32 = 196.0;

enum TracksState {
    Idle,
    Loading,
    Failed(SharedString),
}

enum RecommendState {
    Idle,
    Loading,
    Failed(SharedString),
    LoginRequired,
    /// Arc-shared so the track-row closures capture refcounts, not deep
    /// clones (every visible row re-clones its track every redraw).
    Ready(Vec<Arc<NeteaseTrackInfo>>),
}

pub struct NeteaseRanksView {
    tab: Tab,
    ranks: RanksState,
    selected: Option<NeteaseRank>,
    tracks: Vec<Arc<NeteaseTrackInfo>>,
    tracks_state: TracksState,
    track_page: i64,
    has_more_tracks: bool,
    /// full trackIds of the open chart (it is a playlist under the hood),
    /// fetched once together with the first page and sliced locally for
    /// every page after; lives and dies with the view
    track_ids: Option<Arc<[i64]>>,
    /// bumped on every track request so a stale page can't append after a
    /// newer one was issued for the same chart
    track_generation: u64,
    recommend: RecommendState,
    /// ids liked during this session (drives the star icon)
    liked: HashSet<i64>,
    /// ids with a like/unlike request in flight; extra clicks on those rows
    /// are ignored until the request settles
    like_in_flight: HashSet<i64>,
    scroll_handle: ScrollHandle,
    /// Scroll position of the virtualized track list (open rank or daily
    /// recommend); reset whenever the visible list changes so it starts at
    /// the top.
    tracks_scroll_handle: UniformListScrollHandle,
}

impl NeteaseRanksView {
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
                track_ids: None,
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
            let client = netease::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.toplist().await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    this.ranks = match request {
                        Ok(Ok(response)) => {
                            let ranks = parse_ranks(&response.body);
                            if ranks.is_empty() {
                                RanksState::Failed(
                                    tr!("NETEASE_NO_RANKS", "No charts found").into(),
                                )
                            } else {
                                RanksState::Ready(ranks)
                            }
                        }
                        Ok(Err(err)) => RanksState::Failed(
                            tr!(
                                "NETEASE_LOAD_FAILED",
                                "Request failed: {{err}}",
                                err = err.to_string()
                            )
                            .into(),
                        ),
                        Err(err) => RanksState::Failed(
                            tr!("NETEASE_LOAD_FAILED", err = err.to_string()).into(),
                        ),
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
            let client = netease::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.recommend_songs().await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    this.recommend = match request {
                        Ok(Ok(response)) => {
                            let tracks = parse_tracks(&response.body, "/data/dailySongs");
                            if tracks.is_empty() {
                                RecommendState::Failed(
                                    tr!("NETEASE_NO_RECOMMEND", "No recommendations today").into(),
                                )
                            } else {
                                RecommendState::Ready(tracks.into_iter().map(Arc::new).collect())
                            }
                        }
                        // 301: the endpoint requires a logged-in session
                        Ok(Err(err)) if err.status() == 301 => RecommendState::LoginRequired,
                        Ok(Err(err)) => RecommendState::Failed(
                            tr!("NETEASE_LOAD_FAILED", err = err.to_string()).into(),
                        ),
                        Err(err) => RecommendState::Failed(
                            tr!("NETEASE_LOAD_FAILED", err = err.to_string()).into(),
                        ),
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn open_rank(&mut self, rank: NeteaseRank, cx: &mut Context<Self>) {
        self.selected = Some(rank);
        self.tracks.clear();
        self.tracks_state = TracksState::Loading;
        self.track_page = 0;
        self.has_more_tracks = false;
        // the cached trackIds belong to the previously open chart
        self.track_ids = None;
        self.scroll_handle = ScrollHandle::new();
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();
        self.load_tracks_page(1, cx);
    }

    /// Fetches one page of the open chart's tracks. The first page also
    /// fetches (and caches) the full trackIds list; later pages slice that
    /// cache locally and only pay for one song-detail request instead of
    /// re-downloading the whole playlist detail every time.
    fn load_tracks_page(&mut self, page: i64, cx: &mut Context<Self>) {
        let Some(rank) = self.selected.clone() else {
            return;
        };

        self.tracks_state = TracksState::Loading;
        self.track_generation += 1;
        let generation = self.track_generation;
        // Arc: handed to the request without copying the id list
        let cached_ids = self.track_ids.clone();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = netease::shared_client();
            let rankid = rank.id;
            let offset = (page - 1) * TRACKS_PER_PAGE;
            let request = crate::RUNTIME
                .spawn(async move {
                    match cached_ids {
                        Some(ids) => {
                            let response = client
                                .playlist_tracks_page(&ids, TRACKS_PER_PAGE, offset)
                                .await?;
                            Ok::<_, netease::client::NeteaseError>((ids, response))
                        }
                        None => {
                            let ids = client.playlist_track_ids(rankid).await?;
                            let response = client
                                .playlist_tracks_page(&ids, TRACKS_PER_PAGE, offset)
                                .await?;
                            Ok((Arc::<[i64]>::from(ids), response))
                        }
                    }
                })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    // the selection may have changed, or a newer page may have
                    // been requested, while this request was in flight
                    let still_current = this.selected.as_ref().is_some_and(|r| r.id == rank.id)
                        && this.track_generation == generation;

                    if still_current {
                        match request {
                            Ok(Ok((ids, response))) => {
                                this.track_ids = Some(ids);
                                let page_tracks = parse_tracks(&response.body, "/songs");
                                // the song-detail response carries no total, so a full
                                // page is the "maybe more" signal
                                this.has_more_tracks = page_tracks.len() as i64 >= TRACKS_PER_PAGE;
                                this.tracks.extend(page_tracks.into_iter().map(Arc::new));
                                this.track_page = page;
                                this.tracks_state = TracksState::Idle;
                            }
                            Ok(Err(err)) => {
                                this.tracks_state = TracksState::Failed(err.to_string().into());
                            }
                            Err(err) => {
                                this.tracks_state = TracksState::Failed(err.to_string().into());
                            }
                        }
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
        self.track_ids = None;
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();
    }

    fn is_liked(&self, id: i64) -> bool {
        self.liked.contains(&id) || crate::ui::netease::liked_set_contains(id)
    }

    fn toggle_like(&mut self, track: &NeteaseTrackInfo, cx: &mut Context<Self>) {
        let id = track.id;
        // one like/unlike request per track at a time; clicks while a request
        // is in flight are ignored
        if !self.like_in_flight.insert(id) {
            return;
        }
        let want_liked = !self.is_liked(id);
        if !netease::shared_client().logged_in() {
            // the shared helper only toasts a login hint here; don't flip the
            // star for a request that will never be sent
            self.like_in_flight.remove(&id);
            if want_liked {
                crate::ui::netease::like_track(cx, track);
            } else {
                crate::ui::netease::unlike_track(cx, track);
            }
            return;
        }
        // snapshot for the rollback watcher
        let was_liked = self.liked.contains(&id);
        // optimistic update
        if want_liked {
            self.liked.insert(id);
            crate::ui::netease::like_track(cx, track);
        } else {
            self.liked.remove(&id);
            crate::ui::netease::unlike_track(cx, track);
        }
        cx.notify();
        self.watch_like_outcome(id, want_liked, was_liked, cx);
    }

    /// The shared like/unlike helpers flip the process-wide liked-set on
    /// success and leave it untouched on failure, so the outcome is
    /// observable via `crate::ui::netease::liked_set_contains`. Watch it
    /// (bounded) and roll the optimistic local update back when the request
    /// did not land, so a failed like doesn't leave a wrong star.
    fn watch_like_outcome(
        &mut self,
        id: i64,
        want_liked: bool,
        was_liked: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let deadline = std::time::Instant::now() + LIKE_WATCH_TIMEOUT;
            loop {
                cx.background_executor()
                    .timer(LIKE_WATCH_POLL_INTERVAL)
                    .await;
                if crate::ui::netease::liked_set_contains(id) == want_liked {
                    // landed: release the dedup marker, nothing to roll back
                    this.update(cx, |this, _| {
                        this.like_in_flight.remove(&id);
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
                this.like_in_flight.remove(&id);
                if was_liked {
                    this.liked.insert(id);
                } else {
                    this.liked.remove(&id);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.tab == Tab::DailyRecommend {
            tr!("NETEASE_DAILY_RECOMMEND", "Daily Recommend")
        } else {
            tr!("NETEASE_RANKS", "Top Charts")
        };

        let mut header = view_header(title.to_string());

        if self.selected.is_none() {
            header = header.right(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(self.tab_button(Tab::Ranks, tr!("NETEASE_RANKS").to_string(), cx))
                    .child(self.tab_button(
                        Tab::DailyRecommend,
                        tr!("NETEASE_DAILY_RECOMMEND").to_string(),
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
            .id(format!("netease-rank-tab-{:?}", tab))
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
            .id("netease-ranks-grid-container")
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
                        uniform_list("netease-ranks-grid", rows, move |range, _, cx| {
                            let Some(view) = entity.upgrade() else {
                                return Vec::new();
                            };
                            let ranks: &[NeteaseRank] = match &view.read(cx).ranks {
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

    /// One rank card. Built from inside the uniform_list closure where only
    /// `&App` is available, so the click reaches the view through the weak
    /// entity handle (same pattern as the track rows below).
    fn render_rank_card(
        rank: &NeteaseRank,
        index: usize,
        theme: &Theme,
        entity: &WeakEntity<Self>,
    ) -> impl IntoElement {
        crate::ui::online_track_row::rank_card(
            "netease-rank",
            index,
            theme,
            &rank.cover_url,
            &rank.name,
            Some(&rank.update_frequency),
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

    /// Rows are built from inside the uniform_list render closure where only
    /// `&App` is available, so there is no `cx.listener` here: the like
    /// handler reaches the view through a weak handle instead (same pattern
    /// as kugou_ranks).
    /// The rows the virtualized container renders: the open rank's tracks, or
    /// the daily-recommend list while that tab is showing. Both lists used to
    /// render from their own state; the container reads a single slice, so
    /// this picks the right one.
    fn visible_tracks(&self) -> &[Arc<NeteaseTrackInfo>] {
        if self.selected.is_some() || self.tab != Tab::DailyRecommend {
            &self.tracks
        } else if let RecommendState::Ready(tracks) = &self.recommend {
            tracks
        } else {
            &self.tracks
        }
    }

    fn render_track_row(
        &self,
        track: &Arc<NeteaseTrackInfo>,
        index: usize,
        entity: &Entity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let liked = self.is_liked(track.id);
        // The row closures must own their data ('static): capture Arc clones
        // (refcount bump only) instead of three full clones per visible row
        // per redraw.
        let play = track.clone();
        let like = track.clone();
        let download = track.clone();
        let weak = entity.downgrade();

        crate::ui::online_track_row::track_row(
            theme,
            track,
            index,
            "netease-rank-track",
            true,
            false,
            liked,
            move |_, _, cx| {
                crate::ui::netease::play_track_now(cx, &play);
            },
            move |_, _, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| this.toggle_like(&like, cx));
            },
            move |_, _, cx| {
                crate::ui::netease::download_track_ui(cx, (*download).clone());
            },
        )
    }

    /// The "Load More" pill below the virtualized track list.
    fn render_load_more_button(cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().justify_center().pt(px(12.0)).child(
            button()
                .id("netease-rank-load-more")
                .child(tr!("NETEASE_LOAD_MORE", "Load More"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.load_more(cx);
                })),
        )
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.global::<Theme>();

        match self.tab {
            Tab::Ranks => match &self.ranks {
                RanksState::Loading => div()
                    .text_sm()
                    .text_color(theme.text_secondary)
                    .py(px(24.0))
                    .child(tr!("NETEASE_LOADING", "Loading..."))
                    .into_any_element(),
                RanksState::Failed(message) => div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .py(px(12.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.status_error)
                            .child(message.clone()),
                    )
                    .child(
                        div().flex().justify_center().child(
                            button()
                                .id("netease-ranks-retry")
                                .child(tr!("NETEASE_RETRY", "Retry"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.load_ranks(cx);
                                })),
                        ),
                    )
                    .into_any_element(),
                // unreachable in practice: Render::render routes a Ready rank
                // grid to the virtualized container below; this arm only
                // keeps the match exhaustive
                RanksState::Ready(_) => div().into_any_element(),
            },
            Tab::DailyRecommend => match &self.recommend {
                RecommendState::Idle | RecommendState::Loading => div()
                    .text_sm()
                    .text_color(theme.text_secondary)
                    .py(px(24.0))
                    .child(tr!("NETEASE_LOADING"))
                    .into_any_element(),
                RecommendState::LoginRequired => div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .py(px(48.0))
                    .w_full()
                    .child(div().text_sm().text_color(theme.text_secondary).child(tr!(
                        "NETEASE_DAILY_LOGIN_REQUIRED",
                        "Log in to see your daily recommendations"
                    )))
                    .child(
                        button()
                            .id("netease-open-settings")
                            .intent(ButtonIntent::Primary)
                            .child(tr!("NETEASE_OPEN_SETTINGS", "Open Settings"))
                            .on_click(|_, _, cx| {
                                open_settings_window_with_section(cx, SettingsSectionKind::Netease);
                            }),
                    )
                    .into_any_element(),
                RecommendState::Failed(message) => div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .py(px(12.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.status_error)
                            .child(message.clone()),
                    )
                    .child(
                        div().flex().justify_center().child(
                            button()
                                .id("netease-recommend-retry")
                                .child(tr!("NETEASE_RETRY"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.load_recommend(cx);
                                })),
                        ),
                    )
                    .into_any_element(),
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

impl Render for NeteaseRanksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scroll_handle = self.scroll_handle.clone();

        // Mirror kugou_ranks: while a track list is on screen it is its own
        // scroll container and replaces the page-level scroller wholesale, so
        // the accumulated rows stay virtualized regardless of page count.
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
            .id("netease-ranks-view")
            .key_context("NeteaseRanks")
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
                    .id("netease-ranks-tracks-container")
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
                                    "netease-rank-tracks",
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
                        "netease-ranks-tracks-scrollbar",
                        tracks_scroll_handle,
                    )),
            );
        } else if grid_ready {
            root = root.child(self.render_ranks_grid(cx));
        } else {
            // Borrowed only inside this fallback branch: all `&mut cx` uses
            // above (header, grid) are already done.
            let theme = cx.global::<Theme>();
            let content: AnyElement = if self.selected.is_some() {
                match &self.tracks_state {
                    TracksState::Loading if self.tracks.is_empty() => {
                        muted_line(tr!("NETEASE_LOADING"), theme).into_any_element()
                    }
                    TracksState::Failed(message) if self.tracks.is_empty() => {
                        error_line(message.clone(), theme).into_any_element()
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
                        .id("netease-ranks-scroll")
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
                .child(floating_scrollbar("netease-ranks-scrollbar", scroll_handle));
        }

        root
    }
}
