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
        // 把 shell 作为子视图实体渲染：在 update 闭包里调用 shell.render 会返回一个
        // 借用 window/cx 的元素。
        self.shell.clone().into_any_element()
    }
}
