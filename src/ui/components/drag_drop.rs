use std::{cell::Cell, path::PathBuf, sync::Arc};

use gpui::{
    Anchor, App, AppContext, Bounds, Context, Div, DragMoveEvent, ElementId, Entity, Hsla,
    IntoElement, ParentElement, Pixels, Point, Render, RenderOnce, SharedString, Styled, Window,
    anchored, div, point, prelude::FluentBuilder, px, size,
};
use palette::IntoColor;

use super::scrollbar::ScrollableHandle;
use crate::ui::util::reduced_motion_enabled;

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

/// 单曲目的拖拽数据，可投放到队列或歌单。提供了 source_list_id 与 source_index 时还
/// 支持列表内重排。
#[derive(Clone, Debug)]
pub struct TrackDragData {
    pub track_id: Option<i64>,
    pub album_id: Option<i64>,
    pub path: PathBuf,
    pub display_name: SharedString,
    /// 来源列表 ID，仅当从可重排列表（如歌单或队列）拖出时提供。
    pub source_list_id: Option<ElementId>,
    pub source_index: Option<usize>,
    /// 从列表拖出多个条目时附带的其余选中索引。共享 `Arc<[usize]>`（不是 `Vec`）：拖拽进行期间
    /// 每帧都会从缓存的基准载荷重新盖章，所以这个成员必须按引用计数克隆，而不是重新分配索引列表。
    /// `all_indices()` / 投放路径只通过切片访问读取它，因此它们感知不到容器类型。
    pub additional_indices: Arc<[usize]>,
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
            additional_indices: Arc::from([]),
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
            additional_indices: Arc::from([]),
        }
    }

    pub fn with_reorder_info(mut self, list_id: impl Into<ElementId>, index: usize) -> Self {
        self.source_list_id = Some(list_id.into());
        self.source_index = Some(index);
        self
    }

    pub fn with_additional_indices(mut self, indices: impl Into<Arc<[usize]>>) -> Self {
        self.additional_indices = indices.into();
        self
    }

    /// 本次拖拽涉及的全部索引（source_index + additional_indices），已排序并去重。
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
    /// 当前投放目标：(index, position)
    pub drop_target: Option<(usize, DropPosition)>,
    pub is_dragging: bool,
    pub drag_mouse_y: Option<Pixels>,
    /// 被拖越的列表条目数。边缘滚动会让静止指针下的可视槽位变化，所以动画帧续体需要它来
    /// 重算投放目标。
    pub item_count: usize,
}

impl DragDropState {
    pub fn new() -> Self {
        Self::default()
    }

    fn update_drop_target(&mut self, index: usize, position: DropPosition) {
        self.drop_target = Some((index, position));
    }

    fn clear_drop_target(&mut self) {
        self.drop_target = None;
    }

    pub fn end_drag(&mut self) {
        self.dragging_indices.clear();
        self.is_dragging = false;
        self.drop_target = None;
        self.drag_mouse_y = None;
    }

    fn set_mouse_y(&mut self, y: Pixels) {
        self.drag_mouse_y = Some(y);
    }
}

