//! Session 持久化去抖：队列快照/播放位置的节流持久化与挂起快照冲刷。
//! 自 queue_manager.rs 原样搬出；经 `use super::*` 白盒访问 QueueManager
//! 的私有字段与关联函数。内部方法放宽为 `pub(super)`（宿主子树可见），
//! `flush_pending_session` 维持 `pub`（thread.rs 主循环经原路径调用）。

use std::time::{Duration, Instant};

use super::*;

/// Minimum interval between queue-snapshot sends to the session storage worker.
/// A send deep-copies the whole queue (see `send_session_with_queue`), so burst
/// mutations (batch drops, rapid skips) coalesce into one snapshot per window.
const SESSION_PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

impl QueueManager {
    /// Persist the queue session when only playback state changed.
    ///
    /// This reuses the stored queue snapshot and updates fields like
    /// the current position, shuffle mode, and repeat mode. A pending
    /// debounced queue snapshot is upgraded to a full send so the stored
    /// position never references stale queue contents.
    pub(super) fn persist_session_state(&mut self) {
        if self.session_dirty {
            self.send_session_with_queue();
            return;
        }

        let queue_position = self.current_position();
        let shuffle = self.shuffle;
        let repeat = self.repeat;

        self.storage_tx.send_modify(|session| {
            session.queue_position = queue_position;
            session.shuffle = shuffle;
            session.repeat = repeat;
        });
    }

    /// Persist the queue session when queue contents or ordering changed.
    ///
    /// Sends are debounced: the first mutation after an idle window sends
    /// immediately (leading edge), further mutations within the window only
    /// mark the session dirty and are flushed by `flush_pending_session` from
    /// the playback main loop. Sending snapshots the whole queue, so this keeps
    /// batch mutations at one snapshot per window instead of one per item.
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

    /// Flush a debounced queue-session snapshot if one is pending. Cheap no-op
    /// otherwise; called every playback main-loop iteration.
    pub fn flush_pending_session(&mut self) {
        if self.session_dirty {
            self.send_session_with_queue();
        }
    }

    /// Take and send a consistent queue snapshot to the session storage worker.
    ///
    /// With the copy-on-write queue this is two refcount bumps under the read
    /// lock — the worker serializes from its own ref after the lock is
    /// released. Call sites debounce to keep it off burst paths.
    pub(super) fn send_session_with_queue(&mut self) {
        let queue = self.queue.read().unwrap_or_else(|e| e.into_inner());
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
        self.last_session_send = Some(Instant::now());
    }
}
