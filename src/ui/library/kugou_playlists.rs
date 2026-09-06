//! KuGou cloud playlists page: lists the logged-in user's playlists and their
//! tracks. Tracks are streamed on double-click via a freshly fetched play URL.
//! Gated behind the `kugou` cargo feature.

use std::collections::HashSet;

use cntp_i18n::{tr, trn};
use gpui::{
    App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use gpui::prelude::FluentBuilder;

use crate::{
    kugou,
    toasts::{Toast, emit_toast},
    ui::{
        components::{
            button::{ButtonIntent, button},
            icons::{ARROW_LEFT, PLAYLIST, icon},
            modal::modal,
            nav_button::nav_button,
            scrollbar::floating_scrollbar,
            textbox::Textbox,
        },
        kugou::{
            KugouPlaylistInfo, KugouTrackInfo, parse_playlists, parse_tracks,
        },
        library::view_header::view_header,
        settings::{SettingsSectionKind, open_settings_window_with_section},
        theme::Theme,
    },
};

/// How many tracks to fetch per page.
const TRACKS_PER_PAGE: i64 = 100;

/// uniform_list strides rows by one fixed height measured from the first
/// row, so track rows are pinned to their natural height: 8px vertical
/// padding × 2 + title line (text_sm) + 1px gap + subtitle line (text_xs)
/// + 1px bottom border.
const TRACK_ROW_HEIGHT: f32 = 60.0;

enum PlaylistsState {
    LoggedOut,
    Loading,
    Failed(SharedString),
    Ready(Vec<KugouPlaylistInfo>),
}

impl PlaylistsState {
    fn as_ready(&self) -> Option<&Vec<KugouPlaylistInfo>> {
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

#[derive(Clone)]
struct SelectedPlaylist {
    global_collection_id: String,
    listid: i64,
    name: SharedString,
}

pub struct KugouPlaylistsView {
    playlists: PlaylistsState,
    selected: Option<SelectedPlaylist>,
    tracks: Vec<KugouTrackInfo>,
    tracks_state: TracksState,
    /// last playlist page that was loaded (1-based, counts DOWN because the
    /// API returns tracks newest-first while we display oldest-first)
    track_page: i64,
    /// whether the playlist has more tracks than currently loaded
    has_more_tracks: bool,
    /// hashes of tracks liked during this session (drives the star icon)
    liked: HashSet<String>,
    /// hashes the user explicitly unliked during this session. Only relevant
    /// while viewing the "liked songs" playlist (listid 2), where every track
    /// is already liked by default.
    unliked: HashSet<String>,
    scroll_handle: ScrollHandle,
    /// Scroll position of the virtualized track list; reset per playlist so
    /// a newly opened playlist starts at the top.
    tracks_scroll_handle: UniformListScrollHandle,
    /// Import dialog state.
    import_open: bool,
    import_input: Option<Entity<Textbox>>,
    import_busy: bool,
    import_status: SharedString,
}

impl KugouPlaylistsView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let logged_in = kugou::shared_client().logged_in();

        cx.new(|cx| {
            let mut view = Self {
                playlists: PlaylistsState::Loading,
                selected: None,
                tracks: Vec::new(),
                tracks_state: TracksState::Idle,
                track_page: 0,
                has_more_tracks: false,
                liked: HashSet::new(),
                unliked: HashSet::new(),
                scroll_handle: ScrollHandle::new(),
                tracks_scroll_handle: UniformListScrollHandle::new(),
                import_open: false,
                import_input: None,
                import_busy: false,
                import_status: SharedString::default(),
            };

            if logged_in {
                view.refresh_playlists(cx);
                view.ensure_profile(cx);
            } else {
                view.playlists = PlaylistsState::LoggedOut;
            }

            view
        })
    }

    /// Fetch the user profile once if the cache is empty, then notify so the
    /// header card appears without a re-request on later visits.
    fn ensure_profile(&mut self, cx: &mut Context<Self>) {
        let client = kugou::shared_client();
        if client.cached_user_profile().is_some() {
            return;
        }
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let _ = crate::RUNTIME
                .spawn(async move { client.user_profile().await })
                .await;
            weak.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn refresh_playlists(&mut self, cx: &mut Context<Self>) {
        self.playlists = PlaylistsState::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let request =
                crate::RUNTIME.spawn(async move { client.user_playlists(1, 100).await }).await;

            let _ = this.update(cx, |this, cx| {
                this.playlists = match request {
                    Ok(Ok(response)) => PlaylistsState::Ready(parse_playlists(&response.body)),
                    Ok(Err(err)) => PlaylistsState::Failed(load_failed_message(&err)),
                    Err(err) => PlaylistsState::Failed(load_failed_message(&err)),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_playlist(&mut self, playlist: &KugouPlaylistInfo, cx: &mut Context<Self>) {
        self.selected = Some(SelectedPlaylist {
            global_collection_id: playlist.global_collection_id.clone(),
            listid: playlist.listid,
            name: playlist.name.clone(),
        });
        self.tracks.clear();
        self.tracks_state = TracksState::Loading;
        self.track_page = 0;
        self.has_more_tracks = false;
        self.unliked.clear();
        self.scroll_handle = ScrollHandle::new();
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();

        // warm the liked-set cache so the play-bar star lights up instantly
        // for tracks of this playlist
        if playlist.listid == 2 {
            crate::ui::kugou::prime_liked_cache();
        }

        self.load_first_page(cx);
    }

    fn load_more_tracks(&mut self, cx: &mut Context<Self>) {
        if self.track_page <= 1 {
            return;
        }
        // pages count DOWN: the API returns newest-first, we display
        // oldest-first, so "more" means the next older page
        let next_page = self.track_page - 1;
        self.tracks_state = TracksState::Loading;
        cx.notify();
        self.load_tracks_page(next_page, true, cx);
    }

    fn retry_tracks(&mut self, cx: &mut Context<Self>) {
        self.tracks_state = TracksState::Loading;
        cx.notify();
        self.load_first_page(cx);
    }

    /// Loads the first batch of tracks for the selected playlist.
    ///
    /// The v4 endpoint returns tracks newest-first (fsort descending), while
    /// the UI shows oldest-first. So a cheap probe of page 1 reads the total
    /// `data.count`, then the actual first batch is fetched from the LAST page
    /// and reversed; `load_more_tracks` walks backwards from there.
    fn load_first_page(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let listid = selected.listid;
            let request = crate::RUNTIME
                .spawn(async move {
                    // probe page 1 for the total count
                    let probe = client.playlist_tracks(listid, 1, TRACKS_PER_PAGE).await?;
                    let count = probe
                        .body
                        .pointer("/data/count")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let total_pages = if count > 0 {
                        (count + TRACKS_PER_PAGE - 1) / TRACKS_PER_PAGE
                    } else {
                        1
                    };

                    let response = if total_pages <= 1 {
                        probe
                    } else {
                        client
                            .playlist_tracks(listid, total_pages, TRACKS_PER_PAGE)
                            .await?
                    };
                    Ok::<_, kugou::KugouError>((response, total_pages))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                // the selection may have changed while the request was in flight
                let still_current = this
                    .selected
                    .as_ref()
                    .is_some_and(|s| s.global_collection_id == selected.global_collection_id);

                match request {
                    Ok(Ok((response, total_pages))) if still_current => {
                        let mut page_tracks = parse_tracks(&response.body, "/data/info");
                        page_tracks.reverse();
                        this.tracks = page_tracks;
                        this.track_page = total_pages;
                        this.has_more_tracks = total_pages > 1;
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

    fn load_tracks_page(&mut self, page: i64, append: bool, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let listid = selected.listid;
            let request = crate::RUNTIME.spawn(async move {
                client.playlist_tracks(listid, page, TRACKS_PER_PAGE).await
            })
            .await;

            let _ = this.update(cx, |this, cx| {
                // the selection may have changed while the request was in flight
                let still_current = this
                    .selected
                    .as_ref()
                    .is_some_and(|s| s.global_collection_id == selected.global_collection_id);

                match request {
                    Ok(Ok(response)) if still_current => {
                        let mut page_tracks = parse_tracks(&response.body, "/data/info");
                        page_tracks.reverse();
                        if append {
                            this.tracks.extend(page_tracks);
                        } else {
                            this.tracks = page_tracks;
                        }
                        this.track_page = page;
                        this.has_more_tracks = page > 1;
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
        cx.notify();
    }

    /// Whether `hash` should show as liked right now. Inside the "liked songs"
    /// playlist (listid 2) every row is liked by default unless the user
    /// unliked it this session; elsewhere it's the set of tracks liked this
    /// session.
    fn is_track_liked(&self, hash: &str) -> bool {
        if self.selected.as_ref().is_some_and(|s| s.listid == 2) {
            !self.unliked.contains(hash)
        } else {
            self.liked.contains(hash)
        }
    }

    // ---- External playlist import (NetEase / QQ → KuGou) ----

    fn open_import(&mut self, cx: &mut Context<Self>) {
        if self.import_input.is_none() {
            self.import_input =
                Some(Textbox::new_with_value_submit(cx, Default::default(), |_, _| {}));
        }
        self.import_open = true;
        self.import_busy = false;
        self.import_status = SharedString::default();
        cx.notify();
    }

    fn close_import(&mut self, cx: &mut Context<Self>) {
        self.import_open = false;
        self.import_busy = false;
        cx.notify();
    }

    fn begin_import(&mut self, cx: &mut Context<Self>) {
        if self.import_busy {
            return;
        }
        let text = self
            .import_input
            .as_ref()
            .map(|input| input.read(cx).value(cx).to_string())
            .unwrap_or_default();
        if text.trim().is_empty() {
            return;
        }

        self.import_busy = true;
        self.import_status = SharedString::from(tr!("KUGOU_IMPORT_PARSING", "Parsing link…").to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();

            let parsed = match kugou::import::parse_share_link(&text).await {
                Ok(p) => p,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.import_busy = false;
                        this.import_status = SharedString::from(tr!(
                            "KUGOU_IMPORT_FAILED",
                            "导入失败：{{err}}",
                            err = err
                        )
                        .to_string());
                        cx.notify();
                    });
                    return;
                }
            };

            let playlist_name = if parsed.name.trim().is_empty() {
                tr!("KUGOU_IMPORT_DEFAULT_NAME", "Imported playlist").to_string()
            } else {
                parsed.name
            };

            if let Err(err) = client.create_playlist(&playlist_name).await {
                let _ = this.update(cx, |this, cx| {
                    this.import_busy = false;
                    this.import_status = SharedString::from(tr!(
                        "KUGOU_IMPORT_CREATE_FAILED",
                        "Could not create playlist: {{err}}",
                        err = err.to_string()
                    )
                    .to_string());
                    cx.notify();
                });
                return;
            }

            let Some(listid) = find_playlist_id(&client, &playlist_name).await else {
                let _ = this.update(cx, |this, cx| {
                    this.import_busy = false;
                    this.import_status = SharedString::from(tr!(
                        "KUGOU_IMPORT_NOT_FOUND",
                        "Created playlist not found"
                    )
                    .to_string());
                    cx.notify();
                });
                return;
            };

            let total = parsed.songs.len() as i64;
            let mut matched: Vec<(String, String, i64, i64)> = Vec::new();
            for song in &parsed.songs {
                if let Some(m) = kugou_match(&client, song).await {
                    matched.push(m);
                }
            }

            if matched.is_empty() {
                let _ = this.update(cx, |this, cx| {
                    this.import_busy = false;
                    this.import_status = SharedString::from(tr!(
                        "KUGOU_IMPORT_NO_MATCH",
                        "No tracks matched"
                    )
                    .to_string());
                    this.refresh_playlists(cx);
                    cx.notify();
                });
                return;
            }

            for chunk in matched.chunks(100) {
                let _ = client.playlist_add_songs(listid, chunk).await;
            }

            let matched_count = matched.len() as i64;
            let summary = tr!(
                "KUGOU_IMPORT_DONE",
                "Imported: matched {{matched}}/{{total}}",
                matched = matched_count,
                total = total
            );
            let _ = this.update(cx, |this, cx| {
                this.import_busy = false;
                this.import_status = summary.clone().into();
                this.refresh_playlists(cx);
                cx.notify();
            });
            emit_toast(Toast::success(summary));
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(selected) = &self.selected {
            view_header(tr!("KUGOU_PLAYLISTS").to_string())
                .left(nav_button("kugou-back", ARROW_LEFT).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.close_playlist(cx);
                    },
                )))
                .subtitle(format!(
                    "{} • {}",
                    selected.name,
                    kugou_track_count(self.tracks.len() as i64)
                ))
        } else {
            view_header(tr!("KUGOU_PLAYLISTS").to_string()).right(
                button()
                    .id("kugou-import")
                    .child(tr!("KUGOU_IMPORT_PLAYLIST", "Import playlist (NetEase/QQ)"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_import(cx);
                    })),
            )
        }
    }

    fn render_playlist_row(
        &self,
        index: usize,
        playlist: &KugouPlaylistInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let row_id = playlist.global_collection_id.clone();

        div()
            .id(("kugou-playlist", index))
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
            .child(icon(PLAYLIST).w(px(16.0)).h(px(16.0)).flex_shrink(0.0))
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
                    .child(kugou_track_count(playlist.count)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let playlist = this
                    .playlists
                    .as_ready()
                    .and_then(|playlists| {
                        playlists
                            .iter()
                            .find(|p| p.global_collection_id == row_id)
                            .cloned()
                    });

                if let Some(playlist) = playlist {
                    this.open_playlist(&playlist, cx);
                }
            }))
    }

    /// Rows are built from inside the uniform_list render closure where only
    /// `&App` is available, so there is no `cx.listener` here: the like
    /// handler reaches the view through a weak handle instead (same pattern
    /// as the import modal's callbacks).
    fn render_track_row(
        &self,
        track: &KugouTrackInfo,
        index: usize,
        entity: &Entity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let liked = self.is_track_liked(&track.hash);
        let play = track.clone();
        let like = track.clone();
        let download = track.clone();
        let weak = entity.downgrade();

        crate::ui::kugou::kugou_track_row(
            theme,
            track,
            index,
            "kugou-track",
            true,
            true,
            liked,
            move |_, _, cx| {
                crate::ui::kugou::play_track_now(cx, &play);
            },
            move |_, _, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| {
                    let hash = like.hash.clone();
                    if this.is_track_liked(&hash) {
                        this.unliked.insert(hash.clone());
                        this.liked.remove(&hash);
                        crate::ui::kugou::unlike_track(cx, &like);
                    } else {
                        this.unliked.remove(&hash);
                        this.liked.insert(hash.clone());
                        crate::ui::kugou::like_track(cx, &like);
                    }
                    cx.notify();
                });
            },
            move |_, _, cx| {
                crate::ui::kugou::download_track_ui(cx, download.clone());
            },
        )
    }
}

fn load_failed_message(err: &impl std::fmt::Display) -> SharedString {
    tr!(
        "KUGOU_LOAD_FAILED",
        "Request failed: {{err}}",
        err = err.to_string()
    )
    .to_string()
    .into()
}

/// Localized "{count} track(s)" label. Single place where the plural string is
/// defined so the i18n generator doesn't see duplicate definitions.
fn kugou_track_count(count: i64) -> cntp_i18n::I18nString {
    trn!(
        "KUGOU_TRACK_COUNT",
        "{{count}} track",
        "{{count}} tracks",
        count = count
    )
}

impl Render for KugouPlaylistsView {
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
                    content = content.child(
                        div()
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .py(px(24.0))
                            .child(tr!("KUGOU_LOADING")),
                    );
                }
                TracksState::Failed(message) if self.tracks.is_empty() => {
                    content = content
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.status_error)
                                .py(px(12.0))
                                .child(message.clone()),
                        )
                        .child(
                            button()
                                .id("kugou-retry-tracks")
                                .child(tr!("KUGOU_RETRY", "Retry"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.retry_tracks(cx);
                                })),
                        );
                }
                _ if self.tracks.is_empty() => {
                    content = content.child(
                        div()
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .py(px(24.0))
                            .child(tr!("KUGOU_PLAYLIST_EMPTY", "This playlist is empty")),
                    );
                }
                // loaded tracks are rendered by the virtualized uniform_list
                // in the scroll branch below, not by page-flow content
                _ => {}
            }
        } else {
            // playlist overview
            match &self.playlists {
                PlaylistsState::LoggedOut => {
                    content = content
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.text_secondary)
                                .py(px(24.0))
                                .child(tr!(
                                    "KUGOU_LOGIN_REQUIRED",
                                    "Log in to KuGou in Settings to see your playlists."
                                )),
                        )
                        .child(
                            button()
                                .id("kugou-open-settings")
                                .intent(ButtonIntent::Primary)
                                .child(tr!("KUGOU_OPEN_SETTINGS", "Open Settings"))
                                .on_click(|_, _, cx| {
                                    open_settings_window_with_section(
                                        cx,
                                        SettingsSectionKind::Kugou,
                                    );
                                }),
                        );
                }
                PlaylistsState::Loading => {
                    content = content.child(
                        div()
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .py(px(24.0))
                            .child(tr!("KUGOU_LOADING")),
                    );
                }
                PlaylistsState::Failed(message) => {
                    content = content
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.status_error)
                                .py(px(12.0))
                                .child(message.clone()),
                        )
                        .child(
                            button()
                                .id("kugou-retry-playlists")
                                .child(tr!("KUGOU_RETRY"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.refresh_playlists(cx);
                                })),
                        );
                }
                PlaylistsState::Ready(playlists) => {
                    if playlists.is_empty() {
                        content = content.child(
                            div()
                                .text_sm()
                                .text_color(theme.text_secondary)
                                .py(px(24.0))
                                .child(tr!("KUGOU_NO_PLAYLISTS", "No playlists found")),
                        );
                    } else {
                        for (index, playlist) in playlists.iter().enumerate() {
                            content = content.child(self.render_playlist_row(index, playlist, cx));
                        }
                    }
                }
            }
        }

        let mut root = div()
            .id("kugou-playlists-view")
            .key_context("KugouPlaylists")
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
                    .id("kugou-playlists-track-container")
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
                                    "kugou-playlist-tracks",
                                    track_count,
                                    move |range, _, cx| {
                                        let start = range.start;
                                        let view = list_entity.read(cx);
                                        view.tracks[range]
                                            .iter()
                                            .enumerate()
                                            .map(|(i, track)| {
                                                div()
                                                    .h(px(TRACK_ROW_HEIGHT))
                                                    .child(view.render_track_row(
                                                        track,
                                                        start + i,
                                                        &list_entity,
                                                        cx,
                                                    ))
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
                            div()
                                .flex()
                                .justify_center()
                                .pt(px(12.0))
                                .child(
                                    button()
                                        .id("kugou-load-more")
                                        .child(tr!("KUGOU_LOAD_MORE", "Load More"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.load_more_tracks(cx);
                                        })),
                                ),
                        )
                    })
                    .child(floating_scrollbar(
                        "kugou-playlist-tracks-scrollbar",
                        tracks_scroll_handle,
                    )),
            );
        } else {
            root = root
                .child(
                    div()
                        .id("kugou-playlists-scroll")
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
                    "kugou-playlists-scrollbar",
                    scroll_handle,
                ));
        }

        root.when(self.import_open, |this| this.child(self.render_import_modal(cx)))
    }
}

