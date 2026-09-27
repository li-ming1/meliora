use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::*;
use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;

use super::table_data::{
    COLUMN_MIN_WIDTH, COLUMN_RESIZE_HANDLE_WIDTH, Column, DEFAULT_COLUMN_WIDTH, TABLE_HEADER_GROUP,
    TABLE_HEADER_HEIGHT,
};
use crate::ui::theme::Theme;

#[derive(Default)]
struct ResizeState {
    is_dragging: bool,
    start_x: Pixels,
    start_width: f32,
}

pub struct ColumnResizeHandle<C>
where
    C: Column + 'static,
{
    id: ElementId,
    column_index: usize,
    columns: Entity<Arc<IndexMap<C, f32, FxBuildHasher>>>,
    default_width: f32,
}

impl<C> ColumnResizeHandle<C>
where
    C: Column + 'static,
{
    pub fn new(
        column_index: usize,
        columns: Entity<Arc<IndexMap<C, f32, FxBuildHasher>>>,
        default_width: f32,
    ) -> Self {
        Self {
            // Composite (name, index) id: zero per-frame allocation. The
            // handle is built once per resizable column on every table frame
            // (see `Table::render`'s header loop), so a `format!` id here was
            // a per-frame string allocation per column.
            id: ElementId::named_usize("column-resize-handle", column_index),
            column_index,
            columns,
            default_width,
        }
    }
}

impl<C> IntoElement for ColumnResizeHandle<C>
where
    C: Column + 'static,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<C> Element for ColumnResizeHandle<C>
where
    C: Column + 'static,
{
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let border_color = cx.global::<Theme>().border_color;

        let mut element = div()
            .id(self.id.clone())
            .w(px(COLUMN_RESIZE_HANDLE_WIDTH))
            .h(px(TABLE_HEADER_HEIGHT))
            .flex_shrink_0()
            .cursor_col_resize()
            .ml(px(-COLUMN_RESIZE_HANDLE_WIDTH / 2.0))
            .mr(px(-COLUMN_RESIZE_HANDLE_WIDTH / 2.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .w(px(1.0))
                    .h_full()
                    .bg(border_color)
                    .invisible()
                    .group_hover(SharedString::from(TABLE_HEADER_GROUP), |this| {
                        this.visible()
                    }),
            )
            .into_any_element();

        let layout_id = element.request_layout(window, cx);
        (layout_id, element)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);

        let columns_entity = self.columns.clone();
        let column_index = self.column_index;
        let default_width = self.default_width;

        window.with_optional_element_state(
            id,
            move |state: Option<Option<Rc<RefCell<ResizeState>>>>, cx| {
                let state = state
                    .flatten()
                    .unwrap_or_else(|| Rc::new(RefCell::new(ResizeState::default())));

                // start drag on mouse down
                let state_down = state.clone();
                let columns_down = columns_entity.clone();
                cx.on_mouse_event(move |ev: &MouseDownEvent, _, window, cx| {
                    if ev.button != MouseButton::Left {
                        return;
                    }

                    if !bounds.contains(&ev.position) {
                        return;
                    }

                    window.prevent_default();
                    cx.stop_propagation();

                    if ev.click_count == 2 {
                        // reset to default on dbl click
                        columns_down.update(cx, |columns, cx| {
                            // should be cheap
                            let mut new_columns = (**columns).clone();
                            if let Some((_, width)) = new_columns.get_index_mut(column_index) {
                                *width = default_width;
                            }
                            *columns = Arc::new(new_columns);
                            cx.notify();
                        });
                        window.refresh();
                        return;
                    }

                    let current_width = columns_down
                        .read(cx)
                        .get_index(column_index)
                        .map(|(_, w)| *w)
                        .unwrap_or(DEFAULT_COLUMN_WIDTH);

                    let mut state = state_down.borrow_mut();
                    state.is_dragging = true;
                    state.start_x = ev.position.x;
                    state.start_width = current_width;
                });

                // change width on drag
                let state_move = state.clone();
                let columns_move = columns_entity.clone();
                cx.on_mouse_event(move |ev: &MouseMoveEvent, _, _window, cx| {
                    let state_ref = state_move.borrow();
                    if !state_ref.is_dragging {
                        return;
                    }

                    let delta_x: f32 = (ev.position.x - state_ref.start_x).into();
                    let new_width = (state_ref.start_width + delta_x).max(COLUMN_MIN_WIDTH);

                    drop(state_ref);

                    columns_move.update(cx, |columns, cx| {
                        // should be cheap
                        let mut new_columns = (**columns).clone();
                        if let Some((_, width)) = new_columns.get_index_mut(column_index) {
                            *width = new_width;
                        }
                        *columns = Arc::new(new_columns);
                        cx.notify();
                    });

                    // the notify above already schedules the frame; a global
                    // refresh here would bypass every view cache per mouse move
                });

                // mouse up, end the drag
                let state_up = state.clone();
                cx.on_mouse_event(move |ev: &MouseUpEvent, _, _, _| {
                    if ev.button != MouseButton::Left {
                        return;
                    }

                    let mut state = state_up.borrow_mut();
                    state.is_dragging = false;
                });

                ((), Some(state))
            },
        );
    }
}

pub fn column_resize_handle<C>(
    column_index: usize,
    columns: Entity<Arc<IndexMap<C, f32, FxBuildHasher>>>,
    default_width: f32,
) -> ColumnResizeHandle<C>
where
    C: Column + 'static,
{
    ColumnResizeHandle::new(column_index, columns, default_width)
}
