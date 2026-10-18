use std::rc::Rc;

use gpui::*;

use crate::{
    library::types::{ArtistWithCounts, table::ArtistColumn},
    ui::library::table_shell::TableShell,
};

use super::{NavigationHistory, ViewSwitchMessage};

#[derive(Clone)]
pub struct ArtistView {
    shell: Entity<TableShell<ArtistWithCounts, ArtistColumn>>,
}

impl ArtistView {
    pub(super) fn new(
        cx: &mut App,
        view_switch_model: Entity<NavigationHistory>,
        initial_scroll_offset: Option<f32>,
    ) -> Entity<Self> {
        let handler_model = view_switch_model.clone();
        let handler = Rc::new(move |cx: &mut App, id: &i64| {
            handler_model.update(cx, |_, cx| cx.emit(ViewSwitchMessage::Artist(*id)))
        });

        let shell: Entity<TableShell<ArtistWithCounts, ArtistColumn>> =
            TableShell::new(cx, handler, (), initial_scroll_offset);

        cx.new(|_| ArtistView { shell })
    }

    pub fn get_scroll_offset(&self, cx: &App) -> f32 {
        self.shell.read(cx).get_scroll_offset(cx)
    }
}

impl Render for ArtistView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // 子视图实体：借用的缘由见 album_view。
        self.shell.clone().into_any_element()
    }
}