impl KugouPlaylistsView {
    fn render_import_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let busy = self.import_busy;
        let status = self.import_status.clone();
        let input = self.import_input.clone();
        let weak = cx.entity().downgrade();

        modal()
            .on_exit(move |_window, cx| {
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |view, cx| {
                        view.import_open = false;
                        view.import_busy = false;
                        cx.notify();
                    });
                }
            })
            .child(
                div()
                    .w(px(420.0))
                    .flex_col()
                    .gap(px(12.0))
                    .p(px(20.0))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(tr!("KUGOU_IMPORT_TITLE", "Import playlist")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .child(tr!(
                                "KUGOU_IMPORT_HINT",
                                "Paste a NetEase Cloud Music or QQ Music playlist share link"
                            )),
                    )
                    .when_some(input, |this, input| this.child(input))
                    .when(!status.is_empty(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.text_secondary)
                                .child(status),
                        )
                    })
                    .child(
                        div().flex().gap(px(8.0)).justify_end().child(
                            button()
                                .id("kugou-import-cancel")
                                .child(tr!("KUGOU_IMPORT_CANCEL", "Cancel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_import(cx);
                                })),
                        )
                        .child(
                            button()
                                .id("kugou-import-start")
                                .intent(ButtonIntent::Primary)
                                .child(if busy {
                                    tr!("KUGOU_IMPORT_RUNNING", "Importing…")
                                } else {
                                    tr!("KUGOU_IMPORT_START", "Import")
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.begin_import(cx);
                                })),
                        ),
                    ),
            )
    }
}

/// Searches KuGou for the first playable track matching `name`. Returns
/// `(title, hash, album_id, mix_song_id)` for `playlist_add_songs`.
async fn kugou_match(
    client: &kugou::KugouClient,
    name: &str,
) -> Option<(String, String, i64, i64)> {
    let resp = client.search(name, 1, 1).await.ok()?;
    let track = parse_tracks(&resp.body, "/data/lists").into_iter().next()?;
    if track.hash.is_empty() {
        return None;
    }
    Some((track.title.to_string(), track.hash, track.album_id, track.mix_song_id))
}

/// Resolves the just-created playlist's numeric listid from the user's list
/// by name. The create response does not reliably carry it, and the list may
/// not reflect the new playlist immediately, so retry with a short pause.
async fn find_playlist_id(client: &kugou::KugouClient, name: &str) -> Option<i64> {
    for attempt in 0..3 {
        if let Ok(resp) = client.user_playlists(1, 100).await
            && let Some(pl) = parse_playlists(&resp.body)
                .into_iter()
                .find(|p| p.name.as_ref() == name)
        {
            return Some(pl.listid);
        }
        if attempt < 2 {
            let _ = crate::RUNTIME
                .spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                })
                .await;
        }
    }
    None
}
