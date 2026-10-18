use gpui::*;
use prelude::FluentBuilder;

use crate::{
    settings::storage::{DEFAULT_LYRICS_FRACTION, DEFAULT_QUEUE_WIDTH},
    ui::{
        components::resizable::{ResizeEdge, resizable},
        lyrics::Lyrics,
        models::Models,
        queue::Queue,
    },
};

// ─── RightSidebar 组件 ─────────────────────────────────────────────────

/// 队列面板的宽度上下限（px），由左边缘的拖拽手柄做钳位。
const QUEUE_MIN_WIDTH: f32 = 225.0;
const QUEUE_MAX_WIDTH: f32 = 800.0;
/// 歌词面板的高度上下限（占侧栏的比例），由 `percent_mode` 下顶边缘的拖拽手柄
/// 做钳位。
const LYRICS_MIN_FRACTION: f32 = 0.10;
const LYRICS_MAX_FRACTION: f32 = 0.85;

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

    /// 渲染可拖宽的队列面板；歌词面板在其下方，队列隐藏时占满整列。
    pub fn render(&self, cx: &mut App, show_queue: bool, show_lyrics: bool) -> impl IntoElement {
        let queue_width = cx.global::<Models>().queue_width.clone();
        let lyrics_height_entity = cx.global::<Models>().lyrics_height.clone();

        let queue =
            AnyView::from(self.queue.clone()).cached(StyleRefinement::default().size_full());

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
                    // 队列区：填满歌词面板之上的剩余空间。
                    .when(show_queue, |outer: Div| {
                        let queue_wrapper = div()
                            .when(show_lyrics, |d: Div| d.flex_1().min_h(px(0.0)))
                            .when(!show_lyrics, |d: Div| d.h_full())
                            .overflow_hidden()
                            .child(queue);
                        outer.child(queue_wrapper)
                    })
                    // 歌词区：底部固定高度，可从其顶边缘拖拽调整。
                    .when(show_lyrics, |outer: Div| {
                        let lyrics = AnyView::from(self.lyrics.clone())
                            .cached(StyleRefinement::default().size_full());
                        // 无论顶边缘可拖（显示队列）还是固定（隐藏队列），
                        // 都是同一个被裁剪的面板。
                        let lyrics_pane = div().h_full().overflow_hidden().child(lyrics);
                        if show_queue {
                            outer.child(
                                resizable(
                                    "lyrics-resizable",
                                    lyrics_height_entity.clone(),
                                    ResizeEdge::Top,
                                )
                                .percent_mode()
                                .min_size(px(LYRICS_MIN_FRACTION))
                                .max_size(px(LYRICS_MAX_FRACTION))
                                .default_size(DEFAULT_LYRICS_FRACTION)
                                .flex_shrink(1.0)
                                .w_full()
                                .child(lyrics_pane),
                            )
                        } else {
                            outer.child(lyrics_pane)
                        }
                    }),
            )
    }
}
