mod replaygain;

use crate::{
    playback::{
        events::RepeatState, interface::PlaybackInterface, queue::QueueItemUIData,
        thread::PlaybackState,
    },
    settings::SettingsGlobal,
    ui::{
        components::{
            context::context,
            icons::{
                MENU, MICROPHONE, NEXT_TRACK, PAUSE, PLAY, PREV_TRACK, REPEAT, REPEAT_OFF,
                REPEAT_ONCE, SHUFFLE, STAR, STAR_FILLED, VOLUME, VOLUME_OFF, icon,
            },
            managed_image::{ManagedImageKey, managed_image},
            menu::{menu, menu_check_item, menu_item},
            tooltip::{build_tooltip, tooltip_container},
            transition::TransitionExt,
        },
        library::context_menus::{
            info_section::InfoSectionContextMenu, navigate_to_track_album_and_reveal,
            navigate_to_track_artist,
        },
        models::{
            CurrentTrack, HasLikedState, LIKED_SONGS_PLAYLIST_ID, subscribe_liked_updates,
            toggle_like,
        },
    },
};
#[cfg(feature = "kugou")]
use crate::ui::kugou::{
    KugouTrackInfo, like_track as kugou_like_track, online_track_is_liked,
    online_track_matching_path, unlike_track as kugou_unlike_track,
};
#[cfg(feature = "netease")]
use crate::ui::netease::{
    NeteaseTrackInfo, like_track as netease_like_track,
    online_track_is_liked as netease_track_is_liked,
    online_track_matching_path as netease_track_matching_path, unlike_track as netease_unlike_track,
};

/// The currently playing online track, if any: the running stream URL is
/// mapped back to its source through the per-source registries.
#[cfg(any(feature = "kugou", feature = "netease"))]
#[derive(Clone, Debug, PartialEq)]
enum OnlinePlayingTrack {
    #[cfg(feature = "kugou")]
    Kugou(KugouTrackInfo),
    #[cfg(feature = "netease")]
    Netease(NeteaseTrackInfo),
}

#[cfg(any(feature = "kugou", feature = "netease"))]
impl OnlinePlayingTrack {
    /// Identity key used to detect "still the same track" across async
    /// callbacks: the KuGou hash or the NetEase song id.
    fn identity(&self) -> String {
        match self {
            #[cfg(feature = "kugou")]
            OnlinePlayingTrack::Kugou(track) => format!("kugou:{}", track.hash),
            #[cfg(feature = "netease")]
            OnlinePlayingTrack::Netease(track) => format!("netease:{}", track.id),
        }
    }

    fn matches_identity(&self, identity: &str) -> bool {
        self.identity() == identity
    }
}
use cntp_i18n::tr;
use gpui::{InteractiveElement, *};
use prelude::FluentBuilder;
use std::{path::PathBuf, rc::Rc, time::Duration};

use self::replaygain::ReplayGainButton;
use super::{
    components::{
        resizable::{ResizeEdge, resizable},
        slider::slider,
    },
    global_actions::{Next, PlayPause, Previous, StopAfterCurrent},
    models::{Models, PlaybackInfo},
    theme::Theme,
};

use crate::library::types::Track;
use crate::settings::storage::{DEFAULT_CONTROLS_LEFT_WIDTH, DEFAULT_CONTROLS_RIGHT_WIDTH};
use crate::ui::util::format_duration;

pub struct Controls {
    info_section: Entity<InfoSection>,
    scrubber: Entity<Scrubber>,
    secondary_controls: Entity<SecondaryControls>,
    left_width: Entity<Pixels>,
    right_width: Entity<Pixels>,
}

impl Controls {
    pub fn new(cx: &mut App, show_queue: Entity<bool>, show_lyrics: Entity<bool>) -> Entity<Self> {
        let models = cx.global::<Models>();
        let left_width = models.controls_left_width.clone();
        let right_width = models.controls_right_width.clone();
        cx.new(|cx| Self {
            info_section: InfoSection::new(cx),
            scrubber: Scrubber::new(cx),
            secondary_controls: SecondaryControls::new(cx, show_queue, show_lyrics),
            left_width,
            right_width,
        })
    }
}

impl Render for Controls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let decorations = window.window_decorations();
        let theme = cx.global::<Theme>();

        div()
            .flex()
            .h(px(68.0))
            .w_full()
            .bg(theme.background_secondary)
            .border_t_1()
            .border_color(theme.border_color)
            .map(|div| match decorations {
                Decorations::Server => div,
                Decorations::Client { tiling } => div
                    .when(!(tiling.bottom || tiling.left), |div| {
                        div.rounded_bl(px(theme.radius_md))
                    })
                    .when(!(tiling.bottom || tiling.right), |div| {
                        div.rounded_br(px(theme.radius_md))
                    }),
            })
            .on_any_mouse_down(|_, _, cx| {
                cx.stop_propagation();
            })
            .child(
                resizable(
                    "controls-left-resizable",
                    self.left_width.clone(),
                    ResizeEdge::Right,
                )
                .min_size(px(150.0))
                .max_size(px(500.0))
                .default_size(DEFAULT_CONTROLS_LEFT_WIDTH)
                .border_width(px(0.0))
                // deliberately uncached: this subtree went stale after shell
                // restructures (cached replays graft the old frame)
                .child(self.info_section.clone()),
            )
            .child(self.scrubber.clone())
            .child(
                resizable(
                    "controls-right-resizable",
                    self.right_width.clone(),
                    ResizeEdge::Left,
                )
                .min_size(px(180.0))
                .max_size(px(500.0))
                .default_size(DEFAULT_CONTROLS_RIGHT_WIDTH)
                .border_width(px(0.0))
                .child(
                    AnyView::from(self.secondary_controls.clone())
                        .cached(StyleRefinement::default().flex().w_full().h_full()),
                ),
            )
    }
}

