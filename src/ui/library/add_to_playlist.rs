use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, Global, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};
use nucleo::Utf32String;
use rustc_hash::FxHashSet;
use tracing::error;

use crate::{
    library::{
        db::{self},
        playlist::announce_playlist_update,
        types::Playlist,
    },
    toasts::emit_toast,
    ui::{
        app::Pool,
        components::{
            icons::PLAYLIST_ADD,
            modal::modal,
            palette::{ExtraItem, ExtraItemProvider, FinderItemLeft, Palette, PaletteItem},
        },
        library_access::{LibraryAccess, is_duplicate_playlist_name, playlist_name_taken_toast},
        models::{Models, PlaylistInfoTransfer},
    },
};

#[derive(Clone, Debug, PartialEq)]
enum TrackList {
    Single(i64),
    // 共享形态：对话框每一行都对选区做快照，而快照里的 ids 从不被读回（accept 读的是
    // 活列表），所以给每行发一份新的 Vec clone 就是 O(歌单数 x 选区数) 的死数据。
    Multi(Arc<[i64]>),
}

impl TrackList {
    fn from_ids(ids: Vec<i64>) -> Self {
        if ids.len() == 1 {
            TrackList::Single(ids[0])
        } else {
            TrackList::Multi(ids.into())
        }
    }

    fn first(&self) -> i64 {
        match self {
            TrackList::Single(id) => *id,
            // 空的 multi 列表是专辑流程的初始状态：它的曲目 ids 在对话框打开之后才由
            // 后台加载。曲目 id 0 在数据库中绝不可能存在，所以它退化到「不在歌单里」
            // 分支，而不是在索引上 panic。
            TrackList::Multi(ids) => ids.first().copied().unwrap_or(0),
        }
    }

    fn is_multi(&self) -> bool {
        matches!(self, TrackList::Multi(ids) if ids.len() > 1)
    }

    fn ids(&self) -> &[i64] {
        match self {
            TrackList::Single(id) => std::slice::from_ref(id),
            TrackList::Multi(ids) => ids,
        }
    }
}

/// 当前曲目选区的主线程共享句柄。所有访问都在 UI 线程上（实体本身与它的调色板提供者
/// 闭包），所以 `RefCell` 就够——不需要锁，也不需要原子量。
type SharedTrackList = Rc<RefCell<TrackList>>;

fn read_track_list(shared: &SharedTrackList) -> TrackList {
    shared.borrow().clone()
}

/// 一行调色板条目：选区快照、目标歌单，以及那首单曲是否已在该歌单里。第三个槽位每次
/// 重建只解析一次——用一条归属集合查询——于是 `middle_content` 从不查数据库；
/// 过去它对每次调色板重建（每敲一个键）的每个可见行都跑一次 UI 线程
/// `block_on` `playlist_has_track` 查询（教义 §2.3 / §14）。
type PlaylistEntry = (TrackList, Playlist, bool);

/// 单曲时一次查询该曲目已在的全部歌单；多选返回 None（行内不做已有标记，
/// 与原 per-playlist 短路一致）。查询失败降级为 None（全部按未加入显示），
/// 观察者不 panic。
fn track_playlist_ids(track_list: &TrackList, cx: &mut App) -> Option<FxHashSet<i64>> {
    if track_list.is_multi() {
        return None;
    }
    match cx.playlist_ids_for_track(track_list.first()) {
        Ok(ids) => Some(ids),
        Err(err) => {
            error!("Failed to resolve playlist membership for the add-to-playlist dialog: {err:?}");
            None
        }
    }
}

/// 等待派生的歌单 DB 任务，用调用点的消息记录失败，并把失败形态原样带回——
/// 调用方需要区分"撞了歌单名唯一约束"（要提示用户改名）与其它失败。
enum PlaylistTaskFailure {
    Db(sqlx::Error),
    /// 任务本身 panic。消息已在 `await_playlist_task` 里记进日志，这里只作标记。
    Panicked,
}

