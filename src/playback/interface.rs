use gpui::App;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{
    playback::{dsp::spectrum::SpectrumTapConsumer, events::RepeatState},
    power::PowerManager,
    settings::{equalizer::EqualizerSettings, playback::PlaybackSettings},
    ui::{app::Pool, models::{CurrentTrack, ImageEvent, Models, PlaybackInfo}},
};

use super::{
    events::{PlaybackCommand, PlaybackEvent},
    queue::QueueItemData,
    thread::PlaybackState,
};

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

    pub fn play(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::Pause);
    }

    pub fn queue(&self, item: QueueItemData) {
        let _ = self.cmd_tx.send(PlaybackCommand::Queue(item));
    }

    pub fn queue_list(&self, items: Vec<QueueItemData>) {
        let _ = self.cmd_tx.send(PlaybackCommand::QueueList(items));
    }

    pub fn insert_at(&self, item: QueueItemData, position: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::InsertAt { item, position });
    }

    pub fn insert_list_at(&self, items: Vec<QueueItemData>, position: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::InsertListAt { items, position });
    }

    pub fn next(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::Next);
    }

    pub fn previous(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::Previous);
    }

    pub fn clear_queue(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::ClearQueue);
    }

    pub fn jump(&self, index: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::Jump(index));
    }

    pub fn seek(&self, position: f64) {
        let _ = self.cmd_tx.send(PlaybackCommand::Seek(position));
    }

    pub fn set_volume(&self, volume: f64) {
        let _ = self.cmd_tx.send(PlaybackCommand::SetVolume(volume));
    }

    pub fn replace_queue(&self, items: Vec<QueueItemData>) {
        let _ = self.cmd_tx.send(PlaybackCommand::ReplaceQueue(items));
    }

    pub fn replace_queue_with_index(&self, items: Vec<QueueItemData>, idx: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::ReplaceQueueWithIndex(items, idx));
    }

    pub fn toggle_stop_after_current(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::StopAfterCurrent);
    }

    pub fn toggle_shuffle(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::ToggleShuffle);
    }

    pub fn set_repeat(&self, state: RepeatState) {
        let _ = self.cmd_tx.send(PlaybackCommand::SetRepeat(state));
    }

    pub fn remove_item(&self, idx: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::RemoveItem(idx));
    }

    pub fn remove_items(&self, indices: Vec<usize>) {
        let _ = self.cmd_tx.send(PlaybackCommand::RemoveItems(indices));
    }

    pub fn move_item(&self, from: usize, to: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::MoveItem { from, to });
    }

    pub fn move_items(&self, indices: Vec<usize>, to: usize) {
        let _ = self.cmd_tx.send(PlaybackCommand::MoveItems { indices, to });
    }

    pub fn undo(&self) {
        let _ = self.cmd_tx.send(PlaybackCommand::Undo);
    }

    pub fn update_settings(&self, settings: PlaybackSettings) {
        let _ = self.cmd_tx.send(PlaybackCommand::SettingsChanged(settings));
    }

    pub fn set_equalizer(&self, settings: EqualizerSettings) {
        let _ = self.cmd_tx.send(PlaybackCommand::SetEqualizer(settings));
    }

    pub fn set_position_broadcast_active(&self, active: bool) {
        let _ = self.cmd_tx.send(PlaybackCommand::SetPositionBroadcastActive(active));
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
        let mut events_rx = None;
        std::mem::swap(&mut self.events_rx, &mut events_rx);

        let metadata_model = app.global::<Models>().metadata.clone();
        let albumart_model = app.global::<Models>().albumart.clone();
        let queue_model = app.global::<Models>().queue.clone();

        let playback_info = app.global::<PlaybackInfo>().clone();
        let power_manager = app.global::<PowerManager>().clone();
        let stats = app.global::<crate::stats::StatsHandle>().0.clone();
        let stats_pool = app.global::<Pool>().0.clone();

        let Some(mut events_rx) = events_rx else {
            panic!("broadcast thread already started");
        };

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
                            metadata_model.update(cx, |m, cx| {
                                *m = *v;
                                cx.notify()
                            });
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
                            playback_info.playback_state.update(cx, |m, cx| {
                                *m = v;
                                cx.notify()
                            });

                            if v == PlaybackState::Stopped {
                                playback_info.current_track.update(cx, |m, cx| {
                                    *m = None;
                                    cx.notify()
                                });
                            }

                            power_manager.set_state(cx, v);

                            let rows = {
                                let mut recorder = stats.lock().unwrap_or_else(|e| e.into_inner());
                                recorder.on_state(v);
                                recorder.take_pending()
                            };
                            crate::stats::flush_rows_async(stats_pool.clone(), rows);
                        }
                        PlaybackEvent::PositionChanged(v) => {
                            playback_info.position.update(cx, |m, cx| {
                                *m = v;
                                cx.notify()
                            });

                            let rows = {
                                let mut recorder = stats.lock().unwrap_or_else(|e| e.into_inner());
                                recorder.on_position(v);
                                recorder.take_pending()
                            };
                            crate::stats::flush_rows_async(stats_pool.clone(), rows);
                        }
                        PlaybackEvent::DurationChanged(v) => {
                            playback_info.duration.update(cx, |m, cx| {
                                *m = v;
                                cx.notify()
                            });
                        }
                        PlaybackEvent::SongChanged(path) => {
                            let (track_key, meta) =
                                crate::stats::song_info(&path, &queue_model, cx);
                            let rows = {
                                let mut recorder = stats.lock().unwrap_or_else(|e| e.into_inner());
                                recorder.on_song_changed(track_key, meta);
                                recorder.take_pending()
                            };
                            crate::stats::flush_rows_async(stats_pool.clone(), rows);

                            playback_info.current_track.update(cx, |m, cx| {
                                *m = Some(CurrentTrack::new(path.clone()));
                                cx.notify()
                            });
                        }
                        PlaybackEvent::QueueUpdated => {
                            queue_model.update(cx, |_, cx| cx.notify());
                        }
                        PlaybackEvent::ShuffleToggled(v) => {
                            playback_info.shuffling.update(cx, |m, cx| {
                                *m = v;
                                cx.notify()
                            });
                        }
                        PlaybackEvent::VolumeChanged(v) => {
                            // equality guard: during a slider drag the echo-back
                            // notify would re-render the source slider per tick
                            playback_info.volume.update(cx, |m, cx| {
                                if *m != v {
                                    *m = v;
                                    cx.notify();
                                }
                            });

                            // Note: `prev_volume` should not be to small.
                            // Its value needs to be visible in UI
                            // while toggling volume `on` / `off` and even
                            // an user used a slider to move volume to `0`
                            if v > 0.05 {
                                playback_info.prev_volume.update(cx, |m, cx| {
                                    if *m != v {
                                        *m = v;
                                        cx.notify();
                                    }
                                });
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
                            playback_info.repeating.update(cx, |m, cx| {
                                if *m != v {
                                    *m = v;
                                    cx.notify();
                                }
                            })
                        }
                        PlaybackEvent::StopAfterCurrentChanged(v) => {
                            playback_info.stop_after_current.update(cx, |m, cx| {
                                if *m != v {
                                    *m = v;
                                    cx.notify();
                                }
                            })
                        }
                        PlaybackEvent::SampleRateChanged(rate) => {
                            playback_info.sample_rate.update(cx, |m, cx| {
                                if *m != rate {
                                    *m = rate;
                                    cx.notify();
                                }
                            })
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
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                cx.update(crate::ui::components::managed_image::drain_pending_tile_drops);
            }
        })
        .detach();
    }
}

/// Replace the current queue with the given items.
pub fn replace_queue(items: Vec<QueueItemData>, app: &mut App) {
    let playback_interface = app.global::<PlaybackInterface>();
    playback_interface.replace_queue(items);
}
