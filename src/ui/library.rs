use std::{path::PathBuf, time::Duration};

use album_view::AlbumView;
use artist_detail_view::ArtistDetailView;
use artist_view::ArtistView;
use cntp_i18n::tr;
use files_view::FilesView;
use gpui::{prelude::FluentBuilder, *};

use crate::library::scan::ScanEvent;
use release_view::ReleaseView;
use tracing::debug;
use track_view::TrackView;

#[derive(Clone, Default)]
struct ScrollStateStorage {
    album_view_scroll: Option<f32>,
    track_view_scroll: Option<f32>,
    artist_view_scroll: Option<f32>,
    files_view_scroll: Option<f32>,
    files_expanded: Vec<PathBuf>,
}

use crate::{
    library::db::LibraryAccess,
    settings::storage::DEFAULT_SPLIT_FRACTION,
    ui::{
        command_palette::{CommandCategory, CommandManager, CommandSpec},
        components::{
            resizable::{ResizeEdge, resizable},
            table::table_data::TABLE_MAX_WIDTH,
        },
        library::{
            playlist_view::{Import, PlaylistView},
            update_playlist::UpdatePlaylist,
        },
    },
};

use super::models::Models;

pub mod add_to_playlist;
mod album_view;
mod artist_detail_view;
mod artist_view;
pub mod collection_summary;
pub mod context_menus;
pub mod files_view;
#[cfg(feature = "kugou")]
mod kugou_playlists;
#[cfg(feature = "kugou")]
mod kugou_ranks;
#[cfg(feature = "netease")]
mod netease_playlists;
#[cfg(feature = "netease")]
mod netease_ranks;
pub mod missing_folder_dialog;
pub mod playlist_view;
mod release_view;
pub mod sidebar;
mod table_view_header;
pub mod track_item;
mod track_view;
mod update_playlist;
pub mod view_header;

actions!(library, [NavigateBack, NavigateForward, EscapeBack]);

/// Absolute close button anchored to the detail-view host (album/release).
pub fn detail_close_button(id: impl Into<ElementId>) -> impl IntoElement {
    crate::ui::components::nav_button::nav_button(
        id,
        crate::ui::components::icons::CROSS,
    )
    .absolute()
    .top(px(12.0))
    .right(px(18.0))
    .on_click(|_, window, cx| {
        window.dispatch_action(Box::new(EscapeBack), cx);
    })
    .tooltip(crate::ui::components::tooltip::build_tooltip(tr!(
        "CLOSE_RELEASE_DETAIL",
        "Close"
    )))
}

/// The navigation history + a cursor noting what the current message is.
#[derive(Debug)]
pub struct NavigationHistory {
    startup_view: ViewSwitchMessage,
    history: Vec<ViewSwitchMessage>,
    cursor: usize,
}

impl NavigationHistory {
    pub fn new(startup_view: ViewSwitchMessage) -> Self {
        Self {
            startup_view,
            history: vec![startup_view],
            cursor: 0,
        }
    }

    pub fn current(&self) -> ViewSwitchMessage {
        self.history[self.cursor]
    }

    pub fn can_go_back(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.cursor < self.history.len() - 1
    }

    /// Returns the history entry immediately before the cursor, if any.
    pub fn previous(&self) -> Option<ViewSwitchMessage> {
        if self.cursor > 0 {
            Some(self.history[self.cursor - 1])
        } else {
            None
        }
    }

    pub fn go_back(&mut self) -> Option<ViewSwitchMessage> {
        if self.can_go_back() {
            self.cursor -= 1;
            Some(self.current())
        } else {
            None
        }
    }

    pub fn go_forward(&mut self) -> Option<ViewSwitchMessage> {
        if self.can_go_forward() {
            self.cursor += 1;
            Some(self.current())
        } else {
            None
        }
    }