pub struct InfoSection {
    track_name: Option<SharedString>,
    artist_name: Option<SharedString>,
    playback_info: PlaybackInfo,
    is_hovering_art: bool,
    current_track_path: Option<PathBuf>,
    current_library_track: Option<Rc<Track>>,
    can_navigate_to_album: bool,
    can_navigate_to_artist: bool,
    image_element_key: u64,
    is_liked: Option<i64>,
    /// Bumped per track change; a background library resolve only lands if its
    /// generation still matches, so fast switches never apply stale results.
    library_resolve_generation: usize,
    #[cfg(any(feature = "kugou", feature = "netease"))]
    online_track: Option<OnlinePlayingTrack>,
    #[cfg(any(feature = "kugou", feature = "netease"))]
    is_online_liked: bool,
    queue_item_data: Option<Entity<Option<QueueItemUIData>>>,
    queue_item_subscription: Option<Subscription>,
}

impl HasLikedState for InfoSection {
    fn is_liked(&self) -> Option<i64> {
        self.is_liked
    }
    fn set_liked(&mut self, item_id: Option<i64>) {
        self.is_liked = item_id;
    }
}

fn update_track_metadata(this: &mut InfoSection, metadata: &crate::media::metadata::Metadata) {
    // Only overwrite with values the stream actually provides: an online file
    // can carry tags (or just cover art / ReplayGain frames) without a usable
    // title/artist, and wiping the queue-item-derived names for those left
    // the play bar on "Unknown Track" until the next track change.
    if let Some(name) = metadata.name.clone() {
        this.track_name = Some(SharedString::from(name));
    }
    if let Some(artist) = metadata.artist.clone().or(metadata.album_artist.clone()) {
        this.artist_name = Some(SharedString::from(artist));
    }
}

fn resolve_queue_item_metadata(this: &mut InfoSection, cx: &mut Context<InfoSection>) {
    // Dropping cancels the subscription; detaching kept a live observer on the
    // old queue-item entity for the rest of the session (and let it fire).
    drop(this.queue_item_subscription.take());
    this.queue_item_data = None;

    let queue = cx.global::<Models>().queue.read(cx);
    let position = queue.position;
    let item = queue
        .data
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(position)
        .cloned();

    let Some(item) = item else { return };

    // The queue position is the single source of truth for "what is playing";
    // keying off `current_track_path` here breaks cover art whenever the path
    // lags the queue (e.g. an online URL refreshed after a session restore),
    // leaving the info-section thumbnail blank while the queue item shows art.
    let data = item.get_data(cx);
    this.queue_item_data = Some(data.clone());

    // SongChanged is broadcast before QueuePositionChanged, so this can run
    // while the UI position still points at the previous track. Only fill the
    // names from a slot that actually holds the track that just started -
    // filling from a stale slot latches the wrong track's names (the
    // fill-if-none policy then blocks the position-change resolve from
    // correcting them).
    let slot_is_current = this
        .current_track_path
        .as_ref()
        .is_some_and(|path| path == item.get_path());

    let item_path = item.get_path().clone();
    let subscription = cx.observe(&data, move |this: &mut InfoSection, data, cx| {
        let data = data.read(cx).clone();
        if let Some(data) = data {
            if this
                .current_track_path
                .as_ref()
                .is_some_and(|path| *path == item_path)
            {
                if this.track_name.is_none() {
                    this.track_name = data.name;
                }
                if this.artist_name.is_none() {
                    this.artist_name = data.artist_name;
                }
                cx.notify();
            }
        }
    });
    this.queue_item_subscription = Some(subscription);

    let data = data.read(cx).clone();
    if slot_is_current
        && let Some(data) = data
    {
        if this.track_name.is_none() {
            this.track_name = data.name;
        }
        if this.artist_name.is_none() {
            this.artist_name = data.artist_name;
        }
        cx.notify();
    }
}

