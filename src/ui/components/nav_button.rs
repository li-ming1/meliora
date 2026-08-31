use gpui::{
    Div, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, StyleRefinement, Styled, div, px, rgba,
};

use crate::ui::{
    components::{icons::icon, transition::HoverTransition},
    theme::Theme,
};

#[derive(IntoElement)]
pub struct NavButton {
    id: ElementId,
    div: Div,
    icon: &'static str,
    enabled: bool,
}

impl StatefulInteractiveElement for NavButton {}

impl InteractiveElement for NavButton {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl Styled for NavButton {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl NavButton {
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.enabled = !disabled;
        self
    }
}

impl RenderOnce for NavButton {
    fn render(self, _: &mut gpui::Window, cx: &mut gpui::App) -> impl gpui::IntoElement {
        let theme = cx.global::<Theme>();

        let div = self
            .div
            .id(self.id.clone())
            .flex()
            .justify_center()
            .items_center()
            .rounded(px(theme.radius_sm))
            .text_sm()
            .border_1()
            .border_color(rgba(0x00000000))
            .cursor_pointer()
            .active(|style: gpui::StyleRefinement| {
                style
                    .bg(theme.nav_button_active)
                    .border_color(theme.nav_button_active_border)
            })
            .child(icon(self.icon).size(px(16.0)));

        if !self.enabled {
            return div.opacity(0.35).into_any_element();
        }

        HoverTransition::new(
            self.id,
            div,
            rgba(0x00000000),
            theme.nav_button_hover,
        )
        .into_any_element()
    }
}

pub fn nav_button(id: impl Into<ElementId>, icon: &'static str) -> NavButton {
    NavButton {
        id: id.into(),
        div: div().size(px(28.0)),
        icon,
        enabled: true,
    }
}
