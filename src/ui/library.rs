use std::{path::PathBuf, time::Duration};

use album_view::AlbumView;
use artist_detail_view::ArtistDetailView;
use artist_view::ArtistView;
use cntp_i18n::tr;
use files_view::FilesView;
use gpui::{prelude::FluentBuilder, *};
use release_view::ReleaseView;
use tracing::debug;
use track_view::TrackView;

use super::models::Models;
use crate::{
    library::scan::ScanEvent,
    settings::storage::DEFAULT_SPLIT_FRACTION,
    ui::{
        command_palette::{CommandCategory, CommandManager, CommandSpec},
        components::{
            resizable::{ResizeEdge, resizable},
            table::table_data::capped_table_width,
        },
        library::{
            playlist_view::{Import, PlaylistView},
            update_playlist::UpdatePlaylist,
        },
        library_access::LibraryAccess,
    },
};

#[derive(Clone, Default)]
struct ScrollStateStorage {
    album_view_scroll: Option<f32>,
    track_view_scroll: Option<f32>,
    artist_view_scroll: Option<f32>,
    files_view_scroll: Option<f32>,
    files_expanded: Vec<PathBuf>,
}

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

/// 账号身份变了（登录/登出或换了一个账号）：丢掉所有以登录用户为键的会话缓存。
#[cfg(feature = "kugou")]
pub(crate) fn invalidate_online_account_caches() {
    kugou_playlists::invalidate_cached_playlists();
    kugou_ranks::invalidate_cached_recommend();
}
#[cfg(feature = "kugou")]
mod kugou_view_common;
pub mod missing_folder_dialog;
pub mod playlist_view;
mod release_view;
pub mod sidebar;
mod table_shell;
mod table_view_header;
pub mod track_item;
mod track_view;
mod update_playlist;
pub mod view_header;

actions!(library, [NavigateBack, NavigateForward, EscapeBack]);