impl InfoSection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let metadata_model = cx.global::<Models>().metadata.clone();
            let playback_info = cx.global::<PlaybackInfo>().clone();
            let current_track_model = playback_info.current_track.clone();
            let queue_model = cx.global::<Models>().queue.clone();

            cx.observe(&playback_info.playback_state, |_, _, cx| {
                cx.notify();
            })
            .detach();

            cx.observe(&metadata_model, |this: &mut Self, m, cx| {
                update_track_metadata(this, m.read(cx));
                cx.notify();
            })
            .detach();

            // SongChanged is broadcast before QueuePositionChanged, so re-resolve once the queue
            // position has caught up with a track switch
            cx.observe(&queue_model, |this: &mut Self, _, cx| {
                resolve_queue_item_metadata(this, cx);
            })
            .detach();

            cx.observe(
                &current_track_model,
                |this: &mut Self, current_track, cx| {
                    let current_track = current_track.read(cx).clone();
                    update_current_track_state(this, current_track.as_ref(), cx);
                    #[cfg(any(feature = "kugou", feature = "netease"))]
                    this.detect_online_track();
                    #[cfg(any(feature = "kugou", feature = "netease"))]
                    this.schedule_online_liked_query(cx);
                    resolve_queue_item_metadata(this, cx);
                    cx.notify();
                },
            )
            .detach();

            let initial_current_track = current_track_model.read(cx).clone();
            let current_track_path = initial_current_track
                .as_ref()
                .map(|track| track.get_path().clone());
            let initial_metadata = metadata_model.read(cx).clone();

            subscribe_liked_updates(cx, |this: &Self| {
                this.current_library_track.as_ref().map(|t| t.id)
            });

            let mut info_section = Self {
                artist_name: None,
                track_name: None,
                playback_info,
                is_hovering_art: false,
                current_track_path,
                // Library-derived state (track row, artist navigability,
                // liked state) resolves off-thread below; the section starts
                // with the already-known data only.
                current_library_track: None,
                can_navigate_to_album: false,
                can_navigate_to_artist: false,
                image_element_key: 0,
                is_liked: None,
                library_resolve_generation: 0,
                #[cfg(any(feature = "kugou", feature = "netease"))]
                online_track: None,
                #[cfg(any(feature = "kugou", feature = "netease"))]
                is_online_liked: false,
                queue_item_data: None,
                queue_item_subscription: None,
            };
            update_track_metadata(&mut info_section, &initial_metadata);
            #[cfg(any(feature = "kugou", feature = "netease"))]
            info_section.detect_online_track();
            #[cfg(any(feature = "kugou", feature = "netease"))]
            info_section.schedule_online_liked_query(cx);
            resolve_queue_item_metadata(&mut info_section, cx);
            spawn_library_resolve(&mut info_section, cx);

            info_section
        })
    }

    /// Re-resolves the current playback position as an online (KuGou/NetEase)
    /// track. Online tracks have no library entry, so `current_library_track`
    /// stays `None`; we map the running stream URL back to its track instead.
    #[cfg(any(feature = "kugou", feature = "netease"))]
    fn detect_online_track(&mut self) {
        let matched = self.current_track_path.as_ref().and_then(|path| {
            if !crate::media::is_http_path(path) {
                return None;
            }
            #[cfg(feature = "kugou")]
            if let Some(track) = online_track_matching_path(path) {
                return Some(OnlinePlayingTrack::Kugou(track));
            }
            #[cfg(feature = "netease")]
            if let Some(track) = netease_track_matching_path(path) {
                return Some(OnlinePlayingTrack::Netease(track));
            }
            None
        });
        if matched != self.online_track {
            if let Some(track) = &matched {
                tracing::info!(
                    identity = %track.identity(),
                    "info-section: online track detected for like button"
                );
            } else {
                tracing::debug!("info-section: no online track matched for like button");
            }
        }
        self.online_track = matched;
        self.is_online_liked = false;
    }

    /// Fires off a background query for whether the current online track is
    /// already in its source's liked list, then reflects the result once it
    /// lands (so the star lights up by default for already-liked songs).
    #[cfg(any(feature = "kugou", feature = "netease"))]
    fn schedule_online_liked_query(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.online_track.clone() else { return };
        let identity = track.identity();
        cx.spawn(async move |this, cx| {
            let liked = match &track {
                #[cfg(feature = "kugou")]
                OnlinePlayingTrack::Kugou(track) => {
                    !track.hash.is_empty() && online_track_is_liked(&track.hash).await
                }
                #[cfg(feature = "netease")]
                OnlinePlayingTrack::Netease(track) => netease_track_is_liked(track.id).await,
            };
            this.update(cx, |this, cx| {
                // Only apply if we're still on the same track.
                if this
                    .online_track
                    .as_ref()
                    .is_some_and(|t| t.matches_identity(&identity))
                {
                    this.is_online_liked = liked;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for InfoSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let add_to_state = self.current_library_track.as_ref().map(|track| {
            crate::ui::library::context_menus::add_to_playlist_state(
                "info-section-menu-state",
                track.id,
                window,
                cx,
            )
        });

        let image_key = self
            .current_library_track
            .as_ref()
            .map(|track| ManagedImageKey::Track(track.id))
            .or_else(|| {
                // Online tracks have no library track; reuse the current queue
                // item's stored cover_url, exactly as the queue thumbnails do.
                #[cfg(feature = "online_sources")]
                {
                    self.queue_item_data
                        .as_ref()
                        .and_then(|data| data.read(cx).clone())
                        .and_then(|data| {
                            data.cover_url
                                .filter(|url| !url.is_empty())
                                .map(ManagedImageKey::HttpCover)
                        })
                }
                #[cfg(not(feature = "online_sources"))]
                {
                    None
                }
            })
            .or_else(|| {
                self.current_track_path
                    .as_ref()
                    .map(|p| ManagedImageKey::TrackFile(p.clone()))
            });
        let image_element_key = self.image_element_key;
        let theme = cx.global::<Theme>();
        let state = self.playback_info.playback_state.read(cx);
        let album_navigation_track = self
            .can_navigate_to_album
            .then(|| self.current_library_track.clone())
            .flatten();
        let artist_navigation_track = self
            .can_navigate_to_artist
            .then(|| self.current_library_track.clone())
            .flatten();
        let content = div()
            .id("info-section")
            .flex()
            .w_full()
            .h_full()
            .overflow_x_hidden()
            .flex_shrink_0()
            .child(
                div()
                    .mx(px(12.0))
                    .mt(px(12.0))
                    .mb(px(6.0))
                    .gap(px(10.0))
                    .flex()
                    .w_full()
                    .overflow_x_hidden()
                    .child(
                        div()
                            .id("album-art")
                            .rounded(px(theme.radius_sm))
                            .bg(theme.album_art_background)
                            .shadow_sm()
                            .w(px(36.0))
                            .h(px(36.0))
                            .mb(px(6.0))
                            .flex_shrink_0()
                            .on_hover(cx.listener(|this, is_hovering: &bool, _, cx| {
                                if this.is_hovering_art != *is_hovering {
                                    this.is_hovering_art = *is_hovering;
                                    cx.notify();
                                }
                            }))
                            .when_some(image_key, |this: Stateful<Div>, key| {
                                this.when(self.is_hovering_art, |this: Stateful<Div>| {
                                    this.child(
                                        anchored().anchor(Anchor::BottomRight).child(deferred(
                                            div()
                                                .id("album-art-preview")
                                                .occlude()
                                                .pb(px(26.0))
                                                .child(
                                                    managed_image(
                                                        (
                                                            "album-art-preview-img",
                                                            image_element_key,
                                                        ),
                                                        key.clone(),
                                                    )
                                                    .thumb_max(256)
                                                    .uncached()
                                                    .w(px(256.0))
                                                    .h(px(256.0))
                                                    .rounded(px(theme.radius_lg))
                                                    .shadow_md(),
                                                ),
                                        )),
                                    )
                                })
                                .child(
                                    managed_image(("album-art-thumb", image_element_key), key)
                                        .w(px(36.0))
                                        .h(px(36.0))
                                        .object_fit(ObjectFit::Fill)
                                        .rounded(px(theme.radius_sm))
                                        .thumb()
                                        .uncached(),
                                )
                            }),
                    )
                    .when(*state == PlaybackState::Stopped, |e| {
                        e.child(
                            div()
                                .line_height(rems(1.0))
                                .font_weight(FontWeight::EXTRA_BOLD)
                                .text_size(px(15.0))
                                .flex()
                                .h_full()
                                .items_center()
                                .pb(px(6.0))
                                .child(tr!(
                                    "APP_NAME",
                                    "Meliora",
                                    #description="Use the english name everywhere unless this \
                                        is strictly disagreeable.
                                ")),
                        )
                    })
                    .when(*state != PlaybackState::Stopped, |e| {
                        let is_liked = self.is_liked;
                        let track_id = self.current_library_track.as_ref().map(|t| t.id);
                        #[cfg(any(feature = "kugou", feature = "netease"))]
                        let is_liked_filled = (track_id.is_some() && is_liked.is_some())
                            || (self.online_track.is_some() && self.is_online_liked);
                        #[cfg(not(any(feature = "kugou", feature = "netease")))]
                        let is_liked_filled = is_liked.is_some();
                        #[cfg(any(feature = "kugou", feature = "netease"))]
                        let has_track = track_id.is_some() || self.online_track.is_some();
                        #[cfg(not(any(feature = "kugou", feature = "netease")))]
                        let has_track = track_id.is_some();

                        e.child(
                            div()
                                .flex()
                                .flex_col()
                                .line_height(rems(1.0))
                                .text_size(px(15.0))
                                .gap_1()
                                .w_full()
                                .overflow_x_hidden()
                                .child(
                                    div()
                                        .id("info-section-track-name")
                                        .font_weight(FontWeight::EXTRA_BOLD)
                                        .text_ellipsis()
                                        .w_full()
                                        .when_some(album_navigation_track, |this, track| {
                                            this.cursor_pointer().on_click(move |_, _, cx| {
                                                navigate_to_track_album_and_reveal(cx, &track);
                                            })
                                        })
                                        .child(self.track_name.clone().unwrap_or_else(|| {
                                            tr!("UNKNOWN_TRACK", "Unknown Track").into()
                                        })),
                                )
                                .child(
                                    div()
                                        .id("info-section-artist-name")
                                        .text_ellipsis()
                                        .w_full()
                                        .when_some(artist_navigation_track, |this, track| {
                                            this.cursor_pointer().on_click(move |ev, _, cx| {
                                                navigate_to_track_artist(cx, &track, ev.position());
                                            })
                                        })
                                        .child(self.artist_name.clone().unwrap_or_else(|| {
                                            tr!("UNKNOWN_ARTIST", "Unknown Artist").into()
                                        })),
                                ),
                        )
                        .when(has_track, |e| {
                            e.child(
                                div().pb(px(6.0)).h_full().flex().ml_auto().child(
                                    div()
                                        .id("info-like")
                                        .my_auto()
                                        .rounded_sm()
                                        .p(px(4.0))
                                        .cursor_pointer()
                                        .hover(|this| this.bg(theme.button_secondary_hover))
                                        .active(|this| this.bg(theme.button_secondary_active))
                                        .child(
                                            icon(if is_liked_filled {
                                                STAR_FILLED
                                            } else {
                                                STAR
                                            })
                                            .size(px(14.0))
                                            .text_color(if is_liked_filled {
                                                theme.liked_song
                                            } else {
                                                theme.text_secondary
                                            }),
                                        )
                                        .when(is_liked_filled, |this| {
                                            this.tooltip(build_tooltip(tr!("UNLIKE", "Unlike")))
                                        })
                                        .when(!is_liked_filled, |this| {
                                            this.tooltip(build_tooltip(tr!("LIKE", "Like")))
                                        })
                                        .on_click(cx.listener(move |_this, _, _, cx| {
                                            if let Some(track_id) = track_id {
                                                toggle_like(track_id, cx.entity().clone(), cx);
                                                return;
                                            }
                                            #[cfg(any(feature = "kugou", feature = "netease"))]
                                            let this = _this;
                                            #[cfg(any(feature = "kugou", feature = "netease"))]
                                            if let Some(online) = this.online_track.clone() {
                                                match &online {
                                                    #[cfg(feature = "kugou")]
                                                    OnlinePlayingTrack::Kugou(track) => {
                                                        if this.is_online_liked {
                                                            kugou_unlike_track(cx, track);
                                                        } else {
                                                            kugou_like_track(cx, track);
                                                        }
                                                    }
                                                    #[cfg(feature = "netease")]
                                                    OnlinePlayingTrack::Netease(track) => {
                                                        if this.is_online_liked {
                                                            netease_unlike_track(cx, track);
                                                        } else {
                                                            netease_like_track(cx, track);
                                                        }
                                                    }
                                                }
                                                this.is_online_liked = !this.is_online_liked;
                                                cx.notify();
                                            }
                                        })),
                                ),
                            )
                        })
                    }),
            );

        // An online track without a matched identity leaves the menu with
        // zero items (artist/album/like/add-to all key off the library track,
        // and "reveal" is hidden for streams) - don't attach the popup at all.
        #[cfg(any(feature = "kugou", feature = "netease"))]
        let menu_has_items =
            self.current_library_track.is_some() || self.online_track.is_some();
        #[cfg(not(any(feature = "kugou", feature = "netease")))]
        let menu_has_items = self.current_library_track.is_some();

        if menu_has_items
            && (self.current_track_path.is_some() || self.current_library_track.is_some())
        {
            let show_add_to = add_to_state.as_ref().map(|(s, _)| s.clone());
            let add_to = add_to_state.map(|(_, a)| a);

            // online tracks: swap "show in file manager" for a download action
            #[cfg(any(feature = "kugou", feature = "netease"))]
            let online_download = self.online_track.clone().map(|track| match track {
                #[cfg(feature = "kugou")]
                OnlinePlayingTrack::Kugou(track) => (
                    SharedString::from(crate::ui::kugou::download_label()),
                    Rc::new(move |cx: &mut App| {
                        crate::ui::kugou::download_track_ui(cx, track.clone())
                    }) as Rc<dyn Fn(&mut App)>,
                ),
                #[cfg(feature = "netease")]
                OnlinePlayingTrack::Netease(track) => (
                    SharedString::from(crate::ui::netease::download_label()),
                    Rc::new(move |cx: &mut App| {
                        crate::ui::netease::download_track_ui(cx, track.clone())
                    }) as Rc<dyn Fn(&mut App)>,
                ),
            });
            #[cfg(not(any(feature = "kugou", feature = "netease")))]
            let online_download: Option<(SharedString, Rc<dyn Fn(&mut App)>)> = None;

            div()
                .child(
                    context("info-section-context").with(content).child(
                        div()
                            .bg(theme.elevated_background)
                            .child(InfoSectionContextMenu::new(
                                self.current_track_path.clone(),
                                self.current_library_track.clone(),
                                self.is_liked,
                                show_add_to,
                                online_download,
                            )),
                    ),
                )
                .when_some(add_to, |d, add_to| d.child(add_to))
                .into_any_element()
        } else {
            content.into_any_element()
        }
    }
}

fn update_current_track_state(
    this: &mut InfoSection,
    current_track: Option<&CurrentTrack>,
    cx: &mut Context<InfoSection>,
) {
    this.current_track_path = current_track.map(|track| track.get_path().clone());
    // Name/artist come from the metadata model or the queue item; DB-derived
    // state below lands when the background resolve completes.
    this.track_name = None;
    this.artist_name = None;
    this.current_library_track = None;
    this.can_navigate_to_album = false;
    this.can_navigate_to_artist = false;
    this.is_liked = None;
    this.image_element_key = this.image_element_key.wrapping_add(1);

    spawn_library_resolve(this, cx);
}

/// Resolves the current track's library row, artist navigability and liked
/// state on the runtime — three DB queries per resolve that used to park the
/// UI thread. The generation guard drops results from a track that has
/// already been switched away from; until it lands the section keeps showing
/// the already-known metadata (name/artist/cover) without library actions.
fn spawn_library_resolve(this: &mut InfoSection, cx: &mut Context<InfoSection>) {
    let Some(track_path) = this.current_track_path.clone() else {
        return;
    };
    this.library_resolve_generation += 1;
    let generation = this.library_resolve_generation;
    let pool = cx.global::<crate::ui::app::Pool>().0.clone();

    cx.spawn(async move |this, cx| {
        let resolved = crate::RUNTIME
            .spawn(async move {
                let track = crate::library::db::get_track_by_path(&pool, &track_path)
                    .await
                    .ok()
                    .flatten();
                let Some(track) = track else {
                    return None;
                };
                let can_navigate_to_artist = match track.album_id {
                    Some(album_id) => {
                        crate::library::db::artist_ids_for_album(&pool, album_id)
                            .await
                            .map(|v| !v.is_empty())
                            .unwrap_or(false)
                    }
                    None => false,
                };
                let is_liked = crate::library::db::playlist_has_track(
                    &pool,
                    LIKED_SONGS_PLAYLIST_ID,
                    track.id,
                )
                .await
                .ok()
                .flatten();

                Some(((*track).clone(), can_navigate_to_artist, is_liked))
            })
            .await;

        this.update(cx, |this, cx| {
            if this.library_resolve_generation != generation {
                return;
            }
            match resolved {
                Ok(Some((track, can_navigate_to_artist, is_liked))) => {
                    this.current_library_track = Some(Rc::new(track));
                    this.can_navigate_to_album =
                        this.current_library_track.as_ref().is_some_and(|t| t.album_id.is_some());
                    this.can_navigate_to_artist = can_navigate_to_artist;
                    this.is_liked = is_liked;
                    cx.notify();
                }
                // A query error or a non-library (online) path just leaves the
                // cleared state in place - same as the sync path's None result.
                _ => {}
            }
        })
        .ok();
    })
    .detach();
}

pub struct PlaybackSection {
    info: PlaybackInfo,
}

impl PlaybackSection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            let state = info.playback_state.clone();
            let shuffling = info.shuffling.clone();
            let repeating = info.repeating.clone();
            let stop_after_current = info.stop_after_current.clone();

            cx.observe(&state, |_, _, cx| {
                cx.notify();
            })
            .detach();

            cx.observe(&shuffling, |_, _, cx| {
                cx.notify();
            })
            .detach();

            cx.observe(&repeating, |_, _, cx| {
                cx.notify();
            })
            .detach();

            cx.observe(&stop_after_current, |_, _, cx| {
                cx.notify();
            })
            .detach();

            Self { info }
        })
    }
}

impl Render for PlaybackSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.info.playback_state.read(cx);
        let shuffling = self.info.shuffling.read(cx);
        let repeating = *self.info.repeating.read(cx);
        let stop_after_current = *self.info.stop_after_current.read(cx);
        let theme = cx.global::<Theme>();
        let always_repeat = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .playback
            .always_repeat;
        let repeat_icon_color = match repeating {
            RepeatState::NotRepeating => theme.text,
            RepeatState::Repeating => theme.playback_button_toggled,
            RepeatState::RepeatingOne => theme.playback_button_repeat_one,
        };

        div()
            .mr(auto())
            .ml(auto())
            .mt(px(5.0))
            .flex()
            .w_full()
            .absolute()
            .child(
                div()
                    .rounded(px(theme.radius_sm))
                    .w(px(28.0))
                    .h(px(25.0))
                    .mt(px(3.0))
                    .mr(px(6.0))
                    .ml_auto()
                    .border_color(theme.playback_button_border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|style| style.bg(theme.playback_button_hover).cursor_pointer())
                    .id("header-shuffle-button")
                    .active(|style| style.bg(theme.playback_button_active))
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        cx.stop_propagation();
                        window.prevent_default();
                    })
                    .on_click(|_, _, cx| {
                        cx.global::<PlaybackInterface>().toggle_shuffle();
                    })
                    .child(icon(SHUFFLE).size(px(14.0)).when(*shuffling, |this| {
                        this.text_color(theme.playback_button_toggled)
                    }))
                    .when_else(
                        *shuffling,
                        |this| this.tooltip(build_tooltip(tr!("STOP_SHUFFLING", "Stop Shuffling"))),
                        |this| this.tooltip(build_tooltip(tr!("SHUFFLE"))),
                    ),
            )
            .child(
                div()
                    .rounded(px(theme.radius_sm))
                    .border_color(theme.playback_button_border)
                    .border_1()
                    .flex()
                    .child(
                        div()
                            .w(px(30.0))
                            .h(px(28.0))
                            .rounded_l(px(theme.radius_sm))
                            .bg(theme.playback_button)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .id("header-prev-button")
                            .active(|style| style.bg(theme.playback_button_active))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Previous), cx);
                            })
                            .child(icon(PREV_TRACK).size(px(16.0)))
                            .tooltip(build_tooltip(tr!("PREVIOUS_TRACK", "Previous Track")))
                            .into_transition(theme.playback_button, theme.playback_button_hover),
                    )
                    .child(
                        context("header-play-button-context")
                            .with(
                                div()
                                    .w(px(32.0))
                                    .h(px(28.0))
                                    .bg(theme.playback_button)
                                    .border_l(px(1.0))
                                    .border_r(px(1.0))
                                    .border_color(theme.playback_button_border)
                                    .relative()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .id("header-play-button")
                                    .active(|style| style.bg(theme.playback_button_active))
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        cx.stop_propagation();
                                        window.prevent_default();
                                    })
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(PlayPause), cx);
                                    })
                                    .when(*state == PlaybackState::Playing, |div| {
                                        div.child(icon(PAUSE).size(px(16.0)))
                                            .tooltip(build_tooltip(tr!("PAUSE")))
                                    })
                                    .when(*state != PlaybackState::Playing, |div| {
                                        div.child(icon(PLAY).size(px(16.0)))
                                            .tooltip(build_tooltip(tr!("PLAY")))
                                    })
                                    .when(stop_after_current, |this| {
                                        this.child(
                                            div()
                                                .id("stop-after-current-indicator")
                                                .absolute()
                                                .top(px(3.0))
                                                .right(px(3.0))
                                                .size(px(6.0))
                                                .rounded_full()
                                                .bg(theme.stop_after_current_indicator)
                                                .tooltip(build_tooltip(tr!(
                                                    "STOP_AFTER_CURRENT_TOOLTIP",
                                                    "Will stop after current track"
                                                ))),
                                        )
                                    })
                                    .into_transition(
                                        theme.playback_button,
                                        theme.playback_button_hover,
                                    ),
                            )
                            .child(menu().item(menu_check_item(
                                "stop-after-current-menu-item",
                                stop_after_current,
                                tr!("ACTION_STOP_AFTER_CURRENT"),
                                |_, window, cx| {
                                    window.dispatch_action(Box::new(StopAfterCurrent), cx);
                                },
                            ))),
                    )
                    .child(
                        div()
                            .w(px(30.0))
                            .h(px(28.0))
                            .rounded_r(px(theme.radius_sm))
                            .bg(theme.playback_button)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .id("header-next-button")
                            .active(|style| style.bg(theme.playback_button_active))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Next), cx);
                            })
                            .child(icon(NEXT_TRACK).size(px(16.0)))
                            .tooltip(build_tooltip(tr!("NEXT_TRACK", "Next Track")))
                            .into_transition(theme.playback_button, theme.playback_button_hover),
                    ),
            )
            .child(
                div().mr_auto().child(
                    context("repeat-context")
                        .with(
                            div()
                                .rounded(px(theme.radius_sm))
                                .w(px(28.0))
                                .h(px(25.0))
                                .mt(px(3.0))
                                .ml(px(6.0))
                                .border_color(theme.playback_button_border)
                                .flex()
                                .items_center()
                                .justify_center()
                                .hover(|style| {
                                    style.bg(theme.playback_button_hover).cursor_pointer()
                                })
                                .id("header-repeat-button")
                                .active(|style| style.bg(theme.playback_button_active))
                                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                    cx.stop_propagation();
                                    window.prevent_default();
                                })
                                .on_click(move |_, _, cx| match repeating {
                                    RepeatState::NotRepeating => cx
                                        .global::<PlaybackInterface>()
                                        .set_repeat(RepeatState::Repeating),
                                    RepeatState::Repeating => cx
                                        .global::<PlaybackInterface>()
                                        .set_repeat(RepeatState::RepeatingOne),
                                    RepeatState::RepeatingOne => cx
                                        .global::<PlaybackInterface>()
                                        .set_repeat(RepeatState::NotRepeating),
                                })
                                .tooltip(build_tooltip(match repeating {
                                    RepeatState::NotRepeating => {
                                        tr!("REPEAT")
                                    }
                                    RepeatState::Repeating => tr!("REPEAT_ONE"),
                                    RepeatState::RepeatingOne => {
                                        if always_repeat {
                                            tr!("REPEAT")
                                        } else {
                                            tr!("STOP_REPEATING", "Stop Repeating")
                                        }
                                    }
                                }))
                                .child(
                                    icon(match repeating {
                                        RepeatState::NotRepeating | RepeatState::Repeating => {
                                            REPEAT
                                        }
                                        RepeatState::RepeatingOne => REPEAT_ONCE,
                                    })
                                    .size(px(14.0))
                                    .text_color(repeat_icon_color),
                                ),
                        )
                        .child(
                            div().bg(theme.elevated_background).child(
                                menu()
                                    .when(!always_repeat, |menu| {
                                        menu.item(menu_item(
                                            "repeat-not-repeat",
                                            Some(REPEAT_OFF),
                                            tr!("REPEAT_OFF", "Off"),
                                            move |_, _, cx| {
                                                cx.global::<PlaybackInterface>()
                                                    .set_repeat(RepeatState::NotRepeating);
                                            },
                                        ))
                                    })
                                    .item(menu_item(
                                        "repeat-repeat",
                                        Some(REPEAT),
                                        tr!("REPEAT", "Repeat"),
                                        move |_, _, cx| {
                                            cx.global::<PlaybackInterface>()
                                                .set_repeat(RepeatState::Repeating);
                                        },
                                    ))
                                    .item(menu_item(
                                        "repeat-repeat-one",
                                        Some(REPEAT_ONCE),
                                        tr!("REPEAT_ONE", "Repeat One"),
                                        move |_, _, cx| {
                                            cx.global::<PlaybackInterface>()
                                                .set_repeat(RepeatState::RepeatingOne);
                                        },
                                    )),
                            ),
                        ),
                ),
            )
    }
}