/// 等待派生的歌单 DB 任务，并用调用点的消息记录它的两种失败形态；只有任务完全成功时
/// 才交出载荷，于是调用方可以据此决定要不要发后续事件。
async fn await_playlist_task<T>(
    task: tokio::task::JoinHandle<Result<T, sqlx::Error>>,
    failure_log: &'static str,
    panic_log: &'static str,
) -> Result<T, PlaylistTaskFailure> {
    match task.await {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(err)) => {
            error!("{}: {err:?}", failure_log);
            Err(PlaylistTaskFailure::Db(err))
        }
        Err(err) => {
            error!("{}: {err:?}", panic_log);
            Err(PlaylistTaskFailure::Panicked)
        }
    }
}

impl PaletteItem for PlaylistEntry {
    fn left_content(&self, cx: &mut App) -> Option<FinderItemLeft> {
        self.1.left_content(cx)
    }

    fn middle_content(&self, _cx: &mut App) -> SharedString {
        if self.0.is_multi() || !self.2 {
            tr!(
                "ADD_TO_SELECTED_PLAYLIST",
                "Add to {{name}}",
                name = self.1.name.0.as_str()
            )
            .into()
        } else {
            tr!(
                "REMOVE_FROM_SELECTED_PLAYLIST",
                "Remove from {{name}}",
                name = self.1.name.0.as_str()
            )
            .into()
        }
    }

    fn right_content(&self, cx: &mut App) -> Option<SharedString> {
        self.1.right_content(cx)
    }
}

type MatcherFunc = Box<dyn Fn(&Arc<PlaylistEntry>, &mut App) -> Utf32String + 'static>;
type OnAccept = Box<dyn Fn(&Arc<PlaylistEntry>, &mut App) + 'static>;

/// 全局共享的「添加到歌单」模态框宿主：由主窗口创建并渲染一次，所有列表/网格/队列/
/// 搜索行的右键菜单直接更新它而不再按行分配实体树。
pub struct AddToPlaylistHost(pub Entity<AddToPlaylist>);

impl Global for AddToPlaylistHost {}

/// 打开全局共享的「添加到歌单」浮层并加载给定曲目列表。
pub(crate) fn open_add_to_playlist(track_ids: Vec<i64>, cx: &mut App) {
    let Some(host) = cx.try_global::<AddToPlaylistHost>().map(|h| h.0.clone()) else {
        return;
    };
    host.update(cx, |this, cx| this.open_tracks(track_ids, cx));
}

/// 打开全局共享的「添加到歌单」浮层，并在后台加载专辑内全部曲目 id。
pub(crate) fn open_add_album_to_playlist(album_id: i64, cx: &mut App) {
    let Some(host) = cx.try_global::<AddToPlaylistHost>().map(|h| h.0.clone()) else {
        return;
    };
    host.update(cx, |this, cx| this.open_album(album_id, cx));
}

pub struct AddToPlaylist {
    show: Entity<bool>,
    palette: Entity<Palette<PlaylistEntry, MatcherFunc, OnAccept>>,
    track_list: SharedTrackList,
    /// 专辑异步加载世代：关闭对话框或再次打开其他曲目/专辑时递增，过期结果不覆盖。
    album_load_generation: u64,
}

/// accept 两条分支共用的收尾：等 DB 写任务，成功就广播歌单更新。单曲目（加入或
/// 移出，取决于是否已在歌单里）与批量加入只差任务本体和两句日志措辞，这一层
/// 包装此前抄了两遍。
fn spawn_write_and_announce<T: 'static>(
    cx: &mut App,
    playlist_tracker: Entity<PlaylistInfoTransfer>,
    playlist_id: i64,
    task: tokio::task::JoinHandle<Result<T, sqlx::Error>>,
    failure_log: &'static str,
    panic_log: &'static str,
) {
    cx.spawn(async move |cx| {
        if await_playlist_task(task, failure_log, panic_log)
            .await
            .is_ok()
        {
            announce_playlist_update(&playlist_tracker, cx, playlist_id);
        }
    })
    .detach();
}

