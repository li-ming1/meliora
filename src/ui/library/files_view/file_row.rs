use std::{
    hash::{Hash, Hasher},
    path::Path,
    rc::Rc,
    sync::Arc,
};

use cntp_i18n::{tr, trn};
use gpui::{prelude::FluentBuilder, *};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::{
    library::{db::LibraryAccess, types::Track},
    playback::queue::QueueItemData,
    settings::SettingsGlobal,
    ui::{
        availability::is_track_path_available,
        components::{
            context::context,
            icons::{
                CHEVRON_DOWN, CHEVRON_RIGHT, FILE, FOLDER, FOLDER_OPEN, MUSIC, PLAY, PLAYLIST_ADD,
                STAR, STAR_FILLED, icon,
            },
            managed_image::{ManagedImageKey, managed_image},
            menu::{menu, menu_item, menu_separator},
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
        models::{
            HasLikedState, PlaybackInfo, is_song_liked, subscribe_liked_updates, toggle_like_by_id,
        },
        theme::Theme,
    },
};

pub const ROW_HEIGHT: f32 = 32.0;
const ICON_SIZE: f32 = 14.0;
const ART_SIZE: f32 = 20.0;

pub struct FileRowItem {
    flat_row: FlatRow,
    /// Weak on purpose: FilesView 持有全部行实体，行再强持有 view 会成环，
    /// 把整棵视图树钉在导航历史里无法释放。
    files_view: WeakEntity<FilesView>,
    is_liked: Option<i64>,
    show_add_to: Entity<bool>,
    add_to: Option<Entity<AddToPlaylist>>,
    /// Stable element-id hash for the row context menu; the path never
    /// changes for a row view's lifetime.
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

            // Row construction is pure in-memory state: the full track row
            // (needed by the context menu) and the file-availability stat are
            // resolved lazily when the menu actually opens, like the rest of
            // the menu tree. Construction used to block_on one DB query plus
            // one stat per row.
            // TrackRef.liked is the playlist_item row id; rows store the track
            // id convention (see HasLikedState), so reduce it to a liked flag.
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
        // Cached liked-ids set (see is_song_liked): the batch menu only needs
        // the liked/unliked boolean for icon and label, and unlike deletes by
        // track id — zero DB access here, so a shift-selected 500-row menu
        // opens without touching the database.
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
                        let data = batch_queue_items(cx, &audio_items);
                        play_items_now(cx, data);
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
                        let data = batch_queue_items(cx, &audio_items);
                        play_items_next(cx, data);
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
                    let audio_items = audio_items.clone();
                    move |_, _, cx| {
                        let data = batch_queue_items(cx, &audio_items);
                        queue_items(cx, data);
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
                            let track_ids = track_ids.clone();
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
                                // Unlike deletes by track id (see
                                // toggle_like_by_id): the inner id is ignored,
                                // so pass the track id.
                                for &id in &liked_ids {
                                    toggle_like_by_id(id, Some(id), cx);
                                }
                            } else {
                                for &id in like_ids.iter() {
                                    toggle_like_by_id(id, None, cx);
                                }
                            }
                        },
                    ))
            })
            .into_any_element()
    }
}

fn batch_queue_items(cx: &mut App, items: &[(Arc<Path>, Option<TrackRef>)]) -> Vec<QueueItemData> {
    let mut data = Vec::with_capacity(items.len());
    for (path, track) in items {
        if is_track_path_available(path) {
            data.push(QueueItemData::new(
                cx,
                path.to_path_buf(),
                track.as_ref().map(|track| track.id),
                track.as_ref().and_then(|track| track.album_id),
            ));
        }
    }
    data
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
        // Arc refcount bump; the row is cloned out of the shared flat list
        let path = self.flat_row.path.clone();
        let name = self.flat_row.name.clone();
        let depth = self.flat_row.depth;
        let is_dir = self.flat_row.is_dir;
        let is_audio = self.flat_row.is_audio;
        let expanded = self.flat_row.expanded;
        let loading = self.flat_row.loading;
        let has_children = self.flat_row.has_children;
        let track_ref = self.flat_row.track.clone();

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

        let theme = cx.global::<Theme>().clone();
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

        let two_column = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .two_column_library;

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

        // The AddToPlaylist overlay entities are cheap to create eagerly (their
        // track ids load on first open via keyed state); the menu trees move
        // into menu_on_open so their per-item DB queries and stats only run
        // when the user actually opens a menu.
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

        let batch_menu_state = (batch_items, self.show_add_to.clone());
        let entity_for_menu = cx.entity();
        let is_liked = self.is_liked;
        // Resolved when the menu opens (see menu_on_open below), not per row.
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
                                .rounded(px(theme.radius_sm))
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
                let fv = files_view.clone();
                let path = path.clone();
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
                let menu = if let Some((audio_items, track_ids)) = batch_menu_state
                    .0
                    .clone()
                    .filter(|(items, _)| !items.is_empty())
                {
                    Self::render_batch_menu(
                        &audio_items,
                        &track_ids,
                        entity_for_menu.clone(),
                        batch_menu_state.1.clone(),
                        cx,
                    )
                } else if let Some(track_id) = menu_track_id {
                    // Full track row + availability stat load lazily here
                    // (one DB query + one stat per menu open) instead of once
                    // per row at construction.
                    let is_file_available = is_track_path_available(&path_for_menu);

                    if let Ok(track) = cx.get_track_by_id(track_id) {
                        let (show_add_to, _) =
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
                            show_add_to,
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

                div()
                    .bg(cx.global::<Theme>().elevated_background)
                    .child(menu)
                    .into_any_element()
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
