use gpui::{prelude::FluentBuilder, *};

use crate::ui::{
    components::table::{
        OnSelectHandler, Table,
        table_data::{Column, TABLE_MAX_WIDTH, TableData},
    },
    library::table_view_header::TableViewHeader,
    models::Models,
};

/// Shared scaffold of the three top-level library tables (albums / artists /
/// tracks). Owns the table entity and its view-mode header, restores the
/// initial scroll offset, wires the scan-completion refresh, and renders the
/// standard full-width/max-width page column. Extracted verbatim from
/// AlbumView/ArtistView (whose renders were byte-identical); each view keeps
/// only its own row-open handler and context-menu context — and, for the
/// track table, its extra inner wrapper div (that view keeps its own
/// `Render` impl so every view's element tree stays exactly as before).
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

    /// The wrapped table entity. The track view needs it to hand its
    /// self-referential queue builder a weak handle right after construction.
    pub fn table(&self) -> &Entity<Table<T, C>> {
        &self.table
    }

    /// The wrapped view-mode header entity (the track view's own render
    /// wrapper places it inside its extra div).
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
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .when(!full_width, |this: Div| this.max_w(px(TABLE_MAX_WIDTH)))
            .child(self.header.clone())
            .child(self.table.clone())
    }
}
