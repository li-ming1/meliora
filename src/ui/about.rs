use cntp_i18n::tr;
use gpui::{
    FocusHandle, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, div, img, px,
};

use super::{
    components::modal::{OnExitHandler, modal},
    theme::Theme,
};

const ISSUES_URL: &str = "https://github.com/li-ming1/meliora/issues";
const SOURCE_URL: &str = "https://github.com/li-ming1/meliora";
const LICENSE_URL: &str = "https://choosealicense.com/licenses/apache-2.0/";

/// "New version available" / "restart to apply" line under the version
/// string, rendered from the updater global — the dialog is a `RenderOnce`
/// rebuilt on every open, so it always shows the freshest state.
#[cfg(feature = "online")]
fn update_badge(cx: &gpui::App, link_color: gpui::Rgba) -> Option<gpui::AnyElement> {
    use crate::updater::{UpdateStatus, UpdaterGlobal};

    if !cx.has_global::<UpdaterGlobal>() {
        return None;
    }
    let status = cx.global::<UpdaterGlobal>().state.read(cx).status.clone();
    match status {
        UpdateStatus::Available { version, url, .. } => Some(
            div()
                .id("about-update-available")
                .cursor_pointer()
                .text_color(link_color)
                .hover(move |this| this.border_b_1().border_color(link_color))
                .on_click(move |_, _, cx| cx.open_url(&url))
                .child(tr!(
                    "UPDATE_ABOUT_AVAILABLE",
                    "New version {{version}} available",
                    version = version
                ))
                .into_any_element(),
        ),
        UpdateStatus::ReadyToRestart { version } => Some(
            div()
                .id("about-update-ready")
                .cursor_pointer()
                .text_color(link_color)
                .hover(move |this| this.border_b_1().border_color(link_color))
                .on_click(|_, _, cx| crate::updater::restart_to_apply(cx))
                .child(tr!(
                    "UPDATE_ABOUT_READY",
                    "Update to {{version}} ready — restart to apply",
                    version = version
                ))
                .into_any_element(),
        ),
        _ => None,
    }
}

fn link_label(
    id: impl Into<gpui::ElementId>,
    url: &'static str,
    link_color: gpui::Rgba,
    child: impl IntoElement,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(link_color)
        .hover(move |this| this.border_b_1().border_color(link_color))
        .on_click(move |_, _, cx| cx.open_url(url))
        .child(child)
}

#[derive(IntoElement)]
pub struct AboutDialog {
    on_exit: &'static OnExitHandler,
    focus_handle: FocusHandle,
}

impl RenderOnce for AboutDialog {
    fn render(self, window: &mut gpui::Window, cx: &mut gpui::App) -> impl gpui::IntoElement {
        self.focus_handle.focus(window, cx);
        let theme = cx.global::<Theme>();
        let link_color = theme.text_link;
        #[cfg(feature = "online")]
        let update_badge = update_badge(cx, link_color);
        #[cfg(not(feature = "online"))]
        let update_badge = None::<gpui::AnyElement>;

        modal().on_exit(self.on_exit).child(
            div()
                .track_focus(&self.focus_handle)
                .p(px(20.0))
                .pb(px(18.0))
                .flex()
                .child(img("!bundled:images/logo.png").w(px(66.0)).mr(px(20.0)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div().flex().mr(px(200.0)).child(
                                div()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .font_family("Lexend")
                                            .text_size(px(36.0))
                                            .line_height(px(36.0))
                                            .ml(px(-2.0))
                                            .child(tr!("APP_NAME")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .line_height(px(13.0))
                                            .text_color(theme.text_secondary)
                                            .mt(px(1.0))
                                            .child(crate::VERSION_STRING)
                                            .children(update_badge),
                                    ),
                            ),
                        )
                        .child(
                            div().mt(px(15.0)).flex().child(
                                div()
                                    .text_sm()
                                    .text_size(px(13.0))
                                    .text_color(theme.text_secondary)
                                    .child(
                                        div()
                                            .flex()
                                            .child(tr!(
                                                "ABOUT_LINKS_START",
                                                "\u{200B}",
                                                #description="Because the UI framework we use \
                                                    doesn't support inline elements, we have to \
                                                    use a separate string for each part of this \
                                                    text. Use a zero-width space (U+200B) if a \
                                                    part isn't needed."
                                            ))
                                            .child(link_label(
                                                "about-bug-link",
                                                ISSUES_URL,
                                                link_color,
                                                tr!("ABOUT_LINKS_BUG", "Report a bug"),
                                            ))
                                            .child(tr!("ABOUT_LINKS_MIDDLE", " or "))
                                            .child(link_label(
                                                "about-source-link",
                                                SOURCE_URL,
                                                link_color,
                                                tr!("ABOUT_LINKS_CODE", "view the source code"),
                                            )),
                                    )
                                    .child(div().mt(px(10.0)).child(tr!(
                                        "ABOUT_COPYRIGHT",
                                        "Copyright © 2024 - 2026 William \
                                            Whittaker and contributors."
                                    )))
                                    .child(
                                        div()
                                            .flex()
                                            .child(tr!(
                                                "ABOUT_LICENSE_BEFORE_LINK",
                                                "Licensed under the Apache License, version 2.0. "
                                            ))
                                            .child(link_label(
                                                "about-rights-link",
                                                LICENSE_URL,
                                                link_color,
                                                tr!(
                                                    "ABOUT_LICENSE_LINK",
                                                    "Learn more about your rights."
                                                ),
                                            ))
                                            .child(tr!("ABOUT_LICENSE_AFTER_LINK", "\u{200B}")),
                                    ),
                            ),
                        ),
                ),
        )
    }
}

pub fn about_dialog(focus_handle: FocusHandle, on_exit: &'static OnExitHandler) -> AboutDialog {
    AboutDialog {
        on_exit,
        focus_handle,
    }
}