pub struct Scrubber {
    position: Entity<u64>,
    duration: Entity<u64>,
    playback_section: Entity<PlaybackSection>,
    /// Cached label texts keyed by whole-second (position, duration): the
    /// labels only change once per second, but render runs at ~33Hz while
    /// playing. Invalidated by any second-boundary change.
    time_labels: (u64, u64, SharedString, SharedString, SharedString),
}

impl Scrubber {
    fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let position_model = cx.global::<PlaybackInfo>().position.clone();
            let duration_model = cx.global::<PlaybackInfo>().duration.clone();

            cx.observe(&position_model, |_, _, cx| {
                cx.notify();
            })
            .detach();

            cx.observe(&duration_model, |_, _, cx| {
                cx.notify();
            })
            .detach();

            Self {
                position: position_model,
                duration: duration_model,
                playback_section: PlaybackSection::new(cx),
                // u64::MAX never matches a real second count: forces first build
                time_labels: (
                    u64::MAX,
                    u64::MAX,
                    SharedString::default(),
                    SharedString::default(),
                    SharedString::default(),
                ),
            }
        })
    }
}

impl Render for Scrubber {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let position_ms = *self.position.read(cx);
        let duration_ms = *self.duration.read(cx);
        let position_secs = position_ms / 1_000;
        let duration_secs = duration_ms / 1_000;
        let remaining_secs = duration_secs.saturating_sub(position_secs);

