//! KuGou discovery page: the platform rank list with per-rank tracks and the
//! daily recommend playlist. Streams online tracks like the playlists page.
//! Gated behind the `kugou` cargo feature.

use std::collections::HashSet;

use cntp_i18n::tr;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px,
};
use gpui::prelude::FluentBuilder;

use crate::{
    kugou,
    ui::{
        components::{
            button::{ButtonIntent, button},
            scrollbar::floating_scrollbar,
            managed_image::{ManagedImageKey, managed_image},
        },
        kugou::{
            KugouRank, KugouTrackInfo, parse_rank_tracks, parse_ranks, parse_recommend_tracks,
        },
        library::{EscapeBack, view_header::view_header},
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
    Ready(Vec<KugouRank>),
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
    Ready(Vec<KugouTrackInfo>),
}

pub struct KugouRanksView {
    tab: Tab,
    ranks: RanksState,
    selected: Option<KugouRank>,
    tracks: Vec<KugouTrackInfo>,
    tracks_state: TracksState,
    track_page: i64,
    has_more_tracks: bool,
    recommend: RecommendState,
    /// hashes liked during this session (drives the star icon)
    liked: HashSet<String>,
    /// hashes with a like/unlike request in flight; extra clicks on those
    /// rows are ignored until the request settles
    like_in_flight: HashSet<String>,
    scroll_handle: ScrollHandle,
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
            let client = kugou::shared_client();
            let request = crate::RUNTIME.spawn(async move { client.rank_list().await }).await;

            let _ = this.update(cx, |this, cx| {
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
            let request =
                crate::RUNTIME.spawn(async move { client.everyday_recommend().await }).await;

            let _ = this.update(cx, |this, cx| {
                this.recommend = match request {
                    Ok(Ok(response)) => {
                        let tracks = parse_recommend_tracks(&response.body);
                        if tracks.is_empty() {
                            RecommendState::Failed(
                                tr!("KUGOU_NO_RECOMMEND", "No recommendations today").into(),
                            )
                        } else {
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
        cx.notify();
        self.load_tracks_page(1, cx);
    }

    fn load_tracks_page(&mut self, page: i64, cx: &mut Context<Self>) {
        let Some(rank) = self.selected.clone() else {
            return;
        };

        self.tracks_state = TracksState::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let rankid = rank.rankid;
            let request = crate::RUNTIME
                .spawn(async move { client.rank_audio(rankid, page, TRACKS_PER_PAGE).await })
                .await;

            let _ = this.update(cx, |this, cx| {
                let still_current =
                    this.selected.as_ref().is_some_and(|r| r.rankid == rank.rankid);

                match request {
                    Ok(Ok(response)) if still_current => {
                        let total = response
                            .body
                            .pointer("/data/total")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0);
                        let page_tracks = parse_rank_tracks(&response.body);
                        this.tracks.extend(page_tracks);
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

    fn render_ranks_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let ranks: &[KugouRank] = match &self.ranks {
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
                    .id(("kugou-rank", index))
                    .flex()
                    .flex_col()
                    .w(px(148.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_rank(rank_clone.clone(), cx);
                    }))
                    .child(
                        managed_image(
                            ("kugou-rank-cover", index),
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
                        rank.previews
                            .first()
                            .map(|preview| {
                                div()
                                    .mt(px(2.0))
                                    .text_xs()
                                    .text_color(theme.text_secondary)
                                    .overflow_x_hidden()
                                    .text_ellipsis()
                                    .child(preview.clone())
                            })
                            .unwrap_or(div()),
                    )
            }))
    }

    fn render_track_row(
        &self,
        track: &KugouTrackInfo,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let liked = self.is_liked(&track.hash);
        let play = track.clone();
        let like = track.clone();
        let download = track.clone();

        crate::ui::kugou::kugou_track_row(
            theme,
            track,
            index,
            "kugou-rank-track",
            true,
            false,
            liked,
            cx.listener(move |_, _, _, cx| {
                crate::ui::kugou::play_track_now(cx, &play);
            }),
            cx.listener(move |this, _, _, cx| this.toggle_like(&like, cx)),
            cx.listener(move |_, _, _, cx| {
                crate::ui::kugou::download_track_ui(cx, download.clone());
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
                    .child(tr!("KUGOU_LOADING"))
                    .into_any_element(),
                RanksState::Failed(message) => div()
                    .text_sm()
                    .text_color(theme.status_error)
                    .py(px(12.0))
                    .child(message.clone())
                    .into_any_element(),
                RanksState::Ready(_) => self.render_ranks_grid(cx).into_any_element(),
            },
            Tab::DailyRecommend => match &self.recommend {
                RecommendState::Idle | RecommendState::Loading => div()
                    .text_sm()
                    .text_color(theme.text_secondary)
                    .py(px(24.0))
                    .child(tr!("KUGOU_LOADING"))
                    .into_any_element(),
                RecommendState::Failed(message) => div()
                    .text_sm()
                    .text_color(theme.status_error)
                    .py(px(12.0))
                    .child(message.clone())
                    .into_any_element(),
                RecommendState::Ready(tracks) => div()
                    .flex()
                    .flex_col()
                    .children(
                        tracks
                            .iter()
                            .enumerate()
                            .map(|(index, track)| {
                                self.render_track_row(track, index, cx).into_any_element()
                            }),
                    )
                    .into_any_element(),
            },
        }
    }
}

impl Render for KugouRanksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let scroll_handle = self.scroll_handle.clone();

        let content: AnyElement = if let Some(rank) = self.selected.as_ref() {
            match &self.tracks_state {
                TracksState::Loading if self.tracks.is_empty() => div()
                    .text_sm()
                    .text_color(theme.text_secondary)
                    .py(px(24.0))
                    .child(tr!("KUGOU_LOADING"))
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
                                self.render_track_row(track, index, cx).into_any_element()
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
                                        .id("kugou-rank-load-more")
                                        .child(tr!("KUGOU_LOAD_MORE"))
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
            .child(self.render_header(cx))
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
            .child(floating_scrollbar(
                "kugou-ranks-scrollbar",
                scroll_handle,
            ))
    }
}