use crate::{
    library::db::LibraryAccess,
    playback::{interface::PlaybackInterface, queue::QueueItemData},
    settings::SettingsGlobal,
    ui::{
        availability::is_track_path_available,
        components::{
            context::context,
            drag_drop::{
                AlbumDragData, DragDropItemState, DragDropListConfig, DragDropListManager,
                DragPreview, DropIndicator, TrackDragData, check_drag_cancelled,
                handle_external_drag_move, handle_track_drag_move,
                handle_track_drop_multi, request_edge_scroll,
            },
            icons::{CROSS, DISC, PLAYLIST_ADD, STAR, STAR_FILLED, TRASH, USERS, icon},
            managed_image::{ManagedImageKey, managed_image},
            menu::{menu, menu_item, menu_separator},
            nav_button::nav_button,
            scrollbar::{ScrollableHandle, floating_scrollbar},
            tooltip::build_tooltip,
        },
        library::{
            ViewSwitchMessage, add_to_playlist::AddToPlaylist, context_menus::navigate_to_artists,
        },
    },
};
use cntp_i18n::{tr, trn};
use gpui::*;
use prelude::FluentBuilder;
use rustc_hash::{FxHashMap, FxHashSet};
use std::{path::PathBuf, time::Duration};

use super::{
    components::button::{ButtonSize, ButtonStyle, button},
    models::{
        HasLikedState, Models, PlaybackInfo, is_song_liked, subscribe_liked_updates, toggle_like,
        toggle_like_by_id,
    },
    scroll_follow::SmoothScrollFollow,
    theme::Theme,
    util::create_or_retrieve_view,
};

/// The list identifier for queue drag-drop operations
const QUEUE_LIST_ID: &str = "queue";
/// Height of each queue item in pixels
const QUEUE_ITEM_HEIGHT: f32 = 60.0;
/// Duration of the queue auto-follow animation.
const QUEUE_FOLLOW_ANIMATION_DURATION: Duration = Duration::from_millis(180);

/// Shared selection state for the queue.
pub struct QueueSelection {
    selected: FxHashSet<usize>,
    /// Sorted snapshot of `selected`, maintained on every mutation so the
    /// render path can borrow it instead of allocating and sorting per row
    /// per frame.
    sorted: Vec<usize>,
    anchor: Option<usize>,
}

impl QueueSelection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            selected: FxHashSet::default(),
            sorted: Vec::new(),
            anchor: None,
        })
    }

    pub fn contains(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    pub fn is_multi(&self) -> bool {
        self.selected.len() > 1
    }

    pub fn indices(&self) -> Vec<usize> {
        self.sorted.clone()
    }

    /// Borrowed sorted snapshot; render-path consumers use this to skip the
    /// per-frame allocation entirely.
    pub fn sorted(&self) -> &[usize] {
        &self.sorted
    }

    /// Rebuilds the sorted snapshot after a mutation. Selection changes are
    /// user-action frequency, so the cost here is irrelevant.
    fn resync_sorted(&mut self) {
        self.sorted.clear();
        self.sorted.extend(self.selected.iter().copied());
        self.sorted.sort_unstable();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        self.resync_sorted();
        self.anchor = None;
        cx.notify();
    }

    /// Plain click: deselect all, select only this item.
    pub fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected.clear();
        self.selected.insert(index);
        self.resync_sorted();
        self.anchor = Some(index);
        cx.notify();
    }

    /// Ctrl/Cmd+click: toggle this item in the selection.
    pub fn ctrl_toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected.contains(&index) {
            self.selected.remove(&index);
            if self.anchor == Some(index) {
                self.anchor = self.selected.iter().copied().next();
            }
        } else {
            self.selected.insert(index);
            self.anchor = Some(index);
        }
        self.resync_sorted();
        cx.notify();
    }

    /// Shift+click: select range from anchor to this item.
    /// Replaces any previous range selection. If no anchor exists,
    /// uses `current_position` (the currently-playing track) as the anchor.
    pub fn shift_range(
        &mut self,
        index: usize,
        current_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let anchor = self.anchor.or(current_position).unwrap_or(index);
        self.anchor = Some(anchor);
        self.selected.clear();
        let start = anchor.min(index);
        let end = anchor.max(index);
        for i in start..=end {
            self.selected.insert(i);
        }
        self.resync_sorted();
        cx.notify();
    }
}

pub struct QueueItem {
    item: QueueItemData,
    current: usize,
    idx: usize,
    drag_drop_manager: Entity<DragDropListManager>,
    scroll_handle: UniformListScrollHandle,
    selection: Entity<QueueSelection>,
    add_to: Option<Entity<AddToPlaylist>>,
    show_add_to: Option<Entity<bool>>,
    track_id: Option<i64>,
    is_liked: Option<i64>,
    /// Computed once at view construction: a per-frame `path.exists()` stat
    /// for every visible row is far too expensive.
    is_available: bool,
    /// Formatted duration text, rebuilt only when the item's duration changes.
    duration_text: Option<SharedString>,
    cached_duration: Option<i64>,
    /// Cached drag payload base, rebuilt only when the row's display name
    /// changes (metadata load). Each frame just clones it and re-stamps the
    /// live index and selection instead of reallocating the path and name.
    drag_base: Option<(SharedString, TrackDragData)>,
}

impl HasLikedState for QueueItem {
    fn is_liked(&self) -> Option<i64> {
        self.is_liked
    }
    fn set_liked(&mut self, item_id: Option<i64>) {
        self.is_liked = item_id;
    }
}

