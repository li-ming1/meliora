use std::sync::Mutex;
use std::time::Duration;

use gpui::{App, AsyncApp, Entity};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::library::Pool;
use crate::{
    playback::{dsp::spectrum::SpectrumTapConsumer, events::RepeatState},
    power::PowerManager,
    settings::{equalizer::EqualizerSettings, playback::PlaybackSettings},
    ui::models::{CurrentTrack, ImageEvent, Models, PlaybackInfo},
};

use super::{
    events::{PlaybackCommand, PlaybackEvent},
    queue::QueueItemData,
    thread::PlaybackState,
};

/// Volume echoes only update `prev_volume` above this floor: the remembered
/// value must stay visible in the UI while volume is toggled on/off, even if
/// the user slid it all the way to `0`.
const MIN_REMEMBERED_VOLUME: f64 = 0.05;

/// Cadence of the idle atlas-tile drain spawned by
/// [`PlaybackInterface::start_broadcast`]: keeps evicted tiles reclaimed
/// while playback is paused (the event loop only drains when events arrive).
const IDLE_TILE_DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// The playback interface struct that will be used to communicate between the playback thread and
/// the main thread. This implementation takes advantage of the GPUI Global trait to allow any
/// function (so long as it is running on the main thread) to send commands to the playback thread.
///
/// This interface takes advantage of GPUI's asynchronous runtime to read messages without blocking
/// rendering. Messages are read at quickest every 10ms, however the runtime may choose to run the
/// function that reads events less frequently, depending on the current workload. Because of this,
/// event handling should not perform any heavy operations, which should be instead sent to the
/// data thread for any required additional processing.
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

    /// Starts the broadcast loop that will read events from the playback thread and update data
    /// models accordingly. This function should be called once, and will panic if called more than
    /// once.
    pub fn start_broadcast(&mut self, app: &mut App) {
        // This function's sole responsibility is to read events from the playback thread and update
        // data models accordingly.
        let Some(mut events_rx) = self.events_rx.take() else {
            panic!("broadcast thread already started");
        };

        let metadata_model = app.global::<Models>().metadata.clone();
        let albumart_model = app.global::<Models>().albumart.clone();
        let queue_model = app.global::<Models>().queue.clone();

        let playback_info = app.global::<PlaybackInfo>().clone();
        let power_manager = app.global::<PowerManager>().clone();
        let stats = app.global::<crate::stats::StatsHandle>().0.clone();
        let stats_pool = app.global::<Pool>().0.clone();

        app.spawn(async move |cx| {
            // `recv()` returning None means the playback thread dropped its
            // senders; the loop must end there or it would spin on None.
            while let Some(event) = events_rx.recv().await {
                // Coalesce the backlog: after a busy frame the channel can
                // hold dozens of stale position ticks, and replaying them
                // just redoes the same entity writes. Every variant is
                // value-replacement semantics (the newest wins; QueueUpdated
                // is a bare signal), so keeping only the last occurrence of
                // each variant preserves correctness while shedding the rest.
                let mut latest = vec![event];
                while let Ok(next) = events_rx.try_recv() {
                    let disc = std::mem::discriminant(&next);
                    latest.retain(|e| std::mem::discriminant(e) != disc);
                    latest.push(next);
                }
                for event in latest {
                    match event {
                        PlaybackEvent::MetadataUpdate(v) => {
                            metadata_model.write(cx, *v);
                        }
                        PlaybackEvent::AlbumArtUpdate(v) => {
                            albumart_model.update(cx, |m, cx| {
                                if let Some(v) = v {
                                    cx.emit(ImageEvent(v))
                                } else {
                                    *m = None;
                                    cx.notify()
                                }
                            });
                        }
                        PlaybackEvent::StateChanged(v) => {
                            playback_info.playback_state.write(cx, v);

                            if v == PlaybackState::Stopped {
                                playback_info.current_track.update(cx, |m, cx| {
                                    *m = None;
                                    cx.notify()
                                });
                            }

                            power_manager.set_state(cx, v);

                            record_and_flush(&stats, stats_pool.clone(), |recorder| {
                                recorder.on_state(v);
                                recorder.take_pending()
                            });
                        }
                        PlaybackEvent::PositionChanged(v) => {
                            playback_info.position.update(cx, |m, cx| {
                                *m = v;
                                cx.notify()
                            });

                            record_and_flush(&stats, stats_pool.clone(), |recorder| {
                                recorder.on_position(v);
                                recorder.take_pending()
                            });
                        }
                        PlaybackEvent::DurationChanged(v) => {
                            playback_info.duration.write(cx, v);
                        }
                        PlaybackEvent::SongChanged(path) => {
                            let (track_key, meta) =
                                crate::stats::song_info(&path, &queue_model, cx);
                            record_and_flush(&stats, stats_pool.clone(), |recorder| {
                                recorder.on_song_changed(track_key, meta);
                                recorder.take_pending()
                            });

                            playback_info.current_track.update(cx, |m, cx| {
                                *m = Some(CurrentTrack::new(path.clone()));
                                cx.notify()
                            });
                        }
                        PlaybackEvent::QueueUpdated => {
                            queue_model.update(cx, |_, cx| cx.notify());
                        }
                        PlaybackEvent::ShuffleToggled(v) => {
                            playback_info.shuffling.write(cx, v);
                        }
                        PlaybackEvent::VolumeChanged(v) => {
                            // equality guard: during a slider drag the echo-back
                            // notify would re-render the source slider per tick
                            update_if_changed(&playback_info.volume, cx, v);

                            // `prev_volume` must never dip below the floor: its
                            // value needs to stay visible in the UI while volume
                            // is toggled on/off, even if the user slid it to `0`.
                            if v > MIN_REMEMBERED_VOLUME {
                                update_if_changed(&playback_info.prev_volume, cx, v);
                            }
                        }
                        PlaybackEvent::QueuePositionChanged(v) => {
                            queue_model.update(cx, |m, cx| {
                                if m.position != v {
                                    m.position = v;
                                    cx.notify();
                                }
                            })
                        }
                        PlaybackEvent::RepeatChanged(v) => {
                            update_if_changed(&playback_info.repeating, cx, v);
                        }
                        PlaybackEvent::StopAfterCurrentChanged(v) => {
                            update_if_changed(&playback_info.stop_after_current, cx, v);
                        }
                        PlaybackEvent::SampleRateChanged(rate) => {
                            update_if_changed(&playback_info.sample_rate, cx, rate);
                        }
                    }
                }

                // Reclaim evicted cover atlas tiles. This loop runs on the
                // UI thread regardless of window visibility - a minimized
                // window still receives playback events, while frames (and
                // therefore the request_layout drain) stop being produced.
                // Without this, idle playback leaked one atlas page per
                // song change. The drain must only run between frames
                // (never mid-paint, see managed_image.rs).
                cx.update(crate::ui::components::managed_image::drain_pending_tile_drops);
            }
        })
        .detach();

        // Idle atlas tile reclamation: the event loop above only drains while
        // events arrive; this timer keeps it running when playback is paused.
        // Deliberately a gpui timer, NOT tokio::time — the gpui executor has
        // no tokio reactor on this thread (startup panic, 2026-09-08).
        app.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(IDLE_TILE_DRAIN_INTERVAL)
                    .await;
                cx.update(crate::ui::components::managed_image::drain_pending_tile_drops);
            }
        })
        .detach();
    }
}

/// Feeds one recording step to the shared stats recorder and flushes the
/// rows it produced. A poisoned lock is recovered from: a panic inside an
/// earlier recording step must not wedge the broadcast loop.
fn record_and_flush<T>(
    stats: &Mutex<T>,
    pool: sqlx::SqlitePool,
    record: impl FnOnce(&mut T) -> Vec<crate::stats::ListenRow>,
) {
    let rows = record(&mut stats.lock().unwrap_or_else(|e| e.into_inner()));
    crate::stats::flush_rows_async(pool, rows);
}

/// Writes `value` into the entity and notifies, but only when it actually
/// differs: echo-back events during a slider drag would otherwise re-render
/// the source widget on every tick.
fn update_if_changed<T: PartialEq + 'static>(entity: &Entity<T>, cx: &mut AsyncApp, value: T) {
    entity.update(cx, |m, cx| {
        if *m != value {
            *m = value;
            cx.notify();
        }
    });
}

/// Replace the current queue with the given items.
pub fn replace_queue(items: Vec<QueueItemData>, app: &mut App) {
    let playback_interface = app.global::<PlaybackInterface>();
    playback_interface.replace_queue(items);
}
