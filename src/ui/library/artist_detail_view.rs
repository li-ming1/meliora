use std::{rc::Rc, sync::Arc};

use cntp_i18n::tr;
use gpui::*;
use prelude::FluentBuilder;
use rustc_hash::FxHashMap;

use crate::{
    library::{
        db::{LibraryAccess, LikedTrackSortMethod},
        types::{Album, DBString, Track, table::AlbumColumn},
    },
    playback::thread::PlaybackState,
    ui::{
        availability::is_track_available,
        caching::meliora_cache,
        components::{
            button::{ButtonSize, button},
            dropdown::dropdown,
            icons::{SORT_ASCENDING, SORT_DESCENDING, icon},
            playback_controls::playback_controls,
            scrollbar::floating_scrollbar,
            table::{
                grid_item::GridItem,
                table_data::{GridContext, TABLE_MAX_WIDTH},
            },
            tooltip::build_tooltip,
            uniform_grid::uniform_grid,
        },
        library::{
            context_menus::{AlbumContextMenuContext, queue_items_from_tracks},
            track_item::{ArtistNameVisibility, TrackItem, TrackItemLeftField},
            detail_close_button,
        },
        models::{Models, PlaybackInfo, PlaylistEvent},
        theme::Theme,
        util::{create_or_retrieve_view, prune_views},
    },
};

use super::ViewSwitchMessage;

type GridHandler = dyn Fn(&mut App, &(u32, String)) + 'static;

/// Per-track `is_track_available` results, computed once per track-list load:
/// re-statting every file on every render frame is far too expensive.
fn availability_map(tracks: &[Track]) -> Arc<Vec<bool>> {
    Arc::new(tracks.iter().map(|track| is_track_available(track)).collect())
}

/// uniform_list needs one fixed row height; 40px is what the non-virtualized
/// wrappers (`div().h(px(40.0))`) already pinned every TrackItem to.
const TRACK_ROW_HEIGHT: f32 = 40.0;

/// uniform_list only culls rows while its own viewport is smaller than its
/// content, so a list keeps its natural height up to this many rows and
/// scrolls internally beyond that instead of stretching the page scroll.
const MAX_VISIBLE_TRACK_ROWS: usize = 12;

pub struct ArtistDetailView {
    artist_id: i64,
    artist_name: Option<DBString>,
    album_ids: Arc<Vec<(u32, String)>>,
    liked_track_items: Vec<Entity<TrackItem>>,
    standalone_track_items: Vec<Entity<TrackItem>>,
    all_tracks: Arc<Vec<Track>>,
    all_tracks_available: Arc<Vec<bool>>,
    liked_tracks: Arc<Vec<Track>>,
    liked_tracks_available: Arc<Vec<bool>>,
    standalone_tracks: Arc<Vec<Track>>,
    standalone_tracks_available: Arc<Vec<bool>>,
    scroll_handle: ScrollHandle,
    /// Scroll state of the two virtualized track lists; kept for the view's
    /// lifetime so a re-sort keeps the reader's position, like PlaylistView.
    liked_scroll_handle: UniformListScrollHandle,
    standalone_scroll_handle: UniformListScrollHandle,
    grid_views: Entity<FxHashMap<usize, Entity<GridItem<Album, AlbumColumn>>>>,
    grid_render_counter: Entity<usize>,
    nav_model: Entity<super::NavigationHistory>,
    liked_sort: LikedTrackSortMethod,
    standalone_sort: LikedTrackSortMethod,
}

