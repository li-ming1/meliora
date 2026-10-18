use std::rc::Rc;

use gpui::*;

use crate::ui::theme::Theme;

/// 纯文本 tooltip 换行前的最大宽度。
const TOOLTIP_MAX_WIDTH: f32 = 260.0;

/// 所有 tooltip 容器共用的样式。
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

/// 纯文本 tooltip 视图；由 [`build_tooltip`] 构建。
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

/// 返回一个适合传给 GPUI `.tooltip()` 方法的闭包。
/// tooltip 会在 hover 时自动显示并定位到光标处。
pub fn build_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |_, cx| cx.new(|_| TooltipContent { text: text.clone() }).into()
}

type ComplexTooltipBuildFn = Rc<dyn Fn(&mut Window, &mut App) -> Div + 'static>;

/// 子内容由调用方提供的构建器在每次渲染时生成的 tooltip 视图；由
/// [`build_complex_tooltip`] 构建。
pub struct ComplexTooltipContent {
    build_children: ComplexTooltipBuildFn,
}

impl Render for ComplexTooltipContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        tooltip_container(theme).child((self.build_children)(window, cx))
    }
}

/// 返回一个适合传给 GPUI `.tooltip()` 方法的闭包。与 [`build_tooltip`] 不同，子内容由
/// `children` 在每次 tooltip 渲染时生成，因此可以承载多元素富内容。
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