/// 钉在详情视图宿主（专辑/发行版）上的绝对定位关闭按钮。
pub fn detail_close_button(id: impl Into<ElementId>) -> impl IntoElement {
    crate::ui::components::nav_button::nav_button(id, crate::ui::components::icons::CROSS)
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

/// 导航历史 + 一个指出「当前是哪条」的光标。
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

    /// 返回光标紧邻的前一条历史条目（如果存在）。
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

    /// 导航到一个新视图。光标之后的所有历史条目都被丢弃，然后把新视图追加进去、
    /// 光标前进到它。历史最多保留 100 条。
    ///
    /// 导航到光标已经所在的那个页面是空操作。追加重复条目会让「后退」按钮
    /// 看起来像坏了：它仍然可点（光标不在起点），但按下去只是走到一个一模一样的
    /// 页面、什么都不会变。点侧栏里你当前就在的那一项是很平常的事，所以在这里
    /// 处理，而不是指望每个调用点先自己检查。
    ///
    /// 返回历史是否真的移动了。
    pub fn navigate(&mut self, message: ViewSwitchMessage) -> bool {
        if self.current() == message {
            return false;
        }

        // 在用户已经所在的页面内部重新聚焦——同一张专辑、另一首曲目——是替换
        // 当前条目而不是追加。追加会让后退又落到那张完全相同的专辑上，
        // 看起来就是个死按钮。
        if is_refocus_of(self.current(), message) {
            self.history[self.cursor] = message;
            return true;
        }

        // 丢掉全部前进历史。
        self.history.truncate(self.cursor + 1);

        // 通过逐出最旧的条目，把历史总量封顶在 100 条。
        if self.history.len() >= 100 {
            let remove_idx = self.eviction_index();
            self.history.remove(remove_idx);

            if remove_idx <= self.cursor {
                self.cursor = self.cursor.saturating_sub(1);
            }
        }

        self.history.push(message);
        self.cursor = self.history.len() - 1;
        true
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

    /// 找到（光标之前）匹配谓词的最新一条历史条目。
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

    /// 移除不满足 `f` 的历史条目，同时调整光标：它若还活着就继续指向同一条，
    /// 否则退到最近的前一条幸存者。历史保证永不为空（回退到启动视图）。
    ///
    /// 用于移除已经失效的条目。
    pub fn retain<F>(&mut self, f: F)
    where
        F: Fn(&ViewSwitchMessage) -> bool,
    {
        // 数一下光标及其之前有多少条目会被移除。
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

/// `next` 是否与 `current` 是同一个页面、只是打开了不同的目标：目前只指
/// 「同一张专辑的另一首曲目」。这种移动是页面内部重新聚焦，不是浏览历史上的一步。
fn is_refocus_of(current: ViewSwitchMessage, next: ViewSwitchMessage) -> bool {
    matches!(
        (current, next),
        (ViewSwitchMessage::Release(a, _), ViewSwitchMessage::Release(b, _)) if a == b
    )
}

impl Default for NavigationHistory {
    fn default() -> Self {
        Self::new(ViewSwitchMessage::Albums)
    }
}

impl EventEmitter<ViewSwitchMessage> for NavigationHistory {}

/// 跟踪用户当前所处的顶层分区，好让依赖上下文的动作（比如「向上」）行为正确。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibrarySection {
    Albums,
    Artists,
    Tracks,
    Files,
    Playlists,
}

impl LibrarySection {
    /// 从导航消息推出所属分区。对有歧义的消息（例如 `Release`）返回 `None`，
    /// 让它们保持当前分区。
    fn from_message(msg: &ViewSwitchMessage) -> Option<Self> {
        match msg {
            ViewSwitchMessage::Albums => Some(Self::Albums),
            ViewSwitchMessage::Tracks => Some(Self::Tracks),
            ViewSwitchMessage::Artists | ViewSwitchMessage::Artist(_) => Some(Self::Artists),
            ViewSwitchMessage::Files => Some(Self::Files),
            ViewSwitchMessage::Playlist(_) => Some(Self::Playlists),
            // Release 可能出现在 Albums 或 Artists 下——保持当前分区。
            ViewSwitchMessage::Release(_, _) => None,
            // 酷狗页面不属于本地库分区——保持当前分区。
            #[cfg(feature = "kugou")]
            ViewSwitchMessage::KugouPlaylists | ViewSwitchMessage::KugouRanks => None,
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
    /// 专辑 id、曲目 id
    Release(i64, Option<i64>),
    Artist(i64),
    Playlist(i64),
    /// 酷狗云歌单页（仅 kugou feature）
    #[cfg(feature = "kugou")]
    KugouPlaylists,
    /// 酷狗发现页：榜单 + 每日推荐（仅 kugou feature）
    #[cfg(feature = "kugou")]
    KugouRanks,
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
                // ArtistDetail：不缓存——不加额外存储就无法验证 id 是否仍然匹配
                | (LibraryView::Artists(_), ViewSwitchMessage::Artists)
                | (LibraryView::Files(_), ViewSwitchMessage::Files)
        ) {
            return true;
        }

        #[cfg(feature = "kugou")]
        if matches!(
            (lv, self),
            (
                LibraryView::KugouPlaylists(_),
                ViewSwitchMessage::KugouPlaylists
            ) | (LibraryView::KugouRanks(_), ViewSwitchMessage::KugouRanks)
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
    // 每个页面实例（新导航、前进/后退/刷新、启动）都会在用户导航频率上经过这里；
    // 在此采样内存，可以把周期探针解释不了的那部分浏览驱动阶跃归因出来。
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
        ViewSwitchMessage::Back => panic!("improper use of make_view (cannot make Back)"),
        ViewSwitchMessage::Forward => panic!("improper use of make_view (cannot make Forward)"),
        ViewSwitchMessage::Refresh => panic!("improper use of make_view (cannot make Refresh)"),
    }
}

/// 在共享历史实体上走一步历史（`go_back`/`go_forward`）并返回目的地
/// （光标能移动的话）。
fn step_history(
    history: &Entity<NavigationHistory>,
    cx: &mut App,
    step: fn(&mut NavigationHistory) -> Option<ViewSwitchMessage>,
) -> Option<ViewSwitchMessage> {
    history.update(cx, |history, cx| {
        let destination = step(history);
        cx.notify();
        destination
    })
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
    /// 把即将离开的视图的滚动位置（外加文件视图的展开集合）快照进 `scroll_state`，
    /// 这样重建出来的视图能恢复读者读到的位置。
    fn save_scroll_state(view: &LibraryView, scroll_state: &mut ScrollStateStorage, cx: &App) {
        match view {
            LibraryView::Album(view) => {
                scroll_state.album_view_scroll = Some(view.read(cx).get_scroll_offset(cx));
            }
            LibraryView::Tracks(view) => {
                scroll_state.track_view_scroll = Some(view.read(cx).get_scroll_offset(cx));
            }
            LibraryView::Artists(view) => {
                scroll_state.artist_view_scroll = Some(view.read(cx).get_scroll_offset(cx));
            }
            LibraryView::Files(view) => {
                let view = view.read(cx);
                scroll_state.files_view_scroll = Some(view.get_scroll_offset());
                scroll_state.files_expanded = view.expanded_paths();
            }
            _ => {}
        }
    }

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

            // 这个视图是缓存的，所以只有被显式置脏才重绘：观察它布局依赖的
            // 每一个宽度实体（可拖拽的队列/歌词面板，以及双栏分割）
            let queue_width = cx.global::<Models>().queue_width.clone();
            cx.observe(&queue_width, |_, _, cx| cx.notify()).detach();

            cx.subscribe(
                &switcher_model,
                move |this: &mut Library, m, message, cx| {
                    Self::save_scroll_state(&this.view, &mut this.scroll_state, cx);

                    // 如果我们要离开一个偷走了焦点的视图（例如 PlaylistView），
                    // 就安排一次焦点回收，让 Library 的 div 在下一次渲染时重新拿到焦点。
                    if matches!(this.view, LibraryView::Playlist(_)) {
                        this.reclaim_focus = true;
                    }

                    this.view = match message {
                        ViewSwitchMessage::Back => {
                            if let Some(dest) = step_history(&m, cx, NavigationHistory::go_back) {
                                debug!("back → {:?}", dest);
                                make_view(&dest, cx, &m, &this.scroll_state)
                            } else {
                                this.view.clone()
                            }
                        }

                        ViewSwitchMessage::Forward => {
                            if let Some(dest) = step_history(&m, cx, NavigationHistory::go_forward)
                            {
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
                            let navigated = m.update(cx, |history, cx| {
                                let navigated = history.navigate(*message);
                                cx.notify();
                                navigated
                            });

                            // 重新导航到当前页面是空操作，而为它重建视图会丢掉
                            // 该页面持有的、不在滚动状态里的一切。
                            if navigated {
                                make_view(message, cx, &m, &this.scroll_state)
                            } else {
                                this.view.clone()
                            }
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
                update_playlist: UpdatePlaylist::new(cx, show_update_playlist),
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

/// 每个变体的稳定键，用于在导航变化时重新触发切换视图的淡入。
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
    }
}

impl Render for Library {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Library);
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
            }
        }

        let single_column = |view: &LibraryView| {
            div()
                .w_full()
                .when(!full_width, capped_table_width)
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
            // 双栏
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
            // 单栏——双栏模式，但详情视图不可用
            single_column(self.left_view.as_ref().unwrap_or(&self.view))
        } else {
            // 单栏——双栏模式已关闭
            single_column(&self.view)
        };

        // 每次切换视图都把整个库区域淡入。以当前视图种类为键，于是每次导航都会
        // 重跑这个「每个键只播一次」的挂载动画。
        let view_key = self
            .right_view
            .as_ref()
            .map(library_view_key)
            .unwrap_or_else(|| library_view_key(self.left_view.as_ref().unwrap_or(&self.view)));

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
            // 库内容是切换多种视图的顶层区域，不预设表格/网格语义。
            .role(gpui::Role::Region)
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
                    // 若上一条历史就是父页面，则走后退，
                    // 而不是新建一条历史条目。
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

/// 三张顶层表在同一批扫描事件上刷新；三者共用一个观察者。
pub(crate) fn observe_scan_for_table<
    V: 'static,
    T: crate::ui::components::table::table_data::TableData<C> + 'static,
    C: crate::ui::components::table::table_data::Column + 'static,
>(
    cx: &mut Context<V>,
    state: &Entity<ScanEvent>,
    table: Entity<crate::ui::components::table::Table<T, C>>,
) {
    let table_for_availability = table.clone();
    cx.observe(state, move |_, e, cx| {
        // 只在扫描完成时刷新：TableEvent::NewRows 走 `Table::reload_rows`，
        // 它带代际守卫地在运行时上派生重查询（UI 线程只换行 Arc；见
        // `table rows loaded` 的 elapsed_ms 日志）。*进度*上刷新仍然保持禁用：
        // 扫描中途行集频繁变动，每次换 Arc 都会清空所有缓存的行视图。
        // ScanProgress 仍通过它自己的订阅者喂进度 UI。
        let should_refresh = matches!(
            e.read(cx),
            ScanEvent::ScanCompleteIdle
                | ScanEvent::ScanCompleteWatching
                | ScanEvent::TargetedRescanComplete
        );
        if should_refresh {
            table.update(cx, |_, cx| {
                cx.emit(crate::ui::components::table::TableEvent::NewRows)
            });
        }
    })
    .detach();

    // 在可用性快照落地（启动）之前构建的行会报「不可用」；等最后一份到达时
    // 精确刷新一次。之后的落地都与上面那次扫描完成刷新重合。
    let album_snapshot = cx
        .global::<crate::ui::models::Models>()
        .available_albums
        .clone();
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

#[cfg(test)]
mod tests {
    // 刻意不写 `use super::*`：把本模块的 GPUI glob 再导入一次会让 `#[test]`
    // 的展开超过默认递归上限。
    use super::{NavigationHistory, ViewSwitchMessage};

    fn history() -> NavigationHistory {
        NavigationHistory::new(ViewSwitchMessage::Albums)
    }

    /// 报告出来的症状，最小化版本：点你当前已经在的那个侧栏项，然后按后退。
    /// 那个重复条目过去会吞掉这一步，让按钮仍可点却只是走到一个一模一样的页面。
    #[test]
    fn re_navigating_to_the_current_page_does_not_grow_history() {
        let mut h = history();
        assert!(!h.navigate(ViewSwitchMessage::Albums));
        assert_eq!(h.history.len(), 1);
        assert!(!h.can_go_back(), "back must stay disabled after a no-op");
    }

    #[test]
    fn clicking_the_current_sidebar_item_keeps_back_meaningful() {
        let mut h = history();
        h.navigate(ViewSwitchMessage::Artists);
        // 用户已经在 Artists 上时再点一次 "Artists"。
        assert!(!h.navigate(ViewSwitchMessage::Artists));

        assert_eq!(h.go_back(), Some(ViewSwitchMessage::Albums));
        assert!(!h.can_go_back());
    }

    #[test]
    fn navigation_records_one_entry_per_distinct_page() {
        let mut h = history();
        assert!(h.navigate(ViewSwitchMessage::Artists));
        assert!(h.navigate(ViewSwitchMessage::Tracks));
        assert_eq!(h.history.len(), 3);
        assert!(h.can_go_back());

        assert_eq!(h.go_back(), Some(ViewSwitchMessage::Artists));
        assert_eq!(h.go_back(), Some(ViewSwitchMessage::Albums));
        assert!(!h.can_go_back());
        assert_eq!(h.go_back(), None);
    }

    /// 另一张专辑是真实的一步；同一张专辑的另一首曲目是重新聚焦、替换该条目；
    /// 完全相同的消息则是普通空操作。
    #[test]
    fn detail_pages_compare_by_their_payload() {
        let mut h = history();
        assert!(h.navigate(ViewSwitchMessage::Release(1, None)));
        assert!(h.navigate(ViewSwitchMessage::Release(1, Some(7))));
        assert_eq!(h.history.len(), 2, "same album re-focus replaces the entry");
        assert!(!h.navigate(ViewSwitchMessage::Release(1, Some(7))));

        assert!(h.navigate(ViewSwitchMessage::Release(2, None)));
        assert_eq!(h.history.len(), 3);
    }

    /// 「后退没反应」的第二种报告形态：跳到你正在看的那张专辑里的一首曲目
    /// （比如从搜索里）过去会追加一步，于是后退又落到同一张专辑。
    #[test]
    fn refocusing_within_an_album_leaves_back_a_real_step() {
        let mut h = history();
        h.navigate(ViewSwitchMessage::Release(1, None));
        h.navigate(ViewSwitchMessage::Release(1, Some(7)));

        assert_eq!(h.go_back(), Some(ViewSwitchMessage::Albums));
        assert!(!h.can_go_back());
    }

    #[test]
    fn navigating_away_drops_the_forward_branch() {
        let mut h = history();
        h.navigate(ViewSwitchMessage::Artists);
        h.navigate(ViewSwitchMessage::Tracks);
        h.go_back();
        assert!(h.can_go_forward());
        assert_eq!(h.go_forward(), Some(ViewSwitchMessage::Tracks));

        h.go_back();
        h.navigate(ViewSwitchMessage::Files);
        assert!(
            !h.can_go_forward(),
            "the Tracks branch must be gone after a new navigation"
        );
    }

    /// 删除歌单时使用：它的条目离开历史，光标落在最近的前一条幸存者上。
    #[test]
    fn retain_keeps_the_cursor_on_a_surviving_entry() {
        let mut h = history();
        h.navigate(ViewSwitchMessage::Playlist(1));
        h.navigate(ViewSwitchMessage::Playlist(2));

        h.retain(|v| *v != ViewSwitchMessage::Playlist(2));
        assert_eq!(h.current(), ViewSwitchMessage::Playlist(1));

        // 全部移除时回退到启动视图。
        h.retain(|v| *v != ViewSwitchMessage::Playlist(1));
        assert_eq!(h.current(), ViewSwitchMessage::Albums);
        assert_eq!(h.history.len(), 1);
        assert!(!h.can_go_back());
    }

    /// 上限绝不可能逐出用户当前站立的那一条。
    #[test]
    fn history_cap_keeps_the_current_entry() {
        let mut h = history();
        for id in 0..150i64 {
            h.navigate(ViewSwitchMessage::Playlist(id));
        }
        assert!(h.can_go_back(), "the current entry must survive eviction");
        assert_eq!(h.current(), ViewSwitchMessage::Playlist(149));

        let mut seen = Vec::new();
        while let Some(msg) = h.go_back() {
            seen.push(msg);
        }
        assert!(seen.contains(&ViewSwitchMessage::Playlist(148)));
        assert!(
            !seen.contains(&ViewSwitchMessage::Playlist(0)),
            "the oldest entries are the ones evicted: {} entries kept",
            seen.len()
        );
    }
}