impl QueueItem {
    pub fn new(
        cx: &mut App,
        item: QueueItemData,
        idx: usize,
        drag_drop_manager: Entity<DragDropListManager>,
        scroll_handle: UniformListScrollHandle,
        selection: Entity<QueueSelection>,
    ) -> Entity<Self> {
        cx.new(move |cx| {
            cx.on_release(|m: &mut QueueItem, cx| {
                m.item.drop_data(cx);
            })
            .detach();

            let queue = cx.global::<Models>().queue.clone();
            cx.observe(&queue, |this: &mut QueueItem, queue, cx| {
                this.current = queue.read(cx).position;
                cx.notify();
            })
            .detach();

            let track_id = item.get_db_id();
            let data = item.get_data(cx);
            // one stat per row view lifetime, not once per rendered frame
            let is_available = is_track_path_available(item.get_path());

            cx.observe(&data, |_, _, cx| {
                cx.notify();
            })
            .detach();

            // Observe drag-drop state changes to update visual feedback
            cx.observe(&drag_drop_manager, |_, _, cx| {
                cx.notify();
            })
            .detach();

            cx.observe(&selection, |_, _, cx| {
                cx.notify();
            })
            .detach();

            // Pure cache read, zero IO: the liked-id set is reloaded on every
            // liked-playlist change, and is_song_liked yields the track id
            // convention every row stores (see HasLikedState / is_song_liked).
            let is_liked = track_id.and_then(|id| is_song_liked(&**cx, id));

            subscribe_liked_updates(cx, |this: &QueueItem| this.track_id);

            Self {
                item,
                idx,
                current: queue.read(cx).position,
                drag_drop_manager,
                scroll_handle,
                selection,
                // the AddToPlaylist palette (and its show flag) is only built
                // the first time the context menu actually needs it
                add_to: None,
                show_add_to: None,
                track_id,
                is_liked,
                is_available,
                duration_text: None,
                cached_duration: None,
                drag_base: None,
            }
        })
    }

    pub fn update_idx(&mut self, idx: usize) {
        self.idx = idx;
    }
}

