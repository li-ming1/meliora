#[cfg(feature = "kugou")]
use crate::ui::kugou::{
    KugouTrackInfo, like_track as kugou_like_track, online_track_is_liked,
    online_track_matching_path, unlike_track as kugou_unlike_track,
};
#[cfg(feature = "netease")]
use crate::ui::netease::{
    NeteaseTrackInfo, like_track as netease_like_track,
    online_track_is_liked as netease_track_is_liked,
    online_track_matching_path as netease_track_matching_path,
    unlike_track as netease_unlike_track,
};
use crate::{
    library::types::Track,
    playback::{queue::QueueItemUIData, thread::PlaybackState},
    ui::{
        components::{
            context::context,
            icons::{STAR, STAR_FILLED, icon},
            managed_image::{ManagedImageKey, managed_image},
            tooltip::build_tooltip,
        },
        design::ICON_SM,
        library::context_menus::{
            info_section::InfoSectionContextMenu, navigate_to_track_album_and_reveal,
            navigate_to_track_artist,
        },
        models::{
            CurrentTrack, HasLikedState, LIKED_SONGS_PLAYLIST_ID, Models, PlaybackInfo,
            subscribe_liked_updates, toggle_like,
        },
        queue_metadata,
        theme::Theme,
    },
};
use cntp_i18n::tr;
use gpui::{InteractiveElement, prelude::FluentBuilder, *};
use std::{path::PathBuf, rc::Rc};

use super::observe_notify;

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

