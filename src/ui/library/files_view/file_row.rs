use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use cntp_i18n::{tr, trn};
use gpui::{prelude::FluentBuilder, *};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::{
    library::types::Track,
    playback::queue::QueueItemData,
    ui::{
        availability::{BATCH_ACTION_TICKET, available_paths, is_track_path_available},
        components::{
            context::context,
            icons::{
                CHEVRON_DOWN, CHEVRON_RIGHT, FILE, FOLDER, FOLDER_OPEN, MUSIC, PLAY, PLAYLIST_ADD,
                STAR, STAR_FILLED, icon,
            },
            managed_image::{ManagedImageKey, managed_image},
            menu::{menu, menu_item, menu_separator},
            table::table_data::elevated_menu_element,
        },
        library::{
            add_to_playlist::AddToPlaylist,
            context_menus::track::TrackContextMenu,
            context_menus::{
                TrackContextMenuContext, add_to_playlist_state, navigate_to_track_album_and_reveal,
                play_items_next, play_items_now, queue_items,
            },
            files_view::{
                BatchItems, FilesView, FlatRow, TrackRef, file_context_menu::FileContextMenu,
            },
        },
        library_access::LibraryAccess,
        models::{
            HasLikedState, PlaybackInfo, is_song_liked, subscribe_liked_updates, toggle_album_like,
        },
        theme::Theme,
        util::two_column_enabled,
    },
};

pub const ROW_HEIGHT: f32 = 32.0;
const ICON_SIZE: f32 = 14.0;
const ART_SIZE: f32 = 20.0;

pub struct FileRowItem {
    flat_row: FlatRow,
    /// 刻意用弱引用：FilesView 持有全部行实体，行再强持有 view 会成环，
    /// 把整棵视图树钉在导航历史里无法释放。
    files_view: WeakEntity<FilesView>,
    is_liked: Option<i64>,
    show_add_to: Entity<bool>,
    add_to: Option<Entity<AddToPlaylist>>,
    /// 行右键菜单的稳定元素 id 哈希；路径在一行视图的生命周期内永不变化。
    path_hash: usize,
}

fn path_hash(path: &Path) -> usize {
    let mut h = FxHasher::default();
    path.hash(&mut h);
    h.finish() as usize
}

impl FileRowItem {
    pub fn new(cx: &mut App, flat_row: FlatRow, files_view: Entity<FilesView>) -> Entity<Self> {
        cx.new(|cx| {
            cx.observe(&files_view, |_, _, cx| cx.notify()).detach();
            let files_view = files_view.downgrade();

            // 行的构造只是纯内存状态：完整的曲目行（右键菜单要用）与文件可用性 stat
            // 都在菜单真正打开时才惰性解析，和菜单树的其余部分一样。
            // 构造过去要每行一次 block_on DB 查询再加一次 stat。
            // TrackRef.liked 是 playlist_item 的行 id；行里存的是曲目 id 约定
            // （见 HasLikedState），所以把它归约成一个已点赞标志。
            let is_liked = flat_row
                .track
                .as_ref()
                .and_then(|t| t.liked.is_some().then_some(t.id));
            subscribe_liked_updates(cx, |this: &FileRowItem| {
                this.flat_row.track.as_ref().map(|t| t.id)
            });

            let path_hash = path_hash(&flat_row.path);

            if flat_row.is_audio {
                let current_track = cx.global::<PlaybackInfo>().current_track.clone();
                cx.observe(&current_track, |_, _, cx| cx.notify()).detach();
            }

            Self {
                flat_row,
                files_view,
                is_liked,
                show_add_to: cx.new(|_| false),
                add_to: None,
                path_hash,
            }
        })
    }

