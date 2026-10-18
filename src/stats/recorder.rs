use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::playback::thread::PlaybackState;

use super::TrackMeta;

/// 此刻的 Unix 秒。
pub(crate) fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 一段已完成、可直接持久化的收听区间。
pub struct ListenRow {
    /// 这一行收听开始的 Unix 秒。
    pub ts: i64,
    pub track_key: String,
    pub seconds: i64,
    /// 切歌时抓到的展示元数据；本地 tracks 为 `None`，它们在写入时从音乐库解析。
    pub meta: Option<TrackMeta>,
}

/// 播放期间按这个大小为一档刷写已完成的行，于是 DB 写入节奏是每分钟收听一个小批次。
const ROW_SECONDS: i64 = 60;
/// 单个位置事件最多计入这么多墙钟时间。它给不可用的位置增量（跳转定位、单曲循环的
/// 回绕点）封顶，并扛过进程挂起之后的时间空洞。
const MAX_TRUST_MS: i64 = 2_000;

/// 纯累加逻辑：在播放期间对广播的位置事件做差分，用墙钟时间钳掉异常值，并产出
/// 已完成的行。
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
    /// 当前未刷写那一行开始的 Unix 时间戳。
    row_start_ts: i64,
    row_ms: i64,
}

impl ActiveTrack {
    /// 把迄今累出的 `seconds` 组成已完成的那一行。
    fn listen_row(&self, seconds: i64) -> ListenRow {
        ListenRow {
            ts: self.row_start_ts,
            track_key: self.key.clone(),
            seconds,
            meta: self.meta.clone(),
        }
    }
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

    fn on_state_at(&mut self, state: PlaybackState, ts: i64) {
        if state == PlaybackState::Playing {
            // 基线重置：状态切换之后的第一个位置事件会重建它，因此在暂停期间做的
            // 跳转定位永远不会被计入。
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

    fn on_song_changed_at(&mut self, track_key: String, meta: Option<TrackMeta>, ts: i64) {
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

    /// 刷写当前活跃曲目的半行并重置位置基线。用于暂停、停止、切歌与退出应用。
    fn finalize(&mut self, ts: i64) {
        if let Some(mut cur) = self.current.take() {
            let secs = cur.row_ms / 1000;
            if secs > 0 {
                self.pending.push(cur.listen_row(secs));
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
            // 只有当位置增量与墙钟时间一致时才可信；其他情形（跳转定位、回绕点、
            // 过期 tick）改按墙钟的那一段计入。
            let counted = if delta_ms < 0 || delta_ms > wall_ms {
                wall_ms.min(MAX_TRUST_MS)
            } else {
                delta_ms
            };
            if counted > 0 {
                cur.row_ms += counted;
                if cur.row_ms >= ROW_SECONDS * 1000 {
                    let secs = cur.row_ms / 1000;
                    let row = cur.listen_row(secs);
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
        // 62 次各 1 秒的 tick，墙钟间隔也是 1 秒：第一次建立基线，累计 61 秒，
        // 60 秒成一行。
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
        rec.accumulate(1000, 1000); // 建立基线
        rec.accumulate(60_000, 1000); // +1 s
        rec.accumulate(5_000, 250); // 回绕点 / 向后跳转
        rec.on_state_at(PlaybackState::Paused, 4);
        assert_eq!(pending_secs(&mut rec), 1); // 1000 + 1000 + 250 ms
    }

    #[test]
    fn seek_forward_is_capped_at_wall() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.accumulate(1000, 1000); // 建立基线
        rec.accumulate(120_000, 250); // +2 min 跳转，墙钟只有 250 ms
        rec.accumulate(121_000, 1000); // 正常 tick
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
        rec.accumulate(30_000, 1000); // 基线 tick
        rec.accumulate(31_000, 1000);
        rec.on_state_at(PlaybackState::Paused, 300);
        assert_eq!(pending_secs(&mut rec), 1);
    }

    #[test]
    fn repeat_one_loop_jump_keeps_counting() {
        let mut rec = StatsRecorder::new();
        playing_with_song(&mut rec, 0);
        rec.accumulate(1000, 1000); // 建立基线
        rec.accumulate(2_000, 1000);
        // 引擎内部循环：位置跳回 0，且没有发出 SongChanged。
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