impl InfoSection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let metadata_model = cx.global::<Models>().metadata.clone();
            let playback_info = cx.global::<PlaybackInfo>().clone();
            let current_track_model = playback_info.current_track.clone();
            let queue_model = cx.global::<Models>().queue.clone();

            observe_notify(cx, &playback_info.playback_state);

            cx.observe(&metadata_model, |this: &mut Self, m, cx| {
                this.update_track_metadata(m.read(cx));
                cx.notify();
            })
            .detach();

            // SongChanged is broadcast before QueuePositionChanged, so re-resolve once the queue
            // position has caught up with a track switch
            cx.observe(&queue_model, |this: &mut Self, _, cx| {
                this.resolve_queue_item_metadata(cx);
            })
            .detach();

            cx.observe(
                &current_track_model,
                |this: &mut Self, current_track, cx| {
                    let current_track = current_track.read(cx).clone();
                    this.update_current_track_state(current_track.as_ref(), cx);
                    #[cfg(any(feature = "kugou", feature = "netease"))]
                    this.detect_online_track();
                    #[cfg(any(feature = "kugou", feature = "netease"))]
                    this.schedule_online_liked_query(cx);
                    this.resolve_queue_item_metadata(cx);
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
            info_section.update_track_metadata(&initial_metadata);
            #[cfg(any(feature = "kugou", feature = "netease"))]
            info_section.detect_online_track();
            #[cfg(any(feature = "kugou", feature = "netease"))]
            info_section.schedule_online_liked_query(cx);
            info_section.resolve_queue_item_metadata(cx);
            info_section.spawn_library_resolve(cx);

            info_section
        })
    }

    /// Overwrites the display names with stream metadata. Only values the
    /// stream actually provides are taken: an online file can carry tags (or
    /// just cover art / ReplayGain frames) without a usable title/artist, and
    /// wiping the queue-item-derived names for those left the play bar on
    /// "Unknown Track" until the next track change.
    fn update_track_metadata(&mut self, metadata: &crate::media::metadata::Metadata) {
        if let Some(name) = metadata.name.clone() {
            self.track_name = Some(SharedString::from(name));
        }
        if let Some(artist) = metadata
            .artist
            .clone()
            .or_else(|| metadata.album_artist.clone())
        {
            self.artist_name = Some(SharedString::from(artist));
        }
    }

    /// Adopts the queue item's display names only where the section has none:
    /// names already provided by stream metadata must not be clobbered.
    fn fill_missing_names(&mut self, data: QueueItemUIData) {
        if self.track_name.is_none() {
            self.track_name = data.name;
        }
        if self.artist_name.is_none() {
            self.artist_name = data.artist_name;
        }
    }

    /// Re-wires the queue-item subscription to the item at the current queue
    /// position and adopts whatever display data it already holds.
    fn resolve_queue_item_metadata(&mut self, cx: &mut Context<Self>) {
        // Dropping cancels the subscription; detaching kept a live observer on the
        // old queue-item entity for the rest of the session (and let it fire).
        drop(self.queue_item_subscription.take());
        self.queue_item_data = None;

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
        // The handle comes from the UI-side metadata side table (P2-B step 4):
        // the same uid always resolves to the same entity, and the current
        // track's uid stays live in the queue, so the retained handle and the
        // subscription below can never go stale.
        let data = queue_metadata::queue_ui_data(cx, &item);
        self.queue_item_data = Some(data.clone());

        // SongChanged is broadcast before QueuePositionChanged, so this can run
        // while the UI position still points at the previous track. Only fill the
        // names from a slot that actually holds the track that just started -
        // filling from a stale slot latches the wrong track's names (the
        // fill-if-none policy then blocks the position-change resolve from
        // correcting them).
        let slot_is_current = self
            .current_track_path
            .as_ref()
            .is_some_and(|path| path == item.get_path());

        let item_path = item.get_path().clone();
        let subscription = cx.observe(&data, move |this: &mut Self, data, cx| {
            let data = data.read(cx).clone();
            if let Some(data) = data
                && this
                    .current_track_path
                    .as_ref()
                    .is_some_and(|path| *path == item_path)
            {
                this.fill_missing_names(data);
                cx.notify();
            }
        });
        self.queue_item_subscription = Some(subscription);

        let data = data.read(cx).clone();
        if slot_is_current && let Some(data) = data {
            self.fill_missing_names(data);
            cx.notify();
        }
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
        let Some(track) = self.online_track.clone() else {
            return;
        };
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

    /// The star button at the right edge of the section: filled while the
    /// track is liked (library row or online source), toggling whichever
    /// like-state applies to the current track.
    fn like_button(&self, cx: &Context<Self>, is_liked_filled: bool, track_id: Option<i64>) -> Div {
        let theme = cx.global::<Theme>();

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
                    icon(if is_liked_filled { STAR_FILLED } else { STAR })
                        .size(ICON_SM)
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
                        toggle_like(track_id, cx.entity(), cx);
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
        )
    }

    /// Clears all track-derived state and starts a fresh library resolve.
    /// Name/artist come from the metadata model or the queue item; DB-derived
    /// state lands when the background resolve completes.
    fn update_current_track_state(
        &mut self,
        current_track: Option<&CurrentTrack>,
        cx: &mut Context<Self>,
    ) {
        self.current_track_path = current_track.map(|track| track.get_path().clone());
        self.track_name = None;
        self.artist_name = None;
        self.current_library_track = None;
        self.can_navigate_to_album = false;
        self.can_navigate_to_artist = false;
        self.is_liked = None;
        self.image_element_key = self.image_element_key.wrapping_add(1);

        self.spawn_library_resolve(cx);
    }

    /// Resolves the current track's library row, artist navigability and liked
    /// state on the runtime — three DB queries per resolve that used to park the
    /// UI thread. The generation guard drops results from a track that has
    /// already been switched away from; until it lands the section keeps showing
    /// the already-known metadata (name/artist/cover) without library actions.
    fn spawn_library_resolve(&mut self, cx: &mut Context<Self>) {
        let Some(track_path) = self.current_track_path.clone() else {
            return;
        };
        self.library_resolve_generation += 1;
        let generation = self.library_resolve_generation;
        let pool = cx.global::<crate::ui::app::Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let resolved = crate::RUNTIME
                .spawn(async move {
                    let track = crate::library::db::get_track_by_path(&pool, &track_path)
                        .await
                        .ok()
                        .flatten();
                    let track = track?;
                    let can_navigate_to_artist =
                        crate::library::db::artist_ids_for_track(&pool, track.id)
                            .await
                            .map(|v| !v.is_empty())
                            .unwrap_or(false);
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
                // A query error or a non-library (online) path just leaves the
                // cleared state in place - same as the sync path's None result.
                if let Ok(Some((track, can_navigate_to_artist, is_liked))) = resolved {
                    this.current_library_track = Some(Rc::new(track));
                    this.can_navigate_to_album = this
                        .current_library_track
                        .as_ref()
                        .is_some_and(|t| t.album_id.is_some());
                    this.can_navigate_to_artist = can_navigate_to_artist;
                    this.is_liked = is_liked;
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
                            // Double-click enters the immersive view; the
                            // single-click path stays free (hover shows the
                            // big preview).
                            .on_click(|event: &ClickEvent, _, cx| {
                                if event.click_count() == 2 {
                                    crate::ui::immersive::set_immersive(true, cx);
                                }
                            })
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
                            e.child(self.like_button(cx, is_liked_filled, track_id))
                        })
                    }),
            );

        // An online track without a matched identity leaves the menu with
        // zero items (artist/album/like/add-to all key off the library track,
        // and "reveal" is hidden for streams) - don't attach the popup at all.
        #[cfg(any(feature = "kugou", feature = "netease"))]
        let menu_has_items = self.current_library_track.is_some() || self.online_track.is_some();
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
                                self.can_navigate_to_artist,
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