    fn render_batch_menu(
        audio_items: &Rc<Vec<(Arc<Path>, Option<TrackRef>)>>,
        track_ids: &Rc<Vec<i64>>,
        entity_for_add: Entity<Self>,
        show_add_to: Entity<bool>,
        cx: &mut App,
    ) -> AnyElement {
        // 缓存的 liked-ids 集合（见 is_song_liked）：批量菜单只需要点赞/未点赞这个布尔值
        // 来选图标与标签，而取消赞是按曲目 id 删除的——这里零 DB 访问，所以 shift 选中
        // 500 行的菜单打开时完全不碰数据库。
        let liked_ids: SmallVec<[i64; 32]> = track_ids
            .iter()
            .copied()
            .filter(|id| is_song_liked(cx, *id).is_some())
            .collect();
        let any_liked = !liked_ids.is_empty();
        let count = audio_items.len();

        let audio_items = Rc::clone(audio_items);

        menu()
            .item(menu_item(
                "files_multi_play",
                Some(PLAY),
                trn!(
                    "PLAY_N_NOW",
                    "Play {{count}} track now",
                    "Play {{count}} tracks now",
                    count = count
                ),
                {
                    let audio_items = audio_items.clone();
                    move |_, _, cx| {
                        queue_batch_items(audio_items.clone(), BatchAction::PlayNow, cx);
                    }
                },
            ))
            .item(menu_item(
                "files_multi_play_next",
                None::<SharedString>,
                tr!("PLAY_NEXT"),
                {
                    let audio_items = audio_items.clone();
                    move |_, _, cx| {
                        queue_batch_items(audio_items.clone(), BatchAction::PlayNext, cx);
                    }
                },
            ))
            .item(menu_item(
                "files_multi_queue",
                None::<SharedString>,
                trn!(
                    "ADD_N_TO_QUEUE",
                    "Add {{count}} track to queue",
                    "Add {{count}} tracks to queue",
                    count = count
                ),
                {
                    move |_, _, cx| {
                        queue_batch_items(audio_items.clone(), BatchAction::Enqueue, cx);
                    }
                },
            ))
            .when(!track_ids.is_empty(), |m| {
                let track_ids = Rc::clone(track_ids);
                let like_ids = track_ids.clone();
                m.item(menu_separator())
                    .item(menu_item(
                        "files_multi_add_to_playlist",
                        Some(PLAYLIST_ADD),
                        tr!("ADD_TO_PLAYLIST"),
                        {
                            move |_, _, cx| {
                                entity_for_add.update(cx, |item, cx| {
                                    match &item.add_to {
                                        Some(add_to) => {
                                            add_to.read(cx).set_track_ids((*track_ids).clone())
                                        }
                                        None => {
                                            item.add_to = Some(AddToPlaylist::new(
                                                cx,
                                                item.show_add_to.clone(),
                                                (*track_ids).clone(),
                                            ));
                                        }
                                    }
                                    cx.notify();
                                });
                                show_add_to.write(cx, true);
                            }
                        },
                    ))
                    .item(menu_item(
                        "files_multi_like",
                        Some(if any_liked { STAR_FILLED } else { STAR }),
                        if any_liked {
                            tr!("UNLIKE")
                        } else {
                            tr!("LIKE")
                        },
                        move |_, _, cx| {
                            if any_liked {
                                // 与"按 track id 取消收藏"不同：只摘除已赞子集，
                                // 未赞的其余选中项保持原状
                                toggle_album_like(liked_ids.to_vec(), true, cx);
                            } else {
                                // 全部未赞（any_liked 为假），整批一次入赞
                                toggle_album_like((*like_ids).clone(), false, cx);
                            }
                        },
                    ))
            })
            .into_any_element()
    }
}

enum BatchAction {
    PlayNow,
    PlayNext,
    Enqueue,
}

/// 整个选区的可用性在一次 blocking-task 批次里解析：同步循环过去要在 UI 线程上
/// 每个选中行 stat 一个文件，而失活网络挂载上单次 `Path::exists` 就能花掉几毫秒。
/// 可用子集随后照旧排队，只是晚一个任务跳转。
fn queue_batch_items(
    items: Rc<Vec<(Arc<Path>, Option<TrackRef>)>>,
    action: BatchAction,
    cx: &mut App,
) {
    let ticket = BATCH_ACTION_TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    cx.spawn(async move |cx| {
        let paths: Vec<PathBuf> = items.iter().map(|(path, _)| path.to_path_buf()).collect();
        let available = crate::RUNTIME
            .spawn_blocking(move || available_paths(paths))
            .await
            .unwrap_or_default();
        // 更新的点击已取代这一次（后点击优先，见 BATCH_ACTION_TICKET）
        if ticket != BATCH_ACTION_TICKET.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        cx.update(|cx| {
            let data = items
                .iter()
                .filter(|(path, _)| available.contains(path.as_ref()))
                .map(|(path, track)| {
                    QueueItemData::new(
                        path.to_path_buf(),
                        track.as_ref().map(|track| track.id),
                        track.as_ref().and_then(|track| track.album_id),
                    )
                })
                .collect::<Vec<_>>();

            match action {
                BatchAction::PlayNow => play_items_now(cx, data),
                BatchAction::PlayNext => play_items_next(cx, data),
                BatchAction::Enqueue => queue_items(cx, data),
            }
        });
    })
    .detach();
}