impl AddToPlaylist {
    /// 打开对话框时重建调色板列表：一次 `list_all_playlists` 加一次集合查询取回
    /// "这些曲目已经躺在哪些歌单里"（替代逐歌单的 `playlist_has_track`
    /// `block_on`）。查询失败时保留调色板当前的列表，而不是让观察者 panic。
    fn rebuild_playlist_list(&mut self, track_list: &SharedTrackList, cx: &mut Context<Self>) {
        let current = read_track_list(track_list);
        self.palette.update(cx, |palette, cx| {
            let Ok(playlists) = cx.list_all_playlists() else {
                error!("Failed to load playlists for the add-to-playlist dialog");
                return;
            };
            let existing = track_playlist_ids(&current, cx);
            let new_playlists = playlists
                .iter()
                .cloned()
                .map(|playlist| {
                    let has_track = existing
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&playlist.id));
                    (current.clone(), playlist, has_track)
                })
                .map(Arc::new)
                .collect::<Vec<_>>();

            cx.emit(new_playlists);

            palette.reset(cx);
        });

        cx.notify();
    }

    pub fn new_shared(cx: &mut App) -> Entity<Self> {
        let show = cx.new(|_| false);
        Self::new(cx, show, Vec::new())
    }

    pub fn new(cx: &mut App, show: Entity<bool>, track_ids: Vec<i64>) -> Entity<Self> {
        cx.new(|cx| {
            let track_list: SharedTrackList = Rc::new(RefCell::new(TrackList::from_ids(track_ids)));

            let track_list_for_observe = track_list.clone();
            cx.observe(&show, move |this: &mut Self, _, cx| {
                // 关闭路径的 palette.reset 已由 render 的 on_exit 或 open_accept 完成；
                // 关闭时仍需 notify 自己让模态框节点撤掉，打开时重建列表。
                if !*this.show.read(cx) {
                    cx.notify();
                    return;
                }

                this.rebuild_playlist_list(&track_list_for_observe, cx);
            })
            .detach();

            let matcher: MatcherFunc =
                Box::new(|playlist, _| Utf32String::from(playlist.1.name.0.as_str()));

            let show_clone = show.clone();

            // Accept 在点击时读共享曲目列表，而不是逐项快照：专辑流程里 ids 是在对话框
            // 已经打开之后才落到共享列表里的，所以构造那些项时抓下的快照仍是空的，
            // 按它 accept 会让首次打开静默地什么都不加。
            let track_list_for_accept = track_list.clone();

            let on_accept: OnAccept = Box::new(move |playlist, cx| {
                let track_ids = track_list_for_accept.borrow().ids().to_vec();
                let playlist_id = playlist.1.id;

                let pool = cx.global::<Pool>().0.clone();
                let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

                if track_ids.len() == 1 {
                    let track_id = track_ids[0];
                    let has_track = cx.playlist_has_track(playlist_id, track_id).ok().flatten();
                    let task = if let Some(id) = has_track {
                        crate::RUNTIME
                            .spawn(async move { db::remove_playlist_item(&pool, id).await })
                    } else {
                        crate::RUNTIME.spawn(async move {
                            db::add_playlist_item(&pool, playlist_id, track_id)
                                .await
                                .map(|_| ())
                        })
                    };
                    spawn_write_and_announce(
                        cx,
                        playlist_tracker,
                        playlist_id,
                        task,
                        "could not remove/add track from playlist",
                        "remove/add from playlist task panicked",
                    );
                } else {
                    let task = crate::RUNTIME.spawn(async move {
                        db::add_playlist_items(&pool, playlist_id, &track_ids, None).await
                    });
                    spawn_write_and_announce(
                        cx,
                        playlist_tracker,
                        playlist_id,
                        task,
                        "could not add tracks to playlist",
                        "add tracks to playlist task panicked",
                    );
                }

                show_clone.write(cx, false);
            });

            // MELIORA 可靠性审计（2026-09-13）：这里刻意「不」解析 items。调色板只在
            // `show` 为真时才可见，而每条打开路径都在创建之后翻转 `show`，这让上面的
            // 观察者在同一次 effect 冲刷、模态框第一帧之前重建完整列表——并带上全新的
            // 逐歌单 `has_track`。在这里解析的代价是：每次 keyed-state 创建都要一次
            // `list_all_playlists` block_on 加每歌单一次 `playlist_has_track` block_on；
            // 而那份额有状态在行滚出虚拟化列表时就被丢弃（gpui `Frame::finish` 只保留
            // 上一帧访问过的元素状态），于是用户从未右键点过的行在每次滚动时都重复这些
            // 查询（教义 §2.3 / §14 / §34）。
            let items: Vec<Arc<PlaylistEntry>> = Vec::new();

            let palette = Palette::new(cx, items, matcher, on_accept, &show);

            let track_list_for_create = track_list.clone();
            let show_for_create = show.clone();
            // 只在主线程使用的调色板：闭包捕获了 gpui 实体（非 Send），
            // 对只在 UI 线程上调用的提供者来说这没问题。
            #[allow(clippy::arc_with_non_send_sync)]
            let provider: ExtraItemProvider = Arc::new(move |query: &str| {
                let name = query.trim();
                if name.is_empty() {
                    return Vec::new();
                }

                let name_string = name.to_string();
                let display = tr!("CREATE_PLAYLIST", name = name_string);

                let show_clone2 = show_for_create.clone();
                let track_list_for_accept = track_list_for_create.clone();

                vec![ExtraItem {
                    left: Some(FinderItemLeft::Icon(PLAYLIST_ADD.into())),
                    middle: display.into(),
                    right: None,
                    on_accept: Arc::new(move |cx| {
                        let pool = cx.global::<Pool>().0.clone();
                        let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
                        let name_string = name_string.clone();
                        // toast 里要显示用户输入的名字：RUNTIME 任务会把
                        // name_string 移走，这里留一份克隆给失败分支用
                        let create_name_for_toast = name_string.clone();
                        // 接受时再取选中曲目（对齐主 accept）：对话框打开后
                        // 选集仍可能被专辑流程后台补齐
                        let create_track_ids = track_list_for_accept.borrow().ids().to_vec();
                        // 对话框关闭句柄给成功分支（provider 闭包是 Fn，show_clone2
                        // 本体不能被 async 块 move 走）
                        let show_for_close = show_clone2.clone();

                        cx.spawn(async move |cx| {
                            let task = crate::RUNTIME.spawn(async move {
                                let playlist_id = db::create_playlist(&pool, &name_string).await?;
                                db::add_playlist_items(&pool, playlist_id, &create_track_ids, None)
                                    .await?;
                                Ok::<i64, sqlx::Error>(playlist_id)
                            });

                            match await_playlist_task(
                                task,
                                "could not create playlist and add track",
                                "create playlist task panicked",
                            )
                            .await
                            {
                                Ok(playlist_id) => {
                                    announce_playlist_update(&playlist_tracker, cx, playlist_id);
                                    // 成功才关对话框：撞名/失败分支保持打开让用户
                                    // 改名重试——下方的 write(false) 此前在 detach
                                    // 之后无条件执行，:385 注释声称的"撞名时留着"
                                    // 意图从未落地。（本 fork 的 AsyncApp::update
                                    // 直接返回 R、应用消失时内部 panic——此处更新
                                    // 失败即窗口已死，无需兜底。）
                                    cx.update(|app| {
                                        show_for_close.write(app, false);
                                    });
                                }
                                // 撞名时对话框保持打开（成功分支才关闭），弹
                                // toast 让用户改名重试；其它失败同样保持打开、
                                // 沿用既有的只记日志。
                                Err(PlaylistTaskFailure::Db(err))
                                    if is_duplicate_playlist_name(&err) =>
                                {
                                    emit_toast(playlist_name_taken_toast(&create_name_for_toast));
                                }
                                Err(_) => {}
                            }
                        })
                        .detach();
                    }),
                }]
            });

            cx.update_entity(&palette, |palette, cx| {
                palette.register_extra_provider(provider.clone(), cx);
            });

            Self {
                show,
                palette,
                track_list,
                album_load_generation: 0,
            }
        })
    }

    pub fn set_track_ids(&self, track_ids: Vec<i64>) {
        *self.track_list.borrow_mut() = TrackList::from_ids(track_ids);
    }

    fn open_tracks(&mut self, track_ids: Vec<i64>, cx: &mut Context<Self>) {
        self.album_load_generation = self.album_load_generation.wrapping_add(1);
        self.set_track_ids(track_ids);
        // write(true) 无条件 notify，show 观察者在同一次 effect 冲刷里重建列表
        // （构造处注释声明的机制）——此前 was_open 手动分支是冗余的第二次
        // rebuild，且各含一次 UI 线程阻塞查询（重开场景观察者已覆盖）。
        self.show.write(cx, true);
    }

    fn open_album(&mut self, album_id: i64, cx: &mut Context<Self>) {
        self.album_load_generation = self.album_load_generation.wrapping_add(1);
        let generation = self.album_load_generation;
        self.set_track_ids(Vec::new());
        // 同 open_tracks：重开场景的列表重建由 show 观察者覆盖；后台补齐落地
        // 后的重建见下方 spawn 回调（round 19 REG-2 / 第四轮 C4-1）。
        self.show.write(cx, true);

        let pool = cx.global::<Pool>().0.clone();
        cx.spawn(async move |this, cx| {
            let tracks = crate::RUNTIME
                .spawn(async move { db::list_tracks_in_album(&pool, album_id).await })
                .await
                .ok()
                .and_then(|result| result.ok())
                .map(|tracks| tracks.iter().map(|track| track.id).collect::<Vec<i64>>())
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                if this.album_load_generation == generation {
                    this.set_track_ids(tracks);
                    // 回归（round 19 REG-2 确认项，第四轮 C4-1 补修——round 19
                    // 裁决漏列）：补齐只写共享列表、行不重建，行内 has_track
                    // 恒为空列表快照——单曲专辑已在歌单时行文案恒「加入」而
                    // accept 实查活列表执行「移出」，用户意图反转。对话框仍
                    // 开才重建；代纪守卫已覆盖补齐与再次打开的竞态。
                    if *this.show.read(cx) {
                        let track_list = this.track_list.clone();
                        this.rebuild_playlist_list(&track_list, cx);
                    }
                }
            });
        })
        .detach();
    }
}

impl Render for AddToPlaylist {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show = self.show.clone();
        let palette = self.palette.clone();
        let show_read = *self.show.read(cx);

        if show_read {
            cx.update_entity(&palette, |palette, cx| {
                palette.focus(window, cx);
            });

            let weak = cx.weak_entity();
            modal()
                .child(div().w(px(550.0)).h(px(300.0)).child(palette.clone()))
                .on_exit(move |_, cx| {
                    let _ = weak.update(cx, |this, _| {
                        this.album_load_generation = this.album_load_generation.wrapping_add(1);
                    });
                    show.update(cx, |show, cx| {
                        *show = false;
                        cx.update_entity(&palette, |palette, cx| {
                            palette.reset(cx);
                        });
                        cx.notify();
                    })
                })
                .into_any_element()
        } else {
            div().into_any_element()
        }
    }
}