pub struct DragDropListManager {
    pub state: DragDropState,
    pub config: DragDropListConfig,
    /// 为动画帧期间的边缘滚动计算而保存的边界
    pub container_bounds: Option<Bounds<Pixels>>,
    /// 有边缘滚动帧回调待执行时置位。拖拽期间 `DragMoveEvent` 会在窗口内每次鼠标移动都触发，
    /// 所以逐事件排定滚动循环而不加这道守卫，会累积出多条自我延续的链：滚动速度与
    /// `window.refresh()` 调用次数都随鼠标上报频率增长。见 `request_edge_scroll`。
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

/// 拖放列表里单个条目的视觉状态。
#[derive(Clone, Copy, Debug, Default)]
pub struct DragDropItemState {
    pub is_being_dragged: bool,
    /// 投放指示线是否显示在顶部（即本条目之前）
    pub is_drop_target_before: bool,
    /// 投放指示线是否显示在底部（即本条目之后）
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

/// 显示拖拽条目将投放到何处的视觉指示器。
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

/// 钉在条目某条水平边上的一条 2px 横线：`bottom` 为 false 时是上边（投放到本条目之前），
/// 为 true 时是下边（投放到本条目之后）。
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

/// 拖拽预览元素，展示被拖条目的简化版本。
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

fn calculate_drop_position(mouse_y: Pixels, item_bounds: Bounds<Pixels>) -> DropPosition {
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

/// 计算鼠标落在哪个条目索引以及投放位置。鼠标不在任何有效条目上时返回 None。
fn calculate_drop_target(
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

fn get_edge_scroll_direction(
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

/// 需要时执行边缘滚动，返回是否发生了滚动。
fn perform_edge_scroll(
    scroll_handle: &ScrollableHandle,
    direction: EdgeScrollDirection,
    config: &EdgeScrollConfig,
) -> bool {
    match direction {
        // 记住 GPUI 的滚动 offset 是负值
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

/// 拖拽移动处理器的共享尾部：边缘滚动、投放目标计算与一次合并的 manager 更新。返回是否发生了
/// 滚动；调用方随后经 [`request_edge_scroll`] 排定边缘滚动循环。
///
/// 这里的一切都要求指针真的落在容器内：拖拽进行中 `DragMoveEvent` 会在窗口内每次鼠标移动都
/// 触发，而不只在这个元素上方。否则指针在同一行的别处也会跑边缘滚动（并排定链），而滞留在
/// 区域内的一次旧 mouse_y 会让已排定的链在指针离开之后继续滚动。
///
/// `dragging_indices` 传 `None` 表示沿用 state 现值：同一拖拽会话内 payload
/// 恒定，indices 首次设置后不变，无需按鼠标报告率每次重建。
#[allow(clippy::too_many_arguments)] // 内部辅助函数：一遍穿过拖拽状态把这些参数串起来
fn update_drag_move_state<V: 'static>(
    manager: &Entity<DragDropListManager>,
    scroll_handle: &ScrollableHandle,
    config: &DragDropListConfig,
    mouse_pos: Point<Pixels>,
    container_bounds: Bounds<Pixels>,
    item_count: usize,
    dragging_indices: Option<Vec<usize>>,
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

    // 单次 update 仅合并状态写入、不 notify：Entity::update 直接转发
    // App::update_entity，无任何通知（此处闭包忽略 cx，manager 从不
    // notify）。active-drag 期间每次 MouseMoveEvent 由 gpui refresh 整个
    // 窗口，行重绘由该机制驱动，不依赖观察者。
    manager.update(cx, |m, _| {
        m.state.is_dragging = true;
        m.state.item_count = item_count;
        if let Some(indices) = dragging_indices {
            m.state.dragging_indices = indices;
        }
        if contains {
            m.state.set_mouse_y(mouse_pos.y);
        } else {
            // 停掉因滞留在区域内的旧 mouse_y 而排定的边缘滚动链
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

/// 处理拖放列表的 drag move 事件。
///
/// 返回是否发生了滚动；若发生，调用方应经 [`request_edge_scroll`] 排定边缘滚动循环。
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

    // DragMoveEvent 在拖拽期间按鼠标报告率对每次移动触发（125-1000Hz），
    // 拖拽进行中 payload 恒定，传 None 沿用现值，跳过每次事件的 Vec 分配。
    let dragging_indices = if manager.read(cx).state.is_dragging {
        None
    } else {
        Some(vec![drag_data.source_index])
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

/// 处理列表中 TrackDragData 的 drag move 事件。
///
/// 列表内部重排（source_list_id 匹配）与外部投放（拖拽来自其它列表或组件）都适用。
/// 返回是否发生了滚动；若发生，调用方应经 [`request_edge_scroll`] 排定边缘滚动循环。
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

    // 同 handle_drag_move：拖拽进行中 payload 恒定，传 None 跳过
    // all_indices 的 collect+sort 堆分配。
    let dragging_indices = if manager.read(cx).state.is_dragging {
        None
    } else if is_internal {
        Some(drag_data.all_indices())
    } else {
        Some(Vec::new())
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

/// 处理 TrackDragData 在列表内部的重排投放。
///
/// 只处理源自同一列表的投放（source_list_id 匹配）。应当发生有效重排时，以
/// (source_index, target_index) 调用 on_reorder。
pub fn handle_track_drop<V: 'static, F>(
    manager: Entity<DragDropListManager>,
    drag_data: &TrackDragData,
    cx: &mut Context<V>,
    on_reorder: F,
) where
    F: FnOnce(usize, usize, &mut Context<V>),
{
    let config_list_id = manager.read(cx).config.list_id.clone();

    // 只处理源自本列表的拖拽
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

/// 处理可能带多选索引的 TrackDragData 投放。
///
/// 与 `handle_track_drop` 类似，但把完整的 `TrackDragData` 交给 `on_multi_reorder`，
/// 由调用方分派单条目还是多条目移动。
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

    // 没有 source index 的载荷永远不可重排：把它默认为 0（旧的兜底）会移动错误的行。
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

/// 更新外部拖拽（例如 AlbumDragData）的拖放状态。
///
/// 计算投放目标、执行边缘滚动，并在发生滚动时返回 `true`。不会填充 `dragging_indices`。
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
        Some(Vec::new()),
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

/// 排定自我延续的边缘滚动循环（见 [`schedule_edge_scroll`]）。
///
/// 调用方必须经这道守卫，而不是直接排 `schedule_edge_scroll`：拖拽进行中 `DragMoveEvent`
/// 会在窗口内每次鼠标移动都触发，逐事件排定会累积出多条并发的帧链——每条每帧滚动一次并调用
/// `window.refresh()`，滚动速度与重绘成本都随鼠标上报频率成倍增长。这里每个 manager 至多
/// 有一条链待执行。
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
        // 列表（连同其 manager）可能在拖拽途中被丢弃。
        let Some(manager) = weak_manager.upgrade() else {
            return;
        };
        manager.read(cx).edge_scroll_armed.set(false);
        schedule_edge_scroll(manager, scroll_handle, window, cx);
    });
}

/// 只要指针留在边缘区内就持续驱动边缘滚动：滚动一次、经 [`request_edge_scroll`] 重新武装、
/// 直到 `continue_edge_scroll` 说停。队列与歌单的拖拽处理器共用。
pub fn schedule_edge_scroll(
    manager: Entity<DragDropListManager>,
    scroll_handle: ScrollableHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let reduced_motion = reduced_motion_enabled(cx);
    if reduced_motion {
        return;
    }

    let should_continue = manager.update(cx, |manager, _| {
        continue_edge_scroll(manager, &scroll_handle)
    });

    if should_continue {
        request_edge_scroll(manager, scroll_handle, window, cx);
        window.refresh();
    }
}

/// 在动画帧期间继续边缘滚动。
///
/// 返回是否应继续滚动（调用方应再排一帧）。滚动会在没有 DragMoveEvent 的情况下把行移到静止
/// 指针之下，所以必须在这里重算投放目标，否则指示线（以及投放结果）会停在滚动前的那一行。
fn continue_edge_scroll(
    manager: &mut DragDropListManager,
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
    if !perform_edge_scroll(scroll_handle, direction, &manager.config.scroll_config) {
        return false;
    }

    let target = calculate_drop_target(
        point(bounds.origin.x, mouse_y),
        bounds,
        scroll_handle.offset().y,
        manager.config.item_height,
        manager.state.item_count,
    );
    match target {
        Some((index, position)) => manager.state.update_drop_target(index, position),
        None => manager.state.clear_drop_target(),
    }
    true
}