        // reuse the formatted labels while the whole-second key is unchanged
        let (position_text, duration_text, remaining_text) =
            if self.time_labels.0 == position_secs && self.time_labels.1 == duration_secs {
                let (_, _, p, d, r) = &self.time_labels;
                (p.clone(), d.clone(), r.clone())
            } else {
                let p = SharedString::from(format_duration(position_secs as i64, true));
                let d = SharedString::from(format_duration(duration_secs as i64, true));
                let r = SharedString::from(format!(
                    "-{}",
                    format_duration(remaining_secs as i64, true)
                ));
                self.time_labels = (position_secs, duration_secs, p.clone(), d.clone(), r.clone());
                (p, d, r)
            };

        let window_width = window.viewport_size().width;

        div()
            .pl(px(13.0))
            .pr(px(13.0))
            .border_x(px(1.0))
            .border_color(theme.border_color)
            .flex_grow(1.0)
            .flex()
            .flex_col()
            .text_size(px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .relative()
            .child(
                div()
                    .w_full()
                    .flex()
                    .relative()
                    .items_end()
                    .mt(px(6.0))
                    .mb(px(6.0))
                    .child(
                        div()
                            .mr(px(6.0))
                            .line_height(rems(1.0))
                            .child(position_text),
                    )
                    .when(window_width > px(900.0), |this| {
                        this.child(
                            div()
                                .line_height(rems(1.0))
                                .border_color(rgb(0x4b5563))
                                .border_l(px(2.0))
                                .pl(px(6.0))
                                .text_color(rgb(0xcbd5e1))
                                .child(duration_text),
                        )
                    })
                    .child(self.playback_section.clone())
                    .child(div().h(px(30.0)))
                    .child(
                        div()
                            .ml(auto())
                            .line_height(rems(1.0))
                            .child(remaining_text),
                    ),
            )
            .child(
                slider()
                    .w_full()
                    .h(px(6.0))
                    .rounded(px(theme.radius_sm))
                    .id("scrubber-back")
                    .change_interval(Duration::from_millis(33))
                    .value(if duration_ms > 0 {
                        position_ms as f32 / duration_ms as f32
                    } else {
                        0.0
                    })
                    .on_change(move |v, _, cx| {
                        let info = cx.global::<PlaybackInfo>().clone();

                        if duration_ms > 0
                            && *info.playback_state.read(cx) != PlaybackState::Stopped
                        {
                            cx.global::<PlaybackInterface>()
                                .seek(v as f64 * duration_ms as f64 / 1_000.0);
                        }
                    }),
            )
    }
}