impl ArtistDetailView {
    pub(super) fn new(
        cx: &mut App,
        artist_id: i64,
        nav_model: Entity<super::NavigationHistory>,
    ) -> Entity<Self> {
        let view: Entity<Self> = cx.new(|cx| {
            let artist = cx.get_artist_by_id(artist_id).ok();
            let artist_name = artist.as_ref().and_then(|a| a.name.clone());

            let album_ids = Arc::new(cx.list_albums_by_artist(artist_id).unwrap_or_default());

            let all_tracks = cx
                .get_all_tracks_by_artist(artist_id)
                .unwrap_or_else(|_| Arc::new(Vec::new()));
            let all_tracks_available = availability_map(&all_tracks);

            let liked_sort = *cx.global::<Models>().liked_tracks_sort_method.read(cx);

            let liked_tracks = cx
                .get_liked_tracks_by_artist(artist_id, liked_sort)
                .unwrap_or_else(|_| Arc::new(Vec::new()));
            let liked_tracks_available = availability_map(&liked_tracks);

            let liked_track_items: Vec<Entity<TrackItem>> = liked_tracks
                .iter()
                .map(|track| {
                    TrackItem::new(
                        cx,
                        track.clone(),
                        false,
                        ArtistNameVisibility::OnlyIfDifferent(artist_name.clone()),
                        TrackItemLeftField::Art,
                        None,
                        false,
                        None,
                        Some(liked_tracks.clone()),
                        false,
                        false,
                    )
                })
                .collect();

            let standalone_sort = LikedTrackSortMethod::ReleaseOrder;

            let standalone_tracks = cx
                .get_standalone_tracks_by_artist(artist_id, standalone_sort)
                .unwrap_or_else(|_| Arc::new(Vec::new()));
            let standalone_tracks_available = availability_map(&standalone_tracks);

            let standalone_track_items: Vec<Entity<TrackItem>> = standalone_tracks
                .iter()
                .map(|track| {
                    TrackItem::new(
                        cx,
                        track.clone(),
                        false,
                        ArtistNameVisibility::OnlyIfDifferent(artist_name.clone()),
                        TrackItemLeftField::Art,
                        None,
                        false,
                        None,
                        Some(standalone_tracks.clone()),
                        false,
                        false,
                    )
                })
                .collect();

            let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

            cx.subscribe(&playlist_tracker, move |this: &mut Self, _, ev, cx| {
                if let PlaylistEvent::PlaylistUpdated(1) = ev {
                    let liked_tracks = cx
                        .get_liked_tracks_by_artist(artist_id, this.liked_sort)
                        .unwrap_or_else(|_| Arc::new(Vec::new()));

                    this.set_liked_tracks(liked_tracks, cx);
                }
            })
            .detach();

            let grid_views = cx.new(|_| FxHashMap::default());
            let grid_render_counter = cx.new(|_| 0usize);

            ArtistDetailView {
                artist_id,
                artist_name,
                album_ids,
                liked_track_items,
                standalone_track_items,
                all_tracks,
                all_tracks_available,
                liked_tracks: liked_tracks.clone(),
                liked_tracks_available,
                standalone_tracks: standalone_tracks.clone(),
                standalone_tracks_available,
                scroll_handle: ScrollHandle::new(),
                liked_scroll_handle: UniformListScrollHandle::new(),
                standalone_scroll_handle: UniformListScrollHandle::new(),
                grid_views,
                grid_render_counter,
                nav_model: nav_model.clone(),
                liked_sort,
                standalone_sort,
            }
        });

        view
    }

    pub fn update_liked_sort(&mut self, sort_method: LikedTrackSortMethod, cx: &mut Context<Self>) {
        let current_descending = Self::is_descending(self.liked_sort);
        let next_sort = Self::apply_direction(Self::base_sort(sort_method), current_descending);

        if self.liked_sort == next_sort {
            return;
        }

        self.liked_sort = next_sort;
        self.sync_sort_with_model(cx);

        let liked_tracks = cx
            .get_liked_tracks_by_artist(self.artist_id, self.liked_sort)
            .unwrap_or_else(|_| Arc::new(Vec::new()));

        self.set_liked_tracks(liked_tracks, cx);
    }

