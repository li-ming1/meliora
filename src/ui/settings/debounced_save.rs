//! 拖动滑条类高频编辑的尾沿保存防抖，interface 页（`grid_min_item_width`
//! 滑条）与 playback 页（preamp 滑条）共用。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use gpui::{Context, Entity};

use crate::settings::{Settings, save_settings};

/// Trailing-edge debounce for slider drags (equalizer-view pattern): every
/// tick bumps the generation and schedules a single save ~300ms out, so
/// `save_settings` - and the PlaybackInterface/ScanInterface pushes it
/// performs - run once per drag instead of once per mouse-move tick.
/// Consumers: the interface page's `grid_min_item_width` slider and the
/// playback page's preamp slider. The disk write keeps its own 500ms
/// trailing-edge debounce inside `save_settings`.
///
/// The flush is detached and keyed on the generation counter instead of
/// being stored as a page-owned `Task`: dropping the page (section
/// switch / settings-window close) cancels a stored Task, which would
/// silently lose the trailing save - the live slider value sits in the
/// settings model but never reaches `save_settings`. The save is routed
/// through the app-lifetime settings entity so it survives the page.
/// Each page holds its own instance, so the two pages debounce independently.
pub struct DebouncedSave {
    generation: Arc<AtomicU64>,
}

impl DebouncedSave {
    pub fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn schedule<T: 'static>(&self, settings: &Entity<Settings>, cx: &mut Context<T>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let generation_counter = Arc::clone(&self.generation);
        let settings = settings.clone();
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            // A newer edit superseded this tick; the newest task owns the save.
            if generation_counter.load(Ordering::Relaxed) != generation {
                return;
            }
            settings.update(cx, |settings, cx| save_settings(cx, settings));
        })
        .detach();
    }
}
