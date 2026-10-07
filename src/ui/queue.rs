use crate::ui::design::ICON_SM;
use crate::{
    playback::{interface::PlaybackInterface, queue::QueueItemData},
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        availability::is_track_path_available,
        components::{
            context::context,
            drag_drop::{
                AlbumDragData, DragDropItemState, DragDropListConfig, DragDropListManager,
                DragPreview, DropIndicator, DropPosition, TrackDragData, check_drag_cancelled,
                handle_external_drag_move, handle_track_drag_move, handle_track_drop_multi,
                request_edge_scroll,
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
        library_access::LibraryAccess,
        queue_metadata,
    },
};
use cntp_i18n::{tr, trn};
use gpui::*;
use prelude::FluentBuilder;
use rustc_hash::{FxHashMap, FxHashSet};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tracing::error;

use super::{
    components::button::{ButtonSize, ButtonStyle, button},
    models::{
        HasLikedState, Models, PlaybackInfo, is_song_liked, subscribe_liked_updates,
        toggle_album_like, toggle_like,
    },
    scroll_follow::SmoothScrollFollow,
    theme::Theme,
    util::{VIEW_KEEP_AROUND, create_or_retrieve_view},
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
    /// Bumped by `resync_sorted` on every mutation. Lets render-path consumers
    /// (the per-row drag-companion cache) key their caches on a selection
    /// version number instead of re-deriving per-frame data; a mutation that
    /// leaves the set unchanged only causes a harmless cache rebuild.
    epoch: u64,
    anchor: Option<usize>,
}

impl QueueSelection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            selected: FxHashSet::default(),
            sorted: Vec::new(),
            epoch: 0,
            anchor: None,
        })
    }

    pub fn contains(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    pub fn is_multi(&self) -> bool {
        self.selected.len() > 1
    }

    /// Selection version: changes whenever any mutator runs (they all funnel
    /// through `resync_sorted`), even if the resulting set is identical.
    pub fn epoch(&self) -> u64 {
        self.epoch
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
    /// user-action frequency, so the cost here is irrelevant. Every mutator
    /// funnels through this method, making it the single bump point for the
    /// epoch that per-row drag caches key on.
    fn resync_sorted(&mut self) {
        self.sorted.clear();
        self.sorted.extend(self.selected.iter().copied());
        self.sorted.sort_unstable();
        self.epoch = self.epoch.wrapping_add(1);
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
        self.selected.extend(start..=end);
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
    /// Cached multi-drag companions: sorted selection minus this row, keyed on
    /// (selection epoch, own index). `QueueSelection::sorted()` is already a
    /// snapshot maintained on every mutation, so the per-frame recompute that
    /// remained was only the filter+collect `Vec` feeding the drag payload;
    /// with the payload's `additional_indices` now an `Arc<[usize]>`, this
    /// cache turns that per-frame allocation into a refcount bump.
    drag_others: Option<(u64, usize, Arc<[usize]>)>,
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
                queue_metadata::queue_drop_data(cx, &m.item);
            })
            .detach();

            let queue = cx.global::<Models>().queue.clone();
            cx.observe(&queue, |this: &mut QueueItem, queue, cx| {
                let position = queue.read(cx).position;
                // Only the rows whose current-track highlight actually moved
                // re-render; without this guard every queue notification
                // repaints all n rows (an O(n) fan-out per track switch).
                if this.current != position {
                    this.current = position;
                    cx.notify();
                }
            })
            .detach();

            let track_id = item.get_db_id();
            // metadata lives in the ui-side side table keyed by the item's
            // uid (same loaders, same restore branches as the legacy slot)
            let data = queue_metadata::queue_ui_data(cx, &item);
            // one stat per row view lifetime, not once per rendered frame
            let is_available = is_track_path_available(item.get_path());

            cx.observe(&data, |_, _, cx| {
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
            let is_liked = track_id.and_then(|id| is_song_liked(cx, id));

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
                drag_others: None,
            }
        })
    }

    pub fn update_idx(&mut self, idx: usize) {
        self.idx = idx;
    }

    /// Opens (creating on first use) the row's add-to-playlist palette over
    /// `track_ids`; shared by the single-row and multi-select context menus.
    fn open_add_to_palette(&mut self, track_ids: Vec<i64>, cx: &mut Context<Self>) {
        let show = self
            .show_add_to
            .get_or_insert_with(|| cx.new(|_| false))
            .clone();
        match &self.add_to {
            Some(add_to) => add_to.read(cx).set_track_ids(track_ids),
            None => self.add_to = Some(AddToPlaylist::new(cx, show.clone(), track_ids)),
        }
        show.write(cx, true);
        cx.notify();
    }
}