    /// Navigates to a new view. All history entries after the cursor are discarded, then the new
    /// view is appended and the cursor advances to it. History is capped at 100 entries.
    pub fn navigate(&mut self, message: ViewSwitchMessage) {
        // Drop any forward history.
        self.history.truncate(self.cursor + 1);

        // Cap total history at 100 entries by evicting the oldest.
        if self.history.len() >= 100 {
            let remove_idx = self.eviction_index();
            self.history.remove(remove_idx);

            if remove_idx <= self.cursor {
                self.cursor = self.cursor.saturating_sub(1);
            }
        }

        self.history.push(message);
        self.cursor = self.history.len() - 1;
    }

    fn eviction_index(&self) -> usize {
        let oldest_idx = 0;
        let most_recent_key_idx = self
            .history
            .iter()
            .rposition(ViewSwitchMessage::is_key_page);

        if most_recent_key_idx == Some(oldest_idx) && self.history.len() > 1 {
            1
        } else {
            oldest_idx
        }
    }

    /// Finds the most recent history entry (before the cursor) that matches a predicate.
    pub fn last_matching(
        &self,
        pred: impl Fn(&ViewSwitchMessage) -> bool,
    ) -> Option<ViewSwitchMessage> {
        self.history[..self.cursor]
            .iter()
            .rev()
            .find(|m| pred(m))
            .copied()
    }

    /// Removes history entries that do not satisfy `f`, adjusting the cursor so that it continues
    /// to point at the same entry if it survives, or backs up to the nearest preceding survivor
    /// otherwise. History is guaranteed to never become empty (falls back to the startup view).
    ///
    /// Used to remove entries that are no longer valid.
    pub fn retain<F>(&mut self, f: F)
    where
        F: Fn(&ViewSwitchMessage) -> bool,
    {
        // Count how many entries at or before the cursor will be removed.
        let removed_before_or_at_cursor = self.history[..=self.cursor]
            .iter()
            .filter(|v| !f(v))
            .count();

        self.history.retain(f);

        if self.history.is_empty() {
            self.history.push(self.startup_view);
            self.cursor = 0;
        } else {
            self.cursor = self
                .cursor
                .saturating_sub(removed_before_or_at_cursor)
                .min(self.history.len() - 1);
        }
    }
}

impl Default for NavigationHistory {
    fn default() -> Self {
        Self::new(ViewSwitchMessage::Albums)
    }
}

impl EventEmitter<ViewSwitchMessage> for NavigationHistory {}

/// Tracks which top-level section the user is currently in so that
/// context-dependent actions (like "go up") can behave correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibrarySection {
    Albums,
    Artists,
    Tracks,
    Files,
    Playlists,
}

impl LibrarySection {
    /// Derive the section from a navigation message. Returns `None` for
    /// ambiguous messages (e.g. `Release`) that should keep the current section.
    fn from_message(msg: &ViewSwitchMessage) -> Option<Self> {
        match msg {
            ViewSwitchMessage::Albums => Some(Self::Albums),
            ViewSwitchMessage::Tracks => Some(Self::Tracks),
            ViewSwitchMessage::Artists | ViewSwitchMessage::Artist(_) => Some(Self::Artists),
            ViewSwitchMessage::Files => Some(Self::Files),
            ViewSwitchMessage::Playlist(_) => Some(Self::Playlists),
            // Release can appear under Albums or Artists – keep current section.
            ViewSwitchMessage::Release(_, _) => None,
            // The KuGou page is not a local-library section – keep current.
            #[cfg(feature = "kugou")]
            ViewSwitchMessage::KugouPlaylists | ViewSwitchMessage::KugouRanks => None,
            // The NetEase pages are not local-library sections – keep current.
            #[cfg(feature = "netease")]
            ViewSwitchMessage::NeteasePlaylists | ViewSwitchMessage::NeteaseRanks => None,
            ViewSwitchMessage::Back | ViewSwitchMessage::Forward | ViewSwitchMessage::Refresh => {
                None
            }
        }
    }
}

