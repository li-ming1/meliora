use std::{cell::Cell, path::PathBuf};

use gpui::{
    Anchor, App, AppContext, Bounds, Context, Div, DragMoveEvent, ElementId, Entity, Hsla,
    IntoElement, ParentElement, Pixels, Point, Render, RenderOnce, SharedString, Styled, Window,
    anchored, div, point, prelude::FluentBuilder, px, size,
};
use palette::IntoColor;

use super::scrollbar::ScrollableHandle;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropPosition {
    Before,
    After,
}

#[derive(Clone, Debug)]
pub struct DragData {
    pub source_index: usize,
    pub list_id: ElementId,
}

impl DragData {
    pub fn new(source_index: usize, list_id: impl Into<ElementId>) -> Self {
        Self {
            source_index,
            list_id: list_id.into(),
        }
    }
}

/// Drag data for individual tracks that can be dropped onto the queue or playlists.
/// Also supports reordering when source_list_id and source_index are provided.
#[derive(Clone, Debug)]
pub struct TrackDragData {
    pub track_id: Option<i64>,
    pub album_id: Option<i64>,
    pub path: PathBuf,
    pub display_name: SharedString,
    /// Source list ID, if dragged from a reorderable list (e.g. a playlist or the queue).
    pub source_list_id: Option<ElementId>,
    pub source_index: Option<usize>,
    /// Additional selected indices when dragging multiple items from a list.
    pub additional_indices: Vec<usize>,
}

impl TrackDragData {
    pub fn new(path: impl Into<PathBuf>, display_name: impl Into<SharedString>) -> Self {
        Self {
            track_id: None,
            album_id: None,
            path: path.into(),
            display_name: display_name.into(),
            source_list_id: None,
            source_index: None,
            additional_indices: Vec::new(),
        }
    }

    pub fn from_track(
        track_id: i64,
        album_id: Option<i64>,
        path: impl Into<PathBuf>,
        display_name: impl Into<SharedString>,
    ) -> Self {
        Self {
            track_id: Some(track_id),
            album_id,
            path: path.into(),
            display_name: display_name.into(),
            source_list_id: None,
            source_index: None,
            additional_indices: Vec::new(),
        }
    }

    pub fn with_reorder_info(mut self, list_id: impl Into<ElementId>, index: usize) -> Self {
        self.source_list_id = Some(list_id.into());
        self.source_index = Some(index);
        self
    }

    pub fn with_additional_indices(mut self, indices: Vec<usize>) -> Self {
        self.additional_indices = indices;
        self
    }

    /// All indices being dragged (source_index + additional_indices), sorted and deduped.
    pub fn all_indices(&self) -> Vec<usize> {
        let mut all = self.source_index.into_iter().collect::<Vec<_>>();
        all.extend_from_slice(&self.additional_indices);
        all.sort_unstable();
        all.dedup();
        all
    }
}

#[derive(Clone, Debug)]
pub struct AlbumDragData {
    pub album_id: i64,
    pub display_name: SharedString,
}

