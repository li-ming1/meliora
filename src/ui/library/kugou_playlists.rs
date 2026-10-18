//! 酷狗云歌单页：列出已登录用户的歌单及其曲目。点击行时现取播放链接再串流
//! （共享行会忽略双击的第二次点击，故双击不会把「取链接 + 打开」跑两遍）。
//! 受 `kugou` cargo feature 门控。

use std::{
    collections::HashSet,
    sync::{Arc, LazyLock, Mutex},
};

use crate::ui::online_track_row::{error_line, muted_line};
use cntp_i18n::{tr, trn};
use futures::{StreamExt as _, stream};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use tokio::sync::mpsc::UnboundedSender;

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
        kugou::{KugouPlaylistInfo, KugouTrackInfo, parse_playlists, parse_tracks},
        library::{kugou_view_common, view_header::view_header},
        online_common::{
            CachedRoot, LoadMoreSpec, cached_root, clear_root, online_list_container,
            page_scroll_container, store_root,
        },
        settings::{SettingsSectionKind, open_settings_window_with_section},
        theme::Theme,
    },
};

/// 每页抓取多少条曲目。
const TRACKS_PER_PAGE: i64 = 100;

/// 歌单总览每页抓取多少个用户歌单。
const PLAYLISTS_PER_PAGE: i64 = 100;

/// 歌曲按这个块大小分批上传进新建的歌单。
const PLAYLIST_ADD_CHUNK: usize = 100;

/// 歌单总览的会话缓存（TTL 与辅助函数在 `online_common`）：视图实体每次切标签
/// 都会重建，缓存新鲜时整轮重取直接跳过。本视图内的变更（导入、重试）都走
/// `refresh_playlists`，它绕过缓存并重新回填，因此用户引起的变化不会被掩盖。
static CACHED_PLAYLISTS: LazyLock<Mutex<Option<CachedRoot<Vec<KugouPlaylistInfo>>>>> =
    LazyLock::new(|| Mutex::new(None));

/// 账号身份变化（登录/登出，可能换到另一个账号）：丢弃缓存的总览，
/// 使它绝不可能跨账号被命中。
pub(crate) fn invalidate_cached_playlists() {
    clear_root(&CACHED_PLAYLISTS);
}

/// 匹配导入歌单时的并发酷狗歌曲搜索数。刻意取小：只有一个共享客户端去打签名的
/// web 搜索端点，被限流的搜索会静默丢掉这首歌（本地 M3U 查找可容忍 8）。
const KUGOU_MATCH_CONCURRENCY: usize = 4;

/// 总览歌单行钉死为自然高度：上下 padding 8px × 2 + 标题行（text_sm）+
/// 1px 下边框（占主导的是单行歌单名；16px 图标与数量行更矮）。曲目行用共享的
/// `kugou_view_common::ONLINE_TRACK_ROW_HEIGHT`。
const PLAYLIST_ROW_HEIGHT: f32 = 40.0;

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

/// 导入管线的结果，在 Tokio 运行时上产生，回到主线程才映射为 UI 状态
/// （与 locale 相关的字符串在主线程构造）。
enum ImportOutcome {
    Done { matched: i64, total: i64 },
    ParseFailed(String),
    CreateFailed(String),
    NotFound,
    NoMatch,
}

/// 导入管线运行期间流向 UI 的消息：逐曲匹配进度，以及最终结果
/// （发出结果的同时关闭通道）。
enum ImportEvent {
    Progress { done: usize, total: usize },
    Finished(ImportOutcome),
}

#[derive(Clone)]
struct SelectedPlaylist {
    global_collection_id: String,
    listid: i64,
    name: SharedString,
    /// 歌单列表报回的曲目数；让 load_first_page 直接取最后一页，
    /// 而不必为了总数去探测第 1 页
    count: i64,
}

