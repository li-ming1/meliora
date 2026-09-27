//! NetEase cloud playlists page: the user's own and subscribed playlists,
//! paged from `/api/user/playlist`, with each playlist opening into its
//! append-paged track list. Tracks are streamed on click via a freshly
//! fetched play URL. Gated behind the `netease` cargo feature (via the
//! parent module).

use std::sync::Arc;

use cntp_i18n::{tr, trn};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Div, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};

use crate::{
    netease,
    ui::{
        components::{
            button::{ButtonIntent, button},
            icons::ARROW_LEFT,
            managed_image::{ManagedImageKey, managed_image},
            nav_button::nav_button,
            scrollbar::floating_scrollbar,
        },
        library::{EscapeBack, view_header::view_header},
        netease::{NeteasePlaylistInfo, NeteaseTrackInfo, parse_playlists, parse_tracks},
        settings::{SettingsSectionKind, open_settings_window_with_section},
        theme::Theme,
    },
};

/// How many playlists to fetch per page.
const PLAYLISTS_PER_PAGE: i64 = 30;

/// How many tracks to fetch per page.
const TRACKS_PER_PAGE: i64 = 50;

/// uniform_list strides rows by one fixed height measured from the first
/// row, so track rows are pinned to their natural height: 8px vertical
/// padding × 2 + title line (text_sm) + 1px gap + subtitle line (text_xs)
/// + 1px bottom border.
const TRACK_ROW_HEIGHT: f32 = 60.0;

enum PlaylistsState {
    LoggedOut,
    Loading,
    Failed(SharedString),
    Ready(Vec<NeteasePlaylistInfo>),
}

impl PlaylistsState {
    fn as_ready(&self) -> Option<&Vec<NeteasePlaylistInfo>> {
        match self {
            Self::Ready(playlists) => Some(playlists),
            _ => None,
        }
    }
}

enum TracksState {
    Idle,
    Loading,
    Failed(SharedString),
}

pub struct NeteasePlaylistsView {
    playlists: PlaylistsState,
    /// last playlists page that was loaded (1-based)
    playlist_page: i64,
    /// whether the account has more playlists than currently loaded
    has_more_playlists: bool,
    /// set while a playlists request is in flight; only the newest request
    /// (see `playlists_generation`) may clear it, so clicks can't stack pages
    playlists_loading: bool,
    /// bumped on every playlists request; an older response landing after a
    /// newer one was issued must not touch the list
    playlists_generation: u64,
    selected: Option<NeteasePlaylistInfo>,
    /// Arc-shared so the track-row closures capture refcounts, not deep
    /// clones (every visible row re-clones its track every redraw).
    tracks: Vec<Arc<NeteaseTrackInfo>>,
    tracks_state: TracksState,
    /// last track page that was loaded (1-based)
    track_page: i64,
    /// whether the playlist has more tracks than currently loaded
    has_more_tracks: bool,
    /// full trackIds of the open playlist, fetched once together with the
    /// first page and sliced locally for every page after; lives and dies
    /// with the view (dropped when another playlist is opened or closed)
    track_ids: Option<Arc<[i64]>>,
    /// bumped on every track request so a stale page can't append after a
    /// newer one was issued for the same playlist
    track_generation: u64,
    scroll_handle: ScrollHandle,
    /// Scroll position of the virtualized track list; reset per playlist so
    /// a newly opened playlist starts at the top.
    tracks_scroll_handle: UniformListScrollHandle,
}

