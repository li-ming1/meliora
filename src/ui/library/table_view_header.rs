use gpui::{prelude::FluentBuilder, *};

use crate::ui::{
    components::{
        icons::{GRID, GRID_INACTIVE, LIST, LIST_INACTIVE},
        nav_button::{NavButton, nav_button},
        table::table_data::TableData,
        table::{Table, TableViewMode},
        tooltip::build_tooltip,
    },
    library::view_header::view_header,
    theme::Theme,
};

use cntp_i18n::tr;

pub struct TableViewHeader<T, C>
where
    T: TableData<C> + 'static,
    C: crate::ui::components::table::table_data::Column + 'static,
{
    table: Entity<Table<T, C>>,
}

impl<T, C> TableViewHeader<T, C>
where
    T: TableData<C> + 'static,
    C: crate::ui::components::table::table_data::Column + 'static,
{
    pub fn new(cx: &mut App, table: Entity<Table<T, C>>) -> Entity<Self> {
        cx.new(|_| Self { table })
    }

    /// One list/grid view-mode toggle. Both buttons share this shape and
    /// differ only in element id, target mode, active state (which drives
    /// both the icon and the pressed styling) and tooltip text.
    #[allow(clippy::too_many_arguments)]
    fn view_mode_button(
        theme: &Theme,
        table: &Entity<Table<T, C>>,
        id: &'static str,
        mode: TableViewMode,
        active: bool,
        active_icon: &'static str,
        inactive_icon: &'static str,
        tooltip: impl Into<SharedString>,
    ) -> NavButton {
        let table = table.clone();
        nav_button(id, if active { active_icon } else { inactive_icon })
            .on_click(move |_, _, cx| {
                table.update(cx, |t, cx| {
                    t.set_view_mode(mode, cx);
                });
            })
            .when(active, |this| {
                this.bg(theme.nav_button_pressed)
                    .border_color(theme.nav_button_pressed_border)
            })
            .tooltip(build_tooltip(tooltip))
    }
}

impl<T, C> Render for TableViewHeader<T, C>
where
    T: TableData<C> + 'static,
    C: crate::ui::components::table::table_data::Column + 'static,
{
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        let table_ref = self.table.clone();
        let right = if T::supports_grid_view() {
            let view_mode = table_ref.read(cx).get_view_mode(cx);
            let is_grid = view_mode == TableViewMode::Grid;

            Some(
                div()
                    .flex()
                    .gap_1()
                    .child(Self::view_mode_button(
                        theme,
                        &table_ref,
                        "list_toggle",
                        TableViewMode::List,
                        !is_grid,
                        LIST,
                        LIST_INACTIVE,
                        tr!("LIST_VIEW", "List View"),
                    ))
                    .child(Self::view_mode_button(
                        theme,
                        &table_ref,
                        "grid_toggle",
                        TableViewMode::Grid,
                        is_grid,
                        GRID,
                        GRID_INACTIVE,
                        tr!("GRID_VIEW", "Grid View"),
                    )),
            )
        } else {
            None
        };

        // The view-mode toggles ride on the shared page header, keeping every
        // top-level view's header visually identical.
        view_header(Table::<T, C>::get_table_name()).when_some(right, |h, right| h.right(right))
    }
}
