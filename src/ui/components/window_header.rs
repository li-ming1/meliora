use crate::ui::design::{ICON_SM, SPACE_SM};
use gpui::{prelude::FluentBuilder, *};
use smallvec::SmallVec;

use crate::ui::{
    components::icons::{CROSS, MAXIMIZE, MINIMIZE, MINUS, icon},
    constants::{TITLEBAR_LEFT_PAD_BOTTOM, TITLEBAR_LEFT_PAD_TOP, TITLEBAR_LEFT_PAD_X},
    theme::Theme,
};

/// Height of the titlebar and of each window button (the buttons fill it).
const TITLEBAR_HEIGHT: Pixels = px(37.0);

/// Width of a titlebar window button (close / minimize / maximize).
const WINDOW_BUTTON_WIDTH: Pixels = px(36.0);

/// Spacer clearing the macOS traffic lights at the titlebar's left edge.
const MACOS_TRAFFIC_LIGHT_SPACER: Pixels = px(72.0);

#[derive(IntoElement)]
pub struct WindowHeader {
    left: SmallVec<[AnyElement; 2]>,
    div: Div,
    main_window: bool,
}

impl Default for WindowHeader {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowHeader {
    pub fn new() -> Self {
        Self {
            left: SmallVec::new(),
            div: div(),
            main_window: false,
        }
    }

    pub fn left(mut self, element: impl IntoElement) -> Self {
        self.left.push(element.into_any_element());
        self
    }

    pub fn main_window(mut self, main_window: bool) -> Self {
        self.main_window = main_window;
        self
    }
}

impl Styled for WindowHeader {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl RenderOnce for WindowHeader {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let decorations = window.window_decorations();
        let theme = cx.global::<Theme>();

        let left_container = div()
            .pl(TITLEBAR_LEFT_PAD_X)
            .pb(TITLEBAR_LEFT_PAD_BOTTOM)
            .pt(TITLEBAR_LEFT_PAD_TOP)
            .flex()
            .items_center()
            .gap(SPACE_SM)
            .children(self.left);

        self.div
            .flex()
            .items_center()
            .w_full()
            .text_sm()
            .min_h(TITLEBAR_HEIGHT)
            .max_h(TITLEBAR_HEIGHT)
            .bg(theme.background_secondary)
            .border_b_1()
            .id("titlebar")
            .border_color(theme.border_color)
            .window_control_area(WindowControlArea::Drag)
            .when(cfg!(not(target_os = "windows")), |this| {
                this.on_mouse_down(MouseButton::Left, move |ev, window, _| {
                    if ev.click_count != 2 {
                        window.start_window_move();
                    }
                })
                .on_click(|ev, window, _| {
                    if ev.click_count() == 2 {
                        window.zoom_window();
                    }
                })
            })
            .map(|div| match decorations {
                Decorations::Server => div,
                Decorations::Client { tiling } => div
                    .when(!(tiling.top || tiling.left), |div| {
                        div.rounded_tl(px(theme.radius_md))
                    })
                    .when(!(tiling.top || tiling.right), |div| {
                        div.rounded_tr(px(theme.radius_md))
                    }),
            })
            .when(cfg!(target_os = "macos"), |this| {
                this.child(div().w(MACOS_TRAFFIC_LIGHT_SPACER))
            })
            .child(left_container)
            .when(cfg!(not(target_os = "macos")), |this| {
                this.child(
                    div()
                        .flex()
                        .ml_auto()
                        .items_center()
                        .child(WindowButton::Minimize)
                        .child(WindowButton::Maximize)
                        .child(WindowButton::Close(self.main_window)),
                )
            })
    }
}

pub fn header() -> WindowHeader {
    WindowHeader::new()
}

#[derive(PartialEq, Clone, Copy, IntoElement)]
pub enum WindowButton {
    Close(bool),
    Minimize,
    Maximize,
}

impl RenderOnce for WindowButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let is_close = matches!(self, WindowButton::Close(_));

        let (bg, hover, active) = if is_close {
            (
                theme.close_button,
                theme.close_button_hover,
                theme.close_button_active,
            )
        } else {
            (
                theme.window_button,
                theme.window_button_hover,
                theme.window_button_active,
            )
        };

        let (id, control_area, glyph) = match self {
            WindowButton::Close(_) => ("close", WindowControlArea::Close, CROSS),
            WindowButton::Minimize => ("minimize", WindowControlArea::Min, MINUS),
            WindowButton::Maximize => {
                let glyph = if window.is_maximized() {
                    MINIMIZE
                } else {
                    MAXIMIZE
                };
                ("maximize", WindowControlArea::Max, glyph)
            }
        };

        div()
            .flex()
            .w(WINDOW_BUTTON_WIDTH)
            .h(TITLEBAR_HEIGHT)
            .items_center()
            .justify_center()
            .cursor_pointer()
            .id(id)
            .bg(bg)
            .hover(|this| this.bg(hover))
            .active(|this| this.bg(active))
            .window_control_area(control_area)
            .text_size(px(12.0))
            .occlude()
            .child(icon(glyph).size(ICON_SM))
            .when(is_close, |this| this.rounded_tr(px(theme.radius_md)))
            .on_click(move |_, window, cx| match self {
                WindowButton::Close(false) => window.remove_window(),
                WindowButton::Close(true) => cx.quit(),
                WindowButton::Minimize => {
                    if !cfg!(target_os = "windows") {
                        window.minimize_window()
                    }
                }
                WindowButton::Maximize => {
                    if !cfg!(target_os = "windows") {
                        window.zoom_window()
                    }
                }
            })
    }
}
