use std::sync::Arc;

use gpui::{prelude::FluentBuilder, *};

use super::{
    OnSelectHandler,
    table_data::{Column, GridContext, TableData, TableDragData, with_drag_handlers},
};
use crate::ui::{
    components::{
        context::context,
        managed_image::{ManagedImageKey, managed_image},
    },
    theme::Theme,
};

#[derive(Clone)]
pub struct GridItem<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    context_menu_context: T::ContextMenuContext,
    grid_context: GridContext,
    row: Arc<T>,
    id: ElementId,
    image_key: Option<ManagedImageKey>,
    primary_text: SharedString,
    secondary_text: Option<SharedString>,
    on_select: Option<OnSelectHandler<T, C>>,
    is_available: bool,
    /// Prebuilt once; `on_drag` takes the payload by value every frame, so
    /// render clones this instead of re-running `get_drag_data`.
    drag_data: Option<TableDragData>,
}

impl<T, C> GridItem<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    pub fn new(
        cx: &mut App,
        id: T::Identifier,
        on_select: Option<OnSelectHandler<T, C>>,
        context_menu_context: T::ContextMenuContext,
        context: GridContext,
    ) -> Option<Entity<Self>> {
        let row = T::get_row(cx, id.clone()).ok().flatten()?;

        let element_id = row.get_element_id().into();
        let image_key = row.get_full_image_key();
        let is_available = row.is_available(cx);
        let drag_data = row.get_drag_data();
        let grid_content = row.get_grid_content_for(cx, context);
        let (primary_text, secondary_text) = grid_content.unwrap_or(("".into(), None));

        Some(cx.new(|_| Self {
            context_menu_context,
            grid_context: context,
            row,
            id: element_id,
            image_key,
            primary_text,
            secondary_text,
            on_select,
            is_available,
            drag_data,
        }))
    }
}

impl<T, C> Render for GridItem<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let is_available = self.is_available;
        // shared-row variant: the menu builder holds an `Arc` refcount and
        // deep-clones the row only when the menu opens, not every frame
        let context_menu = T::get_context_menu_shared(
            &self.row,
            window,
            cx,
            &self.context_menu_context,
            self.grid_context,
            is_available,
        );
        let theme = cx.global::<Theme>();

        let drag_data = if is_available {
            self.drag_data.clone()
        } else {
            None
        };

        let mut container = div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .p(px(8.0))
            .rounded_lg()
            .id(self.id.clone())
            .when_some(self.on_select.clone(), {
                let row_data = self.row.clone();
                move |div, on_select| {
                    if is_available {
                        div.on_click(move |_, _, cx| {
                            let id = row_data.get_table_id();
                            on_select(cx, &id)
                        })
                        .cursor_pointer()
                        .hover(|this| this.bg(theme.nav_button_hover))
                        .active(|this| this.bg(theme.nav_button_active))
                    } else {
                        div.cursor_default().opacity(0.5)
                    }
                }
            })
            .when(self.on_select.is_none() && !is_available, |this| {
                this.opacity(0.5)
            })
            .on_aux_click({
                let row_data = self.row.clone();
                move |ev, window, cx| {
                    if ev.is_middle_click() {
                        row_data.handle_middle_mouse(window, cx, GridContext::Table);
                    }
                }
            });

        container = with_drag_handlers(container, drag_data);

        let mut img_container = div()
            .w_full()
            .flex_1()
            .rounded(px(theme.radius_md))
            .bg(theme.album_art_background)
            .overflow_hidden();

        if let Some(image) = self.image_key.clone() {
            img_container = img_container.child(
                managed_image((self.id.clone(), "grid_image"), image)
                    .thumb_max(256)
                    .w_full()
                    .h_full()
                    .aspect_square()
                    .rounded(px(theme.radius_md))
                    .object_fit(ObjectFit::Fill),
            );
        }

        let content = container
            .child(img_container)
            .child(
                div()
                    .mt(px(8.0))
                    .w_full()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_ellipsis()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(self.primary_text.clone()),
            )
            .when_some(self.secondary_text.clone(), |this, secondary| {
                this.child(
                    gpui::div()
                        .w_full()
                        .text_xs()
                        .text_color(theme.text_secondary)
                        .text_ellipsis()
                        .overflow_hidden()
                        .child(secondary),
                )
            });

        if let Some((menu_builder, overlay)) = context_menu {
            let ctx = context(self.id.clone())
                .w_full()
                .h_full()
                .with(content)
                // menu tree is built only when the menu opens
                .menu_on_open(move |window, cx| {
                    div()
                        .bg(cx.global::<Theme>().elevated_background)
                        .child(menu_builder(window, cx))
                        .into_any_element()
                });
            match overlay {
                Some(overlay) => div()
                    .size_full()
                    .child(ctx)
                    .child(overlay)
                    .into_any_element(),
                None => ctx.into_any_element(),
            }
        } else {
            content.into_any_element()
        }
    }
}
