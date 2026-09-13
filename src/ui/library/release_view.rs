use std::{rc::Rc, sync::Arc, time::Duration};

use cntp_i18n::tr;
use gpui::*;
use prelude::FluentBuilder;

use crate::{
    library::{
        db::LibraryAccess,
        types::{
            Album, DATE_PRECISION_FULL_DATE, DATE_PRECISION_YEAR, DATE_PRECISION_YEAR_MONTH,
            DBString, Track,
        },
    },
    playback::thread::PlaybackState,
    ui::{
        availability::is_track_available,
        caching::meliora_cache,
        components::{
            button::{ButtonSize, button},
            icons::{DOTS_VERTICAL, STAR, STAR_FILLED, icon},
            playback_controls::playback_controls,
            popover::{PopoverPosition, popover},
            scrollbar::{ScrollableHandle, floating_scrollbar},
            table::table_data::TABLE_MAX_WIDTH,
            tooltip::build_tooltip,
        },
        library::{
            collection_summary::format_collection_summary,
            context_menus::{
                AlbumContextMenuContext, add_album_to_playlist_state, album::AlbumContextMenu,
                navigate_to_album_artists, queue_items_from_tracks,
            },
            track_item::{ArtistNameVisibility, TrackItem, TrackItemLeftField},
            detail_close_button,
        },
        models::{LIKED_SONGS_PLAYLIST_ID, Models, PlaybackInfo, PlaylistEvent, toggle_album_like},
        scroll_follow::SmoothScrollFollow,
        theme::Theme,
    },
};

const RELEASE_SCROLL_ANIMATION_DURATION: Duration = Duration::from_millis(250);

fn compute_all_liked(cx: &App, tracks: &[Track]) -> bool {
    if tracks.is_empty() {
        return false;
    }
    let ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
    cx.playlist_contains_all_tracks(LIKED_SONGS_PLAYLIST_ID, &ids)
        .unwrap_or(false)
}

/// Prebuilt track rows for the release view (merged from
/// `library/track_listing.rs`, whose only remaining constructor call site
/// lives here).
struct TrackListing {
    tracks: Arc<Vec<Entity<TrackItem>>>,
    original_tracks: Arc<Vec<Track>>,
}

impl TrackListing {
    fn new(
        cx: &mut App,
        tracks: Arc<Vec<Track>>,
        artist_name_visibility: ArtistNameVisibility,
        vinyl_numbering: bool,
        show_go_to_album: bool,
        show_go_to_artist: bool,
    ) -> Self {
        // find biggest track number and provide it to track item for measurement
        let max_track_num_str = tracks
            .iter()
            .filter_map(|t| t.track_number)
            .max()
            .map(|n| format!("{}", n).into());

        Self {
            tracks: Arc::new({
                let tracks_for_closure = tracks.clone();
                tracks
                    .iter()
                    .enumerate()
                    .map(move |(index, track)| {
                        TrackItem::new(
                            cx,
                            track.clone(),
                            index == 0
                                || track.track_number == Some(1)
                                || tracks_for_closure
                                    .get(index - 1)
                                    .is_some_and(|t| t.disc_number != track.disc_number),
                            artist_name_visibility.clone(),
                            TrackItemLeftField::TrackNum,
                            None,
                            vinyl_numbering,
                            max_track_num_str.clone(),
                            None,
                            show_go_to_album,
                            show_go_to_artist,
                        )
                    })
                    .collect()
            }),
            original_tracks: tracks,
        }
    }

    fn tracks(&self) -> &Arc<Vec<Track>> {
        &self.original_tracks
    }

    /// Returns track rows as a lazy iterator: the caller's `.children(...)`
    /// consumes it directly, avoiding a per-frame `Vec<AnyElement>` allocation.
    fn track_elements(&self) -> impl Iterator<Item = AnyElement> + '_ {
        self.tracks
            .iter()
            .cloned()
            .map(|track| track.into_any_element())
    }
}

pub struct ReleaseView {
    album: Arc<Album>,
    artist_name: Option<DBString>,
    tracks: Arc<Vec<Track>>,
    /// Per-track availability, computed once at load instead of N stats per frame.
    tracks_available: Arc<Vec<bool>>,
    track_listing: TrackListing,
    collection_summary: SharedString,
    release_info: Option<SharedString>,
    img_path: SharedString,
    scroll_handle: ScrollHandle,
    pending_scroll: Option<usize>,
    scroll_follow: SmoothScrollFollow,
    scroll_frame_scheduled: bool,
    all_liked: bool,
    menu_open: bool,
    track_ids: std::rc::Rc<[i64]>,
    /// Parsed once; the footer renders it every frame otherwise.
    release_date_utc: Option<chrono::DateTime<chrono::Utc>>,
}