#[derive(IntoElement)]
struct SidebarToggleButton {
    div: Stateful<Div>,
    icon_path: &'static str,
    active: bool,
}

impl StatefulInteractiveElement for SidebarToggleButton {}

impl InteractiveElement for SidebarToggleButton {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl Styled for SidebarToggleButton {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl RenderOnce for SidebarToggleButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let icon_color = if self.active {
            theme.playback_button_toggled
        } else {
            theme.text
        };

        self.div
            .rounded(px(theme.radius_sm))
            .w(px(25.0))
            .h(px(25.0))
            .mt(px(2.0))
            .flex()
            .items_center()
            .justify_center()
            .border_color(theme.playback_button_border)
            .bg(theme.playback_button)
            .cursor_pointer()
            .hover(|this| this.bg(theme.playback_button_hover))
            .active(|this| this.bg(theme.playback_button_active))
            .child(icon(self.icon_path).size(px(14.0)).text_color(icon_color))
    }
}

fn sidebar_toggle_button(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    active: bool,
) -> SidebarToggleButton {
    SidebarToggleButton {
        div: div().id(id.into()),
        icon_path,
        active,
    }
}

pub struct SecondaryControls {
    info: PlaybackInfo,
    show_queue: Entity<bool>,
    show_lyrics: Entity<bool>,
    replaygain_button: Entity<ReplayGainButton>,
}