#[derive(Clone)]
enum LibraryView {
    Album(Entity<AlbumView>),
    Tracks(Entity<TrackView>),
    Release(Entity<ReleaseView>),
    Playlist(Entity<PlaylistView>),
    Artists(Entity<ArtistView>),
    ArtistDetail(Entity<ArtistDetailView>),
    Files(Entity<FilesView>),
    #[cfg(feature = "kugou")]
    KugouPlaylists(Entity<kugou_playlists::KugouPlaylistsView>),
    #[cfg(feature = "kugou")]
    KugouRanks(Entity<kugou_ranks::KugouRanksView>),
    #[cfg(feature = "netease")]
    NeteasePlaylists(Entity<netease_playlists::NeteasePlaylistsView>),
    #[cfg(feature = "netease")]
    NeteaseRanks(Entity<netease_ranks::NeteaseRanksView>),
}

impl LibraryView {
    fn split_key(&self) -> &'static str {
        match self {
            LibraryView::Album(_) => "albums",
            LibraryView::Tracks(_) => "tracks",
            LibraryView::Artists(_) => "artists",
            LibraryView::Playlist(_) => "playlist",
            LibraryView::Release(_) => "albums",
            LibraryView::ArtistDetail(_) => "artists",
            LibraryView::Files(_) => "files",
            #[cfg(feature = "kugou")]
            LibraryView::KugouPlaylists(_) => "albums",
            #[cfg(feature = "kugou")]
            LibraryView::KugouRanks(_) => "albums",
            #[cfg(feature = "netease")]
            LibraryView::NeteasePlaylists(_) => "albums",
            #[cfg(feature = "netease")]
            LibraryView::NeteaseRanks(_) => "albums",
        }
    }
}

pub struct Library {
    view: LibraryView,
    left_view: Option<LibraryView>,
    right_view: Option<LibraryView>,
    section: LibrarySection,
    update_playlist: Entity<UpdatePlaylist>,
    focus_handle: FocusHandle,
    scroll_state: ScrollStateStorage,
    reclaim_focus: bool,
    _focus_lost_sub: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewSwitchMessage {
    Albums,
    Tracks,
    Artists,
    Files,
    /// album id, track id
    Release(i64, Option<i64>),
    Artist(i64),
    Playlist(i64),
    /// KuGou cloud playlists page (kugou feature only)
    #[cfg(feature = "kugou")]
    KugouPlaylists,
    /// KuGou discovery page: ranks + daily recommend (kugou feature only)
    #[cfg(feature = "kugou")]
    KugouRanks,
    /// NetEase cloud playlists page (netease feature only)
    #[cfg(feature = "netease")]
    NeteasePlaylists,
    /// NetEase discovery page: charts + daily recommend (netease feature only)
    #[cfg(feature = "netease")]
    NeteaseRanks,
    Back,
    Forward,
    Refresh,
}

impl ViewSwitchMessage {
    pub fn is_detail_page(&self) -> bool {
        matches!(
            self,
            ViewSwitchMessage::Release(_, _) | ViewSwitchMessage::Artist(_)
        )
    }

    pub fn is_key_page(&self) -> bool {
        !self.is_detail_page()
            && !matches!(
                self,
                ViewSwitchMessage::Back | ViewSwitchMessage::Forward | ViewSwitchMessage::Refresh
            )
    }