impl Render for QueueItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let album_id = self.item.get_db_album_id();
        let track_id = self.item.get_db_id();
        let data_entity = self.item.get_data(cx);
        let ui_data = &*data_entity.read(cx);
        let theme = cx.global::<Theme>().clone();
        let is_available = self.is_available;
        let is_selected = self.selection.read(cx).contains(self.idx);

        if let Some(item) = ui_data.as_ref() {
            // Rebuild the duration text only when the item's duration changes.
            let duration_text = if self.cached_duration == item.duration {
                self.duration_text.clone()
            } else {
                let text = item
                    .duration
                    .map(|d| SharedString::from(crate::ui::util::format_duration_compact(d)));
                self.cached_duration = item.duration;
                self.duration_text = text.clone();
                text
            };
            let scrollbar_always_visible = {
                let settings = cx.global::<SettingsGlobal>();
                let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();

                settings.model.read(cx).interface.always_show_scrollbars
                    && scroll_handle.should_draw_vertical_scrollbar()
            };
            let is_current = self.current == self.idx;
            let image_key = track_id.map(ManagedImageKey::Track).or_else(|| {
                #[cfg(feature = "online_sources")]
                {
                    // online (KuGou) tracks have no db id; use their HTTP art
                    if let Some(cover) = item.cover_url.clone().filter(|c| !c.is_empty()) {
                        return Some(ManagedImageKey::HttpCover(cover));
                    }
                }
                Some(ManagedImageKey::TrackFile(self.item.get_path().to_path_buf()))
            });
            let idx = self.idx;
            let current = self.current;
            let selection = self.selection.clone();
            let selection_for_drag = selection.clone();
            let selection_for_aux = selection.clone();
            let selection_for_menu = selection.clone();
            let selection_read = selection.read(cx);
            let is_multi_selected = selection_read.is_multi() && selection_read.contains(idx);
            let single_track_id = self.track_id;
            let queue_item_entity = cx.entity().clone();
            // add_to is created lazily on first menu use; its presence no
            // longer distinguishes library tracks from online ones
            let has_add_to = self.track_id.is_some();
            let is_liked = self.is_liked.is_some();

            let item_state =
                DragDropItemState::for_index(self.drag_drop_manager.read(cx), self.idx);

            let track_name = item
                .name
                .clone()
                .unwrap_or_else(|| tr!("UNKNOWN_TRACK").into());

            context(ElementId::View(cx.entity_id()))
                .with(
                    div()
                        .w_full()
                        .id("item-contents")
                        .flex()
                        .flex_shrink_0()
                        .overflow_x_hidden()
                        .gap(px(11.0))
                        .h(px(QUEUE_ITEM_HEIGHT))
                        .px(px(17.0))
                        .py(px(11.0))
                        // add extra padding when the scrollbar is always drawn
                        // 11px queue item pad + 4px scrollbar + 10px buffer
                        .when(scrollbar_always_visible, |div| div.pr(px(25.0)))
                        .when(is_available, |div| div.cursor_pointer())
                        .when(!is_available, |div| div.cursor_default().opacity(0.5))
                        .relative()
                        // Default bottom border - always present
                        .border_b(px(1.0))
                        .border_color(theme.border_color)
                        .when(item_state.is_being_dragged, |div| div.opacity(0.5))
                        .when(is_selected && !item_state.is_being_dragged, |div| {
                            div.bg(theme.queue_item_selected)
                        })
                        .when(
                            !is_selected && is_current && !item_state.is_being_dragged,
                            |div| div.bg(theme.queue_item_current),
                        )
                        .when(is_available, |div| {
                            div.on_click(move |event: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                let modifiers = event.modifiers();
                                let ctrl = modifiers.control || modifiers.platform;

                                let select_on_click = cx
                                    .global::<SettingsGlobal>()
                                    .model
                                    .read(cx)
                                    .interface
                                    .queue_select_on_click;

                                if event.click_count() == 2 {
                                    cx.global::<PlaybackInterface>().jump(idx);
                                } else if ctrl {
                                    selection.update(cx, |s, cx| s.ctrl_toggle(idx, cx));
                                } else if modifiers.shift {
                                    selection
                                        .update(cx, |s, cx| s.shift_range(idx, Some(current), cx));
                                } else if select_on_click {
                                    selection.update(cx, |s, cx| s.select(idx, cx));
                                } else {
                                    cx.global::<PlaybackInterface>().jump(idx);
                                }
                            })
                        })
                        .when(
                            is_available && !is_selected && !item_state.is_being_dragged,
                            |div| {
                                div.hover(|div| div.bg(theme.queue_item_hover))
                                    .active(|div| div.bg(theme.queue_item_active))
                            },
                        )
                        .when(
                            is_available && is_selected && !item_state.is_being_dragged,
                            |div| {
                                div.hover(|div| div.bg(theme.queue_item_selected))
                                    .active(|div| div.bg(theme.queue_item_active))
                            },
                        )
                        .when(is_available, |div| {
                            // The drag payload base is rebuilt only when the
                            // display name changed (metadata load); each frame
                            // just clones it and re-stamps the live index and
                            // selection instead of reallocating path and name.
                            if self
                                .drag_base
                                .as_ref()
                                .is_none_or(|(name, _)| *name != track_name)
                            {
                                let base = if let Some(tid) = self.track_id {
                                    TrackDragData::from_track(
                                        tid,
                                        album_id,
                                        self.item.get_path().to_path_buf(),
                                        track_name.clone(),
                                    )
                                } else {
                                    TrackDragData::new(
                                        self.item.get_path().to_path_buf(),
                                        track_name.clone(),
                                    )
                                };
                                self.drag_base = Some((track_name.clone(), base));
                            }
                            let mut drag_data = self.drag_base.as_ref().unwrap().1.clone();
                            drag_data = drag_data.with_reorder_info(QUEUE_LIST_ID, idx);

                            if is_selected {
                                // borrowed sorted snapshot; only the per-drag
                                // `others` Vec is still allocated
                                let others: Vec<usize> = selection_for_drag
                                    .read(cx)
                                    .sorted()
                                    .iter()
                                    .filter(|&&i| i != idx)
                                    .copied()
                                    .collect();
                                drag_data = drag_data.with_additional_indices(others);
                            }

                            div.on_drag(drag_data, {
                                // capture a clone: the move closure below must
                                // not consume the `track_name` reused later in
                                // this render
                                let track_name = track_name.clone();
                                move |_, _, _, cx| {
                                    DragPreview::new(cx, track_name.clone())
                                }
                            })
                            .drag_over::<TrackDragData>(
                                move |style, _, _, _| style.bg(gpui::rgba(0x88888822)),
                            )
                        })
                        .on_aux_click(move |ev: &ClickEvent, _, cx| {
                            if ev.is_right_click() && !selection_for_aux.read(cx).contains(idx) {
                                selection_for_aux.update(cx, |s, cx| s.select(idx, cx));
                            }
                        })
                        .when_some(self.add_to.clone(), |this, that| this.child(that))
                        .child(DropIndicator::with_state(
                            item_state.is_drop_target_before,
                            item_state.is_drop_target_after,
                            theme.button_primary,
                        ))
                        .child(
                            div()
                                .id("album-art")
                                .rounded(px(theme.radius_sm))
                                .bg(theme.album_art_background)
                                .shadow_sm()
                                .w(px(36.0))
                                .h(px(36.0))
                                .flex_shrink_0()
                                .when_some(image_key, |div, key| {
                                    div.child(
                                        managed_image(("queue-art", idx), key)
                                            .w(px(36.0))
                                            .h(px(36.0))
                                            .object_fit(ObjectFit::Fill)
                                            .rounded(px(theme.radius_sm))
                                            .thumb(),
                                    )
                                }),
                        )
                        .child(
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
                                        .w_full()
                                        .text_ellipsis()
                                        .font_weight(FontWeight::EXTRA_BOLD)
                                        // reuse the per-frame `track_name`
                                        // computed above instead of calling
                                        // tr! again on every frame
                                        .child(track_name.clone()),
                                )
                                .child(
                                    div()
                                        .overflow_x_hidden()
                                        .flex()
                                        .w_full()
                                        .max_w_full()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_ellipsis()
                                                .overflow_x_hidden()
                                                .flex_shrink(1.0)
                                                .child(item.artist_name.clone().unwrap_or_else(
                                                    || tr!("UNKNOWN_ARTIST").into(),
                                                )),
                                        )
                                        .when_some(duration_text, |child, text| {
                                            child.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .ml(px(6.0))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.text_secondary)
                                                    .child(text),
                                            )
                                        }),
                                ),
                        ),
                )
                .menu_on_open(move |_, cx| {
                    if is_multi_selected {
                        // The selection and queue snapshots are taken here, at
                        // menu-open time, instead of on every render frame.
                        let remove_indices = selection_for_menu.read(cx).indices();
                        let remove_count = remove_indices.len();
                        let selected_track_ids: Vec<i64> = {
                            let queue = cx.global::<Models>().queue.read(cx);
                            let queue_data = queue.data.read().unwrap_or_else(|e| e.into_inner());
                            remove_indices
                                .iter()
                                .filter_map(|&i| queue_data.get(i).and_then(|item| item.get_db_id()))
                                .collect()
                        };
                        let add_to_ids = selected_track_ids.clone();
                        let entity_for_add = queue_item_entity.clone();

                        // Cached liked-ids set (see is_song_liked): the menu
                        // only needs the liked/unliked boolean for icon and
                        // label, and unlike deletes by track id — no
                        // playlist_item row id and zero DB access here.
                        let liked_ids: Vec<i64> = selected_track_ids
                            .iter()
                            .copied()
                            .filter(|id| is_song_liked(cx, *id).is_some())
                            .collect();
                        let any_liked = !liked_ids.is_empty();

                        menu()
                            .when(!add_to_ids.is_empty(), |menu| {
                                menu.item(menu_item(
                                    "add_to_playlist",
                                    Some(PLAYLIST_ADD),
                                    tr!("ADD_TO_PLAYLIST"),
                                    move |_, _, cx| {
                                        entity_for_add.update(cx, |item, cx| {
                                            let show = item
                                                .show_add_to
                                                .get_or_insert_with(|| cx.new(|_| false))
                                                .clone();
                                            match &item.add_to {
                                                Some(add_to) => {
                                                    add_to.read(cx).set_track_ids(add_to_ids.clone());
                                                }
                                                None => {
                                                    item.add_to = Some(AddToPlaylist::new(
                                                        cx,
                                                        show.clone(),
                                                        add_to_ids.clone(),
                                                    ));
                                                }
                                            }
                                            show.write(cx, true);
                                            cx.notify();
                                        });
                                    },
                                ))
                                .item(menu_separator())
                            })
                            .when(!selected_track_ids.is_empty(), |menu| {
                                let track_ids_for_like = selected_track_ids.clone();
                                let liked_ids = liked_ids.clone();
                                menu.item(menu_item(
                                    "toggle_like",
                                    Some(if any_liked { STAR_FILLED } else { STAR }),
                                    if any_liked {
                                        tr!("UNLIKE")
                                    } else {
                                        tr!("LIKE")
                                    },
                                    move |_, _, cx| {
                                        if any_liked {
                                            // Unlike deletes by track id (see
                                            // toggle_like_by_id): the inner id
                                            // is ignored, so pass the track id.
                                            for &track_id in &liked_ids {
                                                toggle_like_by_id(track_id, Some(track_id), cx);
                                            }
                                        } else {
                                            for &track_id in &track_ids_for_like {
                                                toggle_like_by_id(track_id, None, cx);
                                            }
                                        }
                                    },
                                ))
                                .item(menu_separator())
                            })
                            .item(menu_item(
                                "remove_items",
                                Some(CROSS),
                                trn!(
                                    "REMOVE_N_FROM_QUEUE",
                                    "Remove {{count}} track from queue",
                                    "Remove {{count}} tracks from queue",
                                    count = remove_count
                                ),
                                move |_, _, cx| {
                                    cx.global::<PlaybackInterface>()
                                        .remove_items(remove_indices.clone());
                                },
                            ))
                    } else {
                        let entity_for_add = queue_item_entity.clone();
                        let artist_ids = single_track_id
                            .and_then(|id| cx.artist_ids_for_track(id).ok())
                            .unwrap_or_default();
                        let can_go_to_artist = !artist_ids.is_empty();
                        menu()
                            .when(has_add_to, |menu| {
                                menu.item(
                                    menu_item(
                                        "go_to_album",
                                        Some(DISC),
                                        tr!("GO_TO_ALBUM", "Go to album"),
                                        move |_, _, cx| {
                                            if let Some(album_id) = album_id {
                                                let switcher =
                                                    cx.global::<Models>().switcher_model.clone();
                                                switcher.update(cx, |_, cx| {
                                                    cx.emit(ViewSwitchMessage::Release(
                                                        album_id, None,
                                                    ));
                                                })
                                            }
                                        },
                                    )
                                    .disabled(!is_available || album_id.is_none()),
                                )
                                .item(
                                    menu_item(
                                        "go_to_artist",
                                        Some(USERS),
                                        tr!("GO_TO_ARTIST", "Go to artist"),
                                        move |ev, _, cx| {
                                            navigate_to_artists(
                                                cx,
                                                artist_ids.clone(),
                                                ev.position(),
                                            );
                                        },
                                    )
                                    .disabled(!is_available || !can_go_to_artist),
                                )
                                .item(menu_separator())
                                .item(menu_item(
                                    "add_to_playlist",
                                    Some(PLAYLIST_ADD),
                                    tr!("ADD_TO_PLAYLIST"),
                                    move |_, _, cx| {
                                        if let Some(track_id) = single_track_id {
                                            entity_for_add.update(cx, |item, cx| {
                                                let show = item
                                                    .show_add_to
                                                    .get_or_insert_with(|| cx.new(|_| false))
                                                    .clone();
                                                match &item.add_to {
                                                    Some(add_to) => {
                                                        add_to
                                                            .read(cx)
                                                            .set_track_ids(vec![track_id]);
                                                    }
                                                    None => {
                                                        item.add_to = Some(AddToPlaylist::new(
                                                            cx,
                                                            show.clone(),
                                                            vec![track_id],
                                                        ));
                                                    }
                                                }
                                                show.write(cx, true);
                                                cx.notify();
                                            });
                                        }
                                    },
                                ))
                                .item(menu_separator())
                                .when_some(single_track_id, |menu, track_id| {
                                    let entity = queue_item_entity.clone();
                                    menu.item(
                                        menu_item(
                                            "toggle_like",
                                            Some(if is_liked { STAR_FILLED } else { STAR }),
                                            if is_liked { tr!("UNLIKE") } else { tr!("LIKE") },
                                            move |_, _, cx| {
                                                toggle_like(track_id, entity.clone(), cx);
                                            },
                                        )
                                        .disabled(!is_available),
                                    )
                                })
                                .item(menu_separator())
                            })
                            .item(menu_item(
                                "remove_item",
                                Some(CROSS),
                                tr!("REMOVE_FROM_QUEUE", "Remove from queue"),
                                move |_, _, cx| {
                                    let playback = cx.global::<PlaybackInterface>();
                                    playback.remove_item(idx);
                                },
                            ))
                    }
                    .into_any_element()
                })
                .into_any_element()
        } else {
            // Metadata still loading: keep the row's shape with a plain block
            // so the list doesn't jump; the item repaints when data lands.
            div()
                .h(px(QUEUE_ITEM_HEIGHT))
                .border_t(px(1.0))
                .border_color(theme.border_color)
                .w_full()
                .id(ElementId::View(cx.entity_id()))
                .into_any_element()
        }
    }
}

