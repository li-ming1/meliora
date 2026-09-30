//! Shared visual skeleton for the online providers' track rows. The KuGou
//! and NetEase playlists / discovery pages render the identical row layout;
//! only the track type, the localized download label and the per-view play /
//! like / download callbacks differ. One implementation here keeps the two
//! providers visually in lockstep (the "②步收编" the provider glue modules'
//! re-export notes refer to).

use cntp_i18n::{I18nString, tr};
use gpui::{
    App, ClickEvent, Div, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::ui::{
    components::{
        button::button,
        icons::{DOWNLOAD, STAR, STAR_FILLED, icon},
        managed_image::{ManagedImageKey, managed_image},
        tooltip::build_tooltip,
    },
    design::ICON_SM,
    theme::Theme,
    util::format_duration,
};

/// Display fields the shared row needs, implemented by each provider's track
/// info struct (see the impls in `ui::kugou` / `ui::netease`).
pub(crate) trait OnlineTrackDisplay {
    fn title(&self) -> &SharedString;
    fn artist(&self) -> &SharedString;
    fn album(&self) -> &SharedString;
    fn cover_url(&self) -> &SharedString;
    fn duration_secs(&self) -> i64;
    /// Localized "Download" label. Per-provider so the i18n generator keeps
    /// seeing distinct `KUGOU_DOWNLOAD` / `NETEASE_DOWNLOAD` keys.
    fn download_label() -> I18nString;

    /// Right-hand label used in listings: "artist · m:ss".
    fn detail_label(&self) -> SharedString {
        let duration = format_duration(self.duration_secs(), false);
        if self.artist().is_empty() {
            duration.into()
        } else {
            format!("{} · {}", self.artist(), duration).into()
        }
    }
}

/// The discovery pages iterate `Arc<...TrackInfo>` items; a blanket impl lets
/// them pass `&Arc<T>` straight to [`track_row`].
impl<T: OnlineTrackDisplay> OnlineTrackDisplay for std::sync::Arc<T> {
    fn title(&self) -> &SharedString {
        (**self).title()
    }
    fn artist(&self) -> &SharedString {
        (**self).artist()
    }
    fn album(&self) -> &SharedString {
        (**self).album()
    }
    fn cover_url(&self) -> &SharedString {
        (**self).cover_url()
    }
    fn duration_secs(&self) -> i64 {
        (**self).duration_secs()
    }
    fn download_label() -> I18nString {
        T::download_label()
    }
}

/// The shared online-track row used by both providers' playlists and
/// discovery (ranks / daily recommend) pages. The callbacks carry the
/// per-view semantics; the visual skeleton is identical.
#[allow(clippy::too_many_arguments)] // builder-style row API, mirrors the gpui element builders it feeds
pub(crate) fn track_row<T: OnlineTrackDisplay, F1, F2, F3>(
    theme: &Theme,
    track: &T,
    index: usize,
    id_prefix: &'static str,
    show_cover: bool,
    pad_minutes: bool,
    liked: bool,
    on_play: F1,
    on_like: F2,
    on_download: F3,
) -> impl IntoElement
where
    F1: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F2: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F3: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let title: SharedString = if track.title().is_empty() {
        tr!("UNKNOWN_TRACK").into()
    } else {
        track.title().clone()
    };
    let artist: SharedString = if track.artist().is_empty() {
        tr!("UNKNOWN_ARTIST").into()
    } else {
        track.artist().clone()
    };
    let detail: SharedString = if track.album().is_empty() {
        artist
    } else {
        SharedString::from(format!("{artist} · {}", track.album()))
    };
    let download_label = T::download_label();

    div()
        .id((id_prefix, index))
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(4.0))
        .py(px(8.0))
        .pl(px(6.0))
        .border_b_1()
        .border_color(theme.border_color)
        .cursor_pointer()
        .hover(move |this| this.bg(theme.queue_item_hover))
        .on_click(move |event, window, cx| {
            // GPUI delivers one click event per click of a multi-click
            // sequence; only the first click starts playback so a
            // double-click doesn't run the fetch + open churn twice.
            if event.click_count() > 1 {
                return;
            }
            on_play(event, window, cx)
        })
        .child(
            div()
                .text_xs()
                .text_color(theme.text_secondary)
                .w(px(28.0))
                .flex_shrink(0.0)
                .child((index + 1).to_string()),
        )
        .when(show_cover, |this| {
            this.child(
                // composite (name, index) id: zero per-frame allocation; the
                // image is scoped under this row's `.id((id_prefix, index))`
                managed_image(
                    ("thumb", index),
                    ManagedImageKey::HttpCover(track.cover_url().clone()),
                )
                .thumb(),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink(1.0)
                .overflow_x_hidden()
                .gap(px(1.0))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .overflow_x_hidden()
                        .text_ellipsis()
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_secondary)
                        .overflow_x_hidden()
                        .text_ellipsis()
                        .child(detail),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_secondary)
                .ml_auto()
                .flex_shrink(0.0)
                .child(format_duration(track.duration_secs(), pad_minutes)),
        )
        .child(
            button()
                // composite ids scoped under the row's `.id((id_prefix, index))`
                .id(("like", index))
                .child(icon(if liked { STAR_FILLED } else { STAR }).size(ICON_SM))
                .text_color(if liked {
                    theme.liked_song
                } else {
                    theme.text_secondary
                })
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_like(event, window, cx);
                }),
        )
        .child(
            button()
                .id(("download", index))
                .child(icon(DOWNLOAD).size(ICON_SM))
                .text_color(theme.text_secondary)
                .tooltip(build_tooltip(download_label))
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_download(event, window, cx);
                }),
        )
}