    fn library_view_matches(&self, lv: &LibraryView) -> bool {
        if matches!(
            (lv, self),
            (LibraryView::Album(_), ViewSwitchMessage::Albums)
                | (LibraryView::Tracks(_), ViewSwitchMessage::Tracks)
                // ArtistDetail: don't cache – we can't verify the id matches without extra storage
                | (LibraryView::Artists(_), ViewSwitchMessage::Artists)
                | (LibraryView::Files(_), ViewSwitchMessage::Files)
        ) {
            return true;
        }

        #[cfg(feature = "kugou")]
        if matches!(
            (lv, self),
            (LibraryView::KugouPlaylists(_), ViewSwitchMessage::KugouPlaylists)
                | (LibraryView::KugouRanks(_), ViewSwitchMessage::KugouRanks)
        ) {
            return true;
        }

        #[cfg(feature = "netease")]
        if matches!(
            (lv, self),
            (LibraryView::NeteasePlaylists(_), ViewSwitchMessage::NeteasePlaylists)
                | (LibraryView::NeteaseRanks(_), ViewSwitchMessage::NeteaseRanks)
        ) {
            return true;
        }

        false
    }
}

fn make_view(
    message: &ViewSwitchMessage,
    cx: &mut App,
    model: &Entity<NavigationHistory>,
    scroll_state: &ScrollStateStorage,
) -> LibraryView {
    // Every page instance (new navigation, back/forward/refresh, startup)
    // flows through here at user-navigation frequency; sampling memory here
    // attributes the browse-driven steps the periodic probe can't explain.
    crate::log_mem_event(&format!("library view: {message:?}"));

    match message {
        ViewSwitchMessage::Albums => LibraryView::Album(AlbumView::new(
            cx,
            model.clone(),
            scroll_state.album_view_scroll,
        )),
        ViewSwitchMessage::Tracks => {
            LibraryView::Tracks(TrackView::new(cx, scroll_state.track_view_scroll))
        }
        ViewSwitchMessage::Artists => LibraryView::Artists(ArtistView::new(
            cx,
            model.clone(),
            scroll_state.artist_view_scroll,
        )),
        ViewSwitchMessage::Files => LibraryView::Files(FilesView::new(
            cx,
            scroll_state.files_expanded.clone(),
            scroll_state.files_view_scroll,
        )),
        ViewSwitchMessage::Release(id, target_track_id) => {
            LibraryView::Release(ReleaseView::new(cx, *id, *target_track_id))
        }
        ViewSwitchMessage::Artist(id) => {
            LibraryView::ArtistDetail(ArtistDetailView::new(cx, *id, model.clone()))
        }
        ViewSwitchMessage::Playlist(id) => LibraryView::Playlist(PlaylistView::new(cx, *id)),
        #[cfg(feature = "kugou")]
        ViewSwitchMessage::KugouPlaylists => {
            LibraryView::KugouPlaylists(kugou_playlists::KugouPlaylistsView::new(cx))
        }
        #[cfg(feature = "kugou")]
        ViewSwitchMessage::KugouRanks => {
            LibraryView::KugouRanks(kugou_ranks::KugouRanksView::new(cx))
        }
        #[cfg(feature = "netease")]
        ViewSwitchMessage::NeteasePlaylists => {
            LibraryView::NeteasePlaylists(netease_playlists::NeteasePlaylistsView::new(cx))
        }
        #[cfg(feature = "netease")]
        ViewSwitchMessage::NeteaseRanks => {
            LibraryView::NeteaseRanks(netease_ranks::NeteaseRanksView::new(cx))
        }
        ViewSwitchMessage::Back => panic!("improper use of make_view (cannot make Back)"),
        ViewSwitchMessage::Forward => panic!("improper use of make_view (cannot make Forward)"),
        ViewSwitchMessage::Refresh => panic!("improper use of make_view (cannot make Refresh)"),
    }
}

fn library_section_from_history(history: &NavigationHistory) -> LibrarySection {
    LibrarySection::from_message(&history.current())
        .or_else(|| {
            history
                .last_matching(ViewSwitchMessage::is_key_page)
                .and_then(|message| LibrarySection::from_message(&message))
        })
        .unwrap_or(LibrarySection::Albums)
}

impl Library {
    fn sync_visible_views(&mut self, model: &Entity<NavigationHistory>, cx: &mut App) {
        let history = model.read(cx);
        let current_msg = history.current();
        let two_column = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .two_column_library;

        self.section = library_section_from_history(history);

        if two_column {
            if current_msg.is_detail_page() {
                self.right_view = Some(self.view.clone());

                let left_msg = history.last_matching(ViewSwitchMessage::is_key_page);

                let needs_new_left = match (&self.left_view, &left_msg) {
                    (None, Some(_)) | (Some(_), None) => true,
                    (Some(lv), Some(msg)) => !msg.library_view_matches(lv),
                    (None, None) => false,
                };

                if needs_new_left {
                    self.left_view = left_msg
                        .as_ref()
                        .map(|message| make_view(message, cx, model, &self.scroll_state));
                }
            } else {
                self.left_view = Some(self.view.clone());
                self.right_view = None;
            }
        } else {
            self.left_view = None;
            self.right_view = None;
        }
    }

    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let switcher_model = cx.global::<Models>().switcher_model.clone();
            let scroll_state = ScrollStateStorage::default();
            let initial_message = switcher_model.read(cx).current();
            let view = make_view(&initial_message, cx, &switcher_model, &scroll_state);
            let section = library_section_from_history(switcher_model.read(cx));