    fn set_liked_tracks(&mut self, liked_tracks: Arc<Vec<Track>>, cx: &mut Context<Self>) {
        self.liked_tracks = liked_tracks;
        self.liked_tracks_available = availability_map(&self.liked_tracks);

        self.liked_track_items = self
            .liked_tracks
            .iter()
            .map(|track: &Track| {
                TrackItem::new(
                    cx,
                    track.clone(),
                    false,
                    ArtistNameVisibility::OnlyIfDifferent(self.artist_name.clone()),
                    TrackItemLeftField::Art,
                    None,
                    false,
                    None,
                    Some(self.liked_tracks.clone()),
                    false,
                    false,
                )
            })
            .collect();

        cx.notify();
    }

    fn toggle_liked_sort_order(&mut self, cx: &mut Context<Self>) {
        self.liked_sort = Self::toggled_sort(self.liked_sort);
        self.sync_sort_with_model(cx);
        let liked_tracks = cx
            .get_liked_tracks_by_artist(self.artist_id, self.liked_sort)
            .unwrap_or_else(|_| Arc::new(Vec::new()));
        self.set_liked_tracks(liked_tracks, cx);
    }

    fn update_standalone_sort(
        &mut self,
        sort_method: LikedTrackSortMethod,
        cx: &mut Context<Self>,
    ) {
        let current_descending = Self::is_descending(self.standalone_sort);
        let next_sort = Self::apply_direction(Self::base_sort(sort_method), current_descending);

        if self.standalone_sort == next_sort {
            return;
        }

        self.standalone_sort = next_sort;

        let standalone_tracks = cx
            .get_standalone_tracks_by_artist(self.artist_id, self.standalone_sort)
            .unwrap_or_else(|_| Arc::new(Vec::new()));

        self.set_standalone_tracks(standalone_tracks, cx);
    }

    fn set_standalone_tracks(
        &mut self,
        standalone_tracks: Arc<Vec<Track>>,
        cx: &mut Context<Self>,
    ) {
        self.standalone_tracks = standalone_tracks;
        self.standalone_tracks_available = availability_map(&self.standalone_tracks);

        self.standalone_track_items = self
            .standalone_tracks
            .iter()
            .map(|track: &Track| {
                TrackItem::new(
                    cx,
                    track.clone(),
                    false,
                    ArtistNameVisibility::OnlyIfDifferent(self.artist_name.clone()),
                    TrackItemLeftField::Art,
                    None,
                    false,
                    None,
                    Some(self.standalone_tracks.clone()),
                    false,
                    false,
                )
            })
            .collect();

        cx.notify();
    }

    fn toggle_standalone_sort_order(&mut self, cx: &mut Context<Self>) {
        self.standalone_sort = Self::toggled_sort(self.standalone_sort);
        let standalone_tracks = cx
            .get_standalone_tracks_by_artist(self.artist_id, self.standalone_sort)
            .unwrap_or_else(|_| Arc::new(Vec::new()));
        self.set_standalone_tracks(standalone_tracks, cx);
    }

    fn base_sort(sort_method: LikedTrackSortMethod) -> LikedTrackSortMethod {
        match sort_method {
            LikedTrackSortMethod::TitleAsc | LikedTrackSortMethod::TitleDesc => {
                LikedTrackSortMethod::TitleAsc
            }
            LikedTrackSortMethod::ReleaseOrder | LikedTrackSortMethod::ReleaseOrderDesc => {
                LikedTrackSortMethod::ReleaseOrder
            }
            LikedTrackSortMethod::RecentlyAdded | LikedTrackSortMethod::RecentlyAddedAsc => {
                LikedTrackSortMethod::RecentlyAdded
            }
        }
    }

