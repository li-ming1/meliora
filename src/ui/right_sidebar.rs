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

/// Queue pane width limits (px), clamped by the left-edge resize handle.
const QUEUE_MIN_WIDTH: f32 = 225.0;
const QUEUE_MAX_WIDTH: f32 = 800.0;
/// Lyrics pane height limits (fraction of the sidebar), clamped by the
/// top-edge resize handle in `percent_mode`.
const LYRICS_MIN_FRACTION: f32 = 0.10;
const LYRICS_MAX_FRACTION: f32 = 0.85;

/// Queue + lyrics column: the queue pane is width-resizable from its left
/// edge; the lyrics pane sits at the bottom, height-resizable from its top
/// edge when the queue is shown.
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

    /// Renders the width-resizable queue pane; the lyrics pane sits below it,
    /// or fills the whole column when the queue is hidden.
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
                        // Same clipped pane whether the top edge is resizable
                        // (queue shown) or fixed (queue hidden).
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