impl Render for QueueItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let album_id = self.item.get_db_album_id();
        let track_id = self.item.get_db_id();
        let data_entity = queue_metadata::queue_ui_data(cx, &self.item);
        let ui_data = data_entity.read(cx);
        let theme = cx.global::<Theme>();
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
                Some(ManagedImageKey::TrackFile(
                    self.item.get_path().to_path_buf(),
                ))
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
            let queue_item_entity = cx.entity();
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
                                // Multi-drag companions: the sorted selection
                                // minus this row. Cached for the drag session
                                // instead of recomputed per frame — why that is
                                // safe:
                                // 1. `QueueSelection::sorted()` is already a
                                //    snapshot rebuilt on every mutation, so the
                                //    ordered set itself never goes stale; this
                                //    cache only removes the per-frame
                                //    filter+collect allocation that fed the
                                //    payload.
                                // 2. Every selection mutator is click-driven
                                //    (`on_click`/`on_aux_click`), so while a
                                //    drag is live the set cannot change.
                                // 3. Even if a future mutator fires mid-drag,
                                //    its epoch bump rebuilds the cache next
                                //    frame — identical output to the old
                                //    per-frame recompute, never stale.
                                let selection_epoch = selection_for_drag.read(cx).epoch();
                                if self
                                    .drag_others
                                    .as_ref()
                                    .is_none_or(|(e, i, _)| *e != selection_epoch || *i != idx)
                                {
                                    let others: Arc<[usize]> = selection_for_drag
                                        .read(cx)
                                        .sorted()
                                        .iter()
                                        .filter(|&&i| i != idx)
                                        .copied()
                                        .collect();
                                    self.drag_others = Some((selection_epoch, idx, others));
                                }
                                if let Some((_, _, others)) = &self.drag_others {
                                    // refcount bump: `additional_indices` is
                                    // an `Arc<[usize]>` on the payload
                                    drag_data = drag_data.with_additional_indices(others.clone());
                                }
                            }

                            div.on_drag(drag_data, {
                                // capture a clone: the move closure below must
                                // not consume the `track_name` reused later in
                                // this render
                                let track_name = track_name.clone();
                                move |_, _, _, cx| DragPreview::new(cx, track_name.clone())
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
                                        .child(track_name),
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
                        // (index, track_id, path) per selected row, taken at
                        // menu-open time. The menu stays open while the queue
                        // can mutate concurrently (auto-insert on playback,
                        // removal from another entry point), so the Remove-N
                        // callback re-validates this snapshot before deleting:
                        // stale indices alone would delete whatever rows now
                        // occupy those positions.
                        let remove_snapshot: Vec<(usize, Option<i64>, PathBuf)> = {
                            let queue = cx.global::<Models>().queue.read(cx);
                            let queue_data = queue.data.read().unwrap_or_else(|e| e.into_inner());
                            remove_indices
                                .iter()
                                .filter_map(|&i| {
                                    queue_data
                                        .get(i)
                                        .map(|item| (i, item.get_db_id(), item.get_path().clone()))
                                })
                                .collect()
                        };
                        let selected_track_ids: Vec<i64> = remove_snapshot
                            .iter()
                            .filter_map(|&(_, id, _)| id)
                            .collect();
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
                                            item.open_add_to_palette(add_to_ids.clone(), cx);
                                        });
                                    },
                                ))
                                .item(menu_separator())
                            })
                            .when(!selected_track_ids.is_empty(), |menu| {
                                let track_ids_for_like = selected_track_ids.clone();
                                menu.item(menu_item(
                                    "toggle_like",
                                    Some(if any_liked { STAR_FILLED } else { STAR }),
                                    if any_liked {
                                        tr!("UNLIKE")
                                    } else {
                                        tr!("LIKE")
                                    },
                                    move |_, _, cx| {
                                        // One batched toggle: the unlike DELETE
                                        // is a no-op for ids that were never
                                        // liked, so only the liked subset moves
                                        // either way (see toggle_album_like).
                                        toggle_album_like(
                                            track_ids_for_like.clone(),
                                            any_liked,
                                            cx,
                                        );
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
                                    // Execute only if every menu-open snapshot
                                    // slot still holds the same content; a
                                    // queue mutated while the menu was open
                                    // invalidates the captured indices (see
                                    // `remove_snapshot` above).
                                    if remove_snapshot.iter().all(|&(i, id, ref path)| {
                                        queue_slot_matches(cx, i, id, path)
                                    }) {
                                        cx.global::<PlaybackInterface>()
                                            .remove_items(remove_indices.clone());
                                    } else {
                                        error!(
                                            "queue remove cancelled: queue changed while the \
                                             Remove-N menu was open ({} rows)",
                                            remove_count
                                        );
                                        emit_stale_remove_toast();
                                    }
                                },
                            ))
                    } else {
                        let entity_for_add = queue_item_entity.clone();
                        // Snapshot the row content at menu-open time (same
                        // instant as the multi-select snapshot above): the
                        // queue can mutate before the click lands, so the
                        // remove callback re-validates instead of trusting
                        // the captured `idx`.
                        let remove_snapshot = {
                            let queue = cx.global::<Models>().queue.read(cx);
                            let queue_data = queue.data.read().unwrap_or_else(|e| e.into_inner());
                            queue_data
                                .get(idx)
                                .map(|item| (item.get_db_id(), item.get_path().clone()))
                        };
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
                                                item.open_add_to_palette(vec![track_id], cx);
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
                                    // Same stale-index guard as remove_items
                                    // above: only delete while the snapshotted
                                    // row content still sits at this position.
                                    let unchanged =
                                        remove_snapshot.as_ref().is_some_and(|&(id, ref path)| {
                                            queue_slot_matches(cx, idx, id, path)
                                        });
                                    if unchanged {
                                        cx.global::<PlaybackInterface>().remove_item(idx);
                                    } else {
                                        error!(
                                            "queue remove cancelled: queue changed while the \
                                             remove menu was open (row {idx})"
                                        );
                                        emit_stale_remove_toast();
                                    }
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

/// Warn the user that a queued remove action was dropped because the queue
/// changed while its context menu was open (see `queue_slot_matches`).
fn emit_stale_remove_toast() {
    emit_toast(Toast::warning(tr!(
        "QUEUE_REMOVE_CANCELLED_STALE",
        "Queue changed — removal cancelled"
    )));
}

/// The `interface.reduced_motion` setting, read through the settings entity.
fn reduced_motion_enabled(cx: &App) -> bool {
    cx.global::<SettingsGlobal>()
        .model
        .read(cx)
        .interface
        .reduced_motion
}

/// True if the live queue still holds the content captured for `index` when
/// the context menu was opened (same track id and path).
///
/// Guards the queue context-menu remove actions: the menu stays open while
/// the queue can be mutated concurrently (auto-insert on playback, removal
/// from another entry point), so indices captured at menu-open time may point
/// at different rows by the time the click lands — deleting them anyway would
/// remove the wrong rows. The playback-side `dequeue_many` clamps
/// out-of-range indices but cannot know which rows were meant, so the content
/// check has to happen here, on the UI side.
fn queue_slot_matches(cx: &App, index: usize, track_id: Option<i64>, path: &PathBuf) -> bool {
    let queue = cx.global::<Models>().queue.read(cx);
    let queue_data = queue.data.read().unwrap_or_else(|e| e.into_inner());
    queue_data
        .get(index)
        .is_some_and(|item| item.get_db_id() == track_id && item.get_path() == path)
}

pub struct Queue {
    /// Row views keyed by the owning item's uid (`QueueItemData::uid`) —
    /// stable across clones and queue mutations, unlike the legacy per-item
    /// entity-id keys, and always available without touching any entity.
    views_model: Entity<FxHashMap<u64, Entity<QueueItem>>>,
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
    /// `rescan_views_and_summary` on every queue content change and extended
    /// during render as pending metadata loads land.
    summary_total: i64,
    /// Indices whose duration is not resolved yet. Render reads exactly
    /// these (keeping the reactive dependency on their metadata entities)
    /// instead of re-scanning the whole queue every frame.
    pending_durations: Vec<usize>,
    /// Slot-window `(start, end)` the row-view prune last ran for. The list
    /// closure skips rebuilding its keep set while unchanged;
    /// `rescan_views_and_summary` resets this on every queue content change,
    /// because slot keys move with their items — an unchanged slot window can
    /// hold a different key set after a mutation. A position-only notify
    /// skips the reset on purpose: no key moves, and if the follow-scroll
    /// brings a different slot range into view the window tuple itself
    /// changes, so the prune re-runs naturally.
    last_prune_window: (usize, usize),
    /// Set when a queue content change (content_epoch moved, see the
    /// observer) arrived while the panel was hidden: the row-view prune and
    /// summary rescan are deferred to the first render after the panel is
    /// shown again. Position-only notifies don't set this — there is nothing
    /// to catch up on.
    needs_queue_rescan: bool,
    /// Mirror of `models::Queue::content_epoch` as of the last
    /// `rescan_views_and_summary`: the observer's content-change
    /// discriminator. A queue notify whose position moved but whose epoch
    /// did not (the per-track-switch `QueuePositionChanged` path) skips the
    /// rescan entirely; see the observer for why that is safe.
    last_seen_content_epoch: u64,
}

impl Queue {
    pub fn new(cx: &mut App, show_queue: Entity<bool>) -> Entity<Self> {
        cx.new(|cx| {
            let views_model = cx.new(|_| FxHashMap::default());
            let items = cx.global::<Models>().queue.clone();
            let initial_queue_position = items.read(cx).position;
            let initial_content_epoch = items.read(cx).content_epoch;
            let initial_has_current_track =
                cx.global::<PlaybackInfo>().current_track.read(cx).is_some();

            let config = DragDropListConfig::new(QUEUE_LIST_ID, px(QUEUE_ITEM_HEIGHT));
            let drag_drop_manager = DragDropListManager::new(cx, config);
            let selection = QueueSelection::new(cx);

            cx.observe(&items, move |this: &mut Queue, _, cx| {
                let (new_position, new_content_epoch) = {
                    let queue_state = cx.global::<Models>().queue.read(cx);
                    (queue_state.position, queue_state.content_epoch)
                };
                if this.last_queue_position != new_position {
                    this.last_queue_position = new_position;
                    this.follow_current_pending = true;
                    this.scroll_follow.cancel();
                }

                // Position-only notification (QueuePositionChanged: the
                // current-track index moved): it leaves the live uid set and
                // every item's duration state untouched, so all three rescan
                // products are no-ops for it — the row-view retain would keep
                // the same keys, the metadata side-table GC would drop
                // nothing, and the summary state (total + pending indices) is
                // unchanged. Skip the whole O(n) scan on that path; every
                // track switch used to pay it. What a position change DOES
                // need happens elsewhere by design: the current-row highlight
                // is guarded per row in `QueueItem`'s own observer, and the
                // follow-scroll bookkeeping ran right above. The discriminator
                // is the content_epoch mirror (bumped by the event bridge on
                // QueueUpdated only): a batch carrying a content change
                // together with a position move still rescans, because the
                // epoch moved too.
                let content_changed = this.last_seen_content_epoch != new_content_epoch;
                this.last_seen_content_epoch = new_content_epoch;

                // Pruning and the summary rescan only matter while the panel
                // can be seen; while it is hidden the work is deferred to the
                // first render after it is shown again.
                if content_changed {
                    if *this.show_queue.read(cx) {
                        this.rescan_views_and_summary(cx);
                    } else {
                        this.needs_queue_rescan = true;
                    }
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
                // usize::MAX can never match a real slot window: forces the
                // first render to prune
                last_prune_window: (usize::MAX, usize::MAX),
                // the first render builds the summary state from scratch
                needs_queue_rescan: true,
                // matches the entity's epoch at construction; the first
                // content change after this still rescans
                last_seen_content_epoch: initial_content_epoch,
            }
        })
    }

    /// One pass over the queue data, run on every queue content change while
    /// the panel is visible (position-only notifies skip it — the uid set and
    /// every item's duration state are unchanged, see the observer's
    /// content_epoch check) plus the deferred first render after the panel is
    /// shown: drops row views (and metadata side-table slots) whose uid is no
    /// longer in the queue and rebuilds the incremental summary state
    /// (resolved total plus the indices whose duration is still pending).
    fn rescan_views_and_summary(&mut self, cx: &mut Context<Self>) {
        let data = cx.global::<Models>().queue.read(cx).data.clone();
        let queue = data.read().unwrap_or_else(|e| e.into_inner());

        // uids are always available (no per-item entity needed), so unlike
        // the legacy entity-id keys the collection never skips rows.
        let mut valid_keys: Vec<u64> = Vec::with_capacity(queue.len());
        let mut pending: Vec<usize> = Vec::new();
        let mut total: i64 = 0;
        for (i, item) in queue.iter().enumerate() {
            valid_keys.push(item.uid());
            match item.known_duration() {
                Some(secs) => total += secs,
                None => pending.push(i),
            }
        }
        let key_set: FxHashSet<u64> = valid_keys.iter().copied().collect();
        drop(queue);

        self.views_model.update(cx, |m, _| {
            m.retain(|k, _| key_set.contains(k));
        });
        // GC the metadata side table: entries whose item left the queue
        // release their entity handles here (their rows were just dropped by
        // the retain above and cleared their content on release).
        queue_metadata::gc_retaining(cx, key_set.into_iter());
        // slot keys move with their items: force the next window prune to
        // re-run even when the slot window itself is unchanged
        self.last_prune_window = (usize::MAX, usize::MAX);
        self.summary_total = total;
        self.pending_durations = pending;
    }
}

impl Render for Queue {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        check_drag_cancelled(self.drag_drop_manager.clone(), cx);

        // A content change arrived while the panel was hidden: catch up now,
        // before the summary and the list are built.
        if self.needs_queue_rescan {
            self.rescan_views_and_summary(cx);
            self.needs_queue_rescan = false;
        }

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
                match queue
                    .get(idx)
                    .and_then(|item| queue_metadata::queue_loaded_duration(cx, item))
                {
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
        let queue_summary =
            if self.cached_summary.0 == queue_len && self.cached_summary.1 == self.summary_total {
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
        let this_entity = cx.entity();
        let scroll_handle = self.scroll_handle.clone();
        let item_scroll_handle = scroll_handle.clone();
        let drag_drop_manager = self.drag_drop_manager.clone();
        let selection = self.selection.clone();
        let reduced_motion = reduced_motion_enabled(cx);
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

        // Borrow the theme only after the last `&mut cx` use above; the
        // per-element styling below just reads its Copy fields.
        let theme = cx.global::<Theme>();

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
                                    .child(queue_summary),
                            ),
                    )
                    .child(
                        button()
                            .ml_auto()
                            .style(ButtonStyle::Minimal)
                            .size(ButtonSize::Large)
                            .child(icon(TRASH).size(ICON_SM).my_auto())
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
                            let scrolled = handle_track_drag_move(
                                this.drag_drop_manager.clone(),
                                scroll_handle,
                                event,
                                queue_len,
                                cx,
                                reduced_motion_enabled(cx),
                            );
                            this.finish_drag_move(before, scrolled, window, cx);
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
                            let scrolled = handle_external_drag_move(
                                this.drag_drop_manager.clone(),
                                scroll_handle,
                                mouse_pos,
                                container_bounds,
                                queue_len,
                                cx,
                                reduced_motion_enabled(cx),
                            );
                            this.finish_drag_move(before, scrolled, window, cx);
                        },
                    ))
                    .on_drop(cx.listener(
                        move |this: &mut Queue, drag_data: &TrackDragData, _, cx| {
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
                                            cx.global::<PlaybackInterface>().move_item(source, to);
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
                            use crate::ui::library_access::LibraryAccess;

                            if let Ok(tracks) = cx.list_tracks_in_album(drag_data.album_id) {
                                let queue_items: Vec<QueueItemData> = tracks
                                    .iter()
                                    .map(|track| {
                                        let item = QueueItemData::new(
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
                                // each row's view key is its item's uid: stable
                                // across clones and always available — no `cx`
                                // needed while the queue guard is held
                                let keys: Vec<(usize, u64)> =
                                    (start..range.end).map(|i| (i, queue[i].uid())).collect();

                                // Scroll pruning: row views live in `views_model`
                                // keyed by the item's uid and nothing removed them on a
                                // one-way scroll, so a long queue accumulated one
                                // live row entity (with its observers) per slot ever
                                // rendered. Keep only the visible window ± the same
                                // band the table uses; pruned rows rebuild on demand.
                                // The keep set is only rebuilt when the window moved;
                                // queue changes reset `last_prune_window` (in
                                // `rescan_views_and_summary`) so a mutation always
                                // re-prunes even at an unchanged slot window.
                                let last_prune_window = this_entity.read(cx).last_prune_window;
                                let prune_start = start.saturating_sub(VIEW_KEEP_AROUND);
                                let prune_end = (range.end + VIEW_KEEP_AROUND).min(queue.len());
                                let keep = if last_prune_window != (prune_start, prune_end) {
                                    Some(
                                        (prune_start..prune_end)
                                            .filter_map(|i| queue.get(i).map(|item| item.uid()))
                                            .collect::<FxHashSet<u64>>(),
                                    )
                                } else {
                                    None
                                };

                                drop(queue);

                                if let Some(keep) = keep {
                                    views_model
                                        .update(cx, |m, _| m.retain(|k, _| keep.contains(k)));
                                    this_entity.update(cx, |this, _| {
                                        this.last_prune_window = (prune_start, prune_end);
                                    });
                                }

                                keys.into_iter()
                                    .map(|(idx, item_key)| {
                                        let drag_drop_manager = drag_drop_manager.clone();
                                        let scroll_handle = item_scroll_handle.clone();
                                        let item_selection = selection.clone();

                                        // the uid is always in hand, so the legacy
                                        // "build the metadata entity just to read its
                                        // entity id" fallback is gone
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

                                        div().child(view)
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
    /// Shared tail of both drag-move handlers: schedules the edge auto-scroll
    /// when the move scrolled the list — guarded, at most one pending frame
    /// chain (per-event scheduling used to accumulate chains: scroll speed
    /// multiplied with the mouse report rate) — and repaints only when
    /// something visible moved (same gating as the playlist view handlers).
    fn finish_drag_move(
        &mut self,
        before: (bool, Option<(usize, DropPosition)>),
        scrolled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if scrolled {
            request_edge_scroll(
                self.drag_drop_manager.clone(),
                self.scroll_handle.clone().into(),
                window,
                cx,
            );
        }

        let changed = {
            let manager = self.drag_drop_manager.read(cx);
            scrolled || (manager.state.is_dragging, manager.state.drop_target) != before
        };
        if changed {
            cx.notify();
        }
    }

    fn schedule_follow_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.follow_frame_scheduled {
            return;
        }

        self.follow_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.follow_frame_scheduled = false;
            let reduced_motion = reduced_motion_enabled(cx);
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