pub struct Queue {
    views_model: Entity<FxHashMap<usize, Entity<QueueItem>>>,
    show_queue: Entity<bool>,
    scroll_handle: UniformListScrollHandle,
    drag_drop_manager: Entity<DragDropListManager>,
    selection: Entity<QueueSelection>,
    last_queue_position: usize,
    queue_hovered: bool,
    follow_current_pending: bool,
    follow_frame_scheduled: bool,
    scroll_follow: SmoothScrollFollow,
    /// Formatted "(N songs) • (total)" summary, valid for the cached
    /// `(queue_len, total_seconds)` key. The total is maintained
    /// incrementally (see `rescan_views_and_summary`), so the render path
    /// only re-formats when the key actually moves.
    cached_summary: (usize, i64, SharedString),
    /// Sum of the durations resolved so far; rebuilt by
    /// `rescan_views_and_summary` on every queue change and extended during
    /// render as pending metadata loads land.
    summary_total: i64,
    /// Indices whose duration is not resolved yet. Render reads exactly
    /// these (keeping the reactive dependency on their metadata entities)
    /// instead of re-scanning the whole queue every frame.
    pending_durations: Vec<usize>,
    /// Set when a queue notify arrived while the panel was hidden: the
    /// row-view prune and summary rescan are deferred to the first render
    /// after the panel is shown again.
    needs_queue_rescan: bool,
}