impl NeteasePlaylistsView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let logged_in = netease::shared_client().logged_in();

        cx.new(|cx| {
            let mut view = Self {
                playlists: PlaylistsState::Loading,
                playlist_page: 0,
                has_more_playlists: false,
                playlists_loading: false,
                playlists_generation: 0,
                selected: None,
                tracks: Vec::new(),
                tracks_state: TracksState::Idle,
                track_page: 0,
                has_more_tracks: false,
                track_ids: None,
                track_generation: 0,
                scroll_handle: ScrollHandle::new(),
                tracks_scroll_handle: UniformListScrollHandle::new(),
            };

            if logged_in {
                view.load_playlists(cx);
            } else {
                view.playlists = PlaylistsState::LoggedOut;
            }

            view
        })
    }

    fn load_playlists(&mut self, cx: &mut Context<Self>) {
        self.playlists = PlaylistsState::Loading;
        self.playlist_page = 0;
        self.has_more_playlists = false;
        cx.notify();
        self.load_playlists_page(1, false, cx);
    }

    fn load_more_playlists(&mut self, cx: &mut Context<Self>) {
        // one request in flight at a time; `playlists_loading` stays set
        // until the newest request's response lands
        if self.has_more_playlists && !self.playlists_loading {
            self.load_playlists_page(self.playlist_page + 1, true, cx);
        }
    }

    /// Fetches one page of the user's playlists. A full reload (append =
    /// false) goes through `load_playlists`, which resets the state; an
    /// append extends the currently visible list in place.
    fn load_playlists_page(&mut self, page: i64, append: bool, cx: &mut Context<Self>) {
        self.playlists_loading = true;
        self.playlists_generation += 1;
        let generation = self.playlists_generation;

        cx.spawn(async move |this, cx| {
            let client = netease::shared_client();
            let uid = client.user_id().unwrap_or(0);
            let offset = (page - 1) * PLAYLISTS_PER_PAGE;
            let request = crate::RUNTIME
                .spawn(async move { client.user_playlists(uid, PLAYLISTS_PER_PAGE, offset).await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    // a newer request (another page or a full reload) supersedes
                    // this one
                    let current = this.playlists_generation == generation;
                    match request {
                        Ok(Ok(response)) if current => {
                            let page_playlists = parse_playlists(&response.body);
                            // the endpoint carries no total, so a full page is
                            // the "maybe more" signal
                            this.has_more_playlists =
                                page_playlists.len() as i64 >= PLAYLISTS_PER_PAGE;
                            this.playlist_page = page;
                            if append {
                                if let PlaylistsState::Ready(playlists) = &mut this.playlists {
                                    playlists.extend(page_playlists);
                                }
                            } else {
                                this.playlists = PlaylistsState::Ready(page_playlists);
                            }
                        }
                        // an in-flight "load more" keeps the visible list (and
                        // its still-present Load More button) as-is for a retry
                        Ok(Err(err)) if !append && current => {
                            this.playlists = PlaylistsState::Failed(load_failed_message(&err));
                        }
                        Err(err) if !append && current => {
                            this.playlists = PlaylistsState::Failed(load_failed_message(&err));
                        }
                        _ => {}
                    }
                    // only the newest request may release the in-flight gate
                    if current {
                        this.playlists_loading = false;
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn open_playlist(&mut self, playlist: &NeteasePlaylistInfo, cx: &mut Context<Self>) {
        self.selected = Some(playlist.clone());
        self.tracks.clear();
        self.tracks_state = TracksState::Loading;
        self.track_page = 0;
        self.has_more_tracks = false;
        // the cached trackIds belong to the previously open playlist
        self.track_ids = None;
        self.scroll_handle = ScrollHandle::new();
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();
        self.load_tracks_page(1, cx);
    }

    fn load_more_tracks(&mut self, cx: &mut Context<Self>) {
        // one page in flight at a time: tracks_state stays Loading from the
        // click until the response lands, so extra clicks are ignored
        if self.has_more_tracks && !matches!(self.tracks_state, TracksState::Loading) {
            self.load_tracks_page(self.track_page + 1, cx);
        }
    }

    /// Fetches one page of the selected playlist's tracks and appends it to
    /// the visible list. The first page also fetches (and caches) the full
    /// trackIds list; later pages slice that cache locally and only pay for
    /// one song-detail request instead of re-downloading the whole playlist
    /// detail every time.
    fn load_tracks_page(&mut self, page: i64, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
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
            let playlist_id = selected.id;
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
                            let ids = client.playlist_track_ids(playlist_id).await?;
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
                    let still_current = this.selected.as_ref().is_some_and(|p| p.id == selected.id)
                        && this.track_generation == generation;

                    match request {
                        Ok(Ok((ids, response))) if still_current => {
                            this.track_ids = Some(ids);
                            let page_tracks = parse_tracks(&response.body, "/songs");
                            // the song-detail response carries no total, so a full
                            // page is the "maybe more" signal
                            this.has_more_tracks = page_tracks.len() as i64 >= TRACKS_PER_PAGE;
                            this.tracks.extend(page_tracks.into_iter().map(Arc::new));
                            this.track_page = page;
                            this.tracks_state = TracksState::Idle;
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(err)) if still_current => {
                            this.tracks_state = TracksState::Failed(load_failed_message(&err));
                        }
                        Ok(Err(_)) => {}
                        Err(err) if still_current => {
                            this.tracks_state = TracksState::Failed(load_failed_message(&err));
                        }
                        Err(_) => {}
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn close_playlist(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.tracks.clear();
        self.tracks_state = TracksState::Idle;
        self.track_page = 0;
        self.has_more_tracks = false;
        self.track_ids = None;
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(selected) = &self.selected {
            view_header(tr!("NETEASE_PLAYLISTS", "NetEase Playlists").to_string())
                .left(nav_button("netease-back", ARROW_LEFT).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.close_playlist(cx);
                    },
                )))
                .subtitle(format!(
                    "{} • {}",
                    selected.name,
                    netease_track_count(self.tracks.len() as i64)
                ))
        } else {
            view_header(tr!("NETEASE_PLAYLISTS").to_string())
        }
    }

    fn render_playlist_row(
        &self,
        index: usize,
        playlist: &NeteasePlaylistInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let row_id = playlist.id;

        div()
            .id(("netease-playlist", index))
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(4.0))
            .py(px(8.0))
            .pl(px(6.0))
            .border_b_1()
            .border_color(theme.border_color)
            .cursor_pointer()
            .hover(|this| this.bg(theme.queue_item_hover))
            .child(
                managed_image(
                    ("netease-playlist-cover", index),
                    ManagedImageKey::HttpCover(playlist.cover_url.clone()),
                )
                .thumb()
                .w(px(40.0))
                .h(px(40.0))
                .rounded(px(theme.radius_md))
                .flex_shrink(0.0),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .flex_shrink(1.0)
                    .overflow_x_hidden()
                    .text_ellipsis()
                    .child(playlist.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_secondary)
                    .ml_auto()
                    .pl(px(8.0))
                    .flex_shrink(0.0)
                    .child(netease_track_count(playlist.count)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let playlist = this
                    .playlists
                    .as_ready()
                    .and_then(|playlists| playlists.iter().find(|p| p.id == row_id).cloned());

                if let Some(playlist) = playlist {
                    this.open_playlist(&playlist, cx);
                }
            }))
    }

    /// Rows are built from inside the uniform_list render closure where only
    /// `&App` is available, so there is no `cx.listener` here; the like/play
    /// semantics live entirely in the shared NetEase helpers and need no view
    /// state.
    fn render_track_row(
        &self,
        track: &Arc<NeteaseTrackInfo>,
        index: usize,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let liked = crate::ui::netease::liked_set_contains(track.id);
        // The row closures must own their data ('static): capture Arc clones
        // (refcount bump only) instead of three full clones per visible row
        // per redraw.
        let play = track.clone();
        let like = track.clone();
        let download = track.clone();

        crate::ui::netease::netease_track_row(
            theme,
            track,
            index,
            "netease-playlist-track",
            true,
            false,
            liked,
            move |_, _, cx| {
                crate::ui::netease::play_track_now(cx, &play);
            },
            move |_, _, cx| {
                if crate::ui::netease::liked_set_contains(like.id) {
                    crate::ui::netease::unlike_track(cx, &like);
                } else {
                    crate::ui::netease::like_track(cx, &like);
                }
            },
            move |_, _, cx| {
                crate::ui::netease::download_track_ui(cx, (*download).clone());
            },
        )
    }
}

fn load_failed_message(err: &impl std::fmt::Display) -> SharedString {
    tr!("NETEASE_LOAD_FAILED", err = err.to_string())
        .to_string()
        .into()
}

/// Muted placeholder line for the loading and empty states of both the
/// overview and the track list.
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

/// Localized "{count} track(s)" label. Single place where the plural string is
/// defined so the i18n generator doesn't see duplicate definitions.
fn netease_track_count(count: i64) -> cntp_i18n::I18nString {
    trn!(
        "NETEASE_TRACK_COUNT",
        "{{count}} track",
        "{{count}} tracks",
        count = count
    )
}

impl Render for NeteasePlaylistsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let scroll_handle = self.scroll_handle.clone();

        // Once a playlist's tracks are loaded the list itself becomes the
        // scroll container (uniform_list); the page-level scroller only
        // serves the overview and the small loading/error/empty states.
        let tracks_ready = self.selected.is_some() && !self.tracks.is_empty();

        let mut content = div().flex().flex_col().pb(px(24.0));

        if self.selected.is_some() {
            // track list of the open playlist
            match &self.tracks_state {
                TracksState::Loading if self.tracks.is_empty() => {
                    content = content.child(muted_line(tr!("NETEASE_LOADING"), &theme));
                }
                TracksState::Failed(message) if self.tracks.is_empty() => {
                    content = content.child(error_line(message.clone(), &theme)).child(
                        button()
                            .id("netease-retry-tracks")
                            .child(tr!("NETEASE_RETRY"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_tracks_page(1, cx);
                            })),
                    );
                }
                _ if self.tracks.is_empty() => {
                    content = content.child(muted_line(
                        tr!("NETEASE_PLAYLIST_EMPTY", "This playlist is empty"),
                        &theme,
                    ));
                }
                // loaded tracks are rendered by the virtualized uniform_list
                // in the scroll branch below, not by page-flow content
                _ => {}
            }
        } else {
            // playlist overview
            match &self.playlists {
                PlaylistsState::LoggedOut => {
                    content = content.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(12.0))
                            .py(px(48.0))
                            .w_full()
                            .child(div().text_sm().text_color(theme.text_secondary).child(tr!(
                                "NETEASE_LOGIN_REQUIRED",
                                "Log in to NetEase Cloud Music in Settings to see your playlists."
                            )))
                            .child(
                                button()
                                    .id("netease-open-settings")
                                    .intent(ButtonIntent::Primary)
                                    .child(tr!("NETEASE_OPEN_SETTINGS"))
                                    .on_click(|_, _, cx| {
                                        open_settings_window_with_section(
                                            cx,
                                            SettingsSectionKind::Netease,
                                        );
                                    }),
                            ),
                    );
                }
                PlaylistsState::Loading => {
                    content = content.child(muted_line(tr!("NETEASE_LOADING"), &theme));
                }
                PlaylistsState::Failed(message) => {
                    content = content.child(error_line(message.clone(), &theme)).child(
                        button()
                            .id("netease-retry-playlists")
                            .child(tr!("NETEASE_RETRY"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_playlists(cx);
                            })),
                    );
                }
                PlaylistsState::Ready(playlists) => {
                    if playlists.is_empty() {
                        content = content.child(muted_line(
                            tr!("NETEASE_NO_PLAYLISTS", "No playlists found"),
                            &theme,
                        ));
                    } else {
                        for (index, playlist) in playlists.iter().enumerate() {
                            content = content.child(self.render_playlist_row(index, playlist, cx));
                        }

                        if self.has_more_playlists {
                            content = content.child(
                                div().flex().justify_center().pt(px(12.0)).child(
                                    button()
                                        .id("netease-load-more-playlists")
                                        .child(tr!("NETEASE_LOAD_MORE"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.load_more_playlists(cx);
                                        })),
                                ),
                            );
                        }
                    }
                }
            }
        }

        let mut root = div()
            .id("netease-playlists-view")
            .key_context("NeteasePlaylists")
            .on_action(cx.listener(|this, _: &EscapeBack, _, cx| {
                // Escape leaves the open playlist back to the overview
                if this.selected.is_some() {
                    this.close_playlist(cx);
                }
            }))
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(self.render_header(cx));

        if tracks_ready {
            // the uniform_list is its own scroll container, so the page-level
            // scroller is replaced wholesale while tracks are on screen
            let track_count = self.tracks.len();
            let has_more_tracks = self.has_more_tracks;
            let list_entity = cx.entity();
            let tracks_scroll_handle = self.tracks_scroll_handle.clone();

            root = root.child(
                div()
                    .id("netease-playlists-track-container")
                    .relative()
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
                    .pb(px(24.0))
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .flex_grow(1.0)
                            .min_h(px(0.0))
                            .child(
                                uniform_list(
                                    "netease-playlist-tracks",
                                    track_count,
                                    move |range, _, cx| {
                                        let start = range.start;
                                        let view = list_entity.read(cx);
                                        view.tracks[range]
                                            .iter()
                                            .enumerate()
                                            .map(|(i, track)| {
                                                div().h(px(TRACK_ROW_HEIGHT)).child(
                                                    view.render_track_row(track, start + i, cx),
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
                        this.child(
                            div().flex().justify_center().pt(px(12.0)).child(
                                button()
                                    .id("netease-load-more-tracks")
                                    .child(tr!("NETEASE_LOAD_MORE"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load_more_tracks(cx);
                                    })),
                            ),
                        )
                    })
                    .child(floating_scrollbar(
                        "netease-playlist-tracks-scrollbar",
                        tracks_scroll_handle,
                    )),
            );
        } else {
            root = root
                .child(
                    div()
                        .id("netease-playlists-scroll")
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
                    "netease-playlists-scrollbar",
                    scroll_handle,
                ));
        }

        root
    }
}
