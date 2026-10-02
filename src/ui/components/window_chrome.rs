use std::sync::{Arc, LazyLock};

use gpui::{prelude::FluentBuilder, *};

use crate::ui::{constants::APP_SHADOW_SIZE, theme::Theme};

/// Zone in which the cursor announces a resize affordance. Deliberately wider
/// than the resize trigger (`APP_SHADOW_SIZE`, passed to `on_mouse_down`
/// below) so the pointer signals the edge before a drag engages.
const RESIZE_CURSOR_ZONE: Pixels = px(30.0);

/// 窗口根文本统一走等宽数字（tnum）。FontFeatures 本体就是 Arc 包装，全窗口
/// 共享一份，render 每帧免重建 Arc+Vec+String。
static TNUM_FEATURES: LazyLock<FontFeatures> =
    LazyLock::new(|| FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)])));

#[derive(IntoElement)]
pub struct WindowChrome {
    content: AnyElement,
    div: Div,
}

impl WindowChrome {
    pub fn new(content: impl IntoElement) -> Self {
        Self {
            content: content.into_any_element(),
            div: div(),
        }
    }
}

impl Styled for WindowChrome {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl RenderOnce for WindowChrome {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let decorations = window.window_decorations();
        let rounding = px(theme.radius_md);
        let shadow_size = APP_SHADOW_SIZE;
        let border_size = px(1.0);

        window.set_client_inset(shadow_size);

        let mut element = self
            .div
            .id("window-backdrop")
            .bg(transparent_black())
            .flex()
            .map(|div| match decorations {
                Decorations::Server => div,
                Decorations::Client { tiling } => div
                    .child(
                        canvas(
                            |_bounds, window, _| {
                                window.insert_hitbox(
                                    Bounds::new(
                                        point(px(0.0), px(0.0)),
                                        window.window_bounds().get_bounds().size,
                                    ),
                                    HitboxBehavior::Normal,
                                )
                            },
                            move |_bounds, hitbox, window, _| {
                                let mouse = window.mouse_position();
                                let size = window.window_bounds().get_bounds().size;
                                let Some(edge) =
                                    resize_edge(mouse, RESIZE_CURSOR_ZONE, size, tiling)
                                else {
                                    return;
                                };
                                window.set_cursor_style(
                                    match edge {
                                        ResizeEdge::Top | ResizeEdge::Bottom => {
                                            CursorStyle::ResizeUpDown
                                        }
                                        ResizeEdge::Left | ResizeEdge::Right => {
                                            CursorStyle::ResizeLeftRight
                                        }
                                        ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                            CursorStyle::ResizeUpLeftDownRight
                                        }
                                        ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                            CursorStyle::ResizeUpRightDownLeft
                                        }
                                    },
                                    &hitbox,
                                );
                            },
                        )
                        .size_full()
                        .absolute(),
                    )
                    .map(|div| round_untiled_corners(div, tiling, rounding))
                    .when(!tiling.top, |div| div.pt(shadow_size))
                    .when(!tiling.bottom, |div| div.pb(shadow_size))
                    .when(!tiling.left, |div| div.pl(shadow_size))
                    .when(!tiling.right, |div| div.pr(shadow_size))
                    .on_mouse_down(MouseButton::Left, move |e, window, _| {
                        let size = window.window_bounds().get_bounds().size;
                        let pos = e.position;

                        if let Some(edge) = resize_edge(pos, shadow_size, size, tiling) {
                            window.start_window_resize(edge)
                        };
                    }),
            })
            .size_full()
            .child(
                div()
                    .font_family("Inter")
                    .text_color(theme.text)
                    .cursor(CursorStyle::Arrow)
                    .map(|div| match decorations {
                        Decorations::Server => div,
                        Decorations::Client { tiling } => div
                            .when(cfg!(not(target_os = "macos")), |div| {
                                div.border_color(rgba(0x64748b33))
                            })
                            .map(|div| round_untiled_corners(div, tiling, rounding))
                            .when(!tiling.top, |div| div.border_t(border_size))
                            .when(!tiling.bottom, |div| div.border_b(border_size))
                            .when(!tiling.left, |div| div.border_l(border_size))
                            .when(!tiling.right, |div| div.border_r(border_size))
                            .when(!tiling.is_tiled(), |div| {
                                div.shadow(vec![gpui::BoxShadow {
                                    color: Hsla::new(0., 0., 0., 0.4),
                                    blur_radius: shadow_size / 2.,
                                    spread_radius: px(0.),
                                    offset: point(px(0.0), px(0.0)),
                                    inset: false,
                                }])
                            }),
                    })
                    .on_mouse_move(|_e, _, cx| {
                        cx.stop_propagation();
                    })
                    .overflow_hidden()
                    .bg(theme.background_primary)
                    .size_full()
                    .flex()
                    .flex_col()
                    .max_w_full()
                    .max_h_full()
                    .child(self.content),
            );

        let text_styles = element.text_style();
        let ff = &mut text_styles.font_features;
        *ff = Some(TNUM_FEATURES.clone());

        element
    }
}

pub fn window_chrome(content: impl IntoElement) -> WindowChrome {
    WindowChrome::new(content)
}

/// Rounds the corners that face an untiled edge; a tiled edge stays square so
/// the window sits flush against the screen border.
fn round_untiled_corners<D>(div: D, tiling: Tiling, rounding: Pixels) -> D
where
    D: Styled + FluentBuilder,
{
    div.when(!(tiling.top || tiling.right), |div| {
        div.rounded_tr(rounding)
    })
    .when(!(tiling.top || tiling.left), |div| div.rounded_tl(rounding))
    .when(!(tiling.bottom || tiling.right), |div| {
        div.rounded_br(rounding)
    })
    .when(!(tiling.bottom || tiling.left), |div| {
        div.rounded_bl(rounding)
    })
}

/// Hit-tests a window border for a resize handle. Corner zones are
/// `edge_zone * 2` wide so both adjacent edges stay grabbable where they
/// meet; a tiled edge is never resizable. Corners are matched before edges.
fn resize_edge(
    pos: Point<Pixels>,
    edge_zone: Pixels,
    size: Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let corner_zone = edge_zone * 2.0;

    if pos.y < corner_zone && pos.x < corner_zone && !tiling.top && !tiling.left {
        return Some(ResizeEdge::TopLeft);
    }
    if pos.y < corner_zone && pos.x > size.width - corner_zone && !tiling.top && !tiling.right {
        return Some(ResizeEdge::TopRight);
    }
    if pos.y < edge_zone && !tiling.top {
        return Some(ResizeEdge::Top);
    }
    if pos.y > size.height - corner_zone && pos.x < corner_zone && !tiling.bottom && !tiling.left {
        return Some(ResizeEdge::BottomLeft);
    }
    if pos.y > size.height - corner_zone
        && pos.x > size.width - corner_zone
        && !tiling.bottom
        && !tiling.right
    {
        return Some(ResizeEdge::BottomRight);
    }
    if pos.y > size.height - edge_zone && !tiling.bottom {
        return Some(ResizeEdge::Bottom);
    }
    if pos.x < edge_zone && !tiling.left {
        return Some(ResizeEdge::Left);
    }
    if pos.x > size.width - edge_zone && !tiling.right {
        return Some(ResizeEdge::Right);
    }
    None
}
