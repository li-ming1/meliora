use gpui::{prelude::FluentBuilder, *};

use crate::ui::{
    components::table::{
        OnSelectHandler, Table,
        table_data::{Column, TableData, capped_table_width},
    },
    library::table_view_header::TableViewHeader,
    models::Models,
};

/// 三个顶层库表格（专辑 / 艺术家 / 曲目）共享的骨架。持有表格实体及其视图模式头部，
/// 恢复初始滚动偏移，接上扫描完成的刷新，并渲染标准的全宽/最大宽度页面列。
/// 逐字抽取自 AlbumView/ArtistView（两者的 render 原本逐字节相同）；每个视图只保留
/// 自己的行打开处理器与右键菜单上下文——曲目表还额外保留它自己的内层包装 div
/// （该视图保留自己的 `Render` 实现，好让每个视图的元素树与之前完全一致）。
pub struct TableShell<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    table: Entity<Table<T, C>>,
    header: Entity<TableViewHeader<T, C>>,
}

impl<T, C> TableShell<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    pub fn new(
        cx: &mut App,
        on_open_row: OnSelectHandler<T, C>,
        context_menu_context: T::ContextMenuContext,
        initial_scroll_offset: Option<f32>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let state = cx.global::<Models>().scan_state.clone();

            let table_settings = cx.global::<Models>().table_settings.clone();
            let initial_settings = table_settings
                .read(cx)
                .get(Table::<T, C>::get_table_name().as_str())
                .cloned();

            let table = Table::new(
                cx,
                Some(on_open_row),
                context_menu_context,
                initial_scroll_offset,
                initial_settings.as_ref(),
            );

            super::observe_scan_for_table(cx, &state, table.clone());

            Self {
                header: TableViewHeader::new(cx, table.clone()),
                table,
            }
        })
    }

    pub fn get_scroll_offset(&self, cx: &App) -> f32 {
        self.table.read(cx).get_scroll_offset(cx)
    }

    /// 被包装的表格实体。曲目表视图需要它在构造完成后立刻给自己的自引用队列构建器
    /// 递一个弱句柄。
    pub fn table(&self) -> &Entity<Table<T, C>> {
        &self.table
    }

    /// 被包装的视图模式头部实体（曲目视图自己的 render 包装会把它放进那个额外的 div 里）。
    pub fn header(&self) -> &Entity<TableViewHeader<T, C>> {
        &self.header
    }
}

impl<T, C> Render for TableShell<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let full_width = crate::ui::util::full_width_enabled(cx);

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .when(!full_width, capped_table_width)
            .child(self.header.clone())
            .child(self.table.clone())
    }
}
