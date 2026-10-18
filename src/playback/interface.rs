use gpui::App;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{
    playback::{dsp::spectrum::SpectrumTapConsumer, events::RepeatState},
    settings::{equalizer::EqualizerSettings, playback::PlaybackSettings},
};

use super::{
    events::{PlaybackCommand, PlaybackEvent},
    queue::QueueItemData,
};

/// 播放接口结构体，负责播放线程与主线程之间的通信。实现借助 GPUI 的 Global
/// trait，使任意函数（只要运行在主线程上）都能向播放线程发送命令。
///
/// 命令半边留在这里；事件半边（从通道读取 `PlaybackEvent` 并镜像进 UI models，
/// 外加按帧间隔跑的图集瓦片回收排空）在 `ui::playback_bridge`，接收端经
/// [`PlaybackInterface::take_events_rx`] 取走。
///
/// 本接口提供的函数，见播放线程的文档。
pub struct PlaybackInterface {
    cmd_tx: UnboundedSender<PlaybackCommand>,
    events_rx: Option<UnboundedReceiver<PlaybackEvent>>,
    spectrum_tap: Option<SpectrumTapConsumer>,
}

impl gpui::Global for PlaybackInterface {}

impl PlaybackInterface {
    pub fn new(
        cmd_tx: UnboundedSender<PlaybackCommand>,
        events_rx: UnboundedReceiver<PlaybackEvent>,
        spectrum_tap: SpectrumTapConsumer,
    ) -> Self {
        Self {
            cmd_tx,
            events_rx: Some(events_rx),
            spectrum_tap: Some(spectrum_tap),
        }
    }

    /// 频谱抽头的消费端，频谱分析器启动时取走一次。
    pub fn take_spectrum_tap(&mut self) -> Option<SpectrumTapConsumer> {
        self.spectrum_tap.take()
    }

    /// 即发即忘地送往播放线程。无界通道发送只在接收端已消失（播放线程早已
    /// 拆除）时才失败，对这些由 UI 发起的命令无害——故刻意忽略返回值。
    fn send(&self, command: PlaybackCommand) {
        let _ = self.cmd_tx.send(command);
    }

    pub fn play(&self) {
        self.send(PlaybackCommand::Play);
    }

    pub fn pause(&self) {
        self.send(PlaybackCommand::Pause);
    }

    pub fn queue(&self, item: QueueItemData) {
        self.send(PlaybackCommand::Queue(item));
    }

    pub fn queue_list(&self, items: Vec<QueueItemData>) {
        self.send(PlaybackCommand::QueueList(items));
    }

    pub fn insert_at(&self, item: QueueItemData, position: usize) {
        self.send(PlaybackCommand::InsertAt { item, position });
    }

    pub fn insert_list_at(&self, items: Vec<QueueItemData>, position: usize) {
        self.send(PlaybackCommand::InsertListAt { items, position });
    }

    pub fn next(&self) {
        self.send(PlaybackCommand::Next);
    }

    pub fn previous(&self) {
        self.send(PlaybackCommand::Previous);
    }

    pub fn clear_queue(&self) {
        self.send(PlaybackCommand::ClearQueue);
    }

    pub fn jump(&self, index: usize) {
        self.send(PlaybackCommand::Jump(index));
    }

    pub fn seek(&self, position: f64) {
        self.send(PlaybackCommand::Seek(position));
    }

    pub fn set_volume(&self, volume: f64) {
        self.send(PlaybackCommand::SetVolume(volume));
    }

    pub fn replace_queue(&self, items: Vec<QueueItemData>) {
        self.send(PlaybackCommand::ReplaceQueue(items));
    }

    pub fn replace_queue_with_index(&self, items: Vec<QueueItemData>, idx: usize) {
        self.send(PlaybackCommand::ReplaceQueueWithIndex(items, idx));
    }

    pub fn toggle_stop_after_current(&self) {
        self.send(PlaybackCommand::StopAfterCurrent);
    }

    pub fn toggle_shuffle(&self) {
        self.send(PlaybackCommand::ToggleShuffle);
    }

    pub fn set_repeat(&self, state: RepeatState) {
        self.send(PlaybackCommand::SetRepeat(state));
    }

    pub fn remove_item(&self, idx: usize) {
        self.send(PlaybackCommand::RemoveItem(idx));
    }

    pub fn remove_items(&self, indices: Vec<usize>) {
        self.send(PlaybackCommand::RemoveItems(indices));
    }

    pub fn move_item(&self, from: usize, to: usize) {
        self.send(PlaybackCommand::MoveItem { from, to });
    }

    pub fn move_items(&self, indices: Vec<usize>, to: usize) {
        self.send(PlaybackCommand::MoveItems { indices, to });
    }

    pub fn undo(&self) {
        self.send(PlaybackCommand::Undo);
    }

    pub fn update_settings(&self, settings: PlaybackSettings) {
        self.send(PlaybackCommand::SettingsChanged(settings));
    }

    pub fn set_equalizer(&self, settings: EqualizerSettings) {
        self.send(PlaybackCommand::SetEqualizer(settings));
    }

    pub fn set_position_broadcast_active(&self, active: bool) {
        self.send(PlaybackCommand::SetPositionBroadcastActive(active));
    }

    pub fn get_sender(&self) -> UnboundedSender<PlaybackCommand> {
        self.cmd_tx.clone()
    }

    /// 把事件通道的接收端交给 UI 侧广播循环（`ui::playback_bridge::start_broadcast`）：
    /// 它把播放事件镜像进 UI models，并驱动图集瓦片的回收排空。接收端已被
    /// 取走时返回 `None`：该循环必须只跑一次，所以那种情况下 bridge 会 panic
    /// （与原 `PlaybackInterface::start_broadcast` 同一契约）。
    pub fn take_events_rx(&mut self) -> Option<UnboundedReceiver<PlaybackEvent>> {
        self.events_rx.take()
    }
}

/// 用给定条目替换当前队列。
pub fn replace_queue(items: Vec<QueueItemData>, app: &mut App) {
    let playback_interface = app.global::<PlaybackInterface>();
    playback_interface.replace_queue(items);
}
