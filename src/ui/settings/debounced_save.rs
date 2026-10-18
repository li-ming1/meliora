//! 拖动滑条类高频编辑的尾沿保存防抖，interface 页（`grid_min_item_width`
//! 滑条）与 playback 页（preamp 滑条）共用。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use gpui::{Context, Entity};

use crate::settings::{Settings, save_settings};

/// 拖拽滑块用的后沿防抖（equalizer-view 的同一套模式）：每次 tick 递增代号并安排
/// 一次约 300ms 后的保存，于是 `save_settings`——以及它做的
/// PlaybackInterface/ScanInterface 推送——每次拖拽只跑一次，而不是每次 mouse-move
/// tick 跑一次。使用方：interface 页的 `grid_min_item_width` 滑块与 playback 页的
/// 前置增益滑块。落盘本身在 `save_settings` 里还有自己 500ms 的后沿防抖。
///
/// flush 是脱离所有方的、按代号计数触发的，而不是存成页面持有的 `Task`：页面被
/// drop（切换分区 / 关闭设置窗口）会取消存着的 Task，从而静默丢掉最后那次保存——
/// 滑块的实时值在设置模型里，却永远到不了 `save_settings`。保存改走应用生命周期的
/// 设置实体，因此能活过页面。每个页各持一个实例，两个页面独立防抖。
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
            // 有更新的编辑顶掉了这次 tick；最新的那个任务拥有这次保存。
            if generation_counter.load(Ordering::Relaxed) != generation {
                return;
            }
            settings.update(cx, |settings, cx| save_settings(cx, settings));
        })
        .detach();
    }
}
