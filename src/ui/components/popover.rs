use std::rc::Rc;

use gpui::{
    AnyElement, App, Div, InteractiveElement, IntoElement, ParentElement, Pixels, RenderOnce,
    StatefulInteractiveElement, StyleRefinement, Styled, Window, deferred, div, px, relative,
};
use gpui::{actions, prelude::FluentBuilder};

use crate::ui::theme::Theme;

pub type OnDismissHandler = dyn Fn(&mut Window, &mut App);

actions!(popover, [ClosePopover]);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Placement relative to the parent bounds; for example, `RightTop` sits to the
/// right of the parent and aligns to its top edge.
pub enum PopoverPosition {
    RightTop,
    TopRight,
    #[default]
    BottomCenter,
    BottomRight,
}

#[derive(IntoElement)]
pub struct Popover {
    div: Div,
    position: PopoverPosition,
    edge_offset: Pixels,
    on_dismiss: Option<Rc<OnDismissHandler>>,
}

impl Popover {
    pub fn position(mut self, position: PopoverPosition) -> Self {
        self.position = position;
        self
    }

    pub fn edge_offset(mut self, edge_offset: Pixels) -> Self {
        self.edge_offset = edge_offset;
        self
    }

    pub fn on_dismiss(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_dismiss = Some(Rc::new(handler));
        self
    }
}

impl ParentElement for Popover {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.div.extend(elements);
    }
}

impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl StatefulInteractiveElement for Popover {}

impl InteractiveElement for Popover {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl RenderOnce for Popover {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let content = self
            .div
            .occlude()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.elevated_border_color)
            .rounded(px(theme.radius_md))
            .shadow_md()
            .when_some(self.on_dismiss, |this, on_dismiss| {
                this.on_action(move |_: &ClosePopover, window, cx| {
                    on_dismiss(window, cx);
                })
            });

        deferred(anchor(self.position, self.edge_offset, content))
    }
}

fn anchor(position: PopoverPosition, edge_offset: Pixels, content: Div) -> Div {
    let mut anchor = div().absolute().w(px(0.0)).h(px(0.0));
    let mut content = content.absolute();

    match position {
        PopoverPosition::RightTop => {
            anchor = anchor.right(px(0.0));
            content = content.left(px(0.0)).ml(edge_offset);
        }
        PopoverPosition::TopRight | PopoverPosition::BottomRight => {
            anchor = anchor.right(px(0.0));
            content = content.right(px(0.0));
        }
        PopoverPosition::BottomCenter => {
            anchor = anchor.left(relative(0.5));
            content = content.left(px(0.0)).ml(relative(-0.5));
        }
    }

    match position {
        PopoverPosition::RightTop | PopoverPosition::TopRight => {
            anchor = anchor.top(px(0.0));
            content = content.top(px(0.0));
        }
        PopoverPosition::BottomCenter | PopoverPosition::BottomRight => {
            anchor = anchor.bottom(px(0.0));
            content = content.top(px(0.0)).mt(edge_offset);
        }
    }

    anchor.child(content)
}

pub fn popover() -> Popover {
    Popover {
        div: div().p(px(6.0)),
        position: PopoverPosition::BottomCenter,
        edge_offset: px(0.0),
        on_dismiss: None,
    }
}