            // this view is cached, so it only re-renders when explicitly
            // dirtied: observe every width entity its layout depends on (the
            // resizable queue/lyrics panel, and the two-column split)
            let queue_width = cx.global::<Models>().queue_width.clone();
            cx.observe(&queue_width, |_, _, cx| cx.notify()).detach();

            cx.subscribe(
                &switcher_model,
                move |this: &mut Library, m, message, cx| {
                    if let LibraryView::Album(album_view) = &this.view {
                        let scroll_pos = album_view.read(cx).get_scroll_offset(cx);
                        this.scroll_state.album_view_scroll = Some(scroll_pos);
                    } else if let LibraryView::Tracks(track_view) = &this.view {
                        let scroll_pos = track_view.read(cx).get_scroll_offset(cx);
                        this.scroll_state.track_view_scroll = Some(scroll_pos);
                    } else if let LibraryView::Artists(artist_view) = &this.view {
                        let scroll_pos = artist_view.read(cx).get_scroll_offset(cx);
                        this.scroll_state.artist_view_scroll = Some(scroll_pos);
                    } else if let LibraryView::Files(files_view) = &this.view {
                        let fv = files_view.read(cx);
                        this.scroll_state.files_view_scroll = Some(fv.get_scroll_offset());
                        this.scroll_state.files_expanded = fv.expanded_paths();
                    }

                    // if we're navigating away from a view that stole focus (e.g. PlaylistView),
                    // schedule a focus reclaim so the Library div retakes focus on next render.
                    if matches!(this.view, LibraryView::Playlist(_)) {
                        this.reclaim_focus = true;
                    }

                    this.view = match message {
                        ViewSwitchMessage::Back => {
                            let destination =
                                m.update(cx, |history: &mut NavigationHistory, cx| {
                                    let result = history.go_back();
                                    cx.notify();
                                    result
                                });

                            if let Some(dest) = destination {
                                debug!("back → {:?}", dest);
                                make_view(&dest, cx, &m, &this.scroll_state)
                            } else {
                                this.view.clone()
                            }
                        }

                        ViewSwitchMessage::Forward => {
                            let destination =
                                m.update(cx, |history: &mut NavigationHistory, cx| {
                                    let result = history.go_forward();
                                    cx.notify();
                                    result
                                });

                            if let Some(dest) = destination {
                                debug!("forward → {:?}", dest);
                                make_view(&dest, cx, &m, &this.scroll_state)
                            } else {
                                this.view.clone()
                            }
                        }

                        ViewSwitchMessage::Refresh => {
                            let current = m.read(cx).current();
                            make_view(&current, cx, &m, &this.scroll_state)
                        }

                        _ => {
                            m.update(cx, |history, cx| {
                                history.navigate(*message);
                                cx.notify();
                            });

                            make_view(message, cx, &m, &this.scroll_state)
                        }
                    };

                    this.sync_visible_views(&m, cx);

                    cx.notify();
                },
            )
            .detach();

            let split_width_entities: Vec<Entity<Pixels>> = cx
                .global::<Models>()
                .split_widths
                .values()
                .cloned()
                .collect();
            for sw in split_width_entities {
                cx.observe(&sw, |_, _, cx| cx.notify()).detach();
            }

            let focus_handle = cx.focus_handle();

            cx.register_command(
                CommandSpec::new(
                    ("playlist::import", 0),
                    Some(CommandCategory::Playlist),
                    tr!("ACTION_IMPORT_PLAYLIST", "Import M3U Playlist"),
                    Import,
                )
                .focus_handle(focus_handle.clone()),
            );

