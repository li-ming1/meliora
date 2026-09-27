use gpui::{
    AnyElement, App, Bounds, Div, Element, ElementId, FontWeight, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, ParentElement, Pixels,
    RenderOnce, SharedString, Stateful, StatefulInteractiveElement, StyleRefinement, Styled,
    Window, deferred, div, prelude::FluentBuilder, px,
};

use crate::{
    settings::storage::DEFAULT_SIDEBAR_WIDTH,
    ui::{components::icons::icon, design::ICON_MD, theme::Theme},
};

/// A `T` that optionally carries interactive state: the plain variant until
/// something (e.g. `Sidebar::id`) needs a stateful element, `Stateful` after.
pub enum MaybeStateful<T> {
    Stateful(Stateful<T>),
    NotStateful(T),
}

impl<T> Styled for MaybeStateful<T>
where
    T: Styled,
{
    fn style(&mut self) -> &mut StyleRefinement {
        match self {
            MaybeStateful::Stateful(stateful) => stateful.style(),
            MaybeStateful::NotStateful(not_stateful) => not_stateful.style(),
        }
    }
}

impl<T> Element for MaybeStateful<T>
where
    T: Element,
{
    type RequestLayoutState = T::RequestLayoutState;
    type PrepaintState = T::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        match self {
            MaybeStateful::Stateful(stateful) => stateful.id(),
            MaybeStateful::NotStateful(not_stateful) => not_stateful.id(),
        }
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        match self {
            MaybeStateful::Stateful(stateful) => stateful.source_location(),
            MaybeStateful::NotStateful(not_stateful) => not_stateful.source_location(),
        }
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        match self {
            MaybeStateful::Stateful(stateful) => {
                stateful.request_layout(id, inspector_id, window, cx)
            }
            MaybeStateful::NotStateful(not_stateful) => {
                not_stateful.request_layout(id, inspector_id, window, cx)
            }
        }
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> T::PrepaintState {
        match self {
            MaybeStateful::Stateful(stateful) => {
                stateful.prepaint(id, inspector_id, bounds, state, window, cx)
            }
            MaybeStateful::NotStateful(not_stateful) => {
                not_stateful.prepaint(id, inspector_id, bounds, state, window, cx)
            }
        }
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        match self {
            MaybeStateful::Stateful(stateful) => stateful.paint(
                id,
                inspector_id,
                bounds,
                request_layout,
                prepaint,
                window,
                cx,
            ),
            MaybeStateful::NotStateful(not_stateful) => not_stateful.paint(
                id,
                inspector_id,
                bounds,
                request_layout,
                prepaint,
                window,
                cx,
            ),
        }
    }
}

impl<T> IntoElement for MaybeStateful<T>
where
    T: Element,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<T> ParentElement for MaybeStateful<T>
where
    T: ParentElement,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        match self {
            MaybeStateful::Stateful(stateful) => stateful.extend(elements),
            MaybeStateful::NotStateful(not_stateful) => not_stateful.extend(elements),
        }
    }
}

#[derive(IntoElement)]
pub struct Sidebar {
    div: MaybeStateful<Div>,
    width: Option<Pixels>,
}

impl Sidebar {
    pub fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.div = MaybeStateful::Stateful(match self.div {
            MaybeStateful::NotStateful(div) => div.id(id),
            // gpui's own `.id()` is last-writer-wins (it assigns the element
            // id), so re-id-ing an already stateful sidebar replaces the id
            // instead of silently dropping it.
            MaybeStateful::Stateful(mut div) => {
                div.interactivity().element_id = Some(id.into());
                div
            }
        });

        self
    }

    pub fn width(mut self, width: Pixels) -> Self {
        self.width = Some(width);
        self
    }
}

impl Styled for Sidebar {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl ParentElement for Sidebar {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.div.extend(elements);
    }
}

impl RenderOnce for Sidebar {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let width = self.width.unwrap_or(DEFAULT_SIDEBAR_WIDTH);
        self.div.w(width).flex().gap(px(2.0)).flex_col()
    }
}

pub fn sidebar() -> Sidebar {
    Sidebar {
        div: MaybeStateful::NotStateful(div()),
        width: None,
    }
}

#[derive(IntoElement)]
pub struct SidebarItem {
    parent_div: Stateful<Div>,
    children_div: Div,
    icon: Option<&'static str>,
    active: bool,
    collapsed: bool,
    label: Option<SharedString>,
    state_id: ElementId,
}