impl Queue {
    pub fn new(cx: &mut App, show_queue: Entity<bool>) -> Entity<Self> {
        cx.new(|cx| {
            let views_model = cx.new(|_| FxHashMap::default());
            let items = cx.global::<Models>().queue.clone();
            let initial_queue_position = items.read(cx).position;
            let initial_has_current_track =
                cx.global::<PlaybackInfo>().current_track.read(cx).is_some();

            let config = DragDropListConfig::new(QUEUE_LIST_ID, px(QUEUE_ITEM_HEIGHT));
            let drag_drop_manager = DragDropListManager::new(cx, config);
            let selection = QueueSelection::new(cx);

            cx.observe(&items, move |this: &mut Queue, _, cx| {
                let new_position = cx.global::<Models>().queue.read(cx).position;
                if this.last_queue_position != new_position {
                    this.last_queue_position = new_position;
                    this.follow_current_pending = true;
                    this.scroll_follow.cancel();
                }

                // Pruning and the summary rescan only matter while the panel
                // can be seen; while it is hidden the work is deferred to the
                // first render after it is shown again.
                if *this.show_queue.read(cx) {
                    this.rescan_views_and_summary(cx);
                } else {
                    this.needs_queue_rescan = true;
                }

                this.selection.update(cx, |s, cx| s.clear(cx));

                cx.notify();
            })
            .detach();

            Self {
                views_model,
                show_queue,
                scroll_handle: UniformListScrollHandle::new(),
                drag_drop_manager,
                selection,
                last_queue_position: initial_queue_position,
                queue_hovered: false,
                follow_current_pending: initial_has_current_track,
                follow_frame_scheduled: false,
                scroll_follow: SmoothScrollFollow::new(QUEUE_FOLLOW_ANIMATION_DURATION),
                // usize::MAX can never match a real queue length: forces one build
                cached_summary: (usize::MAX, 0, SharedString::default()),
                summary_total: 0,
                pending_durations: Vec::new(),
                // the first render builds the summary state from scratch
                needs_queue_rescan: true,
            }
        })
    }

    /// One pass over the queue data, run on every queue notify while the
    /// panel is visible: drops row views whose slot no longer exists and
    /// rebuilds the incremental summary state (resolved total plus the
    /// indices whose duration is still pending).
    fn rescan_views_and_summary(&mut self, cx: &mut Context<Self>) {
        let data = cx.global::<Models>().queue.read(cx).data.clone();
        let queue = data.read().unwrap_or_else(|e| e.into_inner());

        let mut valid_keys: Vec<usize> = Vec::with_capacity(queue.len());
        let mut pending: Vec<usize> = Vec::new();
        let mut total: i64 = 0;
        for (i, item) in queue.iter().enumerate() {
            if let Some(key) = item.existing_slot_key() {
                valid_keys.push(key);
            }
            match item.known_duration() {
                Some(secs) => total += secs,
                None => pending.push(i),
            }
        }
        let key_set: FxHashSet<usize> = valid_keys.iter().copied().collect();
        drop(queue);

        self.views_model.update(cx, |m, _| {
            m.retain(|k, _| key_set.contains(k));
        });
        self.summary_total = total;
        self.pending_durations = pending;
    }
}