/// Width of one discovery-page rank card in CSS px. Cards paint at this size;
/// the managed-image thumb decodes at 192 device px so 1.25-1.5 DPR stays
/// sharp while shrinking every tile the atlas packs.
pub(crate) const RANK_CARD_W: f32 = 148.0;

/// One discovery-page rank card: cover + name, optional provider subtitle
/// (NetEase shows the chart's update frequency). The click handler is
/// supplied by the caller — the provider view types and their `open_rank`
/// differ, and the cards are built from inside a uniform_list closure where
/// only `&App` is available, so the weak-entity upgrade lives at the call
/// site.
pub(crate) fn rank_card<F>(
    id_prefix: &'static str,
    index: usize,
    theme: &Theme,
    cover_url: &SharedString,
    name: &SharedString,
    subtitle: Option<&SharedString>,
    on_open: F,
) -> impl IntoElement
where
    F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let cover = managed_image(
        ("rank-cover", index),
        ManagedImageKey::HttpCover(cover_url.clone()),
    )
    .thumb_max(192)
    .w(px(RANK_CARD_W))
    .h(px(RANK_CARD_W))
    .rounded(px(theme.radius_md));
    let name = div()
        .mt(px(6.0))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .overflow_x_hidden()
        .text_ellipsis()
        .child(name.clone());
    div()
        .id((id_prefix, index))
        .flex()
        .flex_col()
        .w(px(RANK_CARD_W))
        .cursor_pointer()
        .on_click(on_open)
        .child(cover)
        .child(name)
        .when_some(subtitle, |this, subtitle| {
            this.child(
                div()
                    .mt(px(2.0))
                    .text_xs()
                    .text_color(theme.text_secondary)
                    .overflow_x_hidden()
                    .text_ellipsis()
                    .child(subtitle.clone()),
            )
        })
}

/// Muted placeholder line for empty/loading states.
pub(crate) fn muted_line(text: impl IntoElement, theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.text_secondary)
        .py(px(24.0))
        .child(text)
}

/// Error placeholder line; the caller pairs it with a retry button.
pub(crate) fn error_line(message: impl IntoElement, theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.status_error)
        .py(px(12.0))
        .child(message)
}
