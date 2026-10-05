use std::rc::Rc;

use gpui::*;

use crate::{
    library::types::{Album, table::AlbumColumn},
    ui::library::{context_menus::AlbumContextMenuContext, table_shell::TableShell},
};

use super::ViewSwitchMessage;

#[derive(Clone)]
pub struct AlbumView {
    shell: Entity<TableShell<Album, AlbumColumn>>,
}

impl AlbumView {
    pub(super) fn new(
        cx: &mut App,
        view_switch_model: Entity<super::NavigationHistory>,
        initial_scroll_offset: Option<f32>,
    ) -> Entity<Self> {
        let handler_model = view_switch_model.clone();
        let handler = Rc::new(move |cx: &mut App, id: &(u32, String)| {
            handler_model.update(cx, |_, cx| {
                cx.emit(ViewSwitchMessage::Release(id.0 as i64, None))
            })
        });

        let shell: Entity<TableShell<Album, AlbumColumn>> = TableShell::new(
            cx,
            handler,
            AlbumContextMenuContext::default(),
            initial_scroll_offset,
        );

        cx.new(|_| AlbumView { shell })
    }

    pub fn get_scroll_offset(&self, cx: &App) -> f32 {
        self.shell.read(cx).get_scroll_offset(cx)
    }
}

impl Render for AlbumView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Render the shell as a child view entity: calling shell.render inside
        // an update closure would return an element borrowing window/cx.
        self.shell.clone().into_any_element()
    }
}