impl HasLikedState for FileRowItem {
    fn is_liked(&self) -> Option<i64> {
        self.is_liked
    }

    fn set_liked(&mut self, item_id: Option<i64>) {
        self.is_liked = item_id;
    }
}

impl Render for FileRowItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Arc 引用计数自增；行是从共享的扁平列表里 clone 出来的
        let FlatRow {
            path,
            name,
            depth,
            is_dir,
            is_audio,
            expanded,
            loading,
            has_children,
            track: track_ref,
        } = self.flat_row.clone();

        // 行只在 FilesView 存活期间被渲染；upgrade 失败 = 视图正在释放，
        // 渲染一个无选中态的朴素行即可。
        let Some(files_view) = self.files_view.upgrade() else {
            return div().into_any_element();
        };
        let is_selected = files_view.read(cx).selection_contains(&path);

        let is_current = is_audio
            && cx
                .global::<PlaybackInfo>()
                .current_track
                .read(cx)
                .as_ref()
                .is_some_and(|current| current.get_path() == &*path);

        let theme = cx.global::<Theme>();
        let bg = if is_selected {
            theme.queue_item_selected
        } else if is_current {
            theme.queue_item_current
        } else {
            theme.queue_item
        };
        let text_color = theme.text;
        let guide_color = theme.border_color;
        let icon_color = theme.text_secondary;
        let hover_bg = theme.queue_item_hover;
        let radius_sm = theme.radius_sm;

        let two_column = two_column_enabled(cx);

        let hash_id = self.path_hash;
        let context_id = ElementId::named_usize("fctx", hash_id);

        let batch_items: Option<BatchItems> = {
            let fv = files_view.read(cx);
            if is_selected && fv.is_multi() {
                fv.selected_batch.clone()
            } else {
                None
            }
        };

        // AddToPlaylist 的浮层实体尽早创建很廉价（它们的曲目 ids 首次打开时经 keyed
        // state 加载）；菜单树则挪进 menu_on_open，于是逐条目的 DB 查询与 stat 只在
        // 用户真的打开菜单时才跑。
        let add_to_element: Option<AnyElement> = if batch_items
            .as_ref()
            .is_some_and(|(items, _)| !items.is_empty())
        {
            self.add_to.clone().map(|a| a.into_any_element())
        } else if let Some(track) = &track_ref {
            let (_, add_to) = add_to_playlist_state("files-track-menu", track.id, window, cx);
            Some(add_to.into_any_element())
        } else {
            None
        };

        let show_add_to = self.show_add_to.clone();
        let entity_for_menu = cx.entity();
        let is_liked = self.is_liked;
        // 在菜单打开时才解析（见下面的 menu_on_open），不是每行一次。
        let menu_track_id = track_ref.as_ref().map(|track| track.id);
        let path_for_menu = path.clone();
        let files_view_for_menu = files_view.clone();

        let click_path = path.clone();
        let click_files_view = files_view.clone();
        let track_ref_for_click = track_ref.clone();

        let row_content = div()
            .id(hash_id)
            .px(px(4.0))
            .flex()
            .items_center()
            .w_full()
            .h(px(ROW_HEIGHT))
            .bg(bg)
            .cursor_pointer()
            .hover(|s| s.bg(hover_bg))
            .children((0..depth).map(|_| {
                div()
                    .flex_shrink_0()
                    .w(px(8.0))
                    .h_full()
                    .border_l_1()
                    .border_color(guide_color)
                    .ml(px(12.0))
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .ml(px(8.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(is_dir && has_children, |d| {
                        d.child(
                            icon(if expanded {
                                CHEVRON_DOWN
                            } else {
                                CHEVRON_RIGHT
                            })
                            .size(px(11.0))
                            .text_color(icon_color)
                            .when(loading, |i| i.opacity(0.5)),
                        )
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(ART_SIZE))
                    .h(px(ART_SIZE))
                    .ml(px(4.0))
                    .mr(px(if track_ref.is_some() { 9.0 } else { 6.0 }))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(if let Some(t) = &track_ref {
                        if let Some(album_id) = t.album_id {
                            managed_image(("file-art", hash_id), ManagedImageKey::Album(album_id))
                                .w_full()
                                .h_full()
                                .thumb()
                                .rounded(px(radius_sm))
                                .into_any_element()
                        } else {
                            icon(MUSIC)
                                .size(px(ICON_SIZE))
                                .text_color(icon_color)
                                .into_any_element()
                        }
                    } else if is_dir {
                        icon(if expanded { FOLDER_OPEN } else { FOLDER })
                            .size(px(ICON_SIZE))
                            .text_color(icon_color)
                            .into_any_element()
                    } else if is_audio {
                        icon(MUSIC)
                            .size(px(ICON_SIZE))
                            .text_color(icon_color)
                            .into_any_element()
                    } else {
                        icon(FILE)
                            .size(px(ICON_SIZE))
                            .text_color(icon_color)
                            .into_any_element()
                    }),
            )
            .child(
                div()
                    .flex_grow(1.0)
                    .text_ellipsis()
                    .overflow_hidden()
                    .text_color(text_color)
                    .text_sm()
                    .child(name),
            )
            .pr(px(8.0))
            .on_click(move |ev: &ClickEvent, _win, cx| {
                let modifiers = ev.modifiers();
                let ctrl = modifiers.control || modifiers.platform;

                if ev.click_count() >= 2 && !is_dir {
                    click_files_view.update(cx, |view, cx| {
                        view.play_folder(click_path.to_path_buf(), cx);
                    });
                } else if ctrl {
                    click_files_view.update(cx, |view, cx| {
                        view.toggle_selection(click_path.to_path_buf(), cx)
                    });
                } else if modifiers.shift {
                    click_files_view.update(cx, |view, cx| {
                        view.select_range(click_path.to_path_buf(), cx)
                    });
                } else {
                    click_files_view.update(cx, |view, cx| {
                        if is_dir {
                            view.toggle(click_path.to_path_buf(), cx);
                        }
                        view.select(click_path.to_path_buf(), cx);
                    });

                    if !is_dir
                        && two_column
                        && track_ref_for_click.is_some()
                        && let Ok(Some(track)) = cx.get_track_by_path(&click_path)
                    {
                        navigate_to_track_album_and_reveal(cx, &track);
                    }
                }
            })
            .on_aux_click({
                let fv = files_view;
                move |ev: &ClickEvent, _, cx| {
                    if ev.is_right_click() && !fv.read(cx).selection_contains(&path) {
                        fv.update(cx, |view, cx| view.select(path.to_path_buf(), cx));
                    }
                }
            });

        let ctx = context(context_id)
            .w_full()
            .with(row_content)
            .menu_on_open(move |window, cx| {
                let menu = if let Some((audio_items, track_ids)) =
                    batch_items.clone().filter(|(items, _)| !items.is_empty())
                {
                    Self::render_batch_menu(
                        &audio_items,
                        &track_ids,
                        entity_for_menu.clone(),
                        show_add_to.clone(),
                        cx,
                    )
                } else if let Some(track_id) = menu_track_id {
                    // 完整的曲目行 + 可用性 stat 在这里惰性加载（每次打开菜单一次 DB
                    // 查询 + 一次 stat），而不是构造时每行一次。
                    let is_file_available = is_track_path_available(&path_for_menu);

                    if let Ok(track) = cx.get_track_by_id(track_id) {
                        let (menu_show_add_to, _) =
                            add_to_playlist_state("files-track-menu", track.id, window, cx);

                        let play_from_here = Rc::new({
                            let files_view = files_view_for_menu.clone();
                            let path = path_for_menu.clone();
                            move |cx: &mut App, _: &Track| {
                                files_view.update(cx, |view, cx| {
                                    view.play_folder(path.to_path_buf(), cx);
                                });
                            }
                        });

                        TrackContextMenu::new(
                            Rc::new((*track).clone()),
                            is_file_available,
                            is_liked,
                            TrackContextMenuContext {
                                show_go_to_album: track.album_id.is_some(),
                                show_go_to_artist: true,
                                play_from_here: Some(play_from_here),
                            },
                            None,
                            menu_show_add_to,
                        )
                        .into_any_element()
                    } else {
                        FileContextMenu::new(
                            path_for_menu.clone(),
                            is_dir,
                            is_audio,
                            is_file_available,
                            files_view_for_menu.clone(),
                        )
                        .into_any_element()
                    }
                } else {
                    let is_file_available = is_track_path_available(&path_for_menu);

                    FileContextMenu::new(
                        path_for_menu.clone(),
                        is_dir,
                        is_audio,
                        is_file_available,
                        files_view_for_menu.clone(),
                    )
                    .into_any_element()
                };

                elevated_menu_element(cx, menu)
            });

        div()
            .w_full()
            .child(if let Some(add_to) = add_to_element {
                ctx.child(add_to).into_any_element()
            } else {
                ctx.into_any_element()
            })
            .into_any_element()
    }
}