            cx.register_command(
                CommandSpec::new(
                    ("library::go_back", 0),
                    Some(CommandCategory::Library),
                    tr!("ACTION_GO_BACK", "Go Back"),
                    NavigateBack,
                )
                .focus_handle(focus_handle.clone()),
            );

            cx.register_command(
                CommandSpec::new(
                    ("library::go_forward", 0),
                    Some(CommandCategory::Library),
                    tr!("ACTION_GO_FORWARD", "Go Forward"),
                    NavigateForward,
                )
                .focus_handle(focus_handle.clone()),
            );

            cx.register_command(
                CommandSpec::new(
                    ("library::close_detail_view", 0),
                    Some(CommandCategory::Library),
                    tr!("ACTION_CLOSE_DETAIL_VIEW", "Close Detail View"),
                    EscapeBack,
                )
                .focus_handle(focus_handle.clone()),
            );

            cx.on_release(move |_, cx| {
                cx.unregister_command(("playlist::import", 0));
                cx.unregister_command(("library::go_back", 0));
                cx.unregister_command(("library::go_forward", 0));
                cx.unregister_command(("library::close_detail_view", 0));
            })
            .detach();

            let show_update_playlist = cx.new(|_| false);

            App::on_action(cx, {
                let show_update_playlist = show_update_playlist.clone();
                move |_: &Import, cx| {
                    show_update_playlist.update(cx, |v, cx| {
                        *v = true;
                        cx.notify();
                    })
                }
            });

            let settings = cx.global::<crate::settings::SettingsGlobal>().model.clone();
            cx.observe(&settings, {
                let switcher_model = switcher_model.clone();
                move |this: &mut Library, _, cx| {
                    this.sync_visible_views(&switcher_model, cx);
                    cx.notify();
                }
            })
            .detach();

            let mut library = Library {
                view,
                left_view: None,
                right_view: None,
                section,
                update_playlist: UpdatePlaylist::new(cx, show_update_playlist.clone()),
                focus_handle,
                scroll_state,
                reclaim_focus: true,
                _focus_lost_sub: None,
            };
            library.sync_visible_views(&switcher_model, cx);
            library
        })
    }
}

/// Stable per-variant key used to re-run the view-switch fade when navigation changes.
fn library_view_key(view: &LibraryView) -> &'static str {
    match view {
        LibraryView::Album(_) => "album",
        LibraryView::Tracks(_) => "tracks",
        LibraryView::Release(_) => "release",
        LibraryView::Playlist(_) => "playlist",
        LibraryView::Artists(_) => "artists",
        LibraryView::ArtistDetail(_) => "artist",
        LibraryView::Files(_) => "files",
        #[cfg(feature = "kugou")]
        LibraryView::KugouPlaylists(_) => "kugou-playlists",
        #[cfg(feature = "kugou")]
        LibraryView::KugouRanks(_) => "kugou-ranks",
        #[cfg(feature = "netease")]
        LibraryView::NeteasePlaylists(_) => "netease-playlists",
        #[cfg(feature = "netease")]
        LibraryView::NeteaseRanks(_) => "netease-ranks",
    }
}

impl Render for Library {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self._focus_lost_sub.is_none() {
            self._focus_lost_sub = Some(cx.on_focus_lost(window, |this, window, _cx| {
                this.focus_handle.focus(window, _cx);
            }));
        }
        if self.reclaim_focus {
            self.reclaim_focus = false;
            self.focus_handle.focus(window, cx);
        }
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();
        let two_column = settings.interface.two_column_library;