impl ReleaseView {
    pub(super) fn new(cx: &mut App, album_id: i64, target_track_id: Option<i64>) -> Entity<Self> {
        cx.new(|cx| {
            // Album deleted under us (e.g. cleanup scan): render an empty
            // release page instead of panicking in the UI thread.
            let album = match cx.get_album_by_id(album_id) {
                Ok(album) => album,
                Err(_) => Arc::new(Album {
                    id: album_id,
                    title: DBString::from(""),
                    artist_display_override: None,
                    release_date: None,
                    date_precision: None,
                    label: None,
                    catalog_number: None,
                    isrc: None,
                    vinyl_numbering: false,
                }),
            };
            let tracks = cx.list_tracks_in_album(album_id).unwrap_or_default();
            let artist_name = album.artist_display_override.clone();
            let track_ids: std::rc::Rc<[i64]> = tracks.iter().map(|t| t.id).collect();
            let release_date_utc = album.release_date.as_ref().and_then(|date| {
                chrono::NaiveDate::parse_from_str(date.0.as_str(), "%Y-%m-%d")
                    .ok()
                    .map(|nd| {
                        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                            nd.and_hms_opt(0, 0, 0).expect("date has zero time"),
                            chrono::Utc,
                        )
                    })
            });

            cx.on_release(|this: &mut Self, cx: &mut App| {
                ImageSource::Resource(Resource::Embedded(this.img_path.clone())).remove_asset(cx);
            })
            .detach();

            let track_listing = TrackListing::new(
                cx,
                tracks.clone(),
                ArtistNameVisibility::OnlyIfDifferent(artist_name.clone()),
                album.vinyl_numbering,
                false,
                true,
            );
            let collection_summary = format_collection_summary(
                tracks.len() as i64,
                tracks.iter().map(|track| track.duration).sum(),
            );

            let release_info = {
                let mut info = String::default();

                if let Some(label) = &album.label {
                    info += label.0.as_str();
                }

                if album.label.is_some() && album.catalog_number.is_some() {
                    info += " • ";
                }

                if let Some(catalog_number) = &album.catalog_number {
                    info += catalog_number.0.as_str();
                }

                if !info.is_empty() {
                    Some(SharedString::from(info))
                } else {
                    None
                }
            };

            let pending_scroll = target_track_id.and_then(|track_id| {
                tracks
                    .iter()
                    .position(|track| track.id == track_id && is_track_available(track))
            });

            let tracks_available: Arc<Vec<bool>> = Arc::new(
                tracks
                    .iter()
                    .map(|track| is_track_available(track))
                    .collect(),
            );

            let all_liked = compute_all_liked(cx, &tracks);

            let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
            cx.subscribe(&playlist_tracker, |this: &mut Self, _, ev, cx| {
                if *ev != PlaylistEvent::PlaylistUpdated(LIKED_SONGS_PLAYLIST_ID) {
                    return;
                }
                let new_all_liked = compute_all_liked(cx, &this.tracks);
                if new_all_liked != this.all_liked {
                    this.all_liked = new_all_liked;
                    cx.notify();
                }
            })
            .detach();

            ReleaseView {
                album,
                artist_name,
                tracks,
                tracks_available,
                track_listing,
                collection_summary,
                release_info,
                img_path: SharedString::from(format!("!db://album/{album_id}/full")),
                scroll_handle: ScrollHandle::new(),
                pending_scroll,
                scroll_follow: SmoothScrollFollow::new(RELEASE_SCROLL_ANIMATION_DURATION),
                scroll_frame_scheduled: false,
                all_liked,
                menu_open: false,
                track_ids,
                release_date_utc,
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn render_header(
        &self,
        theme: &Theme,
        has_available_tracks: bool,
        current_track_in_album: bool,
        is_playing: bool,
        show_close_button: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                    .shadow_sm()
                    .w(px(160.0))
                    .h(px(160.0))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .child(
                        img(self.img_path.clone())
                            .min_w(px(160.0))
                            .min_h(px(160.0))
                            .max_w(px(160.0))
                            .max_h(px(160.0))
                            .overflow_hidden()
                            .flex()
                            // TODO: Ideally this should be ObjectFit::Cover, but this
                            // breaks rounding
                            // FIXME: This is a GPUI bug
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
                                        let tracks = self.track_listing.tracks().clone();
                                        move |cx| queue_items_from_tracks(cx, &tracks)
                                    },
                                )
                                .show_add_to_queue(false)
                                .trailing(self.render_menu_button(window, cx)),
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
                    .size(px(16.0))
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

    fn render_menu_button(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = self.menu_open;

        let (show_add_to, add_to) =
            add_album_to_playlist_state("album-menu-state", self.album.id, window, cx);
        let menu_btn = div()
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
                    .child(icon(DOTS_VERTICAL).size(px(16.0)).my_auto()),
            )
            .when(menu_open, |this| {
                let album = Rc::new((*self.album).clone());
                let weak_self = cx.entity().downgrade();
                let close = {
                    let weak_self = weak_self.clone();
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
                                    show_add_to,
                                    AlbumContextMenuContext::default(),
                                )),
                        ),
                )
            });