impl Render for Queue {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        check_drag_cancelled(self.drag_drop_manager.clone(), cx);

        // A notify arrived while the panel was hidden: catch up now, before
        // the summary and the list are built.
        if self.needs_queue_rescan {
            self.rescan_views_and_summary(cx);
            self.needs_queue_rescan = false;
        }

        let theme = cx.global::<Theme>().clone();
        let queue_len = {
            let queue = cx.global::<Models>().queue.clone().read(cx);
            queue.data.read().unwrap_or_else(|e| e.into_inner()).len()
        };
        // Resolve only the items whose duration is still pending: reading
        // them here keeps the reactive dependency on their metadata entities,
        // so the summary refreshes exactly when each load lands — without
        // re-scanning the whole queue every frame.
        if !self.pending_durations.is_empty() {
            let data = cx.global::<Models>().queue.read(cx).data.clone();
            let queue = data.read().unwrap_or_else(|e| e.into_inner());
            let mut i = 0;
            while i < self.pending_durations.len() {
                let idx = self.pending_durations[i];
                match queue.get(idx).and_then(|item| item.loaded_duration(cx)) {
                    Some(secs) => {
                        self.summary_total += secs;
                        self.pending_durations.swap_remove(i);
                    }
                    None => i += 1,
                }
            }
        }
        // Reuse the formatted summary while the (length, total seconds) key is
        // unchanged; only re-run the format!/trn! when either actually moves.
        let queue_summary = if self.cached_summary.0 == queue_len
            && self.cached_summary.1 == self.summary_total
        {
            self.cached_summary.2.clone()
        } else {
            let summary = SharedString::from(format!(
                "{} • {}",
                trn!(
                    "QUEUE_SUMMARY_TRACKS",
                    "{{count}} song",
                    "{{count}} songs",
                    count = queue_len as i64
                ),
                crate::ui::util::format_duration_compact(self.summary_total)
            ));
            self.cached_summary = (queue_len, self.summary_total, summary.clone());
            summary
        };
        let views_model = self.views_model.clone();
        let scroll_handle = self.scroll_handle.clone();
        let item_scroll_handle = scroll_handle.clone();
        let drag_drop_manager = self.drag_drop_manager.clone();
        let selection = self.selection.clone();
        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;
        let is_dragging = self.drag_drop_manager.read(cx).state.is_dragging;

        if self.scroll_follow.is_active() && (self.queue_hovered || is_dragging) {
            self.scroll_follow.cancel();
        }

        if reduced_motion {
            if self.follow_current_pending || self.scroll_follow.is_active() {
                self.advance_follow_animation(window, cx, reduced_motion);
            }
        } else if (self.follow_current_pending || self.scroll_follow.is_active())
            && !self.queue_hovered
            && !is_dragging
        {
            self.schedule_follow_frame(window, cx);
        }