pub struct KugouPlaylistsView {
    playlists: PlaylistsState,
    selected: Option<SelectedPlaylist>,
    /// 用 Arc 共享，好让行闭包捕获的是引用计数而非深拷贝
    /// （每个可见行每次重绘都会重新 clone 自己那条曲目）。
    tracks: Vec<Arc<KugouTrackInfo>>,
    tracks_state: TracksState,
    /// 已加载的歌单最后一页（1 基；方向是 DOWN，因为 API 按最新优先返回曲目，
    /// 而界面按最旧优先显示）
    track_page: i64,
    /// 歌单里是否还有比当前已加载更多的曲目
    has_more_tracks: bool,
    /// 每次（重）加载曲目列表都自增；世代号不再匹配的在途分页响应会被丢弃，
    /// 于是快速关闭再打开同一歌单也不可能把同一陈旧页追加两次。
    track_generation: u64,
    /// 本次会话内点过赞的曲目 hash（驱动星标图标）
    liked: HashSet<String>,
    /// 本次会话内用户显式取消赞的 hash。只在查看「我喜欢的」歌单
    /// （listid 2）时有意义——那里每首曲目默认都已点赞。
    unliked: HashSet<String>,
    /// 有赞/取消赞请求已在途的 hash；这些行上的额外点击在请求落地前一律忽略
    like_in_flight: HashSet<String>,
    scroll_handle: ScrollHandle,
    /// 虚拟化歌单总览的滚动位置；打开某个歌单时重置，
    /// 使返回总览时从顶部开始。
    playlists_scroll_handle: UniformListScrollHandle,
    /// 虚拟化曲目列表的滚动位置；按歌单重置，使新开歌单从顶部开始。
    tracks_scroll_handle: UniformListScrollHandle,
    /// 导入对话框状态。
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
                track_generation: 0,
                liked: HashSet::new(),
                unliked: HashSet::new(),
                like_in_flight: HashSet::new(),
                scroll_handle: ScrollHandle::new(),
                playlists_scroll_handle: UniformListScrollHandle::new(),
                tracks_scroll_handle: UniformListScrollHandle::new(),
                import_open: false,
                import_input: None,
                import_busy: false,
                import_status: SharedString::default(),
            };

            if logged_in {
                view.load_playlists_initial(cx);
                view.ensure_profile(cx);
            } else {
                view.playlists = PlaylistsState::LoggedOut;
            }

            view
        })
    }

    /// 视图构造时的首次加载：缓存新鲜就直接命中会话缓存（实体每次切标签都会
    /// 重建），否则落到网络路径，顺带回填缓存。
    fn load_playlists_initial(&mut self, cx: &mut Context<Self>) {
        if let Some(playlists) = cached_root(&CACHED_PLAYLISTS) {
            self.playlists = PlaylistsState::Ready(playlists);
            cx.notify();
            return;
        }
        self.refresh_playlists(cx);
    }

    /// 缓存为空时抓取一次用户资料，然后 notify，使头部卡片出现，
    /// 后续再访问不必重新请求。
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
            let request = crate::RUNTIME
                .spawn(async move { client.user_playlists(1, PLAYLISTS_PER_PAGE).await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    this.playlists = match request {
                        Ok(Ok(response)) => {
                            let playlists = parse_playlists(&response.body);
                            store_root(&CACHED_PLAYLISTS, playlists.clone());
                            PlaylistsState::Ready(playlists)
                        }
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
            count: playlist.count,
        });
        self.tracks.clear();
        self.tracks_state = TracksState::Loading;
        self.track_page = 0;
        self.has_more_tracks = false;
        // 使上一次会话（本歌单或别的歌单）仍留在途的分页响应失效
        self.track_generation += 1;
        self.unliked.clear();
        self.scroll_handle = ScrollHandle::new();
        self.playlists_scroll_handle = UniformListScrollHandle::new();
        self.tracks_scroll_handle = UniformListScrollHandle::new();
        cx.notify();

        // 预热已点赞集合缓存，让播放条上的星标对本歌单的曲目即刻点亮
        if playlist.listid == 2 {
            crate::ui::kugou::prime_liked_cache();
        }

        self.load_first_page(cx);
    }

    fn load_more_tracks(&mut self, cx: &mut Context<Self>) {
        // 一次只让一页在途：从点击到响应落地 tracks_state 一直是 Loading，
        // 所以多余的点击被忽略
        if self.track_page <= 1 || matches!(self.tracks_state, TracksState::Loading) {
            return;
        }
        // 分页方向是 DOWN：API 按最新优先返回、我们按最旧优先显示，
        // 所以「更多」指的是再往旧的一页
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

    /// 加载所选歌单的第一批曲目。
    ///
    /// v4 端点按最新优先返回曲目（fsort 降序），界面却按最旧优先显示。
    /// 于是先用一次廉价的第 1 页探测读出总数 `data.count`，真正的首批曲目
    /// 改从最后一页取回再反转；`load_more_tracks` 从那里往回走。
    fn load_first_page(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        self.track_generation += 1;
        let generation = self.track_generation;

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let listid = selected.listid;
            let count = selected.count;
            let request = crate::RUNTIME
                .spawn(async move {
                    // 歌单列表本身已带上曲目数，所以直接取最后一页；探测第 1 页
                    // 只作为列表没返回曲目数时的兜底。
                    if count > 0 {
                        let total_pages = page_count(selected.count);
                        let response = client
                            .playlist_tracks(listid, total_pages, TRACKS_PER_PAGE)
                            .await?;
                        Ok::<_, kugou::KugouError>((response, total_pages))
                    } else {
                        let probe = client.playlist_tracks(listid, 1, TRACKS_PER_PAGE).await?;
                        let count = probe
                            .body
                            .pointer("/data/count")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        let total_pages = if count > 0 { page_count(count) } else { 1 };

                        let response = if total_pages <= 1 {
                            probe
                        } else {
                            client
                                .playlist_tracks(listid, total_pages, TRACKS_PER_PAGE)
                                .await?
                        };
                        Ok((response, total_pages))
                    }
                })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    // 请求已在途期间选择项可能变了，或歌单可能被关闭后重开；
                    // 陈旧响应不得覆盖新状态
                    let still_current =
                        this.selected.as_ref().is_some_and(|s| {
                            s.global_collection_id == selected.global_collection_id
                        }) && this.track_generation == generation;

                    match request {
                        Ok(Ok((response, total_pages))) if still_current => {
                            let mut page_tracks = parse_tracks(&response.body, "/data/info");
                            page_tracks.reverse();
                            this.tracks = page_tracks.into_iter().map(Arc::new).collect();
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
        self.track_generation += 1;
        let generation = self.track_generation;

        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let listid = selected.listid;
            let request = crate::RUNTIME
                .spawn(async move { client.playlist_tracks(listid, page, TRACKS_PER_PAGE).await })
                .await;

            let _ = this
                .update(cx, |this, cx| {
                    // 请求已在途期间选择项可能变了，或歌单可能被关闭后重开；
                    // 陈旧响应不得覆盖新状态
                    let still_current =
                        this.selected.as_ref().is_some_and(|s| {
                            s.global_collection_id == selected.global_collection_id
                        }) && this.track_generation == generation;

                    match request {
                        Ok(Ok(response)) if still_current => {
                            let mut page_tracks = parse_tracks(&response.body, "/data/info");
                            page_tracks.reverse();
                            if append {
                                this.tracks.extend(page_tracks.into_iter().map(Arc::new));
                            } else {
                                this.tracks = page_tracks.into_iter().map(Arc::new).collect();
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
        self.track_generation += 1;
        cx.notify();
    }

    /// `hash` 此刻该不该显示为已点赞。除「我喜欢的」歌单（listid 2）之外，
    /// 判据都是会话集合 OR 进程级已点赞集合（由共享的赞/取消赞辅助函数保持同步）；
    /// 在 listid 2 内部由全局集合决定，再减去本会话被用户取消赞的 hash。
    fn is_track_liked(&self, hash: &str) -> bool {
        if self.selected.as_ref().is_some_and(|s| s.listid == 2) {
            crate::ui::kugou::liked_set_contains(hash) && !self.unliked.contains(hash)
        } else {
            self.liked.contains(hash) || crate::ui::kugou::liked_set_contains(hash)
        }
    }

    /// 带点击去重的乐观赞/取消赞；请求没落地时结果监视器把快照回滚。
    /// 对齐 `KugouRanksView::toggle_like`，外加「我喜欢的」歌单（listid 2）
    /// 需要的 `unliked` 集合。
    fn toggle_like(&mut self, track: &KugouTrackInfo, cx: &mut Context<Self>) {
        let hash = track.hash.clone();
        // 每首曲目同时只有一个赞/取消赞请求；请求已在途期间的点击被忽略
        if !self.like_in_flight.insert(hash.clone()) {
            return;
        }
        let want_liked = !self.is_track_liked(&hash);
        // 供回滚监视器使用的快照
        let was_liked = self.liked.contains(&hash);
        let was_unliked = self.unliked.contains(&hash);
        if want_liked {
            self.unliked.remove(&hash);
            self.liked.insert(hash.clone());
            crate::ui::kugou::like_track(cx, track);
        } else {
            self.unliked.insert(hash.clone());
            self.liked.remove(&hash);
            crate::ui::kugou::unlike_track(cx, track);
        }
        cx.notify();
        self.watch_like_outcome(hash, want_liked, was_liked, was_unliked, cx);
    }

    /// 共享的赞/取消赞辅助函数成功时翻转进程级已点赞集合、失败时原样不动，
    /// 因此结果可由 `crate::ui::kugou::liked_set_contains` 观测。监视它
    /// （有界），请求未落地时把 `liked`/`unliked` 的乐观更新回滚到点击前快照。
    fn watch_like_outcome(
        &mut self,
        hash: String,
        want_liked: bool,
        was_liked: bool,
        was_unliked: bool,
        cx: &mut Context<Self>,
    ) {
        kugou_view_common::spawn_like_watcher(
            cx,
            hash,
            want_liked,
            |view, hash| {
                view.like_in_flight.remove(hash);
            },
            move |view, hash| {
                if was_liked {
                    view.liked.insert(hash.to_string());
                } else {
                    view.liked.remove(hash);
                }
                if was_unliked {
                    view.unliked.insert(hash.to_string());
                } else {
                    view.unliked.remove(hash);
                }
            },
        );
    }

    // ---- 外部歌单导入（网易云 / QQ → 酷狗）----

    fn open_import(&mut self, cx: &mut Context<Self>) {
        if self.import_input.is_none() {
            self.import_input = Some(Textbox::new_with_value_submit(
                cx,
                Default::default(),
                |_, _| {},
            ));
        }
        self.import_open = true;
        // 不要重置 `import_busy`：关闭模态框并不会中止管线，而在这里清旗标
        // 会让重新打开的模态框对同一歌单提交第二次导入。由 Finished 事件清除。
        self.import_status = SharedString::default();
        cx.notify();
    }

    fn close_import(&mut self, cx: &mut Context<Self>) {
        self.import_open = false;
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
        self.import_status =
            SharedString::from(tr!("KUGOU_IMPORT_PARSING", "Parsing link…").to_string());
        // 在 UI 线程就地解析，好让本地化的兜底名称在管线跑到 Tokio 运行时上之后
        // 仍用对 locale
        let default_name = tr!("KUGOU_IMPORT_DEFAULT_NAME", "Imported playlist").to_string();
        cx.notify();

        cx.spawn(async move |this, cx| {
            // 管线的每一步都是网络调用，全部跑在 Tokio 运行时上——reqwest 的
            // DNS 解析器在 gpui executor 上会 panic（见 `ui::kugou::fetch_online_lyric`）。
            // 任务持有通道的发送端，所以管线发出最终结果时本循环自然结束
            // （任务死掉时也一样结束，留下与旧 JoinHandle 错误路径完全相同的陈旧 busy 态）。
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::RUNTIME.spawn(async move {
                let outcome = run_import(text, default_name, &progress_tx).await;
                let _ = progress_tx.send(ImportEvent::Finished(outcome));
            });

            while let Some(event) = progress_rx.recv().await {
                // 一口气涌入的匹配完成事件合并为一次状态更新
                let mut event = event;
                while let Ok(next) = progress_rx.try_recv() {
                    event = next;
                }

                let done_toast = this
                    .update(cx, |this, cx| {
                        let done_toast = match event {
                            ImportEvent::Progress { done, total } => {
                                // 匹配进度复用通用的「正在导入…」词条
                                // （专用键还得往词条目录里另外加）
                                this.import_status =
                                    format!("{} {}/{}", tr!("KUGOU_IMPORT_RUNNING"), done, total)
                                        .into();
                                None
                            }
                            ImportEvent::Finished(outcome) => {
                                this.import_busy = false;
                                match outcome {
                                    ImportOutcome::Done { matched, total } => {
                                        let summary = tr!(
                                            "KUGOU_IMPORT_DONE",
                                            "Imported: matched {{matched}}/{{total}}",
                                            matched = matched,
                                            total = total
                                        );
                                        this.import_status = summary.clone().into();
                                        this.refresh_playlists(cx);
                                        // 添加数为 0 意味着每次上传都失败了：
                                        // 计数保留，但要把错误暴露出来。
                                        Some(if matched == 0 {
                                            Toast::error(summary)
                                        } else {
                                            Toast::success(summary)
                                        })
                                    }
                                    ImportOutcome::ParseFailed(err) => {
                                        this.import_status = SharedString::from(
                                            tr!(
                                                "KUGOU_IMPORT_FAILED",
                                                "导入失败：{{err}}",
                                                err = err
                                            )
                                            .to_string(),
                                        );
                                        None
                                    }
                                    ImportOutcome::CreateFailed(err) => {
                                        this.import_status = SharedString::from(
                                            tr!(
                                                "KUGOU_IMPORT_CREATE_FAILED",
                                                "Could not create playlist: {{err}}",
                                                err = err
                                            )
                                            .to_string(),
                                        );
                                        None
                                    }
                                    ImportOutcome::NotFound => {
                                        this.import_status = SharedString::from(
                                            tr!(
                                                "KUGOU_IMPORT_NOT_FOUND",
                                                "Created playlist not found"
                                            )
                                            .to_string(),
                                        );
                                        None
                                    }
                                    ImportOutcome::NoMatch => {
                                        this.import_status = SharedString::from(
                                            tr!("KUGOU_IMPORT_NO_MATCH", "No tracks matched")
                                                .to_string(),
                                        );
                                        this.refresh_playlists(cx);
                                        None
                                    }
                                }
                            }
                        };
                        cx.notify();
                        done_toast
                    })
                    .ok()
                    .flatten();

                if let Some(toast) = done_toast {
                    emit_toast(toast);
                }
            }

            // 导入任务死掉时通道可能在没有 Finished 事件的情况下关闭。
            // 释放 busy 旗标，好让重新打开模态框能重试，而不是永远卡住。
            let _ = this.update(cx, |this, cx| {
                if this.import_busy {
                    this.import_busy = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = view_header(tr!("KUGOU_PLAYLISTS").to_string());

        if let Some(selected) = &self.selected {
            header
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
            header.right(
                button()
                    .id("kugou-import")
                    .child(tr!("KUGOU_IMPORT_PLAYLIST", "Import playlist (NetEase/QQ)"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_import(cx);
                    })),
            )
        }
    }

    /// 行是在总览 uniform_list 的渲染闭包内部构造的，那里只有 `&App`，
    /// 所以点击处理器经弱句柄抵达视图，而不是用 `cx.listener`
    /// （与曲目行、导入模态框回调同一套模式）。
    fn render_playlist_row(
        &self,
        index: usize,
        playlist: &KugouPlaylistInfo,
        entity: &Entity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let row_id = playlist.global_collection_id.clone();
        let weak = entity.downgrade();

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
            .on_click(move |_, _, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| {
                    let playlist = this.playlists.as_ready().and_then(|playlists| {
                        playlists
                            .iter()
                            .find(|p| p.global_collection_id == row_id)
                            .cloned()
                    });

                    if let Some(playlist) = playlist {
                        this.open_playlist(&playlist, cx);
                    }
                });
            })
    }

    /// 行是在 uniform_list 的渲染闭包内部构造的，那里只有 `&App`，
    /// 因此这里没有 `cx.listener` 可用：点赞处理器改经弱句柄抵达视图
    /// （与导入模态框回调同一套模式）。
    fn render_track_row(
        &self,
        track: &Arc<KugouTrackInfo>,
        index: usize,
        entity: &Entity<Self>,
        cx: &App,
    ) -> AnyElement {
        kugou_view_common::kugou_track_row_view(
            self,
            entity,
            track,
            index,
            cx,
            "kugou-track",
            true,
            |view, hash| view.is_track_liked(hash),
            |view, track, cx| view.toggle_like(track, cx),
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

/// `count` 条曲目按每页 `TRACKS_PER_PAGE` 算出的 1 基总页数。
fn page_count(count: i64) -> i64 {
    (count + TRACKS_PER_PAGE - 1) / TRACKS_PER_PAGE
}

/// 本地化的「{count} 首曲目」标签。复数字符串只在这一处定义，
/// 好让 i18n 生成器看不到重复定义。
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
        let theme = cx.global::<Theme>();
        let scroll_handle = self.scroll_handle.clone();

        // 两个大列表（曲目列表、歌单总览）都在各自虚拟化的 uniform_list 内滚动；
        // 页面级滚动器只服务于小的加载/错误/空态。
        let tracks_ready = self.selected.is_some() && !self.tracks.is_empty();
        let overview_ready = self.selected.is_none()
            && matches!(&self.playlists, PlaylistsState::Ready(playlists) if !playlists.is_empty());

        let mut content = div().flex().flex_col().pb(px(24.0));

        if self.selected.is_some() {
            // 已打开歌单的曲目列表
            match &self.tracks_state {
                TracksState::Loading if self.tracks.is_empty() => {
                    content = content.child(muted_line(tr!("KUGOU_LOADING"), theme));
                }
                TracksState::Failed(message) if self.tracks.is_empty() => {
                    content = content.child(error_line(message.clone(), theme)).child(
                        button()
                            .id("kugou-retry-tracks")
                            .child(tr!("KUGOU_RETRY", "Retry"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.retry_tracks(cx);
                            })),
                    );
                }
                _ if self.tracks.is_empty() => {
                    content = content.child(muted_line(
                        tr!("KUGOU_PLAYLIST_EMPTY", "This playlist is empty"),
                        theme,
                    ));
                }
                // 已加载的曲目由下面滚动分支里的虚拟化 uniform_list 渲染，
                // 不由 page-flow 内容渲染
                _ => {}
            }
        } else {
            // 歌单总览
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
                    content = content.child(muted_line(tr!("KUGOU_LOADING"), theme));
                }
                PlaylistsState::Failed(message) => {
                    content = content.child(error_line(message.clone(), theme)).child(
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
                        content = content.child(muted_line(
                            tr!("KUGOU_NO_PLAYLISTS", "No playlists found"),
                            theme,
                        ));
                    }
                    // 非空的总览由下面滚动分支里的虚拟化 uniform_list 渲染，
                    // 不由 page-flow 内容渲染
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
            // uniform_list 本身就是滚动容器，所以曲目在屏时页面级滚动器整体被替换
            let tracks_scroll_handle = self.tracks_scroll_handle.clone();

            root = root.child(online_list_container(
                cx,
                "kugou-playlists-track-container",
                "kugou-playlist-tracks",
                "kugou-playlist-tracks-scrollbar",
                &tracks_scroll_handle,
                kugou_view_common::ONLINE_TRACK_ROW_HEIGHT,
                self.tracks.len(),
                24.0,
                true,
                None,
                |view| &view.tracks,
                |view, index, track, entity, cx| view.render_track_row(track, index, entity, cx),
                self.has_more_tracks.then(|| LoadMoreSpec {
                    id: "kugou-load-more",
                    label: tr!("KUGOU_LOAD_MORE", "Load More").into_any_element(),
                    on_click: Box::new(|view: &mut Self, cx: &mut Context<Self>| {
                        view.load_more_tracks(cx)
                    }),
                }),
            ));
        } else if overview_ready {
            // 歌单总览本身就是滚动容器，所以这里页面级滚动器同样整体被替换
            // （与上面的曲目列表一致）
            let playlist_count = self.playlists.as_ready().map_or(0, |p| p.len());
            let list_entity = cx.entity();
            let playlists_scroll_handle = self.playlists_scroll_handle.clone();

            root = root.child(
                div()
                    .id("kugou-playlists-list-container")
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
                                    "kugou-playlists",
                                    playlist_count,
                                    move |range, _, cx| {
                                        let start = range.start;
                                        let view = list_entity.read(cx);
                                        let PlaylistsState::Ready(playlists) = &view.playlists
                                        else {
                                            return Vec::new();
                                        };
                                        playlists[range]
                                            .iter()
                                            .enumerate()
                                            .map(|(i, playlist)| {
                                                div().h(px(PLAYLIST_ROW_HEIGHT)).child(
                                                    view.render_playlist_row(
                                                        start + i,
                                                        playlist,
                                                        &list_entity,
                                                        cx,
                                                    ),
                                                )
                                            })
                                            .collect()
                                    },
                                )
                                .w_full()
                                .h_full()
                                .track_scroll(&playlists_scroll_handle),
                            ),
                    )
                    .child(floating_scrollbar(
                        "kugou-playlists-list-scrollbar",
                        playlists_scroll_handle,
                    )),
            );
        } else {
            root = root
                .child(page_scroll_container(
                    "kugou-playlists-scroll",
                    &scroll_handle,
                    content.into_any_element(),
                ))
                .child(floating_scrollbar(
                    "kugou-playlists-scrollbar",
                    scroll_handle,
                ));
        }

        root.when(self.import_open, |this| {
            this.child(self.render_import_modal(cx))
        })
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
                    .child(div().text_sm().text_color(theme.text_secondary).child(tr!(
                        "KUGOU_IMPORT_HINT",
                        "Paste a NetEase Cloud Music or QQ Music playlist share link"
                    )))
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
                        div()
                            .flex()
                            .gap(px(8.0))
                            .justify_end()
                            .child(
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

/// 在 `name` 上搜索酷狗，返回第一首可播放的匹配曲目。给出
/// `(title, hash, album_id, mix_song_id)` 供 `playlist_add_songs` 使用。
/// 必须在 Tokio 运行时上运行（网络调用）。
async fn kugou_match(
    client: &kugou::KugouClient,
    name: &str,
) -> Option<(String, String, i64, i64)> {
    let resp = client.search(name, 1, 1).await.ok()?;
    let track = parse_tracks(&resp.body, "/data/lists").into_iter().next()?;
    if track.hash.is_empty() {
        return None;
    }
    Some((
        track.title.to_string(),
        track.hash,
        track.album_id,
        track.mix_song_id,
    ))
}

/// 按名称从用户列表里解析刚建歌单的数值 listid。创建响应并不可靠地带上它，
/// 且列表未必立刻反映新歌单，所以带短暂停顿重试。
/// 必须在 Tokio 运行时上运行（网络调用 + sleep）。
async fn find_playlist_id(client: &kugou::KugouClient, name: &str) -> Option<i64> {
    for attempt in 0..3 {
        if let Ok(resp) = client.user_playlists(1, PLAYLISTS_PER_PAGE).await
            && let Some(pl) = parse_playlists(&resp.body)
                .into_iter()
                .find(|p| p.name.as_ref() == name)
        {
            return Some(pl.listid);
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    }
    None
}

/// 整条导入管线（解析分享链接 → 建歌单 → 逐曲匹配 → 分块上传），
/// 跑在 Tokio 运行时上，因此没有任何网络 await 触碰 gpui executor。
/// 匹配进度经 `progress` 流出；调用方把返回的结果作为通道的最后一个事件转发。
async fn run_import(
    text: String,
    default_name: String,
    progress: &UnboundedSender<ImportEvent>,
) -> ImportOutcome {
    let client = kugou::shared_client();

    let parsed = match kugou::import::parse_share_link(&text).await {
        Ok(p) => p,
        Err(err) => return ImportOutcome::ParseFailed(err),
    };

    let playlist_name = if parsed.name.trim().is_empty() {
        default_name
    } else {
        parsed.name
    };

    if let Err(err) = client.create_playlist(&playlist_name).await {
        return ImportOutcome::CreateFailed(err.to_string());
    }

    let Some(listid) = find_playlist_id(&client, &playlist_name).await else {
        return ImportOutcome::NotFound;
    };

    let total = parsed.songs.len();
    let mut matched: Vec<(String, String, i64, i64)> = Vec::new();
    // 有界且保序的并发：无论哪个搜索先回答，`buffered` 都按 `parsed.songs`
    // 的顺序产出结果，而 `None`（搜索失败或没命中）仍然只是跳过这首歌
    let _ = progress.send(ImportEvent::Progress { done: 0, total });
    // 持有所有权的歌曲 + 每个 future 一次 `Arc` clone：流里没有任何东西借用
    // 外层栈帧，所以 buffered 的 future 保持可发送
    let mut searches = stream::iter(parsed.songs)
        .map(|song| {
            let client = client.clone();
            async move { kugou_match(&client, &song).await }
        })
        .buffered(KUGOU_MATCH_CONCURRENCY);
    // 进度统计每一首处理过的曲目，不只是命中的——命中率低时只数命中会让计数器
    // 停滞，看起来像导入卡死
    let mut processed = 0usize;
    while let Some(m) = searches.next().await {
        if let Some(m) = m {
            matched.push(m);
        }
        processed += 1;
        let _ = progress.send(ImportEvent::Progress {
            done: processed,
            total,
        });
    }

    if matched.is_empty() {
        return ImportOutcome::NoMatch;
    }

    let mut added = 0i64;
    for chunk in matched.chunks(PLAYLIST_ADD_CHUNK) {
        match client.playlist_add_songs(listid, chunk).await {
            Ok(_) => added += chunk.len() as i64,
            Err(error) => tracing::warn!(
                %error,
                added,
                failed = chunk.len(),
                "kugou import: failed to add a chunk of matched songs"
            ),
        }
    }

    // `matched` 现在指「真正加进去的」：部分/全部上传失败
    // 不得被报告成完整成功。
    ImportOutcome::Done {
        matched: added,
        total: total as i64,
    }
}
