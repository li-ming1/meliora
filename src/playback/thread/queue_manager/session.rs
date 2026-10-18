//! Session 持久化去抖：队列快照/播放位置的节流持久化与挂起快照冲刷。
//! 自 queue_manager.rs 原样搬出；经 `use super::*` 白盒访问 QueueManager
//! 的私有字段与关联函数。内部方法放宽为 `pub(super)`（宿主子树可见），
//! `flush_pending_session` 维持 `pub`（thread.rs 主循环经原路径调用）。

use crate::sync::RecoveredRwLock;
use std::time::{Duration, Instant};

use super::*;

/// Minimum interval between queue-snapshot sends to the session storage worker.
/// A send deep-copies the whole queue (see `send_session_with_queue`), so burst
/// mutations (batch drops, rapid skips) coalesce into one snapshot per window.
const SESSION_PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

impl QueueManager {
    /// Persist the queue session when only playback state changed (track
    /// advance, seek, repeat/shuffle toggle).
    ///
    /// Three-way split on the shared debounce window (`last_session_send`):
    ///
    /// - `session_dirty` set (a queue snapshot is pending): upgrade to a full
    ///   send immediately. Correctness requirement — the pending snapshot
    ///   predates this state change, so a state-only patch applied on top of
    ///   it could persist a position referencing the old queue contents
    ///   ("new position, stale queue"). The full snapshot carries both the
    ///   fresh queue and this state change.
    /// - Inside the window: only set `state_dirty`; `flush_pending_session`
    ///   coalesces the burst (rapid skips) into one state-only send.
    ///   Deferring is safe: the watch value always holds the newest state
    ///   atop the stored queue snapshot, so consecutive state updates merge
    ///   losslessly, and the crash exposure window (≤500ms of stale position)
    ///   matches what queue mutations already accept.
    /// - Outside the window: send immediately (leading edge, state-only
    ///   cheap path — the stored queue snapshot is reused untouched) and
    ///   occupy the window, so a queue mutation right after coalesces into
    ///   one snapshot instead of a state write plus a full write.
    pub(super) fn persist_session_state(&mut self) {
        if self.session_dirty {
            self.send_session_with_queue();
            return;
        }

        let due = self
            .last_session_send
            .is_none_or(|sent| sent.elapsed() >= SESSION_PERSIST_DEBOUNCE);
        if due {
            self.send_session_state_only();
        } else {
            self.state_dirty = true;
        }
    }

    /// State-only session send: reuse the stored queue snapshot and refresh
    /// position/shuffle/repeat, then clear `state_dirty` and occupy the
    /// debounce window. Far cheaper than `send_session_with_queue` on the
    /// sender side (no queue/original_queue refcount churn); the storage
    /// worker still rewrites the file per notification, which is exactly
    /// what the window coalesces.
    fn send_session_state_only(&mut self) {
        let queue_position = self.current_position();
        let shuffle = self.shuffle;
        let repeat = self.repeat;

        self.storage_tx.send_modify(|session| {
            session.queue_position = queue_position;
            session.shuffle = shuffle;
            session.repeat = repeat;
        });

        self.state_dirty = false;
        self.last_session_send = Some(Instant::now());
    }

    /// Persist the queue session when queue contents or ordering changed.
    ///
    /// Sends are debounced: the first mutation after an idle window sends
    /// immediately (leading edge), further mutations within the window only
    /// mark the session dirty and are flushed by `flush_pending_session` from
    /// the playback main loop. Sending snapshots the whole queue, so this keeps
    /// batch mutations at one snapshot per window instead of one per item.
    ///
    /// The window anchor is shared with the state-only path
    /// (`persist_session_state`): a state send also occupies the window, so a
    /// queue mutation right after it is deferred into one coalesced snapshot
    /// instead of being split into a state write plus a full write.
    pub(super) fn persist_session_with_queue(&mut self) {
        let due = self
            .last_session_send
            .is_none_or(|sent| sent.elapsed() >= SESSION_PERSIST_DEBOUNCE);
        if due {
            self.send_session_with_queue();
        } else {
            self.session_dirty = true;
        }
    }

    /// Flush pending session sends if any. Cheap no-op otherwise; called
    /// every playback main-loop iteration. Three tiers, checked in order:
    ///
    /// 1. `session_dirty` — a full queue snapshot is owed (its leading-edge
    ///    send was window-blocked). The snapshot carries the current state
    ///    too, so a pending `state_dirty` is subsumed and cleared with it.
    /// 2. `state_dirty` only — playback state changed within the window; a
    ///    state-only send refreshes position/shuffle/repeat on the stored
    ///    queue snapshot.
    /// 3. Neither — nothing to do.
    pub fn flush_pending_session(&mut self) {
        if self.session_dirty {
            self.send_session_with_queue();
        } else if self.state_dirty {
            self.send_session_state_only();
        }
    }

    /// Take and send a consistent queue snapshot to the session storage worker.
    ///
    /// With the copy-on-write queue this is two refcount bumps under the read
    /// lock — the worker serializes from its own ref after the lock is
    /// released. Call sites debounce to keep it off burst paths.
    ///
    /// The full snapshot always covers the playback state, so both dirty
    /// flags clear here and no pending state-only update can survive it.
    pub(super) fn send_session_with_queue(&mut self) {
        let queue = self.queue.read_recovered();
        let queue_snapshot = Arc::clone(&queue);
        let queue_position = Self::cursor_index(queue.len(), self.queue_next);
        drop(queue);

        let original_queue = self.original_queue.clone();
        let shuffle = self.shuffle;
        let repeat = self.repeat;

        self.storage_tx.send_modify(move |session| {
            session.queue = queue_snapshot;
            session.original_queue = original_queue;
            session.queue_position = queue_position;
            session.shuffle = shuffle;
            session.repeat = repeat;
        });

        self.session_dirty = false;
        self.state_dirty = false;
        self.last_session_send = Some(Instant::now());
    }
}