impl SecondaryControls {
    pub fn new(cx: &mut App, show_queue: Entity<bool>, show_lyrics: Entity<bool>) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            let volume = info.volume.clone();

            cx.observe(&volume, |_, _, cx| {
                cx.notify();
            })
            .detach();

            Self {
                info,
                show_queue,
                show_lyrics,
                replaygain_button: ReplayGainButton::new(cx),
            }
        })
    }
}

impl Render for SecondaryControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let volume = *self.info.volume.read(cx);
        let prev_volume = *self.info.prev_volume.read(cx);
        let show_queue = self.show_queue.clone();
        let show_lyrics = self.show_lyrics.clone();
        let lyrics_active = *self.show_lyrics.read(cx);
        let queue_active = *self.show_queue.read(cx);

        div().flex().w_full().h_full().child(
            div()
                .px(px(18.0))
                .flex()
                .w_full()
                .my_auto()
                .pb(px(2.0))
                .child(
                    div()
                        .rounded(px(theme.radius_sm))
                        .w(px(25.0))
                        .h(px(25.0))
                        .mt(px(2.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_color(theme.playback_button_border)
                        .id("volume-button")
                        .cursor_pointer()
                        .bg(theme.playback_button)
                        .hover(|this| this.bg(theme.playback_button_hover))
                        .active(|this| this.bg(theme.playback_button_active))
                        .when(volume <= 0.0, |div| {
                            div.child(icon(VOLUME_OFF).size(px(14.0)))
                                .on_click(move |_, _, cx| {
                                    cx.global::<PlaybackInterface>().set_volume(prev_volume);
                                })
                                .tooltip(build_tooltip(tr!("UNMUTE", "Unmute")))
                        })
                        .when(volume > 0.0, |div| {
                            div.child(icon(VOLUME).size(px(14.0)))
                                .on_click(move |_, _, cx| {
                                    cx.global::<PlaybackInterface>().set_volume(0 as f64);
                                })
                                .tooltip(build_tooltip(tr!("MUTE", "Mute")))
                        }),
                )
                .child(
                    div()
                        .id("volume-container")
                        .mx(px(4.0))
                        .flex_1()
                        .min_w(px(50.0))
                        .hoverable_tooltip(build_volume_tooltip(self.info.volume.clone()))
                        .child(
                            slider()
                                .w_full()
                                .h(px(6.0))
                                .mt(px(11.0))
                                .rounded(px(theme.radius_sm))
                                .id("volume")
                                // match the scrubber: without this every mouse
                                // move sends a SetVolume command down the
                                // playback channel during a drag
                                .change_interval(Duration::from_millis(33))
                                .value((volume) as f32)
                                .on_double_click(|_, cx| {
                                    cx.global::<PlaybackInterface>().set_volume(1.0_f64);
                                })
                                .on_change(move |v, _, cx| {
                                    cx.global::<PlaybackInterface>().set_volume(v as f64);
                                }),
                        )
                        .on_scroll_wheel(move |ev, _, cx| {
                            let delta: f64 = if ev.delta.precise() {
                                f64::from(ev.delta.pixel_delta(px(1.0)).y) * 0.01666666
                            } else {
                                ev.delta.pixel_delta(px(0.01666666)).y.into()
                            };
                            cx.global::<PlaybackInterface>().set_volume(f64::clamp(
                                volume + delta,
                                0_f64,
                                1_f64,
                            ));
                        }),
                )
                .child(self.replaygain_button.clone())
                .child(
                    div()
                        .h(px(24.0))
                        .w(px(1.0))
                        .mt(px(3.0))
                        .mx(px(4.0))
                        .bg(theme.border_color),
                )
                .child(
                    sidebar_toggle_button("queue-button", MENU, queue_active)
                        .on_click(move |_, _, cx| {
                            show_queue.update(cx, |m, cx| {
                                *m = !*m;
                                crate::log_mem_event(if *m {
                                    "sidebar: queue show"
                                } else {
                                    "sidebar: queue hide"
                                });
                                cx.notify();
                            })
                        })
                        .tooltip(build_tooltip(tr!("QUEUE_TITLE"))),
                )
                .child(
                    sidebar_toggle_button("lyrics-button", MICROPHONE, lyrics_active)
                        .on_click(move |_, _, cx| {
                            show_lyrics.update(cx, |m, cx| {
                                *m = !*m;
                                crate::log_mem_event(if *m {
                                    "sidebar: lyrics show"
                                } else {
                                    "sidebar: lyrics hide"
                                });
                                cx.notify();
                            })
                        })
                        .tooltip(build_tooltip(tr!("LYRICS", "Lyrics"))),
                ),
        )
    }
}

// --- volume tooltip (merged from ui/components/volume_tooltip.rs) ---

pub struct VolumeTooltip {
    volume: Entity<f64>,
}

impl VolumeTooltip {
    pub fn new(volume: Entity<f64>, cx: &mut Context<Self>) -> Self {
        cx.observe(&volume, |_, _, cx| {
            cx.notify();
        })
        .detach();

        Self { volume }
    }
}

impl Render for VolumeTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let volume = *self.volume.read(cx);
        let percentage = (volume * 100.0).round() as i32;

        tooltip_container(theme).child(format!("{}%", percentage))
    }
}

pub fn build_volume_tooltip(
    volume: Entity<f64>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window, cx| cx.new(|cx| VolumeTooltip::new(volume.clone(), cx)).into()
}
