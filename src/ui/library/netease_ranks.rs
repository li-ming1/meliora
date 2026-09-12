//! NetEase discovery page: Top Charts (toplist) and Daily Recommend tabs.
//! Gated behind the `netease` cargo feature (via the parent module).

use std::collections::HashSet;
use std::sync::Arc;

use cntp_i18n::tr;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px,
};
use gpui::prelude::FluentBuilder;

use crate::{
    netease,
    ui::{
        components::{
            button::{ButtonIntent, button},
            managed_image::{ManagedImageKey, managed_image},
            scrollbar::floating_scrollbar,
        },
        library::{EscapeBack, view_header::view_header},
        netease::{NeteaseRank, NeteaseTrackInfo, parse_ranks, parse_tracks},
        settings::{SettingsSectionKind, open_settings_window_with_section},
        theme::Theme,
    },
};

const TRACKS_PER_PAGE: i64 = 30;

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
    Ready(Vec<NeteaseRank>),
}

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
    Ready(Vec<NeteaseTrackInfo>),
}

pub struct NeteaseRanksView {
    tab: Tab,
    ranks: RanksState,
    selected: Option<NeteaseRank>,
    tracks: Vec<NeteaseTrackInfo>,
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
            let request = crate::RUNTIME.spawn(async move { client.toplist().await }).await;