    fn apply_direction(
        base_sort_method: LikedTrackSortMethod,
        descending: bool,
    ) -> LikedTrackSortMethod {
        match base_sort_method {
            LikedTrackSortMethod::TitleAsc | LikedTrackSortMethod::TitleDesc => {
                if descending {
                    LikedTrackSortMethod::TitleDesc
                } else {
                    LikedTrackSortMethod::TitleAsc
                }
            }
            LikedTrackSortMethod::ReleaseOrder | LikedTrackSortMethod::ReleaseOrderDesc => {
                if descending {
                    LikedTrackSortMethod::ReleaseOrderDesc
                } else {
                    LikedTrackSortMethod::ReleaseOrder
                }
            }
            LikedTrackSortMethod::RecentlyAdded | LikedTrackSortMethod::RecentlyAddedAsc => {
                if descending {
                    LikedTrackSortMethod::RecentlyAdded
                } else {
                    LikedTrackSortMethod::RecentlyAddedAsc
                }
            }
        }
    }

    fn is_descending(sort_method: LikedTrackSortMethod) -> bool {
        matches!(
            sort_method,
            LikedTrackSortMethod::TitleDesc
                | LikedTrackSortMethod::ReleaseOrderDesc
                | LikedTrackSortMethod::RecentlyAdded
        )
    }

    fn toggled_sort(sort_method: LikedTrackSortMethod) -> LikedTrackSortMethod {
        match sort_method {
            LikedTrackSortMethod::TitleAsc => LikedTrackSortMethod::TitleDesc,
            LikedTrackSortMethod::TitleDesc => LikedTrackSortMethod::TitleAsc,
            LikedTrackSortMethod::ReleaseOrder => LikedTrackSortMethod::ReleaseOrderDesc,
            LikedTrackSortMethod::ReleaseOrderDesc => LikedTrackSortMethod::ReleaseOrder,
            LikedTrackSortMethod::RecentlyAdded => LikedTrackSortMethod::RecentlyAddedAsc,
            LikedTrackSortMethod::RecentlyAddedAsc => LikedTrackSortMethod::RecentlyAdded,
        }
    }

    fn sync_sort_with_model(&self, cx: &mut Context<Self>) {
        let liked_tracks_sort_method = cx.global::<Models>().liked_tracks_sort_method.clone();
        liked_tracks_sort_method.update(cx, |value, _| *value = self.liked_sort);
    }
}

