//! Shared page header for every top-level view: a min-48px row with a bottom
//! hairline, a bold title with an optional subtitle on the left and an
//! optional action bar on the right. Albums/tracks/artists, the files tree and
//! the KuGou playlists page all render through this so headers read as one
//! system.
use gpui::*;
use prelude::FluentBuilder;

use crate::ui::theme::Theme;

#[derive(IntoElement)]
pub struct ViewHeader {
    title: SharedString,
    subtitle: Option<SharedString>,
    left: Option<AnyElement>,
    right: Option<AnyElement>,
}

impl ViewHeader {
    /// Secondary line rendered under the title (e.g. "5 tracks · 23 min").
    #[cfg_attr(not(feature = "online_sources"), allow(dead_code))]
    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Element placed before the title (e.g. a back button).
    #[cfg_attr(not(feature = "online_sources"), allow(dead_code))]
    pub fn left(mut self, left: impl IntoElement) -> Self {
        self.left = Some(left.into_any_element());
        self
    }

    /// Element placed at the trailing edge (e.g. view-mode toggles).
    pub fn right(mut self, right: impl IntoElement) -> Self {
        self.right = Some(right.into_any_element());
        self
    }
}

impl RenderOnce for ViewHeader {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let title = self.title;

        div()
            .flex()
            .border_b_1()
            .border_color(theme.border_color)
            .w_full()
            .child(
                div()
                    .min_h(px(48.0))
                    .w_full()
                    .py(px(12.0))
                    .pl(px(18.0))
                    .pr(px(12.0))
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .min_w(px(0.0))
                            .when_some(self.left, |d, el| d.child(el))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .line_height(rems(1.05))
                                    .min_w(px(0.0))
                                    .child(
                                        div()
                                            .line_height(px(26.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(px(22.0))
                                            .child(title),
                                    )
                                    .when_some(self.subtitle, |d, subtitle| {
                                        d.child(
                                            div()
                                                .text_sm()
                                                .text_color(theme.text_secondary)
                                                .overflow_x_hidden()
                                                .text_ellipsis()
                                                .child(subtitle),
                                        )
                                    }),
                            ),
                    )
                    .when_some(self.right, |d, el| d.child(el)),
            )
    }
}

pub fn view_header(title: impl Into<SharedString>) -> ViewHeader {
    ViewHeader {
        title: title.into(),
        subtitle: None,
        left: None,
        right: None,
    }
}