impl SidebarItem {
    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn active(mut self) -> Self {
        self.active = true;
        self
    }

    pub fn collapsed(mut self) -> Self {
        self.collapsed = true;
        self
    }

    pub fn collapsed_label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
}

impl Styled for SidebarItem {
    fn style(&mut self) -> &mut StyleRefinement {
        self.parent_div.style()
    }
}
impl ParentElement for SidebarItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.children_div.extend(elements);
    }
}

impl StatefulInteractiveElement for SidebarItem {}

impl InteractiveElement for SidebarItem {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.parent_div.interactivity()
    }
}

impl RenderOnce for SidebarItem {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.state_id.clone(), cx, |_, _| false);
        let theme = cx.global::<Theme>();

        let item = self
            .parent_div
            .flex()
            .overflow_x_hidden()
            .when(!self.collapsed, |this| this.w_full().px(px(9.0)))
            .when(self.collapsed, |this| {
                this.w(px(36.0))
                    .h(px(34.0))
                    .items_center()
                    .justify_center()
                    .flex_shrink_0()
            })
            .bg(theme.background_primary)
            .text_sm()
            .border_1()
            // you may ask: what is even the point of setting the border color to this?
            // well, for some as of yet unknown reason, leaving this unset OR leaving this set
            // to transparent_black() results in the hover effects not applying properly.
            // why? i don't know, it makes no god damn sense
            //
            // load bearing color
            .border_color(theme.background_primary)
            .when(self.active, |div| {
                div.bg(theme.nav_button_pressed)
                    .border_color(theme.nav_button_pressed_border)
            })
            .rounded(px(theme.radius_sm))
            .py(px(8.0))
            .line_height(px(18.0))
            .gap(px(6.0))
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|this| {
                this.bg(theme.nav_button_hover)
                    .border_color(theme.nav_button_hover_border)
            })
            .active(|this| {
                this.bg(theme.nav_button_active)
                    .border_color(theme.nav_button_active_border)
            })
            // Placeholder reserves the same box the icon would occupy.
            .when_none(&self.icon, |this| {
                this.child(div().size(ICON_MD).flex_shrink_0().min_w(ICON_MD))
            })
            .when_some(self.icon, |this, used_icon| {
                this.child(
                    icon(used_icon)
                        .size(ICON_MD)
                        .flex_shrink_0()
                        .min_w(px(18.0)),
                )
            })
            .when(!self.collapsed, |this| {
                this.child(
                    self.children_div
                        .flex_shrink(1.0)
                        .flex_col()
                        .flex()
                        .text_ellipsis()
                        .overflow_x_hidden()
                        .w_full(),
                )
            });

        if self.collapsed
            && let Some(label_text) = self.label
        {
            let is_hovered = *state.read(cx);

            div()
                .relative()
                .id(self.state_id)
                .child(item)
                .on_hover({
                    let state = state.clone();
                    move |hover, _, cx| {
                        state.write(cx, *hover);
                    }
                })
                .when(is_hovered, |this| {
                    this.child(deferred(
                        div()
                            .absolute()
                            .left_full()
                            .top_0()
                            .ml(px(4.0))
                            .bg(theme.elevated_background)
                            .border_1()
                            .border_color(theme.elevated_border_color)
                            .rounded(px(theme.radius_sm))
                            .shadow_sm()
                            .px(px(12.0))
                            .pt(px(5.0))
                            .pb(px(6.0))
                            .text_sm()
                            .text_color(theme.text)
                            .whitespace_nowrap()
                            .child(label_text),
                    ))
                })
                .into_any_element()
        } else {
            item.into_any_element()
        }
    }
}

pub fn sidebar_item(id: impl Into<ElementId>) -> SidebarItem {
    let element_id = id.into();
    let state_id = (element_id.clone(), "id").into();
    SidebarItem {
        parent_div: div().id(element_id),
        children_div: div(),
        icon: None,
        active: false,
        collapsed: false,
        label: None,
        state_id,
    }
}

#[derive(IntoElement)]
pub struct SidebarSeparator {}

impl RenderOnce for SidebarSeparator {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        div()
            .w_full()
            .my(px(4.0))
            .border_b_1()
            .border_color(theme.border_color)
    }
}

pub fn sidebar_separator() -> SidebarSeparator {
    SidebarSeparator {}
}
