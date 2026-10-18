//! 在线音源曲目行共用的视觉骨架。酷狗的歌单 / 发现页渲染完全相同的行布局；
//! 只有曲目类型、本地化的下载标签以及逐视图的播放 / 收藏 / 下载回调不同。
//! 这里放一份实现，让各音源在视觉上齐步走（就是音源胶水模块里那条
//! 「②步收编」的 re-export 注释所指的事）。

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

/// 共用行所需的显示字段，由音源的曲目信息结构体实现（见 `ui::kugou` 里的 impl）。
pub(crate) trait OnlineTrackDisplay {
    fn title(&self) -> &SharedString;
    fn artist(&self) -> &SharedString;
    fn album(&self) -> &SharedString;
    fn cover_url(&self) -> &SharedString;
    fn duration_secs(&self) -> i64;
    /// 本地化的「下载」标签。按音源各留一份，i18n 生成器才会一直看到互不相同的
    /// `KUGOU_DOWNLOAD` 键。
    fn download_label() -> I18nString;

    /// 列表里右侧使用的标签：「艺人 · m:ss」。
    fn detail_label(&self) -> SharedString {
        let duration = format_duration(self.duration_secs(), false);
        if self.artist().is_empty() {
            duration.into()
        } else {
            format!("{} · {}", self.artist(), duration).into()
        }
    }
}

/// 发现页遍历的是 `Arc<...TrackInfo>` 条目；一条 blanket impl 让它们可以把
/// `&Arc<T>` 直接交给 [`track_row`]。
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

/// 两个音源的歌单与发现页（榜单 / 每日推荐）共用的在线曲目行。回调携带逐视图的
/// 语义；视觉骨架完全一致。
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
            // GPUI 对连点序列的每次点击都发一个点击事件；只有第一次点击开始播放，
            // 这样双击不会把「抓取 + 打开」的震荡跑两遍。
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
                // 复合的 (name, index) id：每帧零分配；图片被限定在本行的
                // `.id((id_prefix, index))` 作用域下
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
                // 复合 id 限定在本行 `.id((id_prefix, index))` 的作用域下
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

/// 发现页单张榜单卡片的宽度，单位 CSS px。卡片按这个尺寸绘制；managed-image
/// 缩略图按 192 设备像素解码，于是 1.25-1.5 DPR 下仍然锐利，同时缩小图集打包的
/// 每一枚瓦片。
pub(crate) const RANK_CARD_W: f32 = 148.0;

/// 发现页的一张榜单卡片：封面 + 名字，可选的音源副标题。点击处理器由调用方提供
/// ——各音源的视图类型与它们的 `open_rank` 都不同，而卡片是在 uniform_list 闭包
/// 内部构建的，那里只有 `&App`，所以 weak-entity 的 upgrade 留在调用点。
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

/// 给空/加载状态的暗色占位行。
pub(crate) fn muted_line(text: impl IntoElement, theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.text_secondary)
        .py(px(24.0))
        .child(text)
}

/// 错误占位行；调用方把它与一个重试按钮配对使用。
pub(crate) fn error_line(message: impl IntoElement, theme: &Theme) -> Div {
    div()
        .text_sm()
        .text_color(theme.status_error)
        .py(px(12.0))
        .child(message)
}