        div().child(menu_btn).child(add_to).into_any_element()
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
            .when_some(self.album.date_precision, |this, precision| match precision {
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
                        // release_date missing or shorter than a 4-char year
                        // prefix (malformed metadata): skip the release-year
                        // line instead of panicking the render pass
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
            let reduced_motion = cx
                .global::<crate::settings::SettingsGlobal>()
                .model
                .read(cx)
                .interface
                .reduced_motion;
            this.advance_scroll_animation(window, cx, reduced_motion);
        });
    }

    fn advance_scroll_animation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) {
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
                    let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
                    if reduced_motion {
                        self.scroll_follow
                            .jump_to(&scroll_handle, target_scroll_top);
                    } else {
                        self.scroll_follow
                            .animate_to(&scroll_handle, target_scroll_top);
                    }
                    self.pending_scroll = None;
                }
            }
        }

        let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
        if reduced_motion {
            if self.scroll_follow.snap(&scroll_handle) {
                cx.notify();
            }
            return;
        }

        let changed = self.scroll_follow.advance(&scroll_handle);

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

        let Some(item_bounds) = self.scroll_handle.bounds_for_item(track_index + 1) else {
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
        if self.pending_scroll.is_some() || self.scroll_follow.is_active() {
            if reduced_motion {
                // Reduced motion still needs one pass to resolve pending layout and snap any
                // in-flight scroll animation to its final position; we just skip scheduling
                // another animated frame afterward.
                self.advance_scroll_animation(window, cx, reduced_motion);
            } else {
                self.schedule_scroll_frame(window, cx);
            }
        }

        let theme = cx.global::<Theme>().clone();

        let is_playing =
            cx.global::<PlaybackInfo>().playback_state.read(cx) == &PlaybackState::Playing;
        // Flag whether the current track is part of the album: borrow the
        // current path (no clone) and pair each track with its precomputed
        // availability — no per-frame stats.
        let current_path = cx
            .global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .map(|current| current.get_path().as_path());
        let current_track_in_album = current_path.is_some_and(|current| {
            self.tracks
                .iter()
                .zip(self.tracks_available.iter())
                .any(|(track, &available)| available && current == track.location.as_path())
        });
        let has_available_tracks = self.tracks_available.iter().any(|&a| a);

        let scroll_handle = self.scroll_handle.clone();
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();
        let two_column = settings.interface.two_column_library;

        div()
            .image_cache(meliora_cache(("release", self.album.id as u64), 1))
            .flex()
            .flex_col()
            .w_full()
            .max_h_full()
            .relative()
            .overflow_hidden()
            .when(!full_width, |this| this.max_w(px(TABLE_MAX_WIDTH)))
            .child(
                div()
                    .id("release-view")
                    .overflow_y_scroll()
                    .track_scroll(&scroll_handle)
                    .w_full()
                    .flex_shrink(1.0)
                    .overflow_x_hidden()
                    .child(self.render_header(
                        &theme,
                        has_available_tracks,
                        current_track_in_album,
                        is_playing,
                        two_column,
                        window,
                        cx,
                    ))
                    .children(self.track_listing.track_elements())
                    .when(
                        self.release_info.is_some()
                            || self.album.release_date.is_some()
                            || self.album.isrc.is_some(),
                        |this| this.child(self.render_footer(&theme)),
                    ),
            )
            .child(floating_scrollbar("release_scrollbar", scroll_handle).right(px(4.0)))
    }
}