impl AlbumDragData {
    pub fn new(album_id: i64, display_name: impl Into<SharedString>) -> Self {
        Self {
            album_id,
            display_name: display_name.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DragDropListConfig {
    pub list_id: ElementId,
    pub item_height: Pixels,
    pub scroll_config: EdgeScrollConfig,
}

impl DragDropListConfig {
    pub fn new(list_id: impl Into<ElementId>, item_height: Pixels) -> Self {
        Self {
            list_id: list_id.into(),
            item_height,
            scroll_config: EdgeScrollConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EdgeScrollConfig {
    pub edge_zone_height: Pixels,
    pub scroll_speed: Pixels,
}

impl Default for EdgeScrollConfig {
    fn default() -> Self {
        Self {
            edge_zone_height: px(50.0),
            scroll_speed: px(1.0),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DragDropState {
    pub dragging_indices: Vec<usize>,
    /// Current drop target: (index, position)
    pub drop_target: Option<(usize, DropPosition)>,
    pub is_dragging: bool,
    pub drag_mouse_y: Option<Pixels>,
}

impl DragDropState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_drop_target(&mut self, index: usize, position: DropPosition) {
        self.drop_target = Some((index, position));
    }

    pub fn clear_drop_target(&mut self) {
        self.drop_target = None;
    }

    pub fn end_drag(&mut self) {
        self.dragging_indices.clear();
        self.is_dragging = false;
        self.drop_target = None;
        self.drag_mouse_y = None;
    }

    pub fn set_mouse_y(&mut self, y: Pixels) {
        self.drag_mouse_y = Some(y);
    }
}

pub struct DragDropListManager {
    pub state: DragDropState,
    pub config: DragDropListConfig,
    /// Stored bounds for edge scroll calculations during animation frames
    pub container_bounds: Option<Bounds<Pixels>>,
    /// Set while an edge-scroll frame callback is pending. `DragMoveEvent`
    /// fires for every mouse move in the window during a drag, so scheduling
    /// the scroll loop per event without this guard accumulates concurrent
    /// self-perpetuating chains: scroll speed and `window.refresh()` calls
    /// grow with the mouse report rate. See `request_edge_scroll`.
    edge_scroll_armed: Cell<bool>,
}

impl DragDropListManager {
    pub fn new(cx: &mut App, config: DragDropListConfig) -> Entity<Self> {
        cx.new(|_| Self {
            state: DragDropState::new(),
            config,
            container_bounds: None,
            edge_scroll_armed: Cell::new(false),
        })
    }
}

/// Visual state for a single item in a drag-drop list.
#[derive(Clone, Copy, Debug, Default)]
pub struct DragDropItemState {
    pub is_being_dragged: bool,
    /// Whether the drop indicator should show at the top (before this item)
    pub is_drop_target_before: bool,
    /// Whether the drop indicator should show at the bottom (after this item)
    pub is_drop_target_after: bool,
}

impl DragDropItemState {
    pub fn for_index(manager: &DragDropListManager, index: usize) -> Self {
        let state = &manager.state;

        let is_being_dragged = state.dragging_indices.contains(&index);

        let (is_drop_target_before, is_drop_target_after) =
            if let Some((target_idx, position)) = state.drop_target {
                if target_idx == index {
                    match position {
                        DropPosition::Before => (true, false),
                        DropPosition::After => (false, true),
                    }
                } else {
                    (false, false)
                }
            } else {
                (false, false)
            };

        Self {
            is_being_dragged,
            is_drop_target_before,
            is_drop_target_after,
        }
    }
}

/// A visual indicator showing where a dragged item will be dropped.
#[derive(Clone, IntoElement)]
pub struct DropIndicator {
    show_before: bool,
    show_after: bool,
    color: Hsla,
}

impl DropIndicator {
    pub fn with_state(show_before: bool, show_after: bool, color: impl IntoColor<Hsla>) -> Self {
        Self {
            show_before,
            show_after,
            color: color.into_color(),
        }
    }
}

/// A 2px horizontal line pinned to one horizontal edge of the item: the top
/// edge when `bottom` is false (drop goes before this item), the bottom edge
/// when it is true (drop goes after).
fn edge_line(bottom: bool, color: Hsla) -> Div {
    let mut line = div()
        .absolute()
        .left(px(0.0))
        .right(px(0.0))
        .h(px(2.0))
        .bg(color);
    if bottom {
        line = line.bottom(px(0.0));
    } else {
        line = line.top(px(0.0));
    }
    line
}

impl RenderOnce for DropIndicator {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let show_before = self.show_before;
        let show_after = self.show_after;
        let color = self.color;

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .when(show_before, |this: Div| this.child(edge_line(false, color)))
            .when(show_after, |this: Div| this.child(edge_line(true, color)))
    }
}

/// A drag preview element that shows a simplified version of the dragged item.
pub struct DragPreview {
    pub label: SharedString,
}

impl DragPreview {
    pub fn new(cx: &mut App, label: impl Into<SharedString>) -> Entity<Self> {
        cx.new(|_| Self {
            label: label.into(),
        })
    }
}

impl Render for DragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::FontWeight;

        let theme = cx.global::<crate::ui::theme::Theme>();
        let position = window.mouse_position();

        anchored()
            .position(position)
            .anchor(Anchor::TopLeft)
            .offset(point(px(12.0), px(12.0)))
            .child(
                div()
                    .bg(theme.background_secondary)
                    .border_1()
                    .border_color(theme.border_color)
                    .rounded(px(theme.radius_sm))
                    .px(px(8.0))
                    .py(px(4.0))
                    .shadow_md()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(self.label.clone()),
                    ),
            )
    }
}

pub fn calculate_drop_position(mouse_y: Pixels, item_bounds: Bounds<Pixels>) -> DropPosition {
    let item_center_y = item_bounds.origin.y + (item_bounds.size.height / 2.0);
    if mouse_y < item_center_y {
        DropPosition::Before
    } else {
        DropPosition::After
    }
}

pub fn calculate_move_target(
    source_index: usize,
    target_index: usize,
    position: DropPosition,
) -> usize {
    match position {
        DropPosition::Before => {
            if source_index < target_index {
                target_index.saturating_sub(1)
            } else {
                target_index
            }
        }
        DropPosition::After => {
            if source_index <= target_index {
                target_index
            } else {
                target_index + 1
            }
        }
    }
}

/// Calculate which item index the mouse is over and the drop position. If the mouse is not over
/// any valid item, returns None.
pub fn calculate_drop_target(
    mouse_pos: Point<Pixels>,
    container_bounds: Bounds<Pixels>,
    scroll_offset_y: Pixels,
    item_height: Pixels,
    item_count: usize,
) -> Option<(usize, DropPosition)> {
    let relative_y = mouse_pos.y - container_bounds.origin.y - scroll_offset_y;
    let item_index = (relative_y / item_height).floor() as usize;

    if item_index >= item_count {
        return None;
    }

    let item_top = container_bounds.origin.y + (item_height * item_index as f32) + scroll_offset_y;
    let item_bounds = Bounds {
        origin: point(container_bounds.origin.x, item_top),
        size: size(container_bounds.size.width, item_height),
    };
    Some((
        item_index,
        calculate_drop_position(mouse_pos.y, item_bounds),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeScrollDirection {
    Up,
    Down,
    None,
}

pub fn get_edge_scroll_direction(
    mouse_y: Pixels,
    container_bounds: Bounds<Pixels>,
    config: &EdgeScrollConfig,
) -> EdgeScrollDirection {
    let top_zone_end = container_bounds.origin.y + config.edge_zone_height;
    let bottom_zone_start =
        container_bounds.origin.y + container_bounds.size.height - config.edge_zone_height;

    if mouse_y < top_zone_end && mouse_y >= container_bounds.origin.y {
        EdgeScrollDirection::Up
    } else if mouse_y > bottom_zone_start
        && mouse_y <= container_bounds.origin.y + container_bounds.size.height
    {
        EdgeScrollDirection::Down
    } else {
        EdgeScrollDirection::None
    }
}

/// Performs edge scrolling if needed, returns true if scrolling occurred.
pub fn perform_edge_scroll(
    scroll_handle: &ScrollableHandle,
    direction: EdgeScrollDirection,
    config: &EdgeScrollConfig,
) -> bool {
    match direction {
        // remember GPUI scroll offsets are negative
        EdgeScrollDirection::Up => {
            let current_offset = scroll_handle.offset();
            let new_y = (current_offset.y + config.scroll_speed).min(px(0.0));
            if new_y != current_offset.y {
                scroll_handle.set_offset(point(current_offset.x, new_y));
                true
            } else {
                false
            }
        }
        EdgeScrollDirection::Down => {
            let current_offset = scroll_handle.offset();
            let max_offset = scroll_handle.max_offset();
            let new_y = (current_offset.y - config.scroll_speed).max(-max_offset.y);
            if new_y != current_offset.y {
                scroll_handle.set_offset(point(current_offset.x, new_y));
                true
            } else {
                false
            }
        }
        EdgeScrollDirection::None => false,
    }
}

/// Shared tail of the drag-move handlers: edge scroll, drop-target
/// computation and one coalesced manager update. Returns whether scrolling
/// occurred; callers then schedule the edge-scroll loop via
/// [`request_edge_scroll`].
///
/// Everything here is gated on the pointer actually being inside the
/// container: `DragMoveEvent` fires for every mouse move in the window while
/// a drag is active, not only over this element, so otherwise edge scroll
/// runs (and chains get scheduled) while the pointer is anywhere else on the
/// same row, and a stale in-zone mouse_y keeps a scheduled chain scrolling
/// after the pointer left.
#[allow(clippy::too_many_arguments)] // internal helper threading drag-state through one pass
fn update_drag_move_state<V: 'static>(
    manager: &Entity<DragDropListManager>,
    scroll_handle: &ScrollableHandle,
    config: &DragDropListConfig,
    mouse_pos: Point<Pixels>,
    container_bounds: Bounds<Pixels>,
    item_count: usize,
    dragging_indices: Vec<usize>,
    reduced_motion: bool,
    cx: &mut Context<V>,
) -> bool {
    let contains = container_bounds.contains(&mouse_pos);

    let direction = if contains {
        get_edge_scroll_direction(mouse_pos.y, container_bounds, &config.scroll_config)
    } else {
        EdgeScrollDirection::None
    };
    let scrolled = if contains && !reduced_motion {
        perform_edge_scroll(scroll_handle, direction, &config.scroll_config)
    } else {
        false
    };

    let drop_target = if contains {
        calculate_drop_target(
            mouse_pos,
            container_bounds,
            scroll_handle.offset().y,
            config.item_height,
            item_count,
        )
    } else {
        None
    };

    // Single update: every `manager.update` notifies all row observers of
    // this list, so all state changes are coalesced into one.
    manager.update(cx, |m, _| {
        m.state.is_dragging = true;
        m.state.dragging_indices = dragging_indices;
        if contains {
            m.state.set_mouse_y(mouse_pos.y);
        } else {
            // stop any pending edge-scroll chain from a stale in-zone mouse_y
            m.state.drag_mouse_y = None;
        }
        m.container_bounds = Some(container_bounds);
        if let Some((item_index, drop_position)) = drop_target {
            m.state.update_drop_target(item_index, drop_position);
        } else {
            m.state.clear_drop_target();
        }
    });

    scrolled
}

/// Handle a drag move event for a drag-drop list.
///
/// Returns `true` if scrolling occurred. If scrolling occurred, the caller should schedule the
/// edge-scroll loop via [`request_edge_scroll`].
pub fn handle_drag_move<V: 'static>(
    manager: Entity<DragDropListManager>,
    scroll_handle: ScrollableHandle,
    event: &DragMoveEvent<DragData>,
    item_count: usize,
    cx: &mut Context<V>,
    reduced_motion: bool,
) -> bool {
    let drag_data = event.drag(cx);
    let config = manager.read(cx).config.clone();

    if drag_data.list_id != config.list_id {
        return false;
    }

    update_drag_move_state(
        &manager,
        &scroll_handle,
        &config,
        event.event.position,
        event.bounds,
        item_count,
        vec![drag_data.source_index],
        reduced_motion,
        cx,
    )
}

/// Handle a drag move event for TrackDragData in a list.
///
/// Works for both internal reordering (when source_list_id matches) and external
/// drops (when the drag originated from another list or component).
/// Returns `true` if scrolling occurred. If scrolling occurred, the caller should
/// schedule the edge-scroll loop via [`request_edge_scroll`].
pub fn handle_track_drag_move<V: 'static>(
    manager: Entity<DragDropListManager>,
    scroll_handle: ScrollableHandle,
    event: &DragMoveEvent<TrackDragData>,
    item_count: usize,
    cx: &mut Context<V>,
    reduced_motion: bool,
) -> bool {
    let drag_data = event.drag(cx);
    let config = manager.read(cx).config.clone();

    let is_internal = drag_data
        .source_list_id
        .as_ref()
        .map(|id| *id == config.list_id)
        .unwrap_or(false);

    let dragging_indices = if is_internal {
        drag_data.all_indices()
    } else {
        Vec::new()
    };

    update_drag_move_state(
        &manager,
        &scroll_handle,
        &config,
        event.event.position,
        event.bounds,
        item_count,
        dragging_indices,
        reduced_motion,
        cx,
    )
}

pub fn handle_drop<V: 'static, F>(
    manager: Entity<DragDropListManager>,
    drag_data: &DragData,
    cx: &mut Context<V>,
    on_reorder: F,
) where
    F: FnOnce(usize, usize, &mut Context<V>),
{
    let config_list_id = manager.read(cx).config.list_id.clone();

    if drag_data.list_id != config_list_id {
        return;
    }

    let source_index = drag_data.source_index;
    let target = manager.read(cx).state.drop_target;

    if let Some((target_index, position)) = target {
        let final_target = calculate_move_target(source_index, target_index, position);

        if source_index != final_target {
            on_reorder(source_index, final_target, cx);
        }
    }

    manager.update(cx, |m, _| m.state.end_drag());
}

/// Handle a drop of TrackDragData for reordering within a list.
///
/// Only processes the drop if it originated from the same list (source_list_id matches).
/// Calls on_reorder with (source_index, target_index) if a valid reorder should occur.
pub fn handle_track_drop<V: 'static, F>(
    manager: Entity<DragDropListManager>,
    drag_data: &TrackDragData,
    cx: &mut Context<V>,
    on_reorder: F,
) where
    F: FnOnce(usize, usize, &mut Context<V>),
{
    let config_list_id = manager.read(cx).config.list_id.clone();

    // Only handle if this drag originated from our list
    let is_internal = drag_data
        .source_list_id
        .as_ref()
        .map(|id| *id == config_list_id)
        .unwrap_or(false);

    if !is_internal {
        manager.update(cx, |m, _| m.state.end_drag());
        return;
    }

    let Some(source_index) = drag_data.source_index else {
        manager.update(cx, |m, _| m.state.end_drag());
        return;
    };

    let target = manager.read(cx).state.drop_target;

    if let Some((target_index, position)) = target {
        let final_target = calculate_move_target(source_index, target_index, position);

        if source_index != final_target {
            on_reorder(source_index, final_target, cx);
        }
    }

    manager.update(cx, |m, _| m.state.end_drag());
}

/// Handle a drop of TrackDragData with potential multi-selection indices.
///
/// Like `handle_track_drop`, but calls `on_multi_reorder` with the full
/// `TrackDragData` so the caller can dispatch single vs. multi-item moves.
pub fn handle_track_drop_multi<V: 'static, F>(
    manager: Entity<DragDropListManager>,
    drag_data: &TrackDragData,
    cx: &mut Context<V>,
    on_multi_reorder: F,
) where
    F: FnOnce(&TrackDragData, usize, &mut Context<V>),
{
    let config_list_id = manager.read(cx).config.list_id.clone();

    let is_internal = drag_data
        .source_list_id
        .as_ref()
        .map(|id| *id == config_list_id)
        .unwrap_or(false);

    if !is_internal {
        manager.update(cx, |m, _| m.state.end_drag());
        return;
    }

    // A payload without a source index can never be reordered: defaulting it
    // to 0 (the old fallback) would move the wrong rows.
    let Some(source_index) = drag_data.source_index else {
        manager.update(cx, |m, _| m.state.end_drag());
        return;
    };

    let target = manager.read(cx).state.drop_target;

    if let Some((target_index, position)) = target {
        let final_target = calculate_move_target(source_index, target_index, position);

        let is_multi = !drag_data.additional_indices.is_empty();
        if is_multi || source_index != final_target {
            on_multi_reorder(drag_data, final_target, cx);
        }
    }

    manager.update(cx, |m, _| m.state.end_drag());
}

/// Updates drag-drop state for an external drag (e.g. AlbumDragData).
///
/// Computes drop target, performs edge scrolling, and returns `true` if scrolling occurred.
/// Does not populate `dragging_indices`.
pub fn handle_external_drag_move<V: 'static>(
    manager: Entity<DragDropListManager>,
    scroll_handle: ScrollableHandle,
    mouse_pos: Point<Pixels>,
    container_bounds: Bounds<Pixels>,
    item_count: usize,
    cx: &mut Context<V>,
    reduced_motion: bool,
) -> bool {
    let config = manager.read(cx).config.clone();

    update_drag_move_state(
        &manager,
        &scroll_handle,
        &config,
        mouse_pos,
        container_bounds,
        item_count,
        Vec::new(),
        reduced_motion,
        cx,
    )
}