impl Render for ArtistDetailView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let entity = cx.entity();
        let standalone_entity = entity.clone();
        let liked_list_entity = entity.clone();
        let standalone_list_entity = entity.clone();

        let scroll_handle = self.scroll_handle.clone();
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();
        let two_column = settings.interface.two_column_library;
        let grid_min_item_width = crate::settings::interface::clamp_grid_min_item_width(
            settings.interface.grid_min_item_width,
        );

        let album_count = self.album_ids.len();
        let album_ids = self.album_ids.clone();
        let grid_views_model = self.grid_views.clone();
        let grid_render_counter = self.grid_render_counter.clone();
        let nav_model = self.nav_model.clone();

        let is_playing =
            cx.global::<PlaybackInfo>().playback_state.read(cx) == &PlaybackState::Playing;

        // Borrow the current track's path once: the three membership checks
        // below only compare against each row's own location (no clones), and
        // availability comes from the precomputed per-track maps.
        let current_path = cx
            .global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .map(|current| current.get_path().as_path());

        let current_track_in_artist = current_path.is_some_and(|current| {
            self.all_tracks
                .iter()
                .zip(self.all_tracks_available.iter())
                .any(|(track, &available)| available && current == track.location.as_path())
        });
        let has_available_artist_tracks = self.all_tracks_available.iter().any(|&a| a);

        let current_track_in_liked = current_path.is_some_and(|current| {
            self.liked_tracks
                .iter()
                .zip(self.liked_tracks_available.iter())
                .any(|(track, &available)| available && current == track.location.as_path())
        });
        let has_available_liked_tracks = self.liked_tracks_available.iter().any(|&a| a);

        let current_track_in_standalone = current_path.is_some_and(|current| {
            self.standalone_tracks
                .iter()
                .zip(self.standalone_tracks_available.iter())
                .any(|(track, &available)| available && current == track.location.as_path())
        });
        let has_available_standalone_tracks = self.standalone_tracks_available.iter().any(|&a| a);

        let liked_track_header =
            if !self.liked_track_items.is_empty() {
                Some(
                    div()
                        .border_t_1()
                        .border_color(theme.border_color)
                        .px(px(18.0))
                        .pt(px(10.0))
                        .pb(px(5.0))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .text_size(px(18.0))
                                .my_auto()
                                .child(tr!("ARTIST_LIKED_TRACKS", "Liked Tracks")),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_between()
                                .pb(px(13.0))
                                .child(playback_controls(
                                    "artist-liked",
                                    has_available_liked_tracks,
                                    current_track_in_liked,
                                    is_playing,
                                    {
                                        let liked_tracks = self.liked_tracks.clone();
                                        move |cx| queue_items_from_tracks(cx, &liked_tracks)
                                    },
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(12.0))
                                        .items_stretch()
                                        .child(
                                            button()
                                                .id("artist-liked-sort-direction-button")
                                                .size(ButtonSize::Large)
                                                .on_click(cx.listener(
                                                    |this: &mut ArtistDetailView, _, _, cx| {
                                                        this.toggle_liked_sort_order(cx);
                                                    },
                                                ))
                                                .child(
                                                    icon(if Self::is_descending(self.liked_sort) {
                                                        SORT_DESCENDING
                                                    } else {
                                                        SORT_ASCENDING
                                                    })
                                                    .text_color(theme.text_secondary)
                                                    .size(px(16.0)),
                                                )
                                                .tooltip(if Self::is_descending(self.liked_sort) {
                                                    build_tooltip(tr!(
                                                        "SORT_ASCENDING",
                                                        "Sort Ascending"
                                                    ))
                                                } else {
                                                    build_tooltip(tr!(
                                                        "SORT_DESCENDING",
                                                        "Sort Descending"
                                                    ))
                                                }),
                                        )
                                        .child(
                                            dropdown::<LikedTrackSortMethod>(
                                                "artist-liked-sort-dropdown",
                                            )
                                            .option(
                                                LikedTrackSortMethod::RecentlyAdded,
                                                tr!("SORT_RECENTLY_ADDED", "Recently Added"),
                                            )
                                            .option(
                                                LikedTrackSortMethod::TitleAsc,
                                                tr!("SORT_TITLE", "Title"),
                                            )
                                            .option(
                                                LikedTrackSortMethod::ReleaseOrder,
                                                tr!("SORT_RELEASE_ORDER", "Release Order"),
                                            )
                                            .selected(Self::base_sort(self.liked_sort))
                                            .w(px(200.0))
                                            .on_change(move |sort_method, _, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.update_liked_sort(*sort_method, cx);
                                                });
                                            }),
                                        ),
                                ),
                        ),
                )
            } else {
                None
            };

        let standalone_track_header = if !self.standalone_track_items.is_empty() {
            Some(
                div()
                    .border_t_1()
                    .border_color(theme.border_color)
                    .px(px(18.0))
                    .pt(px(10.0))
                    .pb(px(5.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(18.0))
                            .my_auto()
                            .child(tr!("TRACKS")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .pb(px(13.0))
                            .child(playback_controls(
                                "artist-standalone",
                                has_available_standalone_tracks,
                                current_track_in_standalone,
                                is_playing,
                                {
                                    let standalone_tracks = self.standalone_tracks.clone();
                                    move |cx| queue_items_from_tracks(cx, &standalone_tracks)
                                },
                            ))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(12.0))
                                    .items_stretch()
                                    .child(
                                        button()
                                            .id("artist-standalone-sort-direction-button")
                                            .size(ButtonSize::Large)
                                            .on_click(cx.listener(
                                                |this: &mut ArtistDetailView, _, _, cx| {
                                                    this.toggle_standalone_sort_order(cx);
                                                },
                                            ))
                                            .child(
                                                icon(
                                                    if Self::is_descending(self.standalone_sort) {
                                                        SORT_DESCENDING
                                                    } else {
                                                        SORT_ASCENDING
                                                    },
                                                )
                                                .text_color(theme.text_secondary)
                                                .size(px(16.0)),
                                            )
                                            .tooltip(
                                                if Self::is_descending(self.standalone_sort) {
                                                    build_tooltip(tr!("SORT_ASCENDING"))
                                                } else {
                                                    build_tooltip(tr!("SORT_DESCENDING"))
                                                },
                                            ),
                                    )
                                    .child(
                                        dropdown::<LikedTrackSortMethod>(
                                            "artist-standalone-sort-dropdown",
                                        )
                                        .option(
                                            LikedTrackSortMethod::RecentlyAdded,
                                            tr!("SORT_RECENTLY_ADDED"),
                                        )
                                        .option(LikedTrackSortMethod::TitleAsc, tr!("SORT_TITLE"))
                                        .option(
                                            LikedTrackSortMethod::ReleaseOrder,
                                            tr!("SORT_RELEASE_ORDER"),
                                        )
                                        .selected(Self::base_sort(self.standalone_sort))
                                        .w(px(200.0))
                                        .on_change(
                                            move |sort_method, _, cx| {
                                                standalone_entity.update(cx, |this, cx| {
                                                    this.update_standalone_sort(*sort_method, cx);
                                                });
                                            },
                                        ),
                                    ),
                            ),
                    ),
            )
        } else {
            None
        };

        // Virtualized track lists: heights are capped so a long list scrolls
        // internally instead of inflating the page scroll, and rows are read
        // through the entity inside the uniform_list closure (which outlives
        // the render borrow).
        let liked_row_count = self.liked_track_items.len();
        let liked_list_height =
            px(TRACK_ROW_HEIGHT * liked_row_count.min(MAX_VISIBLE_TRACK_ROWS) as f32);
        let liked_scroll_handle = self.liked_scroll_handle.clone();

        let standalone_row_count = self.standalone_track_items.len();
        let standalone_list_height =
            px(TRACK_ROW_HEIGHT * standalone_row_count.min(MAX_VISIBLE_TRACK_ROWS) as f32);
        let standalone_scroll_handle = self.standalone_scroll_handle.clone();

        div()
            .flex()
            .flex_col()
            .w_full()
            .max_h_full()
            .relative()
            .overflow_hidden()
            .when(!full_width, |this| this.max_w(px(TABLE_MAX_WIDTH)))
            .child(
                div()
                    .flex()
                    .w_full()
                    .max_h_full()
                    .relative()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("artist-detail-view")
                            .overflow_y_scroll()
                            .track_scroll(&scroll_handle)
                            .pb(px(18.0))
                            .w_full()
                            .flex_shrink(1.0)
                            .overflow_x_hidden()
                            .child(
                                div()
                                    .pt(px(52.0))
                                    .px(px(18.0))
                                    .w_full()
                                    .relative()
                                    .when(two_column, |this| {
                                        this.child(detail_close_button("artist_detail_close"))
                                    })
                                    .child(
                                        div()
                                            .font_weight(FontWeight::EXTRA_BOLD)
                                            .text_size(rems(2.5))
                                            .line_height(rems(2.75))
                                            .overflow_x_hidden()
                                            .pb(px(10.0))
                                            .w_full()
                                            .text_ellipsis()
                                            .when_some(self.artist_name.clone(), |this, name| {
                                                this.child(name)
                                            }),
                                    )
                                    .when(!self.all_tracks.is_empty(), |this| {
                                        this.child(div().pb(px(18.0)).child(playback_controls(
                                            "artist",
                                            has_available_artist_tracks,
                                            current_track_in_artist,
                                            is_playing,
                                            {
                                                let all_tracks = self.all_tracks.clone();
                                                move |cx| queue_items_from_tracks(cx, &all_tracks)
                                            },
                                        )))
                                    }),
                            )
                            .when(album_count > 0, |this| {
                                let handler: Option<Rc<GridHandler>> =
                                    Some(Rc::new(move |cx, id| {
                                        nav_model.update(cx, |_, cx| {
                                            cx.emit(ViewSwitchMessage::Release(id.0 as i64, None));
                                        });
                                    }));

                                this.child(
                                    div()
                                        .border_t_1()
                                        .border_color(theme.border_color)
                                        .px(px(18.0))
                                        .pt(px(10.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_size(px(18.0))
                                        .child(tr!("ARTIST_ALBUMS", "Albums")),
                                )
                                .child(
                                    div().px(px(10.0)).pt(px(2.0)).pb(px(10.0)).w_full().child(
                                        uniform_grid(
                                            "artist-albums-grid",
                                            album_count,
                                            None,
                                            move |idx, _, cx| {
                                                prune_views(
                                                    &grid_views_model,
                                                    &grid_render_counter,
                                                    idx,
                                                    cx,
                                                );

                                                let item_id = album_ids[idx].clone();

                                                let view = create_or_retrieve_view(
                                                    &grid_views_model,
                                                    idx,
                                                    |cx| {
                                                        GridItem::<Album, AlbumColumn>::new(
                                                            cx,
                                                            item_id,
                                                            handler.clone(),
                                                            AlbumContextMenuContext {
                                                                show_go_to_artist: false,
                                                            },
                                                            GridContext::Standalone,
                                                        )
                                                        .unwrap()
                                                    },
                                                    cx,
                                                );

                                                div()
                                                    .image_cache(meliora_cache(
                                                        ("artist-album-grid", idx + 1),
                                                        1,
                                                    ))
                                                    .size_full()
                                                    .child(view)
                                                    .into_any_element()
                                            },
                                        )
                                        .min_item_width(px(grid_min_item_width))
                                        .gap(px(0.0))
                                        .auto_height(),
                                    ),
                                )
                            })
                            .when_some(liked_track_header, move |this, header| {
                                this.child(header).child(
                                    div()
                                        .w_full()
                                        .border_t_1()
                                        .border_color(theme.border_color)
                                        .image_cache(retain_all("artist_liked_tracks_cache"))
                                        .child(
                                            uniform_list(
                                                "artist-liked-tracks",
                                                liked_row_count,
                                                move |range, _, cx| {
                                                    let view = liked_list_entity.read(cx);
                                                    view.liked_track_items[range]
                                                        .iter()
                                                        .map(|item| {
                                                            div()
                                                                .h(px(TRACK_ROW_HEIGHT))
                                                                .child(item.clone())
                                                        })
                                                        .collect()
                                                },
                                            )
                                            .h(liked_list_height)
                                            .w_full()
                                            .track_scroll(&liked_scroll_handle),
                                        ),
                                )
                            })
                            .when_some(standalone_track_header, move |this, header| {
                                this.child(header).child(
                                    div()
                                        .w_full()
                                        .border_t_1()
                                        .border_color(theme.border_color)
                                        .image_cache(retain_all("artist_standalone_tracks_cache"))
                                        .child(
                                            uniform_list(
                                                "artist-standalone-tracks",
                                                standalone_row_count,
                                                move |range, _, cx| {
                                                    let view = standalone_list_entity.read(cx);
                                                    view.standalone_track_items[range]
                                                        .iter()
                                                        .map(|item| {
                                                            div()
                                                                .h(px(TRACK_ROW_HEIGHT))
                                                                .child(item.clone())
                                                        })
                                                        .collect()
                                                },
                                            )
                                            .h(standalone_list_height)
                                            .w_full()
                                            .track_scroll(&standalone_scroll_handle),
                                        ),
                                )
                            }),
                    )
                    .child(
                        floating_scrollbar("artist_detail_scrollbar", scroll_handle).right(px(4.0)),
                    ),
            )
    }
}
