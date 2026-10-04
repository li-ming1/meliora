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

/// The playback interface struct that will be used to communicate between the playback thread and
/// the main thread. This implementation takes advantage of the GPUI Global trait to allow any
/// function (so long as it is running on the main thread) to send commands to the playback thread.
///
/// The command half lives here; the event half (reading `PlaybackEvent`s off
/// the channel and mirroring them into the UI models, plus the frame-interval
/// atlas-tile drains) lives in `ui::playback_bridge`, which takes the
/// receiver via [`PlaybackInterface::take_events_rx`].
///
/// For the functions provided by this interface, see the documentation for the playback thread.
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

    /// Consumer half of the spectrum taps, taken once when the spectrum analyzer starts.
    pub fn take_spectrum_tap(&mut self) -> Option<SpectrumTapConsumer> {
        self.spectrum_tap.take()
    }

    /// Fire-and-forget send to the playback thread. An unbounded channel send
    /// only fails once the receiver is gone (the playback thread already torn
    /// down), which is harmless for these UI-originated commands — hence the
    /// deliberately ignored result.
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

    /// Hands the event-channel receiver to the UI-side broadcast loop
    /// (`ui::playback_bridge::start_broadcast`), which mirrors playback
    /// events into the UI models and drives the atlas-tile reclaim drains.
    /// Returns `None` when the receiver was already taken: the loop must
    /// run exactly once, so the bridge panics in that case (same contract
    /// as the former `PlaybackInterface::start_broadcast`).
    pub fn take_events_rx(&mut self) -> Option<UnboundedReceiver<PlaybackEvent>> {
        self.events_rx.take()
    }
}

/// Replace the current queue with the given items.
pub fn replace_queue(items: Vec<QueueItemData>, app: &mut App) {
    let playback_interface = app.global::<PlaybackInterface>();
    playback_interface.replace_queue(items);
}
