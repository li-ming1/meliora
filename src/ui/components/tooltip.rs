use std::rc::Rc;

use gpui::*;

use crate::ui::theme::Theme;

/// Maximum width of a plain text tooltip before it wraps.
const TOOLTIP_MAX_WIDTH: f32 = 260.0;

/// Shared styling for all tooltip containers.
pub fn tooltip_container(theme: &Theme) -> Div {
    div()
        .text_sm()
        .rounded(px(theme.radius_md))
        .border_1()
        .font_family("Inter")
        .border_color(theme.elevated_border_color)
        .bg(theme.elevated_background)
        .text_color(theme.text_secondary)
        .shadow_sm()
        .px(px(8.0))
        .pt(px(4.0))
        .pb(px(5.0))
}

/// A plain text tooltip view; built by [`build_tooltip`].
pub struct TooltipContent {
    text: SharedString,
}

impl Render for TooltipContent {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        tooltip_container(theme)
            .max_w(px(TOOLTIP_MAX_WIDTH))
            .child(self.text.clone())
    }
}

/// Returns a closure suitable for passing to GPUI's `.tooltip()` method.
/// The tooltip is automatically shown on hover and positioned at the cursor.
pub fn build_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |_, cx| cx.new(|_| TooltipContent { text: text.clone() }).into()
}

type ComplexTooltipBuildFn = Rc<dyn Fn(&mut Window, &mut App) -> Div + 'static>;

/// A tooltip view whose children are produced by a caller-supplied builder
/// each time it renders; built by [`build_complex_tooltip`].
pub struct ComplexTooltipContent {
    build_children: ComplexTooltipBuildFn,
}

impl Render for ComplexTooltipContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        tooltip_container(theme).child((self.build_children)(window, cx))
    }
}

/// Returns a closure suitable for passing to GPUI's `.tooltip()` method.
/// Unlike [`build_tooltip`], the children are produced by `children` each
/// time the tooltip renders, allowing rich multi-element content.
pub fn build_complex_tooltip(
    children: impl Fn(&mut Window, &mut App) -> Div + 'static,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let children: ComplexTooltipBuildFn = Rc::new(children);
    move |_, cx| {
        cx.new(|_| ComplexTooltipContent {
            build_children: children.clone(),
        })
        .into()
    }
}
