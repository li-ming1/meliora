use cntp_i18n::tr;
use gpui::*;
use prelude::FluentBuilder;

use crate::{
    settings::storage::DEFAULT_QUEUE_WIDTH,
    ui::{
        components::{
            icons::CROSS,
            nav_button::nav_button,
            resizable::{ResizeEdge, resizable},
            tooltip::build_tooltip,
        },
        lyrics::Lyrics,
        models::Models,
        queue::Queue,
        theme::Theme,
    },
};

// ─── RightSidebar 组件 ─────────────────────────────────────────────────

/// 队列面板的宽度上下限（px），由左边缘的拖拽手柄做钳位。
const QUEUE_MIN_WIDTH: f32 = 225.0;
const QUEUE_MAX_WIDTH: f32 = 800.0;

/// 队列 + 歌词这一列：队列面板可从其左边缘拖宽；歌词面板贴在底部，
/// 队列显示时可从其顶边缘拖高。
pub struct RightSidebar {
    queue: Entity<Queue>,
    lyrics: Entity<Lyrics>,
}

impl RightSidebar {
    pub fn new(cx: &mut App) -> Self {
        let queue = Queue::new(cx, cx.global::<Models>().show_queue.clone());
        let lyrics = Lyrics::new(cx);

        Self { queue, lyrics }
    }

    /// 共享的歌词模型：沉浸视图镜像它的已解析状态（含在线抓取），而不是二次加载
    /// 歌词。
    pub(crate) fn lyrics(&self) -> Entity<Lyrics> {
        self.lyrics.clone()
    }

    /// 渲染可拖宽的右侧面板：队列与歌词采用全高度独立或分段呈现，避免挤压截断。
    pub fn render(&self, cx: &mut App, show_queue: bool, show_lyrics: bool) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let queue_width = cx.global::<Models>().queue_width.clone();
        let show_queue_model = cx.global::<Models>().show_queue.clone();
        let show_lyrics_model = cx.global::<Models>().show_lyrics.clone();

        let queue_entity = self.queue.clone();
        let lyrics =
            AnyView::from(self.lyrics.clone()).cached(StyleRefinement::default().size_full());

        resizable("queue-resizable", queue_width, ResizeEdge::Left)
            .min_size(px(QUEUE_MIN_WIDTH))
            .max_size(px(QUEUE_MAX_WIDTH))
            .default_size(DEFAULT_QUEUE_WIDTH)
            .h_full()
            .child(
                div()
                    .h_full()
                    .w_full()
                    .flex()
                    .flex_col()
                    .when(show_queue && !show_lyrics, {
                        let queue_entity = queue_entity.clone();
                        move |d| {
                            let queue_view = AnyView::from(queue_entity).cached(StyleRefinement::default().size_full());
                            d.child(div().h_full().w_full().overflow_hidden().child(queue_view))
                        }
                    })
                    .when(!show_queue && show_lyrics, |d| {
                        let show_lyrics_model = show_lyrics_model.clone();
                        d.child(
                            div()
                                .h_full()
                                .w_full()
                                .flex()
                                .flex_col()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .h(px(40.0))
                                        .px(px(14.0))
                                        .border_b_1()
                                        .border_color(theme.border_color)
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::BOLD)
                                                .child(tr!("LYRICS", "Lyrics")),
                                        )
                                        .child(
                                            nav_button("close-lyrics", CROSS)
                                                .on_click(move |_, _, cx| {
                                                    show_lyrics_model.update(cx, |v, _| {
                                                        *v = false;
                                                    });
                                                })
                                                .tooltip(build_tooltip(tr!("CLOSE", "Close"))),
                                        ),
                                )
                                .child(div().flex_1().h_full().overflow_hidden().child(lyrics)),
                        )
                    })
                    .when(show_queue && show_lyrics, {
                        let show_queue_model = show_queue_model.clone();
                        let show_lyrics_model = show_lyrics_model.clone();
                        move |d| {
                            // 两者同时开启时，默认呈现队列，并在顶部提供分段开关切换
                            let queue_view = AnyView::from(queue_entity).cached(StyleRefinement::default().size_full());
                            d.child(
                                div()
                                    .h_full()
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .h(px(40.0))
                                            .px(px(10.0))
                                            .border_b_1()
                                            .border_color(theme.border_color)
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .p(px(2.5))
                                                    .rounded_full()
                                                    .bg(theme.elevated_background)
                                                    .border_1()
                                                    .border_color(theme.elevated_border_color)
                                                    .shadow_sm()
                                                    .child(
                                                        div()
                                                            .id("right-sidebar-queue-tab")
                                                            .px(px(12.0))
                                                            .py(px(3.5))
                                                            .rounded_full()
                                                            .text_xs()
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .bg(theme.button_primary)
                                                            .text_color(theme.button_primary_text)
                                                            .shadow_sm()
                                                            .cursor_pointer()
                                                            .child(tr!("QUEUE_TITLE")),
                                                    )
                                                    .child(
                                                        div()
                                                            .id("right-sidebar-lyrics-tab")
                                                            .px(px(12.0))
                                                            .py(px(3.5))
                                                            .rounded_full()
                                                            .text_xs()
                                                            .font_weight(FontWeight::MEDIUM)
                                                            .text_color(theme.text_secondary)
                                                            .cursor_pointer()
                                                            .hover(|s| s.text_color(theme.text))
                                                            .on_click(move |_, _, cx| {
                                                                show_queue_model.update(cx, |v, _| *v = false);
                                                            })
                                                            .child(tr!("LYRICS", "Lyrics")),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .id("close-sidebar-wrap")
                                                    .size(px(26.0))
                                                    .rounded_full()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .hover(|s| s.bg(theme.nav_button_hover))
                                                    .child(
                                                        nav_button("close-sidebar", CROSS)
                                                            .on_click(move |_, _, cx| {
                                                                show_lyrics_model.update(cx, |v, _| *v = false);
                                                            })
                                                            .tooltip(build_tooltip(tr!("CLOSE", "Close"))),
                                                    ),
                                            ),
                                    )
                                    .child(div().flex_1().h_full().overflow_hidden().child(queue_view)),
                            )
                        }
                    }),
            )
    }
}