            let _ = this.update(cx, |this, cx| {
                this.ranks = match request {
                    Ok(Ok(response)) => {
                        let ranks = parse_ranks(&response.body);
                        if ranks.is_empty() {
                            RanksState::Failed(tr!("NETEASE_NO_RANKS", "No charts found").into())
                        } else {
                            RanksState::Ready(ranks)
                        }
                    }
                    Ok(Err(err)) => RanksState::Failed(
                        tr!("NETEASE_LOAD_FAILED", "Request failed: {{err}}", err = err.to_string())
                            .into(),
                    ),
                    Err(err) => RanksState::Failed(
                        tr!("NETEASE_LOAD_FAILED", err = err.to_string())
                            .into(),
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
            let request =
                crate::RUNTIME.spawn(async move { client.recommend_songs().await }).await;

            let _ = this.update(cx, |this, cx| {
                this.recommend = match request {
                    Ok(Ok(response)) => {
                        let tracks = parse_tracks(&response.body, "/data/dailySongs");
                        if tracks.is_empty() {
                            RecommendState::Failed(
                                tr!("NETEASE_NO_RECOMMEND", "No recommendations today").into(),
                            )
                        } else {
                            RecommendState::Ready(tracks)
                        }
                    }
                    // 301: the endpoint requires a logged-in session
                    Ok(Err(err)) if err.status() == 301 => RecommendState::LoginRequired,
                    Ok(Err(err)) => RecommendState::Failed(
                        tr!("NETEASE_LOAD_FAILED", err = err.to_string())
                            .into(),
                    ),
                    Err(err) => RecommendState::Failed(
                        tr!("NETEASE_LOAD_FAILED", err = err.to_string())
                            .into(),
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

            let _ = this.update(cx, |this, cx| {
                // the selection may have changed, or a newer page may have
                // been requested, while this request was in flight
                let still_current = this.selected.as_ref().is_some_and(|r| r.id == rank.id)
                    && this.track_generation == generation;

                match request {
                    Ok(Ok((ids, response))) if still_current => {
                        this.track_ids = Some(ids);
                        let page_tracks = parse_tracks(&response.body, "/songs");
                        // the song-detail response carries no total, so a full
                        // page is the "maybe more" signal
                        this.has_more_tracks = page_tracks.len() as i64 >= TRACKS_PER_PAGE;
                        this.tracks.extend(page_tracks);
                        this.track_page = page;
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
        self.track_ids = None;
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
                    .timer(std::time::Duration::from_millis(250))
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
                    .child(self.tab_button(
                        Tab::Ranks,
                        tr!("NETEASE_RANKS").to_string(),
                        cx,
                    ))
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

    fn render_ranks_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let ranks: &[NeteaseRank] = match &self.ranks {
            RanksState::Ready(ranks) => ranks,
            _ => &[],
        };

        div()
            .flex()
            .flex_wrap()
            .gap(px(18.0))
            .children(ranks.iter().enumerate().map(|(index, rank)| {
                let theme = theme.clone();
                let rank_clone = rank.clone();
                div()
                    .id(("netease-rank", index))
                    .flex()
                    .flex_col()
                    .w(px(148.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_rank(rank_clone.clone(), cx);
                    }))
                    .child(
                        managed_image(
                            ("netease-rank-cover", index),
                            ManagedImageKey::HttpCover(rank.cover_url.clone()),
                        )
                        .thumb_max(256)
                        .w(px(148.0))
                        .h(px(148.0))
                        .rounded(px(theme.radius_md)),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .overflow_x_hidden()
                            .text_ellipsis()
                            .child(rank.name.clone()),
                    )
                    .child(
                        div()
                            .mt(px(2.0))
                            .text_xs()
                            .text_color(theme.text_secondary)
                            .overflow_x_hidden()
                            .text_ellipsis()
                            .child(rank.update_frequency.clone()),
                    )
            }))
    }

    fn render_track_row(
        &self,
        track: &NeteaseTrackInfo,
        index: usize,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let liked = self.is_liked(track.id);
        let play = track.clone();
        let like = track.clone();
        let download = track.clone();

        crate::ui::netease::netease_track_row(
            theme,
            track,
            index,
            id_prefix,
            true,
            false,
            liked,
            cx.listener(move |_, _, _, cx| {
                crate::ui::netease::play_track_now(cx, &play);
            }),
            cx.listener(move |this, _, _, cx| this.toggle_like(&like, cx)),
            cx.listener(move |_, _, _, cx| {
                crate::ui::netease::download_track_ui(cx, download.clone());
            }),
        )
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.global::<Theme>().clone();

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
                        div()
                            .flex()
                            .justify_center()
                            .child(
                                button()
                                    .id("netease-ranks-retry")
                                    .child(tr!("NETEASE_RETRY", "Retry"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load_ranks(cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
                RanksState::Ready(_) => self.render_ranks_grid(cx).into_any_element(),
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
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .child(tr!(
                                "NETEASE_DAILY_LOGIN_REQUIRED",
                                "Log in to see your daily recommendations"
                            )),
                    )
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
                        div()
                            .flex()
                            .justify_center()
                            .child(
                                button()
                                    .id("netease-recommend-retry")
                                    .child(tr!("NETEASE_RETRY"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load_recommend(cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
                RecommendState::Ready(tracks) => div()
                    .flex()
                    .flex_col()
                    .children(
                        tracks
                            .iter()
                            .enumerate()
                            .map(|(index, track)| {
                                self.render_track_row(track, index, "netease-daily-track", cx)
                                    .into_any_element()
                            }),
                    )
                    .into_any_element(),
            },
        }
    }
}

impl Render for NeteaseRanksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let scroll_handle = self.scroll_handle.clone();

        let content: AnyElement = if let Some(rank) = self.selected.as_ref() {
            match &self.tracks_state {
                TracksState::Loading if self.tracks.is_empty() => div()
                    .text_sm()
                    .text_color(theme.text_secondary)
                    .py(px(24.0))
                    .child(tr!("NETEASE_LOADING"))
                    .into_any_element(),
                TracksState::Failed(message) if self.tracks.is_empty() => div()
                    .text_sm()
                    .text_color(theme.status_error)
                    .py(px(12.0))
                    .child(message.clone())
                    .into_any_element(),
                _ => div()
                    .flex()
                    .flex_col()
                    .child(div().text_lg().font_weight(FontWeight::BOLD).pb(px(8.0)).child(rank.name.clone()))
                    .children(
                        self.tracks
                            .iter()
                            .enumerate()
                            .map(|(index, track)| {
                                self.render_track_row(track, index, "netease-rank-track", cx)
                                    .into_any_element()
                            }),
                    )
                    .when(self.has_more_tracks, |this| {
                        this.child(
                            div()
                                .flex()
                                .justify_center()
                                .pt(px(12.0))
                                .child(
                                    button()
                                        .id("netease-rank-load-more")
                                        .child(tr!("NETEASE_LOAD_MORE", "Load More"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.load_more(cx);
                                        })),
                                ),
                        )
                    })
                    .into_any_element(),
            }
        } else {
            self.render_content(cx)
        };

        div()
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
            .child(self.render_header(cx))
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
            .child(floating_scrollbar(
                "netease-ranks-scrollbar",
                scroll_handle,
            ))
    }
}