pub fn check_drag_cancelled<V: 'static>(
    manager: Entity<DragDropListManager>,
    cx: &mut Context<V>,
) -> bool {
    let has_active_drag = cx.has_active_drag();
    let our_state_is_dragging = manager.read(cx).state.is_dragging;

    if !has_active_drag && our_state_is_dragging {
        manager.update(cx, |m, _| m.state.end_drag());
        true
    } else {
        false
    }
}

/// Schedule the self-perpetuating edge-scroll loop (see [`schedule_edge_scroll`]).
///
/// Callers must route through this guard instead of scheduling
/// `schedule_edge_scroll` directly: `DragMoveEvent` fires for every mouse move
/// in the window while a drag is active, so per-event scheduling accumulates
/// concurrent frame chains — each scrolling once per frame and calling
/// `window.refresh()`, multiplying scroll speed and repaint cost with the
/// mouse report rate. At most one chain is ever pending per manager here.
pub fn request_edge_scroll(
    manager: Entity<DragDropListManager>,
    scroll_handle: ScrollableHandle,
    window: &mut Window,
    cx: &mut App,
) {
    if manager.read(cx).edge_scroll_armed.get() {
        return;
    }
    manager.read(cx).edge_scroll_armed.set(true);

    let weak_manager = manager.downgrade();
    window.on_next_frame(move |window, cx| {
        // The list (and its manager) may have been dropped mid-drag.
        let Some(manager) = weak_manager.upgrade() else {
            return;
        };
        manager.read(cx).edge_scroll_armed.set(false);
        schedule_edge_scroll(manager, scroll_handle, window, cx);
    });
}

