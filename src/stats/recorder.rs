use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::playback::thread::PlaybackState;

use super::TrackMeta;

/// Unix seconds right now.
pub(crate) fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One finished listening segment ready for persistence.
pub struct ListenRow {
    /// Unix seconds where this row's listening started.
    pub ts: i64,
    pub track_key: String,
    pub seconds: i64,
    /// Display metadata captured at song-change time; `None` for local tracks,
    /// which are resolved from the library at write time instead.
    pub meta: Option<TrackMeta>,
}

/// Completed rows are flushed in quanta of this size while playing, so the DB
/// write cadence is one small batch per minute of listening.
const ROW_SECONDS: i64 = 60;
/// Never credit more wall-clock time than this to a single position event.
/// Bounds the credit for unusable position deltas (seek jumps, repeat-one loop
/// points) and shrugs off the gap after process suspend.
const MAX_TRUST_MS: i64 = 2_000;

/// Pure accumulation logic: diffs broadcast position events while playing,
/// clamps anomalies against wall-clock time, and yields finished rows.
pub struct StatsRecorder {
    state: PlaybackState,
    current: Option<ActiveTrack>,
    last_pos_ms: Option<u64>,
    last_wall: Option<Instant>,
    pending: Vec<ListenRow>,
}

struct ActiveTrack {
    key: String,
    meta: Option<TrackMeta>,
    /// Unix ts where the current unflushed row began.
    row_start_ts: i64,
    row_ms: i64,
}

impl StatsRecorder {
    pub fn new() -> Self {
        Self {
            state: PlaybackState::Stopped,
            current: None,
            last_pos_ms: None,
            last_wall: None,
            pending: Vec::new(),
        }
    }

    pub fn on_state(&mut self, state: PlaybackState) {
        self.on_state_at(state, now_ts());
    }

    pub fn on_state_at(&mut self, state: PlaybackState, ts: i64) {
        if state == PlaybackState::Playing {
            // Baseline reset: the first position event after the transition
            // re-establishes it, so a seek made while paused is never credited.
            self.last_pos_ms = None;
            self.last_wall = None;
            self.state = state;
            return;
        }

        if self.state == PlaybackState::Playing {
            self.finalize(ts);
        }
        self.last_pos_ms = None;
        self.last_wall = None;
        if state == PlaybackState::Stopped {
            self.current = None;
        }
        self.state = state;
    }

    pub fn on_position(&mut self, pos_ms: u64) {
        let wall_ms = self.last_wall.map_or(0, |w| w.elapsed().as_millis() as i64);
        self.accumulate(pos_ms, wall_ms);
        self.last_wall = Some(Instant::now());
    }

    pub fn on_song_changed(&mut self, track_key: String, meta: Option<TrackMeta>) {
        self.on_song_changed_at(track_key, meta, now_ts());
    }

    pub fn on_song_changed_at(&mut self, track_key: String, meta: Option<TrackMeta>, ts: i64) {
        self.finalize(ts);
        self.current = Some(ActiveTrack {
            key: track_key,
            meta,
            row_start_ts: ts,
            row_ms: 0,
        });
    }

    pub fn take_pending(&mut self) -> Vec<ListenRow> {
        std::mem::take(&mut self.pending)
    }

    /// Flush the partial row of the active track and reset the position
    /// baseline. Used on pause, stop, song change and app quit.
    fn finalize(&mut self, ts: i64) {
        if let Some(mut cur) = self.current.take() {
            let secs = cur.row_ms / 1000;
            if secs > 0 {
                self.pending.push(ListenRow {
                    ts: cur.row_start_ts,
                    track_key: cur.key.clone(),
                    seconds: secs,
                    meta: cur.meta.clone(),
                });
            }
            cur.row_ms = 0;
            cur.row_start_ts = ts;
            self.current = Some(cur);
        }
        self.last_pos_ms = None;
        self.last_wall = None;
    }