        fn render_library_view(view: &LibraryView) -> AnyElement {
            match view {
                LibraryView::Album(v) => v.clone().into_any_element(),
                LibraryView::Tracks(v) => v.clone().into_any_element(),
                LibraryView::Release(v) => v.clone().into_any_element(),
                LibraryView::Playlist(v) => v.clone().into_any_element(),
                LibraryView::Artists(v) => v.clone().into_any_element(),
                LibraryView::ArtistDetail(v) => v.clone().into_any_element(),
                LibraryView::Files(v) => v.clone().into_any_element(),
                #[cfg(feature = "kugou")]
                LibraryView::KugouPlaylists(v) => v.clone().into_any_element(),
                #[cfg(feature = "kugou")]
                LibraryView::KugouRanks(v) => v.clone().into_any_element(),
                #[cfg(feature = "netease")]
                LibraryView::NeteasePlaylists(v) => v.clone().into_any_element(),
                #[cfg(feature = "netease")]
                LibraryView::NeteaseRanks(v) => v.clone().into_any_element(),
            }
        }

        let single_column = |view: &LibraryView| {
            div()
                .w_full()
                .when(!full_width, |this: Div| this.max_w(px(TABLE_MAX_WIDTH)))
                .h_full()
                .flex()
                .flex_col()
                .flex_shrink(1.0)
                .mr_auto()
                .overflow_hidden()
                .child(render_library_view(view))
                .into_any_element()
        };