/// Drive edge scrolling for as long as the pointer stays in the edge zone:
/// scroll once, re-arm via [`request_edge_scroll`], and stop when
/// `continue_edge_scroll` says stop. Shared by the queue and playlist drag
/// handlers.
pub fn schedule_edge_scroll(
    manager: Entity<DragDropListManager>,
    scroll_handle: ScrollableHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let reduced_motion = cx
        .global::<crate::settings::SettingsGlobal>()
        .model
        .read(cx)
        .interface
        .reduced_motion;
    if reduced_motion {
        return;
    }

    let should_continue = continue_edge_scroll(manager.read(cx), &scroll_handle);

    if should_continue {
        request_edge_scroll(manager, scroll_handle, window, cx);
        window.refresh();
    }
}

/// Continue edge scrolling during an animation frame.
///
/// Returns `true` if scrolling should continue (caller should schedule another frame).
pub fn continue_edge_scroll(
    manager: &DragDropListManager,
    scroll_handle: &ScrollableHandle,
) -> bool {
    if !manager.state.is_dragging {
        return false;
    }

    let Some(mouse_y) = manager.state.drag_mouse_y else {
        return false;
    };

    let Some(bounds) = manager.container_bounds else {
        return false;
    };

    let direction = get_edge_scroll_direction(mouse_y, bounds, &manager.config.scroll_config);
    perform_edge_scroll(scroll_handle, direction, &manager.config.scroll_config)
}