    fn accumulate(&mut self, pos_ms: u64, wall_ms: i64) {
        if self.state != PlaybackState::Playing {
            return;
        }
        let Some(cur) = &mut self.current else {
            return;
        };
        if let Some(last) = self.last_pos_ms {
            let delta_ms = pos_ms as i64 - last as i64;
            // Position deltas are only trusted when they agree with wall-clock
            // time; anything else (seek, loop point, stale tick) credits the
            // wall fragment instead.
            let counted = if delta_ms < 0 || delta_ms > wall_ms {
                wall_ms.min(MAX_TRUST_MS)
            } else {
                delta_ms
            };
            if counted > 0 {
                cur.row_ms += counted;
                if cur.row_ms >= ROW_SECONDS * 1000 {
                    let secs = cur.row_ms / 1000;
                    let row = ListenRow {
                        ts: cur.row_start_ts,
                        track_key: cur.key.clone(),
                        seconds: secs,
                        meta: cur.meta.clone(),
                    };
                    cur.row_start_ts += secs;
                    cur.row_ms -= secs * 1000;
                    self.pending.push(row);
                }
            }
        }
        self.last_pos_ms = Some(pos_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "local:test.mp3";

    fn playing_with_song(rec: &mut StatsRecorder, ts: i64) {
        rec.on_state_at(PlaybackState::Playing, ts);
        rec.on_song_changed_at(KEY.to_string(), None, ts);
        rec.take_pending();
    }

    fn pending_secs(rec: &mut StatsRecorder) -> i64 {
        rec.take_pending().iter().map(|r| r.seconds).sum()
    }

    #[test]
    fn accumulates_position_deltas_into_rows() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 1_000_000);
        // 62 ticks of 1 s each with 1 s wall gap: the first establishes the
        // baseline, 61 s accumulate, 60 s complete a row.
        for i in 0..=61 {
            rec.accumulate(i * 1000, 1000);
        }
        let rows = rec.take_pending();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seconds, 60);
        assert_eq!(rows[0].ts, 1_000_000);
        assert_eq!(rows[0].track_key, KEY);
    }

    #[test]
    fn first_position_after_baseline_is_not_counted() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.accumulate(120_000, 1000);
        assert_eq!(pending_secs(&mut rec), 0);
    }

    #[test]
    fn seek_backward_credits_only_wall_fragment() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.accumulate(1000, 1000); // baseline
        rec.accumulate(60_000, 1000); // +1 s
        rec.accumulate(5_000, 250); // loop point / seek back
        rec.on_state_at(PlaybackState::Paused, 4);
        assert_eq!(pending_secs(&mut rec), 1); // 1000 + 1000 + 250 ms
    }

    #[test]
    fn seek_forward_is_capped_at_wall() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.accumulate(1000, 1000); // baseline
        rec.accumulate(120_000, 250); // +2 min jump, 250 ms wall
        rec.accumulate(121_000, 1000); // normal tick
        rec.on_state_at(PlaybackState::Paused, 3);
        assert_eq!(pending_secs(&mut rec), 1);
    }

    #[test]
    fn pause_gap_is_not_counted() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        for i in 1..=30 {
            rec.accumulate(i * 1000, 1000);
        }
        rec.on_state_at(PlaybackState::Paused, 100);
        assert_eq!(pending_secs(&mut rec), 29);

        rec.on_state_at(PlaybackState::Playing, 200);
        rec.accumulate(30_000, 1000); // baseline tick
        rec.accumulate(31_000, 1000);
        rec.on_state_at(PlaybackState::Paused, 300);
        assert_eq!(pending_secs(&mut rec), 1);
    }

    #[test]
    fn repeat_one_loop_jump_keeps_counting() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.accumulate(1000, 1000); // baseline
        rec.accumulate(2_000, 1000);
        // Engine-internal loop: position jumps back to 0 without SongChanged.
        rec.accumulate(0, 250);
        rec.accumulate(1_000, 1000);
        rec.accumulate(2_000, 1000);
        rec.on_state_at(PlaybackState::Paused, 5);
        assert_eq!(pending_secs(&mut rec), 3);
    }

    #[test]
    fn song_change_finalizes_partial_row() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        for i in 1..=45 {
            rec.accumulate(i * 1000, 1000);
        }
        rec.on_song_changed_at("local:next.mp3".to_string(), None, 100);
        let rows = rec.take_pending();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seconds, 44);
        assert_eq!(rows[0].track_key, KEY);
    }

    #[test]
    fn stop_drops_active_track() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.on_state_at(PlaybackState::Stopped, 1);
        rec.accumulate(1000, 1000);
        rec.on_state_at(PlaybackState::Paused, 3);
        assert_eq!(pending_secs(&mut rec), 0);
    }
}