        let content = if let (true, Some(left), Some(right)) = (
            two_column,
            self.left_view.as_ref(),
            self.right_view.as_ref(),
        ) {
            // two column
            let key = left.split_key();
            let split_widths = &cx.global::<Models>().split_widths;
            let split_width_model = split_widths
                .get(key)
                .unwrap_or_else(|| split_widths.get("albums").unwrap())
                .clone();

            div()
                .w_full()
                .h_full()
                .flex()
                .flex_shrink(1.0)
                .mr_auto()
                .overflow_hidden()
                .child(
                    resizable("split-resizable", split_width_model, ResizeEdge::Right)
                        .percent_mode()
                        .border_width(px(2.0))
                        .min_size(px(0.10))
                        .max_size(px(0.80))
                        .default_size(DEFAULT_SPLIT_FRACTION)
                        .h_full()
                        .child(
                            div()
                                .w_full()
                                .h_full()
                                .flex()
                                .flex_col()
                                .overflow_hidden()
                                .child(render_library_view(left)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .h_full()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .child(render_library_view(right)),
                )
                .into_any_element()
        } else if two_column {
            // single column - two column mode but views not available
            single_column(self.left_view.as_ref().unwrap_or(&self.view))
        } else {
            // single column - two column mode disabled
            single_column(&self.view)
        };

        // Fade the whole library area in on every view switch. Keyed by the active view
        // kind so each navigation re-runs the (once-per-key) mount animation.
        let view_key = self
            .right_view
            .as_ref()
            .map(library_view_key)
            .unwrap_or_else(|| {
                library_view_key(self.left_view.as_ref().unwrap_or(&self.view))
            });

        let content = div()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .with_animation(
                ElementId::from((
                    ElementId::from("library-view-fade"),
                    SharedString::from(view_key),
                )),
                Animation::new(Duration::from_millis(180)).with_easing(ease_in_out),
                |this, delta| this.opacity(delta),
            )
            .child(content);

        div()
            .id("library")
            .track_focus(&self.focus_handle)
            .key_context("Library")
            .on_action(cx.listener(|this, _: &EscapeBack, _, cx| {
                let switcher = cx.global::<Models>().switcher_model.clone();
                let current = switcher.read(cx).current();

                let parent = match current {
                    ViewSwitchMessage::Release(album_id, _) => {
                        if this.section == LibrarySection::Artists {
                            let artists = cx.artist_ids_for_album(album_id).ok();
                            let parent_artist = match switcher.read(cx).previous() {
                                Some(ViewSwitchMessage::Artist(id))
                                    if artists.as_ref().is_some_and(|list| {
                                        list.iter().any(|(aid, _)| *aid == id)
                                    }) =>
                                {
                                    Some(id)
                                }
                                _ => artists.and_then(|list| list.first().map(|a| a.0)),
                            };
                            parent_artist.map(ViewSwitchMessage::Artist)
                        } else {
                            Some(ViewSwitchMessage::Albums)
                        }
                    }
                    ViewSwitchMessage::Artist(_) => Some(ViewSwitchMessage::Artists),
                    _ => None, // Already at top level
                };

                if let Some(dest) = parent {
                    // If the previous history entry matches the parent, go back
                    // instead of creating a new history entry.
                    let msg = if switcher.read(cx).previous() == Some(dest) {
                        ViewSwitchMessage::Back
                    } else {
                        dest
                    };
                    switcher.update(cx, |_, cx| {
                        cx.emit(msg);
                    });
                }
            }))
            .on_action(cx.listener(|_, _: &NavigateBack, _, cx| {
                let switcher = cx.global::<Models>().switcher_model.clone();
                switcher.update(cx, |_, cx| {
                    cx.emit(ViewSwitchMessage::Back);
                });
            }))
            .on_action(cx.listener(|_, _: &NavigateForward, _, cx| {
                let switcher = cx.global::<Models>().switcher_model.clone();
                switcher.update(cx, |_, cx| {
                    cx.emit(ViewSwitchMessage::Forward);
                });
            }))
            .on_mouse_down(
                MouseButton::Navigate(gpui::NavigationDirection::Back),
                |_, _, cx| {
                    let switcher = cx.global::<Models>().switcher_model.clone();
                    switcher.update(cx, |_, cx| {
                        cx.emit(ViewSwitchMessage::Back);
                    });
                },
            )
            .on_mouse_down(
                MouseButton::Navigate(gpui::NavigationDirection::Forward),
                |_, _, cx| {
                    let switcher = cx.global::<Models>().switcher_model.clone();
                    switcher.update(cx, |_, cx| {
                        cx.emit(ViewSwitchMessage::Forward);
                    });
                },
            )
            .w_full()
            .h_full()
            .flex()
            .flex_shrink(1.0)
            .max_w_full()
            .max_h_full()
            .overflow_hidden()
            .child(content)
            .child(self.update_playlist.clone())
    }
}

/// Three top-level tables refresh on the same scan events; one observer for all.
pub(crate) fn observe_scan_for_table<V: 'static, T: crate::ui::components::table::table_data::TableData<C> + 'static, C: crate::ui::components::table::table_data::Column + 'static>(
    cx: &mut Context<V>,
    state: &Entity<ScanEvent>,
    table: Entity<crate::ui::components::table::Table<T, C>>,
) {
    let table_for_availability = table.clone();
    cx.observe(state, move |_, e, cx| {
        // completion only: TableEvent::NewRows re-queries the whole table on
        // the UI thread and wipes every row view, so refreshing on progress
        // froze the UI every 100 files for the duration of a large scan.
        // ScanProgress still feeds the progress UI through its own subscriber.
        let should_refresh = matches!(
            e.read(cx),
            ScanEvent::ScanCompleteIdle
                | ScanEvent::ScanCompleteWatching
                | ScanEvent::TargetedRescanComplete
        );
        if should_refresh {
            table.update(cx, |_, cx| cx.emit(crate::ui::components::table::TableEvent::NewRows));
        }
    })
    .detach();

    // Rows built before the availability snapshots land (startup) report
    // "unavailable"; refresh exactly once, when the last of them arrives.
    // Later landings coincide with the scan-completion refresh above.
    let album_snapshot = cx.global::<crate::ui::models::Models>().available_albums.clone();
    let artist_snapshot = cx
        .global::<crate::ui::models::Models>()
        .available_artists
        .clone();
    let announced = std::rc::Rc::new(std::cell::Cell::new(false));
    for slot in [album_snapshot.clone(), artist_snapshot.clone()] {
        let announced = announced.clone();
        let album_snapshot = album_snapshot.clone();
        let artist_snapshot = artist_snapshot.clone();
        let table = table_for_availability.clone();
        cx.observe(&slot, move |_, _, cx| {
            if announced.get()
                || album_snapshot.read(cx).is_none()
                || artist_snapshot.read(cx).is_none()
            {
                return;
            }
            announced.set(true);
            table.update(cx, |_, cx| {
                cx.emit(crate::ui::components::table::TableEvent::NewRows)
            });
        })
        .detach();
    }
}
