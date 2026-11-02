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

/// 版本字符串下方的「有新版本」/「重启以应用」那一行，由更新器 global 渲染——
/// 这个对话框是 `RenderOnce`，每次打开都重建，所以它显示的总是最新状态。
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
                .id("about-modal")
                .track_focus(&self.focus_handle)
                .role(gpui::Role::Dialog)
                .p(px(24.0))
                .pb(px(22.0))
                .flex()
                .child(
                    div()
                        .size(px(76.0))
                        .rounded(px(theme.radius_lg))
                        .p(px(8.0))
                        .bg(theme.elevated_background)
                        .border_1()
                        .border_color(theme.elevated_border_color)
                        .shadow_md()
                        .mr(px(22.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(img("!bundled:images/logo.png").size(px(56.0)).rounded(px(theme.radius_md))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div().flex().mr(px(180.0)).child(
                                div()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .font_family("Lexend")
                                            .text_size(px(32.0))
                                            .line_height(px(34.0))
                                            .text_color(theme.text)
                                            .child(tr!("APP_NAME")),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .mt(px(6.0))
                                            .child(
                                                div()
                                                    .px(px(10.0))
                                                    .py(px(2.5))
                                                    .rounded_full()
                                                    .bg(theme.elevated_background)
                                                    .border_1()
                                                    .border_color(theme.elevated_border_color)
                                                    .shadow_sm()
                                                    .text_size(px(11.0))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.text)
                                                    .child(format!("v{}", crate::VERSION_STRING)),
                                            )
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