        div()
            .h_full()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .py(px(11.0))
                    .pl(px(18.0))
                    .pr(px(12.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border_color)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .line_height(rems(1.1))
                            .child(
                                div()
                                    .line_height(px(26.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(22.0))
                                    .child(tr!("QUEUE_TITLE", "Queue")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.text_secondary)
                                    .child(queue_summary.clone()),
                            ),
                    )
                    .child(
                        button()
                            .ml_auto()
                            .style(ButtonStyle::Minimal)
                            .size(ButtonSize::Large)
                            .child(icon(TRASH).size(px(14.0)).my_auto())
                            .child(tr!("CLEAR_QUEUE", "Clear"))
                            .id("clear-queue")
                            .on_click(|_, _, cx| {
                                cx.global::<PlaybackInterface>().clear_queue();
                            }),
                    )
                    .child(
                        nav_button("close", CROSS)
                            .on_click(cx.listener(|this: &mut Self, _, _, cx| {
                                this.show_queue.update(cx, |v, _| {
                                    *v = !(*v);
                                    crate::log_mem_event(if *v {
                                        "sidebar: queue show"
                                    } else {
                                        "sidebar: queue hide"
                                    });
                                })
                            }))
                            .tooltip(build_tooltip(tr!("CLOSE", "Close"))),
                    ),
            )
            .child(
                div()
                    .id("queue-list-container")
                    .flex()
                    .w_full()
                    .h_full()
                    .relative()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.selection.update(cx, |s, cx| s.clear(cx));
                    }))
                    .on_hover(cx.listener(|this, is_hovering: &bool, _, cx| {
                        if this.queue_hovered == *is_hovering {
                            return;
                        }

                        this.queue_hovered = *is_hovering;

                        if *is_hovering {
                            this.scroll_follow.cancel();
                        }

                        cx.notify();
                    }))
                    .on_drag_move::<TrackDragData>(cx.listener(
                        move |this: &mut Queue,
                              event: &DragMoveEvent<TrackDragData>,
                              window,
                              cx| {
                            let before = {
                                let manager = this.drag_drop_manager.read(cx);
                                (manager.state.is_dragging, manager.state.drop_target)
                            };
                            let scroll_handle: ScrollableHandle = this.scroll_handle.clone().into();

                            let reduced_motion = cx
                                .global::<SettingsGlobal>()
                                .model
                                .read(cx)
                                .interface
                                .reduced_motion;
                            let scrolled = handle_track_drag_move(
                                this.drag_drop_manager.clone(),
                                scroll_handle,
                                event,
                                queue_len,
                                cx,
                                reduced_motion,
                            );

                            if scrolled {
                                // guarded, at most one pending frame chain
                                // (per-event scheduling used to accumulate
                                // chains: scroll speed multiplied with the
                                // mouse report rate)
                                request_edge_scroll(
                                    this.drag_drop_manager.clone(),
                                    this.scroll_handle.clone().into(),
                                    window,
                                    cx,
                                );
                            }

                            // repaint only when something visible moved (same
                            // gating as the playlist view handlers)
                            let changed = {
                                let manager = this.drag_drop_manager.read(cx);
                                scrolled
                                    || (manager.state.is_dragging, manager.state.drop_target)
                                        != before
                            };
                            if changed {
                                cx.notify();
                            }
                        },
                    ))
                    .on_drag_move::<AlbumDragData>(cx.listener(
                        move |this: &mut Queue,
                              event: &DragMoveEvent<AlbumDragData>,
                              window,
                              cx| {
                            let before = {
                                let manager = this.drag_drop_manager.read(cx);
                                (manager.state.is_dragging, manager.state.drop_target)
                            };
                            let scroll_handle: ScrollableHandle = this.scroll_handle.clone().into();
                            let mouse_pos = event.event.position;
                            let container_bounds = event.bounds;

                            let reduced_motion = cx
                                .global::<SettingsGlobal>()
                                .model
                                .read(cx)
                                .interface
                                .reduced_motion;
                            let scrolled = handle_external_drag_move(
                                this.drag_drop_manager.clone(),
                                scroll_handle,
                                mouse_pos,
                                container_bounds,
                                queue_len,
                                cx,
                                reduced_motion,
                            );

                            if scrolled {
                                request_edge_scroll(
                                    this.drag_drop_manager.clone(),
                                    this.scroll_handle.clone().into(),
                                    window,
                                    cx,
                                );
                            }

                            // same gating as the track handler above
                            let changed = {
                                let manager = this.drag_drop_manager.read(cx);
                                scrolled
                                    || (manager.state.is_dragging, manager.state.drop_target)
                                        != before
                            };
                            if changed {
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(
                        move |this: &mut Queue, drag_data: &TrackDragData, _, cx| {
                            use crate::ui::components::drag_drop::DropPosition;

                            let is_internal = drag_data
                                .source_list_id
                                .as_ref()
                                .map(|id| *id == QUEUE_LIST_ID.into())
                                .unwrap_or(false);

                            if is_internal {
                                handle_track_drop_multi(
                                    this.drag_drop_manager.clone(),
                                    drag_data,
                                    cx,
                                    |drag_data, to, cx| {
                                        if drag_data.additional_indices.is_empty() {
                                            let Some(source) = drag_data.source_index else {
                                                return;
                                            };
                                            cx.global::<PlaybackInterface>()
                                                .move_item(source, to);
                                        } else {
                                            let corrected_to = to.saturating_sub(
                                                drag_data
                                                    .additional_indices
                                                    .iter()
                                                    .filter(|&&idx| idx < to)
                                                    .count(),
                                            );
                                            let indices = drag_data.all_indices();
                                            cx.global::<PlaybackInterface>()
                                                .move_items(indices, corrected_to);
                                        }
                                    },
                                );
                            } else {
                                let queue_item = QueueItemData::new(
                                    cx,
                                    drag_data.path.clone(),
                                    drag_data.track_id,
                                    drag_data.album_id,
                                );

                                let drop_target = this.drag_drop_manager.read(cx).state.drop_target;

                                if let Some((target_index, position)) = drop_target {
                                    let insert_pos = match position {
                                        DropPosition::Before => target_index,
                                        DropPosition::After => target_index + 1,
                                    };
                                    cx.global::<PlaybackInterface>()
                                        .insert_at(queue_item, insert_pos);
                                } else {
                                    cx.global::<PlaybackInterface>().queue(queue_item);
                                }

                                this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                            }
                            cx.notify();
                        },
                    ))
                    // album drops
                    .on_drop(cx.listener(
                        move |this: &mut Queue, drag_data: &AlbumDragData, _, cx| {
                            use crate::library::db::LibraryAccess;
                            use crate::ui::components::drag_drop::DropPosition;

                            if let Ok(tracks) = cx.list_tracks_in_album(drag_data.album_id) {
                                let queue_items: Vec<QueueItemData> = tracks
                                    .iter()
                                    .map(|track| {
                                        let item = QueueItemData::new(
                                            cx,
                                            track.location.clone(),
                                            Some(track.id),
                                            Some(drag_data.album_id),
                                        );
                                        item.set_known_duration(Some(track.duration));
                                        item
                                    })
                                    .collect();

                                let drop_target = this.drag_drop_manager.read(cx).state.drop_target;

                                if let Some((target_index, position)) = drop_target {
                                    let insert_pos = match position {
                                        DropPosition::Before => target_index,
                                        DropPosition::After => target_index + 1,
                                    };
                                    cx.global::<PlaybackInterface>()
                                        .insert_list_at(queue_items, insert_pos);
                                } else {
                                    cx.global::<PlaybackInterface>().queue_list(queue_items);
                                }
                            }
                            this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                            cx.notify();
                        },
                    ))
                    .child(
                        uniform_list("queue", queue_len, move |range, _, cx| {
                            let start = range.start;

                            let queue = cx
                                .global::<Models>()
                                .queue
                                .clone()
                                .read(cx)
                                .data
                                .read()
                                .unwrap_or_else(|e| e.into_inner());

                            if range.end <= queue.len() {
                                // entity id if the metadata entity exists, None otherwise;
                                // `cx` can't be used while the queue guard is held
                                let keys: Vec<(usize, Option<usize>)> = (start..range.end)
                                    .map(|i| (i, queue[i].existing_slot_key()))
                                    .collect();

                                drop(queue);

                                keys.into_iter()
                                    .filter_map(|(idx, existing_key)| {
                                        let drag_drop_manager = drag_drop_manager.clone();
                                        let scroll_handle = item_scroll_handle.clone();
                                        let item_selection = selection.clone();

                                        let item_key = match existing_key {
                                            Some(key) => key,
                                            None => {
                                                // never rendered before: the view cache
                                                // can't hold it, so build the key now
                                                let item = {
                                                    let queue = cx
                                                        .global::<Models>()
                                                        .queue
                                                        .clone()
                                                        .read(cx)
                                                        .data
                                                        .read()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    // the queue shrank since the list length
                                                    // was snapshotted: skip this row
                                                    queue.get(idx)?.clone()
                                                };
                                                item.slot_key(cx)
                                            }
                                        };

                                        let view = create_or_retrieve_view(
                                            &views_model,
                                            item_key,
                                            move |cx| {
                                                // clone the row only when its view is
                                                // (re)built, not on every cached frame
                                                let item = cx
                                                    .global::<Models>()
                                                    .queue
                                                    .clone()
                                                    .read(cx)
                                                    .data
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner())
                                                    .get(idx)
                                                    .cloned();
                                                // the queue shrank between the row-key pass
                                                // and this rebuild (never observed in
                                                // practice): build a placeholder row instead
                                                // of panicking the frame
                                                let item = match item {
                                                    Some(item) => item,
                                                    None => QueueItemData::new(
                                                        cx,
                                                        PathBuf::new(),
                                                        None,
                                                        None,
                                                    ),
                                                };
                                                QueueItem::new(
                                                    cx,
                                                    item,
                                                    idx,
                                                    drag_drop_manager,
                                                    scroll_handle,
                                                    item_selection,
                                                )
                                            },
                                            cx,
                                        );
                                        if view.read(cx).idx != idx {
                                            view.update(cx, |q, _| q.update_idx(idx));
                                        }

                                        Some(div().child(view))
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        })
                        .w_full()
                        .h_full()
                        .flex()
                        .flex_col()
                        .track_scroll(&scroll_handle),
                    )
                    .child(floating_scrollbar("queue_scrollbar", scroll_handle).right(px(4.0))),
            )
    }
}

impl Queue {
    fn schedule_follow_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.follow_frame_scheduled {
            return;
        }

        self.follow_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.follow_frame_scheduled = false;
            let reduced_motion = cx
                .global::<SettingsGlobal>()
                .model
                .read(cx)
                .interface
                .reduced_motion;
            this.advance_follow_animation(window, cx, reduced_motion);
        });
    }

    fn advance_follow_animation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) {
        if self.queue_hovered || self.drag_drop_manager.read(cx).state.is_dragging {
            self.scroll_follow.cancel();
            return;
        }

        if self.follow_current_pending {
            match self.compute_follow_target(cx) {
                FollowTarget::PendingLayout => {
                    self.schedule_follow_frame(window, cx);
                    return;
                }
                FollowTarget::NoScrollNeeded => {
                    self.follow_current_pending = false;
                    return;
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
                    self.follow_current_pending = false;
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

        if !changed {
            return;
        }

        if self.scroll_follow.is_active() {
            self.schedule_follow_frame(window, cx);
        }

        cx.notify();
    }

    fn compute_follow_target(&self, cx: &App) -> FollowTarget {
        let queue = cx.global::<Models>().queue.read(cx);
        let position = queue.position;
        let queue_len = queue.data.read().unwrap_or_else(|e| e.into_inner()).len();

        if queue_len == 0 || position >= queue_len {
            return FollowTarget::NoScrollNeeded;
        }

        let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
        let bounds = scroll_handle.bounds();
        let viewport_height = bounds.size.height;

        if viewport_height <= px(0.0) {
            return FollowTarget::PendingLayout;
        }

        let current_scroll_top = -scroll_handle.offset().y;
        let current_scroll_bottom = current_scroll_top + viewport_height;
        let max_scroll_top = scroll_handle.max_offset().y.max(px(0.0));

        let item_top = px(position as f32 * QUEUE_ITEM_HEIGHT);
        let item_bottom = item_top + px(QUEUE_ITEM_HEIGHT);

        let target_scroll_top = if item_top < current_scroll_top {
            item_top
        } else if item_bottom > current_scroll_bottom {
            (item_bottom - viewport_height).min(max_scroll_top)
        } else {
            return FollowTarget::NoScrollNeeded;
        };

        if (target_scroll_top - current_scroll_top).abs() <= px(0.1) {
            FollowTarget::NoScrollNeeded
        } else {
            FollowTarget::Target(target_scroll_top)
        }
    }

}

enum FollowTarget {
    PendingLayout,
    NoScrollNeeded,
    Target(Pixels),
}
