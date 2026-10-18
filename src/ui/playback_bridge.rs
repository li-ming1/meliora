//! UI-side consumer of the playback thread's event channel: the broadcast
//! loop that mirrors playback events into the `Models` / `PlaybackInfo`
//! entities, plus the frame-interval atlas-tile drains owned by the image
//! reclaim funnel. Moved verbatim from `playback::interface` (architecture
//! audit: the playback layer must not import `ui::models`); the interface
//! itself stays a pure command sender.

use std::sync::Mutex;
use std::time::Duration;

use gpui::{App, AsyncApp, Entity};

use crate::library::Pool;
use crate::sync::RecoveredMutex;
use crate::{
    playback::{events::PlaybackEvent, interface::PlaybackInterface, thread::PlaybackState},
    power::PowerManager,
    ui::models::{CurrentTrack, ImageEvent, Models, PlaybackInfo},
};

/// Volume echoes only update `prev_volume` above this floor: the remembered
/// value must stay visible in the UI while volume is toggled on/off, even if
/// the user slid it all the way to `0`.
const MIN_REMEMBERED_VOLUME: f64 = 0.05;

/// Cadence of the idle atlas-tile drain spawned by [`start_broadcast`]: keeps
/// evicted tiles reclaimed while playback is paused (the event loop only
/// drains when events arrive).
const IDLE_TILE_DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// Starts the broadcast loop that will read events from the playback thread and update data
/// models accordingly. This function should be called once, and will panic if called more than
/// once.
///
/// The bridge takes advantage of GPUI's asynchronous runtime to read messages without blocking
/// rendering. Events are read via `recv().await` as soon as they arrive; a backlog left by a busy
/// frame is coalesced to the newest event of each kind, though the executor may still delay the
/// wakeup under load. Because of this, event handling should not perform any heavy operations,
/// which should be instead sent to the data thread for any required additional processing.
pub fn start_broadcast(interface: &mut PlaybackInterface, app: &mut App) {
    // This function's sole responsibility is to read events from the playback thread and update
    // data models accordingly.
    let Some(mut events_rx) = interface.take_events_rx() else {
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
                        let (track_key, meta) = crate::stats::song_info(&path, &queue_model, cx);
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
                        queue_model.update(cx, |m, cx| {
                            // Content-change signal for the queue panel's
                            // observer (models::Queue::content_epoch): bumped
                            // here — the single funnel where playback events
                            // become queue-entity writes — so the panel can
                            // skip its O(n) view/summary rescan on
                            // QueuePositionChanged-only notifications (every
                            // track switch). Path-only swaps that ride without
                            // a QueueUpdated (queue_manager's
                            // replace_current_path / replace_next_path) never
                            // notified the UI before either and change no uid
                            // and no duration, so the epoch tracks exactly the
                            // notifications the rescan responded to.
                            m.content_epoch = m.content_epoch.wrapping_add(1);
                            cx.notify();
                        });
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
                    PlaybackEvent::QueuePositionChanged(v) => queue_model.update(cx, |m, cx| {
                        if m.position != v {
                            m.position = v;
                            cx.notify();
                        }
                    }),
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

/// Feeds one recording step to the shared stats recorder and flushes the
/// rows it produced. A poisoned lock is recovered from: a panic inside an
/// earlier recording step must not wedge the broadcast loop.
fn record_and_flush<T>(
    stats: &Mutex<T>,
    pool: sqlx::SqlitePool,
    record: impl FnOnce(&mut T) -> Vec<crate::stats::ListenRow>,
) {
    let rows = record(&mut stats.lock_recovered());
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
