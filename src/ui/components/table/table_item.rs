use std::sync::Arc;

use gpui::{prelude::FluentBuilder, *};
use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;

use super::{
    OnSelectHandler,
    table_data::{
        Column, GridContext, MIN_COLUMN_WIDTH, TABLE_IMAGE_COLUMN_WIDTH, TableData, TableDragData,
        apply_row_selection, with_drag_handlers, wrap_with_context_menu,
    },
};
use crate::ui::theme::Theme;

/// 每个行单元格的固定高度；与表头高度一致。也是外壳在缓存本行视图时传进来的外部高度
/// （缓存路径从不测量内容）。
pub(crate) const ROW_HEIGHT: f32 = 36.0;

#[derive(Clone)]
pub struct TableItem<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    context_menu_context: T::ContextMenuContext,
    data: Option<Vec<Option<SharedString>>>,
    columns: Arc<IndexMap<C, f32, FxBuildHasher>>,
    on_select: Option<OnSelectHandler<T, C>>,
    row: Option<Arc<T>>,
    id: Option<ElementId>,
    image_path: Option<SharedString>,
    is_available: bool,
    /// 预先构建一次；`on_drag` 每帧按值取走载荷，所以 render 里克隆这份而不是重跑
    /// `get_drag_data`。
    drag_data: Option<TableDragData>,
}

impl<T, C> TableItem<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    pub fn new(
        cx: &mut App,
        id: T::Identifier,
        columns: &Entity<Arc<IndexMap<C, f32, FxBuildHasher>>>,
        on_select: Option<OnSelectHandler<T, C>>,
        context_menu_context: T::ContextMenuContext,
    ) -> Entity<Self> {
        let row = T::get_row(cx, id).ok().flatten();

        let id = row.as_ref().map(|row| row.get_element_id().into());

        let columns_read = columns.read(cx).clone();

        let data = row
            .clone()
            .map(|row| Self::snapshot_cells(&row, &columns_read, cx));

        let image_path = row.as_ref().and_then(|row| row.get_image_path());
        let is_available = row.as_ref().is_some_and(|row| row.is_available(cx));
        let drag_data = row.as_ref().and_then(|row| row.get_drag_data());
        cx.new(|cx| {
            cx.observe(columns, |this: &mut TableItem<T, C>, m, cx| {
                // 列宽通知（拖拽）远多于结构变化；get_column 取值只依赖列
                // 身份，仅结构变化（hide/show/reorder）才重快照单元格，
                // 镜像 Table 侧观察者的 structural 判定
                let new = m.read(cx).clone();
                let structural = new.len() != this.columns.len()
                    || new.keys().zip(this.columns.keys()).any(|(a, b)| a != b);

                this.columns = new;

                if structural {
                    this.data = this
                        .row
                        .clone()
                        .map(|row| Self::snapshot_cells(&row, &this.columns, cx));
                }

                cx.notify();
            })
            .detach();

            Self {
                context_menu_context,
                data,
                image_path,
                columns: columns_read,
                on_select,
                id,
                row,
                is_available,
                drag_data,
            }
        })
    }

    /// 按列顺序快照 `row` 在可见列上的单元格值。列集变化时重算（见 `new` 里的观察者）。
    fn snapshot_cells(
        row: &Arc<T>,
        columns: &IndexMap<C, f32, FxBuildHasher>,
        cx: &mut App,
    ) -> Vec<Option<SharedString>> {
        columns
            .keys()
            .map(|column| row.get_column(cx, *column))
            .collect()
    }
}

impl<T, C> Render for TableItem<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::TableItem);
        let row_data = self.row.clone();
        let is_available = self.is_available;
        let context_menu = self.row.as_ref().and_then(|row| {
            // 共享行变体：菜单构建器只持有一份 `Arc` 引用计数，只在菜单打开时才深拷贝整行，
            // 不是每帧
            T::get_context_menu_shared(
                row,
                window,
                cx,
                &self.context_menu_context,
                GridContext::Table,
                is_available,
            )
        });
        let theme = cx.global::<Theme>();
        let drag_data = if is_available {
            self.drag_data.clone()
        } else {
            None
        };

        let row = div()
            .w_full()
            .flex()
            .id(self.id.clone().unwrap_or("bad".into()));

        let mut row =
            apply_row_selection(row, self.on_select.clone(), row_data, is_available, theme);

        row = with_drag_handlers(row, drag_data);

        if T::has_images() {
            row = row.child(
                div()
                    .w(px(TABLE_IMAGE_COLUMN_WIDTH))
                    .h(px(ROW_HEIGHT))
                    .text_sm()
                    .pl(px(11.0))
                    .flex_shrink_0()
                    .text_ellipsis()
                    //.border_r_1()
                    .border_b_1()
                    .border_color(theme.border_color)
                    .flex()
                    .child(
                        div()
                            .m_auto()
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded(px(theme.radius_sm))
                            .bg(theme.album_art_background)
                            .when_some(self.image_path.clone(), |div, image| {
                                div.child(
                                    img(image)
                                        .w(px(22.0))
                                        .h(px(22.0))
                                        .rounded(px(theme.radius_sm)),
                                )
                            }),
                    ),
            );
        }

        if let Some(data) = self.data.as_ref() {
            for (i, column_data) in data.iter().enumerate() {
                // 列可能在数据快照与本次渲染之间变少；跳过过期列，
                // 而不是让这一帧 panic
                let Some((column, width)) = self.columns.get_index(i) else {
                    continue;
                };
                row = row.child(
                    div()
                        // 流动列，与表头的 grow 权重一致
                        .flex_grow(width.max(1.0))
                        .flex_basis(px(0.0))
                        .min_w(px(MIN_COLUMN_WIDTH))
                        .h(px(ROW_HEIGHT))
                        .px(px(12.0))
                        .py(px(6.0))
                        .when(!T::has_images() && i == 0, |div| div.pl(px(17.0)))
                        .when(T::column_monospace(*column), |div| {
                            div.font_family("Roboto Mono")
                        })
                        .text_sm()
                        .overflow_hidden()
                        .text_ellipsis()
                        .border_b_1()
                        .border_color(theme.border_color)
                        .when_some(column_data.clone(), |div, string| div.child(string)),
                );
            }
        }

        // 表格行保持自身宽度：context 元素不带尺寸，overlay 包装是 w_full
        wrap_with_context_menu(
            self.id.clone().unwrap_or("bad-context".into()),
            row,
            context_menu,
            false,
        )
    }
}
