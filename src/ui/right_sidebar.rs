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

// ─── RightSidebar component ───────────────────────────────────────────────────

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

    pub fn render(&self, cx: &mut App, show_queue: bool, show_lyrics: bool) -> impl IntoElement {
        let queue_width = cx.global::<Models>().queue_width.clone();
        let lyrics_height_entity = cx.global::<Models>().lyrics_height.clone();

        let queue =
            AnyView::from(self.queue.clone()).cached(StyleRefinement::default().size_full());

        resizable("queue-resizable", queue_width, ResizeEdge::Left)
            .min_size(px(225.0))
            .max_size(px(800.0))
            .default_size(DEFAULT_QUEUE_WIDTH)
            .h_full()
            .child(
                div()
                    .h_full()
                    .w_full()
                    .flex()
                    .flex_col()
                    // Queue section: fills remaining space above the lyrics pane.
                    .when(show_queue, |outer: Div| {
                        let queue_wrapper = div()
                            .when(show_lyrics, |d: Div| d.flex_1().min_h(px(0.0)))
                            .when(!show_lyrics, |d: Div| d.h_full())
                            .overflow_hidden()
                            .child(queue);
                        outer.child(queue_wrapper)
                    })
                    // Lyrics section: fixed height at bottom, resizable from its top edge.
                    .when(show_lyrics, |outer: Div| {
                        let lyrics = AnyView::from(self.lyrics.clone())
                            .cached(StyleRefinement::default().size_full());
                        if show_queue {
                            outer.child(
                                resizable(
                                    "lyrics-resizable",
                                    lyrics_height_entity.clone(),
                                    ResizeEdge::Top,
                                )
                                .percent_mode()
                                .min_size(px(0.10))
                                .max_size(px(0.85))
                                .default_size(DEFAULT_LYRICS_FRACTION)
                                .flex_shrink(1.0)
                                .w_full()
                                .child(div().h_full().overflow_hidden().child(lyrics)),
                            )
                        } else {
                            outer.child(div().h_full().overflow_hidden().child(lyrics))
                        }
                    }),
            )
    }
}
