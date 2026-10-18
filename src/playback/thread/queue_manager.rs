use std::{
    collections::VecDeque,
    mem::take,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use rand::{rng, seq::SliceRandom};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::{SmallVec, smallvec};

use crate::sync::RecoveredRwLock;
use crate::{
    playback::{events::RepeatState, queue::QueueItemData, session_storage::PlaybackSessionData},
    settings::playback::PlaybackSettings,
};
// 唯一消费者 `current_display` 为 online_sources gated，与 queue.rs 里该类型
// 的 gate 保持一致。use 树嵌套段上不能挂 `#[cfg]`（rustc 判为解析错误），
// 故按全仓惯例拆为独立 use 语句。
#[cfg(feature = "online_sources")]
use crate::playback::queue::PersistedDisplay;

mod session;
mod undo;

pub use self::undo::{UndoAction, UndoResult};

const UNDO_STACK_CAPACITY: usize = 30;

/// 持全量队列快照的动作（`UndoAction::Replaced`）的独立驻留上限：快照经
/// `Arc` 共享零拷贝，但每条仍驻留一整份旧队列（10 万项可达数十 MB），
/// 30 层满栈会威胁 500MB 内存硬上限（见 AGENTS.md）。撤销整队替换的可用
/// 深度随之变浅属有意取舍；单项操作（Inserted/Moved）维持
/// UNDO_STACK_CAPACITY 不变，Removed 另受 UNDO_REMOVED_ITEM_CAP 约束。
const UNDO_SNAPSHOT_CAPACITY: usize = 6;

/// 单条 `UndoAction::Removed`（dequeue_many 批删）携带的项数上限：undo
/// 记录全量 (index, item) 实件，UNDO_STACK_CAPACITY 只限条数不限大小，
/// 一次 5 万项的批删即单条数十 MB 驻留——内存硬上限唯一无界的驻留口子。
/// 超限时不记录该动作并清空整个 undo 栈（该批删不可撤销）：LIFO 纪律下
/// 旧动作不能叠在不可撤销的大删除之后被单独 undo（其逆操作基于删除前的
/// 队列状态），整栈清空是唯一一致做法。批删不可撤销、撤销深度随之变浅
/// 同属有意取舍；恰好等于上限仍正常记录。
const UNDO_REMOVED_ITEM_CAP: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reshuffled {
    Reshuffled,
    NotReshuffled,
}

#[derive(Debug, Clone)]
pub enum QueueNavigationResult {
    /// The queue position changed.
    Changed {
        index: usize,
        path: PathBuf,
        reshuffled: Reshuffled,
    },
    /// The current track should repeat (RepeatOne mode).
    Unchanged { path: PathBuf },
    /// End of queue reached.
    EndOfQueue,
}

#[derive(Debug, Clone)]
pub enum DequeueResult {
    /// An item was removed, queue position adjusted.
    Removed { new_position: usize },
    /// The currently playing item was removed.
    RemovedCurrent {
        /// The path of the next track to play, if any.
        new_path: Option<PathBuf>,
    },
    /// Nothing changed (index out of bounds).
    Unchanged,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DequeueManyResult {
    /// Items were removed, queue position adjusted.
    Removed { new_position: usize },
    /// The currently playing item was removed.
    RemovedCurrent { new_path: Option<PathBuf> },
    /// Nothing changed (indices empty or all out of bounds).
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveResult {
    Moved,
    /// Item was moved and current position changed.
    MovedCurrent {
        new_position: usize,
    },
    /// Nothing changed (same position or invalid).
    Unchanged,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MoveItemsResult {
    Moved,
    MovedCurrent { new_position: usize },
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    /// Item(s) inserted, current position unchanged.
    Inserted { first_index: usize },
    /// Item(s) inserted and current position shifted.
    InsertedMovedCurrent {
        first_index: usize,
        new_position: usize,
    },
    /// Nothing changed (invalid position).
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShuffleResult {
    /// Shuffle was enabled.
    Shuffled,
    /// Shuffle was disabled, with the new position in the unshuffled queue.
    Unshuffled { new_position: usize },
}

/// Short-lived operation result; the size asymmetry is irrelevant because
/// values are dropped immediately after the caller inspects them.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ReplaceResult {
    /// Queue replaced, contains the first item to play.
    Replaced { first_item: Option<QueueItemData> },
    /// Queue is empty after replacement.
    Empty,
}

#[derive(Debug, Clone)]
pub enum JumpResult {
    Jumped { path: PathBuf },
    OutOfBounds,
}

/// Manages the playback queue state.
///
/// This component handles all queue operations including navigation, shuffling,
/// repeat modes, and queue mutations. It does NOT handle side effects like
/// opening tracks or emitting events - those are the responsibility of the
/// PlaybackThread.
pub struct QueueManager {
    playback_settings: PlaybackSettings,
    /// The current queue. Shared with the UI thread for display. The inner
    /// `Arc` makes session snapshots and undo snapshots refcount grabs:
    /// mutations take `Arc::make_mut` so a copy only happens when a snapshot
    /// is actually still alive (copy-on-write).
    queue: Arc<RwLock<Arc<Vec<QueueItemData>>>>,
    /// If shuffled, this holds the original (unshuffled) queue order.
    original_queue: Arc<Vec<QueueItemData>>,
    /// Whether shuffle mode is enabled.
    shuffle: bool,
    /// Index of the next track to play.
    /// If queue_next == 1, we're on track 0.
    /// If queue_next == queue.len(), we're on the last track.
    queue_next: usize,
    repeat: RepeatState,
    storage_tx: tokio::sync::watch::Sender<PlaybackSessionData>,
    undo_stack: VecDeque<UndoAction>,
    /// Queue content changed since the last session snapshot was sent.
    session_dirty: bool,
    /// Playback state (position/shuffle/repeat) changed since the last send
    /// and is waiting for the debounced state-only flush. Parallel to
    /// `session_dirty` (queue contents); shares the same
    /// `last_session_send` window anchor. See `session.rs`.
    state_dirty: bool,
    /// Instant of the last session snapshot send (debounce anchor) — the
    /// anchor is shared by the full-snapshot path and the state-only path,
    /// so a state send also occupies the window.
    last_session_send: Option<Instant>,
}

impl QueueManager {
    /// The queue index the cursor (`queue_next`) sits on, if it is on a real
    /// slot (`None` when nothing is playing or the cursor is past the end).
    fn cursor_index(queue_len: usize, queue_next: usize) -> Option<usize> {
        queue_next
            .checked_sub(1)
            .filter(|current_idx| *current_idx < queue_len)
    }

    /// The queue item the cursor sits on, if it is on a real slot.
    fn cursor_item(queue: &[QueueItemData], queue_next: usize) -> Option<QueueItemData> {
        let current_idx = Self::cursor_index(queue.len(), queue_next)?;
        Some(queue[current_idx].clone())
    }

    fn normalize_repeat_state(
        playback_settings: &PlaybackSettings,
        state: RepeatState,
    ) -> RepeatState {
        if state == RepeatState::NotRepeating && playback_settings.always_repeat {
            RepeatState::Repeating
        } else {
            state
        }
    }

    fn item_is_playable(item: &QueueItemData) -> bool {
        // reuse the shared path_playable helper so remote HTTP(S) streams
        // (kugou) are playable too, not just existing local files
        super::path_playable(item.get_path())
    }

    fn first_playable_index_in(queue: &[QueueItemData]) -> Option<usize> {
        queue.iter().position(Self::item_is_playable)
    }

    fn last_playable_index(queue: &[QueueItemData]) -> Option<usize> {
        queue.iter().rposition(Self::item_is_playable)
    }

    fn next_playable_from(queue: &[QueueItemData], start: usize) -> Option<usize> {
        (start..queue.len()).find(|idx| Self::item_is_playable(&queue[*idx]))
    }

    fn prev_playable_before(queue: &[QueueItemData], end_exclusive: usize) -> Option<usize> {
        (0..end_exclusive)
            .rev()
            .find(|idx| Self::item_is_playable(&queue[*idx]))
    }

    /// In repeat-one mode the track the cursor sits on replays instead of
    /// advancing. Returns the cursor's slot index. `None` when not
    /// repeat-one or the slot is gone/unplayable.
    fn repeat_one_target(
        repeat: RepeatState,
        queue: &[QueueItemData],
        queue_next: usize,
    ) -> Option<usize> {
        if repeat != RepeatState::RepeatingOne {
            return None;
        }
        let index = queue_next.saturating_sub(1);
        let current = queue.get(index)?;
        Self::item_is_playable(current).then_some(index)
    }

    /// Whether a navigation step reshuffled the queue (a repeat-all wrap with
    /// shuffle enabled).
    fn reshuffle_state(shuffle: bool) -> Reshuffled {
        if shuffle {
            Reshuffled::Reshuffled
        } else {
            Reshuffled::NotReshuffled
        }
    }

    /// Persist after a navigation: a reshuffling wrap changed queue contents
    /// (full snapshot), a plain advance only moved the cursor.
    fn persist_after_navigation(&mut self, result: &QueueNavigationResult) {
        if let QueueNavigationResult::Changed { reshuffled, .. } = result {
            if *reshuffled == Reshuffled::Reshuffled {
                self.persist_session_with_queue();
            } else {
                self.persist_session_state();
            }
        }
    }

    /// Shuffle mode: append `item` to `original_queue`, returning the undo
    /// range for the append (`(0, 0)` when not shuffling — `original_queue`
    /// stays empty by definition then).
    fn append_to_original_queue(&mut self, item: &QueueItemData) -> (usize, usize) {
        if self.shuffle {
            let start = self.original_queue.len();
            Arc::make_mut(&mut self.original_queue).push(item.clone());
            (start, 1)
        } else {
            (0, 0)
        }
    }

    /// Shuffle mode: append `items` to `original_queue`, returning the undo
    /// range for the append (`(0, 0)` when not shuffling).
    fn append_all_to_original_queue(&mut self, items: &[QueueItemData]) -> (usize, usize) {
        if self.shuffle {
            let start = self.original_queue.len();
            Arc::make_mut(&mut self.original_queue).extend(items.iter().cloned());
            (start, items.len())
        } else {
            (0, 0)
        }
    }

    /// Resolve an insert's effect on the cursor: items inserted before the
    /// current slot shift it right by `cursor_shift`.
    fn resolve_insert_result(&mut self, insert_pos: usize, cursor_shift: usize) -> InsertResult {
        if insert_pos < self.queue_next {
            self.queue_next += cursor_shift;
            InsertResult::InsertedMovedCurrent {
                first_index: insert_pos,
                new_position: self.queue_next.saturating_sub(1),
            }
        } else {
            InsertResult::Inserted {
                first_index: insert_pos,
            }
        }
    }

    pub fn new(
        queue: Arc<RwLock<Arc<Vec<QueueItemData>>>>,
        playback_settings: PlaybackSettings,
        session: PlaybackSessionData,
        storage_tx: tokio::sync::watch::Sender<PlaybackSessionData>,
    ) -> Self {
        let PlaybackSessionData {
            original_queue: session_original_queue,
            queue_position: session_queue_position,
            shuffle,
            repeat,
            ..
        } = session;
        let (queue_len, original_queue) = {
            let queue = queue.read_recovered();
            let queue_len = queue.len();
            let original_queue = if shuffle && session_original_queue.len() == queue_len {
                session_original_queue
            } else if shuffle {
                Arc::clone(&queue)
            } else {
                Arc::new(Vec::new())
            };

            (queue_len, original_queue)
        };
        let queue_position = session_queue_position.filter(|position| *position < queue_len);

        Self {
            repeat: Self::normalize_repeat_state(&playback_settings, repeat),
            playback_settings,
            queue,
            original_queue,
            shuffle,
            queue_next: queue_position.map_or(0, |position| position + 1),
            storage_tx,
            undo_stack: VecDeque::with_capacity(UNDO_STACK_CAPACITY),
            session_dirty: false,
            state_dirty: false,
            last_session_send: None,
        }
    }

    /// Get the current queue position (0-indexed).
    /// Returns None if no track is playing.
    pub fn current_position(&self) -> Option<usize> {
        Self::cursor_index(self.len(), self.queue_next)
    }

    /// Online-provider identity of the currently playing item, if it is an
    /// HTTP stream (used to re-fetch a fresh URL when a persisted link 403s).
    #[cfg(feature = "online_sources")]
    pub fn current_online_identity(&self) -> Option<super::super::queue::OnlineIdentity> {
        let position = self.current_position()?;
        let queue = self.queue.read_recovered();
        queue.get(position)?.online_identity().cloned()
    }

    /// Persisted display metadata `(name, artist, duration, cover)` of the
    /// currently playing item, for stream-registry re-registration.
    #[cfg(feature = "online_sources")]
    pub fn current_display(&self) -> Option<PersistedDisplay> {
        let position = self.current_position()?;
        let queue = self.queue.read_recovered();
        queue.get(position)?.persisted_display()
    }

    /// Path of the currently playing item, if any.
    #[cfg(feature = "online_sources")]
    pub fn current_path(&self) -> Option<PathBuf> {
        let position = self.current_position()?;
        let queue = self.queue.read_recovered();
        Some(queue.get(position)?.get_path().clone())
    }

    /// Swaps the stored path of the currently playing item (used when its
    /// expired stream URL is replaced by a freshly fetched one).
    #[cfg(feature = "online_sources")]
    pub fn replace_current_path(&self, path: PathBuf) -> bool {
        // Resolve the position before taking the write lock: computing it
        // reads the queue, and a read inside a held write lock self-deadlocks.
        let Some(position) = self.current_position() else {
            return false;
        };
        let Ok(mut guard) = self.queue.write() else {
            return false;
        };
        match Arc::make_mut(&mut guard).get_mut(position) {
            Some(item) => {
                item.replace_path(path);
                true
            }
            None => false,
        }
    }

    /// Swaps the stored path of the item `peek_next_path` resolves to — the
    /// write-back of a gapless prepare that recovered the next entry's
    /// expired stream URL on the background thread. Only fires while that
    /// slot still holds `expected`, the path the prepare was requested with:
    /// between the request and its completion the resolved "next" can move
    /// (a repeat-mode toggle re-aims `next` at the current slot), and
    /// stamping the fresh URL onto a different entry would corrupt its path.
    /// On any mismatch the fresh URL is silently dropped — the transition's
    /// own synchronous refresh retry still recovers it. Like
    /// [`Self::replace_current_path`], the swap itself does not mark the
    /// session dirty; the refreshed URL rides along with the next snapshot.
    pub fn replace_next_path(&self, expected: &Path, fresh: PathBuf) -> bool {
        let index = {
            let queue = self.queue.read_recovered();
            match self.peek_next_index(&queue) {
                Some(index) if queue[index].get_path().as_path() == expected => index,
                _ => return false,
            }
        };
        let Ok(mut guard) = self.queue.write() else {
            return false;
        };
        match Arc::make_mut(&mut guard).get_mut(index) {
            Some(item) => {
                item.replace_path(fresh);
                true
            }
            None => false,
        }
    }

    /// Get the current repeat state.
    pub fn repeat_state(&self) -> RepeatState {
        self.repeat
    }

    /// Get the queue length.
    pub fn len(&self) -> usize {
        self.queue.read_recovered().len()
    }

    /// Returns true when shuffle mode is enabled.
    pub fn is_shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    /// Returns true if every queued item belongs to the same known album.
    pub fn all_items_same_album(&self) -> bool {
        let queue = self.queue.read_recovered();
        let Some(first_album) = queue.first().and_then(QueueItemData::get_db_album_id) else {
            return false;
        };

        queue
            .iter()
            .all(|item| item.get_db_album_id() == Some(first_album))
    }

    /// Get the first playable item's path and index without cloning the item.
    pub fn first_playable_path(&self) -> Option<(PathBuf, usize)> {
        let queue = self.queue.read_recovered();
        Self::first_playable_index_in(&queue).map(|index| (queue[index].get_path().clone(), index))
    }

    /// Get the last playable item's path and index without cloning the item.
    pub fn last_playable_path(&self) -> Option<(PathBuf, usize)> {
        let queue = self.queue.read_recovered();
        Self::last_playable_index(&queue).map(|index| (queue[index].get_path().clone(), index))
    }

    /// Index of the first playable item, for callers that need only the slot.
    pub fn first_playable_index(&self) -> Option<usize> {
        let queue = self.queue.read_recovered();
        Self::first_playable_index_in(&queue)
    }

    /// Set the queue position directly (used after opening a track).
    pub fn set_position(&mut self, index: usize) {
        self.queue_next = index + 1;
        self.persist_session_state();
    }

    /// Set the repeat state.
    pub fn set_repeat(&mut self, state: RepeatState) {
        self.repeat = Self::normalize_repeat_state(&self.playback_settings, state);
        self.persist_session_state();
    }

    /// Update playback settings.
    pub fn update_settings(&mut self, settings: PlaybackSettings) {
        self.playback_settings = settings;

        if self.playback_settings.always_repeat && self.repeat == RepeatState::NotRepeating {
            self.repeat = RepeatState::Repeating;
            self.persist_session_state();
        }
    }

    /// Advance to the next track in the queue.
    ///
    /// Returns information about what track to play next, or if playback should stop.
    ///
    /// Navigation is three-phase: snapshot the queue under the read lock,
    /// decide outside all locks, apply. The playability scans call
    /// `path.exists()` — a stat that reaches ~100 ms on a stalled UNC/SMB
    /// mount and must never run under the write lock the UI queue/lyrics
    /// panels read every frame. The split is safe because the playback thread
    /// is the queue's only mutator: between snapshot and apply the state
    /// cannot drift, so indices decided on the snapshot stay valid against
    /// the live queue.
    pub fn next(&mut self, user_initiated: bool) -> QueueNavigationResult {
        let (queue, queue_next) = {
            let queue = self.queue.read_recovered();
            (Arc::clone(&queue), self.queue_next)
        };

        // Decide, outside all locks.
        if !user_initiated
            && let Some(index) = Self::repeat_one_target(self.repeat, &queue, queue_next)
        {
            return QueueNavigationResult::Unchanged {
                path: queue[index].get_path().clone(),
            };
        }

        // (landing index + result reshuffle flag, plus a reshuffled queue to
        // write back in the apply phase)
        let (new_queue, landing) = if let Some(index) = Self::next_playable_from(&queue, queue_next)
        {
            (None, Some((index, Reshuffled::NotReshuffled)))
        } else if self.repeat == RepeatState::Repeating {
            // Repeat-all wrap. With shuffle on the whole queue is reshuffled
            // first: shuffle a local copy here and write it back below (the
            // `replace_queue` snapshot → work outside the lock → write-back
            // pattern). Playability is order-independent, so the landing scan
            // runs on that copy without re-statting under the write lock.
            let new_queue = if self.shuffle {
                let mut reshuffled = (*queue).clone();
                reshuffled.shuffle(&mut rng());
                Some(Arc::new(reshuffled))
            } else {
                None
            };
            let landing = Self::first_playable_index_in(new_queue.as_deref().unwrap_or(&queue))
                .map(|index| (index, Self::reshuffle_state(self.shuffle)));
            (new_queue, landing)
        } else {
            (None, None)
        };

        let landing_path = landing.as_ref().map(|(index, _)| {
            new_queue.as_deref().unwrap_or(&queue)[*index]
                .get_path()
                .clone()
        });

        // Apply, keeping the old in-lock ordering: queue contents first, then
        // the undo history, then the cursor.
        if let Some(new_queue) = new_queue {
            let mut queue = self.queue.write_recovered();
            *queue = new_queue;
            drop(queue);
            // The wrap reshuffles the whole queue, so every recorded undo
            // entry's indices refer to a stale ordering; applying them later
            // would remove or re-insert the wrong items.
            self.undo_stack.clear();
        }

        let result = if let Some((index, reshuffled)) = landing {
            self.queue_next = index + 1;
            QueueNavigationResult::Changed {
                index,
                path: landing_path.expect("landing index implies a path"),
                reshuffled,
            }
        } else {
            QueueNavigationResult::EndOfQueue
        };

        self.persist_after_navigation(&result);

        result
    }

    /// The queue slot `peek_next_path` resolves to, decided on the given
    /// queue snapshot: the repeat-one target's slot when repeat-one is
    /// active, else the next playable item, else the repeat-all wrap's
    /// landing slot (first playable) when shuffle is off. Kept in lockstep
    /// with `next(false)`'s landing decision so the gapless prepare request,
    /// its URL-refresh hook and the refreshed-path write-back all address
    /// the same entry. `None` when the advance is not predictable (queue end
    /// without repeat, repeat-wrap with shuffle reshuffles).
    fn peek_next_index(&self, queue: &[QueueItemData]) -> Option<usize> {
        Self::repeat_one_target(self.repeat, queue, self.queue_next)
            .or_else(|| Self::next_playable_from(queue, self.queue_next))
            // Repeat-all wrap without shuffle: `next(false)` reshuffles
            // nothing and lands on the first playable slot, so the advance is
            // predictable and the gapless prepare must cover it too — the
            // wrap used to fall into a cold open with a possibly-expired
            // persisted URL, the only audible track gap left in online
            // playback.
            .or_else(|| {
                if self.repeat == RepeatState::Repeating && !self.shuffle {
                    Self::first_playable_index_in(queue)
                } else {
                    None
                }
            })
    }

    /// The path `next(false)` would open, without advancing any state (used
    /// by gapless pre-open). Returns `None` when the advance is not
    /// predictable (queue end, repeat-wrap with shuffle reshuffles).
    pub fn peek_next_path(&self) -> Option<PathBuf> {
        let queue = self.queue.read_recovered();
        self.peek_next_index(&queue)
            .map(|index| queue[index].get_path().clone())
    }

    /// Online-provider identity of the item `peek_next_path` would return,
    /// if it carries one (lets the gapless prepare arm its on-error hook with
    /// a URL refresh for the *next* entry, not the current one).
    #[cfg(feature = "online_sources")]
    pub fn peek_next_online_identity(&self) -> Option<super::super::queue::OnlineIdentity> {
        let queue = self.queue.read_recovered();
        let index = self.peek_next_index(&queue)?;
        queue.get(index)?.online_identity().cloned()
    }

    /// Persisted display metadata `(name, artist, duration, cover)` of the
    /// item `peek_next_path` would return, for stream-registry
    /// re-registration during the prepare's URL refresh.
    #[cfg(feature = "online_sources")]
    pub fn peek_next_display(&self) -> Option<PersistedDisplay> {
        let queue = self.queue.read_recovered();
        let index = self.peek_next_index(&queue)?;
        queue.get(index)?.persisted_display()
    }

    /// Go to the previous track in the queue.
    ///
    /// Three-phase like `next()`: snapshot under the read lock, decide outside
    /// all locks, apply.
    pub fn previous(&mut self) -> QueueNavigationResult {
        let (queue, queue_next) = {
            let queue = self.queue.read_recovered();
            (Arc::clone(&queue), self.queue_next)
        };

        let (new_queue, landing) = if queue_next > 1
            && let Some(index) = Self::prev_playable_before(&queue, queue_next - 1)
        {
            (None, Some((index, Reshuffled::NotReshuffled)))
        } else if self.repeat == RepeatState::Repeating && !queue.is_empty() {
            let new_queue = if self.shuffle {
                let mut reshuffled = (*queue).clone();
                reshuffled.shuffle(&mut rng());
                Some(Arc::new(reshuffled))
            } else {
                None
            };
            // Same stale-undo concern as the wrap in `next()`: recorded
            // indices refer to the pre-reshuffle ordering. Cleared together
            // with the write-back in the apply phase.
            let landing = Self::last_playable_index(new_queue.as_deref().unwrap_or(&queue))
                .map(|index| (index, Self::reshuffle_state(self.shuffle)));
            (new_queue, landing)
        } else {
            (None, None)
        };

        let landing_path = landing.as_ref().map(|(index, _)| {
            new_queue.as_deref().unwrap_or(&queue)[*index]
                .get_path()
                .clone()
        });

        if let Some(new_queue) = new_queue {
            let mut queue = self.queue.write_recovered();
            *queue = new_queue;
            drop(queue);
            self.undo_stack.clear();
        }

        let result = if let Some((index, reshuffled)) = landing {
            self.queue_next = index + 1;
            QueueNavigationResult::Changed {
                index,
                path: landing_path.expect("landing index implies a path"),
                reshuffled,
            }
        } else {
            QueueNavigationResult::EndOfQueue
        };

        self.persist_after_navigation(&result);

        result
    }

    /// Jump to a specific index in the queue.
    pub fn jump(&mut self, index: usize) -> JumpResult {
        let queue = self.queue.read_recovered();

        if index < queue.len() && Self::item_is_playable(&queue[index]) {
            let path = queue[index].get_path().clone();
            drop(queue);
            self.queue_next = index + 1;
            self.persist_session_state();
            JumpResult::Jumped { path }
        } else {
            JumpResult::OutOfBounds
        }
    }

    /// Jump to an index in the original (unshuffled) queue.
    /// If not shuffled, behaves like regular jump.
    pub fn jump_unshuffled(&mut self, index: usize) -> JumpResult {
        if !self.shuffle {
            return self.jump(index);
        }

        let Some(original_item) = self.original_queue.get(index).cloned() else {
            return JumpResult::OutOfBounds;
        };

        let queue = self.queue.read_recovered();
        let pos = queue.iter().position(|item| item == &original_item);
        drop(queue);

        match pos {
            Some(shuffled_index) => self.jump(shuffled_index),
            None => JumpResult::OutOfBounds,
        }
    }

    /// Add a single item to the end of the queue.
    ///
    /// Returns the index where the item was added.
    pub fn queue_item(&mut self, item: QueueItemData) -> usize {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;
        let original_queue_range = self.append_to_original_queue(&item);

        let mut queue = self.queue.write_recovered();
        Arc::make_mut(&mut queue).push(item);

        let index = queue.len() - 1;

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_range: (index, 1),
            original_queue_range,
            previous_queue_next,
            previous_shuffle,
        });

        index
    }

    /// Add multiple items to the end of the queue.
    ///
    /// If shuffle is enabled, the new items are shuffled before being added.
    /// Returns the index of the first item added.
    pub fn queue_items(&mut self, items: Vec<QueueItemData>) -> usize {
        if items.is_empty() {
            return self.len();
        }

        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;
        let original_queue_range = self.append_all_to_original_queue(&items);

        let mut queue = self.queue.write_recovered();
        let first_index = queue.len();
        let items_len = items.len();

        if self.shuffle {
            let mut shuffled = items;
            shuffled.shuffle(&mut rng());
            Arc::make_mut(&mut queue).extend(shuffled);
        } else {
            Arc::make_mut(&mut queue).extend(items);
        }

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_range: (first_index, items_len),
            original_queue_range,
            previous_queue_next,
            previous_shuffle,
        });

        first_index
    }

    /// Insert a single item at a specific position.
    pub fn insert_item(&mut self, position: usize, item: QueueItemData) -> InsertResult {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;
        let original_queue_range = self.append_to_original_queue(&item);

        let mut queue = self.queue.write_recovered();

        let insert_pos = position.min(queue.len());
        Arc::make_mut(&mut queue).insert(insert_pos, item);

        drop(queue);

        let result = self.resolve_insert_result(insert_pos, 1);

        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_range: (insert_pos, 1),
            original_queue_range,
            previous_queue_next,
            previous_shuffle,
        });

        result
    }

    /// Insert multiple items at a specific position.
    pub fn insert_items(&mut self, position: usize, items: Vec<QueueItemData>) -> InsertResult {
        if items.is_empty() {
            return InsertResult::Unchanged;
        }

        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;
        let original_queue_range = self.append_all_to_original_queue(&items);

        let mut queue = self.queue.write_recovered();

        let insert_pos = position.min(queue.len());
        let items_len = items.len();
        Arc::make_mut(&mut queue).splice(insert_pos..insert_pos, items);

        drop(queue);

        let result = self.resolve_insert_result(insert_pos, items_len);

        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_range: (insert_pos, items_len),
            original_queue_range,
            previous_queue_next,
            previous_shuffle,
        });

        result
    }

    /// Remove an item from the queue at the specified index.
    pub fn dequeue(&mut self, index: usize) -> DequeueResult {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        // 移除本身在写锁内完成；随后的下一可播项扫描会做 path.exists() 同步
        // stat（UNC/SMB 上一次可达百 ms 级），必须放在写锁外——这把写锁被
        // UI 队列面板/歌词面板每帧读取。drop 写锁后 `post` 就是准确的移除后
        // 状态：播放线程是队列唯一 mutator，两段之间不存在状态漂移（见
        // `next()` 的三段化说明）。
        let mut queue = self.queue.write_recovered();

        if index >= queue.len() {
            return DequeueResult::Unchanged;
        }

        let removed = Arc::make_mut(&mut queue).remove(index);
        let mut original_queue_items = SmallVec::new();

        if self.shuffle
            && let Some(pos) = self.original_queue.iter().position(|item| item == &removed)
        {
            let original_removed = Arc::make_mut(&mut self.original_queue).remove(pos);
            original_queue_items.push((pos, original_removed));
        }

        let current = self.queue_next.checked_sub(1);
        let post = Arc::clone(&queue);
        drop(queue);

        let res = match current {
            Some(current) if index == current => {
                // The next track shifts into the removed slot. Skip any unplayable
                // items and keep the cursor on the track that will actually play:
                // `current_position()` is what the thread reports via
                // QueuePositionChanged, so it must match the opened path or the UI
                // resolves the wrong (or no) queue item for the new track.
                match Self::next_playable_from(&post, current) {
                    Some(new_index) => {
                        self.queue_next = new_index + 1;
                        DequeueResult::RemovedCurrent {
                            new_path: Some(post[new_index].get_path().clone()),
                        }
                    }
                    None => DequeueResult::RemovedCurrent { new_path: None },
                }
            }
            Some(current) if index < current => {
                self.queue_next -= 1;
                DequeueResult::Removed {
                    new_position: self.queue_next.saturating_sub(1),
                }
            }
            other => DequeueResult::Removed {
                // Nothing playing (queue_next == 0) or the removed index is
                // after the current slot: report the resolved slot without
                // fabricating a cursor. Mirrors `dequeue_many`'s `checked_sub`
                // semantics (a632cef) — the single-item path must not claim a
                // current track while stopped.
                new_position: other.unwrap_or(0),
            },
        };

        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Removed {
            queue_items: smallvec![(index, removed)],
            original_queue_items,
            previous_queue_next,
            previous_shuffle,
        });

        res
    }

    /// 按 QueueItemData::eq 的键 (db_id, db_album_id, path) 在 original_queue
    /// 中定位 `removed`（dequeue_many 队列侧的移除记录）对应的原序项并重建：
    /// 各键消费其最小的 k 个 pristine 位置（k 为队列侧该键的移除数；键无剩余
    /// 则跳过，即旧实现 position 未命中的分支），移除的多重集合与旧的逐项
    /// position+remove 一致，判定与重建各一遍、整体仍 O(n)。
    ///
    /// 行为变更（缺陷修复）：这里记录 pristine 位置，旧实现记录的是随前序
    /// 删除左移的演化位置——undo_reinsert_items 的升序插入契约要求 pristine
    /// 位置，演化位置会在乱序/重复键场景错误重排 undo 还原的 original_queue。
    ///
    /// 不取 `&mut self`：调用点处于 `self.queue.write()` guard 存活期，独占
    /// 借用整个 self 会与 guard 冲突（E0499），故只借 original_queue 这一个
    /// 字段（同 undo_reinsert_items 的无 self 关联函数形态）。
    fn take_from_original_matching(
        original_queue: &mut Arc<Vec<QueueItemData>>,
        removed: &[(usize, QueueItemData)],
    ) -> SmallVec<[(usize, QueueItemData); 1]> {
        let mut removed_original_items: SmallVec<[(usize, QueueItemData); 1]> = SmallVec::new();
        let mut pending: FxHashMap<(Option<i64>, Option<i64>, &Path), usize> = FxHashMap::default();
        for (_, removed) in removed {
            *pending
                .entry((
                    removed.get_db_id(),
                    removed.get_db_album_id(),
                    removed.get_path().as_path(),
                ))
                .or_insert(0) += 1;
        }

        // 判定与移动分两遍：pending 的键借用 removed（调用点传入的队列侧
        // 移除记录），若在 drain 迭代里用 orig_item 的路径做键查找，键元组
        // 里的 &Path 要同时统一到两个借用源的生命周期（一个跨整个循环、一
        // 个仅当前迭代），NLL 下无解且 orig_item 随后要被移动（E0597/E0505）。
        // 先以共享借用扫一遍 original_queue 定位要移除的 pristine 位置
        // （升序消费计数，与单遍判定等价），再 drain 重建。
        let mut orig_remove_set: FxHashSet<usize> = FxHashSet::default();
        for (pristine_pos, orig_item) in original_queue.iter().enumerate() {
            let remove_here = match pending.get_mut(&(
                orig_item.get_db_id(),
                orig_item.get_db_album_id(),
                orig_item.get_path().as_path(),
            )) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    true
                }
                _ => false,
            };
            if remove_here {
                orig_remove_set.insert(pristine_pos);
            }
        }

        let original = Arc::make_mut(original_queue);
        let mut kept = Vec::with_capacity(original.len().saturating_sub(orig_remove_set.len()));
        for (pristine_pos, orig_item) in original.drain(..).enumerate() {
            if orig_remove_set.contains(&pristine_pos) {
                removed_original_items.push((pristine_pos, orig_item));
            } else {
                kept.push(orig_item);
            }
        }
        *original = kept;
        removed_original_items
    }

    pub fn dequeue_many(&mut self, mut indices: Vec<usize>) -> DequeueManyResult {
        if indices.is_empty() {
            return DequeueManyResult::Unchanged;
        }

        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        // 批量移除本身在写锁内完成；随后的下一可播项扫描会做 path.exists()
        // 同步 stat，必须放在写锁外（同 `dequeue`，见 `next()` 的三段化）。
        // drop 写锁后 `post` 就是准确的移除后状态——播放线程是队列唯一
        // mutator，两段之间不存在状态漂移。
        let mut queue = self.queue.write_recovered();

        indices.retain(|idx| *idx < queue.len());
        indices.sort_unstable();
        indices.dedup();

        if indices.is_empty() {
            return DequeueManyResult::Unchanged;
        }

        // 批量移除用单遍重建替代逐项 remove：旧实现对每个被移除项做一次
        // O(n) memmove（k 项即 O(k×n)），全程持有 UI 队列面板/歌词面板每帧
        // 读取的同一把写锁，大库多选删除会秒级冻结 UI 与播放线程。
        // 记录的仍是 pristine 索引（与旧的降序删除等价），顺序升序无碍——
        // undo_reinsert_items 插入前会重新升序排序。
        let remove_set: FxHashSet<usize> = indices.iter().copied().collect();
        let mut removed_queue_items: SmallVec<[(usize, QueueItemData); 1]> = SmallVec::new();
        {
            let queue = Arc::make_mut(&mut queue);
            let mut kept = Vec::with_capacity(queue.len() - remove_set.len());
            for (pristine_idx, item) in queue.drain(..).enumerate() {
                if remove_set.contains(&pristine_idx) {
                    removed_queue_items.push((pristine_idx, item));
                } else {
                    kept.push(item);
                }
            }
            *queue = kept;
        }

        let mut removed_original_items: SmallVec<[(usize, QueueItemData); 1]> = SmallVec::new();
        if self.shuffle {
            removed_original_items =
                Self::take_from_original_matching(&mut self.original_queue, &removed_queue_items);
        }

        let current = self.queue_next.checked_sub(1);
        let removed_current = current.is_some_and(|c| indices.binary_search(&c).is_ok());
        let items_before_current = current
            .map(|c| indices.iter().filter(|&&idx| idx < c).count())
            .unwrap_or(0);
        let post = Arc::clone(&queue);
        drop(queue);

        let res = if removed_current {
            let current = current.expect("removed_current implies current is Some");
            // Removing items before the current track shifts its slot left, so
            // the cursor must follow (current - items_before_current) and then
            // skip unplayable items. Leaving `queue_next` stale made
            // `current_position()` report the removed slot (or None), so no
            // QueuePositionChanged reached the UI and the info section kept
            // the cleared metadata of the removed track ("Unknown Track").
            match Self::next_playable_from(&post, current - items_before_current) {
                Some(new_index) => {
                    self.queue_next = new_index + 1;
                    DequeueManyResult::RemovedCurrent {
                        new_path: Some(post[new_index].get_path().clone()),
                    }
                }
                None => DequeueManyResult::RemovedCurrent { new_path: None },
            }
        } else if self.queue_next > 0 {
            self.queue_next -= items_before_current;
            DequeueManyResult::Removed {
                new_position: self.queue_next - 1,
            }
        } else {
            DequeueManyResult::Removed { new_position: 0 }
        };

        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Removed {
            queue_items: removed_queue_items,
            original_queue_items: removed_original_items,
            previous_queue_next,
            previous_shuffle,
        });

        res
    }

    /// Move an item from one position to another. If it should be logged in the undo history, set
    /// `user_initiated` to `true`.
    pub fn move_item(&mut self, from: usize, to: usize, user_initiated: bool) -> MoveResult {
        if from == to {
            return MoveResult::Unchanged;
        }

        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write_recovered();

        if from >= queue.len() || to >= queue.len() {
            return MoveResult::Unchanged;
        }

        {
            let queue = Arc::make_mut(&mut queue);
            let item = queue.remove(from);
            queue.insert(to, item);
        }

        let res = if let Some(current) = self.queue_next.checked_sub(1) {
            if from == current {
                // Moved the current track
                self.queue_next = to + 1;
                MoveResult::MovedCurrent { new_position: to }
            } else if from < current && to >= current {
                // Moved from before to after current
                self.queue_next -= 1;
                MoveResult::MovedCurrent {
                    new_position: self.queue_next - 1,
                }
            } else if from > current && to <= current {
                // Moved from after to before current
                self.queue_next += 1;
                MoveResult::MovedCurrent {
                    new_position: self.queue_next.saturating_sub(1),
                }
            } else {
                MoveResult::Moved
            }
        } else {
            // Nothing is playing (queue_next == 0): moving items must not
            // fabricate a current track. Mirrors the `move_items` batch path,
            // which already used `checked_sub`.
            MoveResult::Moved
        };

        drop(queue);
        self.persist_session_with_queue();

        if user_initiated {
            self.push_undo_action(UndoAction::Moved {
                original: from,
                new: to,
                previous_queue_next,
                previous_shuffle,
            });
        }

        res
    }

    /// Move multiple items to a single destination.
    ///
    /// Items at `indices` (sorted ascending) are removed, then re-inserted contiguously
    /// starting at `to`. The destination `to` refers to the final position after removal
    /// (i.e. the index where the first moved item should end up).
    pub fn move_items(&mut self, mut indices: Vec<usize>, to: usize) -> MoveItemsResult {
        if indices.is_empty() {
            return MoveItemsResult::Unchanged;
        }
        if indices.len() == 1 {
            let result = self.move_item(indices[0], to, true);
            return match result {
                MoveResult::Moved => MoveItemsResult::Moved,
                MoveResult::MovedCurrent { new_position } => {
                    MoveItemsResult::MovedCurrent { new_position }
                }
                MoveResult::Unchanged => MoveItemsResult::Unchanged,
            };
        }

        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write_recovered();

        indices.retain(|idx| *idx < queue.len());
        indices.sort_unstable();
        indices.dedup();

        if indices.is_empty() {
            drop(queue);
            return MoveItemsResult::Unchanged;
        }

        // `to` is the insert position in the post-removal queue (the caller adjusts
        // for additional removals beyond the primary source index)
        let insert_at = to.min(queue.len() - indices.len());

        // 单遍重建替代逐项 remove+insert（与 dequeue_many 同型）：旧实现对
        // 每个移动项各做一次 O(n) memmove（k 项即 O(k×n) 含逐项 clone），全程
        // 持有 UI 队列面板/歌词面板每帧读取的同一把写锁。drain 一次分区，
        // moved 保持 pristine 升序整块 splice 回目标位置；undo 记录的仍是
        // pristine 升序 (idx, item)，契约不变。
        let move_set: FxHashSet<usize> = indices.iter().copied().collect();
        let mut original_items: SmallVec<[(usize, QueueItemData); 2]> = SmallVec::new();
        {
            let queue = Arc::make_mut(&mut queue);
            let mut kept = Vec::with_capacity(queue.len() - move_set.len());
            for (pristine_idx, item) in queue.drain(..).enumerate() {
                if move_set.contains(&pristine_idx) {
                    original_items.push((pristine_idx, item));
                } else {
                    kept.push(item);
                }
            }
            kept.splice(
                insert_at..insert_at,
                original_items.iter().map(|(_, item)| item.clone()),
            );
            *queue = kept;
        }

        // Adjust current position
        let res = if let Some(current) = previous_queue_next.checked_sub(1) {
            if let Ok(current_offset) = indices.binary_search(&current) {
                let new_position = insert_at + current_offset;
                self.queue_next = new_position + 1;
                MoveItemsResult::MovedCurrent { new_position }
            } else {
                let mut new_current = current;
                for &idx in &indices {
                    if idx < current {
                        new_current = new_current.saturating_sub(1);
                    }
                }
                if insert_at <= new_current {
                    new_current += original_items.len();
                }

                if new_current != current {
                    self.queue_next = new_current + 1;
                    MoveItemsResult::MovedCurrent {
                        new_position: new_current,
                    }
                } else {
                    MoveItemsResult::Moved
                }
            }
        } else {
            MoveItemsResult::Moved
        };

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::MovedMany {
            items: original_items,
            destination: insert_at,
            previous_queue_next,
            previous_shuffle,
        });

        res
    }

    /// Replace the entire queue with new items.
    ///
    /// If shuffle is enabled, the items are shuffled (but original order is preserved).
    pub fn replace_queue(&mut self, items: Vec<QueueItemData>) -> ReplaceResult {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        // 洗牌与首个可播项扫描只依赖传入的 items，不读任何被锁保护的队列
        // 状态，全部挪到锁外：这把写锁被 UI 队列面板/歌词面板每帧读取，
        // 整队 clone + Fisher-Yates（外加逐项 path_playable 的存在性扫描）
        // 留在临界区会在大库上冻住渲染一帧以上。items 仍按值消费且只
        // clone 一次（洗牌件入队列、原件入 original_queue）。
        let (new_queue, new_original_queue) = if previous_shuffle {
            let mut shuffled = items.clone();
            shuffled.shuffle(&mut rng());
            (Arc::new(shuffled), Arc::new(items))
        } else {
            (Arc::new(items), Arc::new(Vec::new()))
        };
        let first_item =
            Self::first_playable_index_in(&new_queue).map(|idx| new_queue[idx].clone());

        let mut queue = self.queue.write_recovered();

        // Snapshot by taking the live Arcs out instead of cloning: the write lock is held for the
        // whole swap so readers never observe the empty intermediate state, and the undo stack
        // gets sole ownership (so the next mutation's `make_mut` never deep-copies either).
        let old_queue = take(&mut *queue);
        let old_original_queue = take(&mut self.original_queue);

        self.original_queue = new_original_queue;
        *queue = new_queue;

        drop(queue);

        self.push_undo_action(UndoAction::Replaced {
            old_queue,
            old_original_queue,
            previous_queue_next,
            previous_shuffle,
        });

        self.queue_next = 0;
        self.persist_session_with_queue();

        if first_item.is_some() {
            ReplaceResult::Replaced { first_item }
        } else {
            ReplaceResult::Empty
        }
    }

    /// Clear the queue.
    ///
    /// If `keep_current` is true, the currently playing track will be preserved and the
    /// queue will contain only that track.
    pub fn clear(&mut self, keep_current: bool) {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write_recovered();

        // Read the current item BEFORE the take-based snapshots below: `take`
        // empties the Vec, so reading `queue.len()` afterwards always saw an
        // empty queue and `clear(true)` silently dropped the playing track it
        // was supposed to keep.
        let current_item = if keep_current {
            Self::cursor_item(&queue, self.queue_next)
        } else {
            None
        };

        // take-based snapshots, same rationale as replace_queue: no full-queue copy
        let queue_clone = take(&mut *queue);
        let old_original_queue = take(&mut self.original_queue);

        Arc::make_mut(&mut queue).clear();
        Arc::make_mut(&mut self.original_queue).clear();

        if let Some(current_item) = current_item {
            Arc::make_mut(&mut queue).push(current_item.clone());
            self.queue_next = 1;

            if self.shuffle {
                Arc::make_mut(&mut self.original_queue).push(current_item);
            }
        } else {
            self.queue_next = 0;
        }

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Replaced {
            old_queue: queue_clone,
            old_original_queue,
            previous_queue_next,
            previous_shuffle,
        });
    }

    /// Whether shuffle mode is currently enabled.
    pub fn shuffled(&self) -> bool {
        self.shuffle
    }

    /// Toggle shuffle mode.
    pub fn toggle_shuffle(&mut self) -> ShuffleResult {
        let previous_queue_next = self.queue_next;
        let result = {
            self.shuffle = !self.shuffle;

            if self.shuffle {
                // 光标之后的整段 Fisher-Yates 在大库上是真实工作量：按
                // replace_queue 的成熟模式，锁外对快照副本洗牌、再拿一次
                // 写锁写回。洗牌前的快照入 original_queue，与旧实现原地
                // `Arc::clone` 捕获的内容一致；洗牌只动 [start..]，光标
                // 槽位不变，queue_next 无需重指（与旧实现一致）。
                let snapshot = {
                    let queue = self.queue.read_recovered();
                    Arc::clone(&queue)
                };
                let mut queue_vec = (*snapshot).clone();
                let start = self.queue_next.min(queue_vec.len());
                if start < queue_vec.len() {
                    queue_vec[start..].shuffle(&mut rng());
                }

                self.original_queue = snapshot;
                let mut queue = self.queue.write_recovered();
                *queue = Arc::new(queue_vec);
                drop(queue);

                self.push_undo_action(UndoAction::Shuffled);

                ShuffleResult::Shuffled
            } else {
                let mut queue = self.queue.write_recovered();

                let current_item = Self::cursor_item(&queue, self.queue_next);

                let had_current = current_item.is_some();

                let new_position = current_item
                    .and_then(|target_item| {
                        self.original_queue
                            .iter()
                            .position(|item| item == &target_item)
                    })
                    .unwrap_or(0);

                // take the shuffled Vec as the undo snapshot instead of cloning it:
                // its contents move into the Arc either way, this just skips the copy
                let shuffled_queue = take(&mut *queue);
                *queue = take(&mut self.original_queue);
                // Only remap the cursor when there was a current track; with
                // nothing playing (or a stale past-end cursor) keep queue_next
                // so unshuffling never fabricates a current item.
                self.queue_next = if had_current {
                    new_position + 1
                } else {
                    self.queue_next
                };

                drop(queue);

                self.push_undo_action(UndoAction::Unshuffled {
                    shuffled_queue,
                    previous_queue_next,
                });

                ShuffleResult::Unshuffled { new_position }
            }
        };

        self.persist_session_with_queue();
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use serde_json::json;
    use tokio::sync::watch;

    use super::{
        DequeueManyResult, DequeueResult, JumpResult, MoveItemsResult, MoveResult, QueueManager,
        QueueNavigationResult, Reshuffled, UndoResult,
    };
    use crate::{
        playback::{
            events::RepeatState, queue::QueueItemData, session_storage::PlaybackSessionData,
        },
        settings::playback::PlaybackSettings,
        test_support::TestDir,
    };

    #[derive(Debug, Clone, PartialEq)]
    pub(super) struct QueueManagerState {
        pub(super) queue: Arc<Vec<QueueItemData>>,
        pub(super) original_queue: Arc<Vec<QueueItemData>>,
        pub(super) queue_next: usize,
        pub(super) shuffle: bool,
    }

    pub(super) fn item(id: i64) -> QueueItemData {
        serde_json::from_value(json!({
            "db_id": id,
            "db_album_id": id / 10,
            "path": format!("/tmp/meliora-undo-{id}.flac"),
        }))
        .expect("valid queue item")
    }

    pub(super) fn manager_with_queue(items: Vec<QueueItemData>) -> QueueManager {
        let queue = Arc::new(RwLock::new(Arc::new(items)));
        let (storage_tx, _storage_rx) = watch::channel(PlaybackSessionData::default());

        QueueManager::new(
            queue,
            PlaybackSettings::default(),
            PlaybackSessionData::default(),
            storage_tx,
        )
    }

    pub(super) fn snapshot(manager: &QueueManager) -> QueueManagerState {
        QueueManagerState {
            queue: manager.queue.read().expect("poisoned queue lock").clone(),
            original_queue: manager.original_queue.clone(),
            queue_next: manager.queue_next,
            shuffle: manager.shuffle,
        }
    }

    pub(super) fn queue_ids(manager: &QueueManager) -> Vec<i64> {
        manager
            .queue
            .read()
            .expect("poisoned queue lock")
            .iter()
            .map(|item| item.get_db_id().expect("test items have db ids"))
            .collect()
    }

    #[test]
    fn dequeue_many_empty_indices_is_unchanged() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);
        let before = snapshot(&manager);

        let res = manager.dequeue_many(vec![]);

        assert!(matches!(res, DequeueManyResult::Unchanged));
        assert_eq!(snapshot(&manager), before);
    }

    #[test]
    fn dequeue_many_out_of_bounds_indices_are_filtered() {
        let mut manager = manager_with_queue(vec![item(1), item(2)]);
        manager.set_position(1);
        let before = snapshot(&manager);

        let res = manager.dequeue_many(vec![5, 6, 7]);

        assert!(matches!(res, DequeueManyResult::Unchanged));
        assert_eq!(snapshot(&manager), before);
    }

    #[test]
    fn dequeue_many_when_nothing_playing_does_not_start_playback() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        // queue_next stays 0 (nothing playing).
        assert_eq!(manager.queue_next, 0);

        let res = manager.dequeue_many(vec![0, 1]);

        match res {
            DequeueManyResult::Removed { new_position } => assert_eq!(new_position, 0),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert_eq!(manager.queue_next, 0);
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn dequeue_when_nothing_playing_does_not_start_playback() {
        let dir = TestDir::new("meliora-queue-dequeue-nothing-playing");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        // queue_next stays 0 (nothing playing).
        assert_eq!(manager.queue_next, 0);

        let res = manager.dequeue(0);

        match res {
            DequeueResult::Removed { new_position } => assert_eq!(new_position, 0),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert_eq!(manager.queue_next, 0);
        assert_eq!(manager.len(), 2);
        assert_eq!(manager.current_position(), None);
    }

    #[test]
    fn dequeue_many_removes_current_item() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(1); // current = index 1 (item 2)

        let res = manager.dequeue_many(vec![1, 3]);

        // new_path depends on files existing on disk; just assert the variant.
        assert!(matches!(res, DequeueManyResult::RemovedCurrent { .. }));
        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn dequeue_many_removes_items_before_current_shifts_position() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(3); // current = index 3 (item 4)

        let res = manager.dequeue_many(vec![0, 1]);

        match res {
            DequeueManyResult::Removed { new_position } => assert_eq!(new_position, 1),
            other => panic!("expected Removed, got {other:?}"),
        }
        // current item (item 4) is now at index 1; queue_next points past it.
        assert_eq!(manager.queue_next, 2);
    }

    #[test]
    fn dequeue_many_removes_items_after_current_keeps_position() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(1); // current = index 1 (item 2)
        let before_queue_next = manager.queue_next;

        let res = manager.dequeue_many(vec![3, 4]);

        match res {
            DequeueManyResult::Removed { new_position } => assert_eq!(new_position, 1),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert_eq!(manager.queue_next, before_queue_next);
    }

    #[test]
    fn dequeue_many_deduplicates_indices() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(0);

        let res = manager.dequeue_many(vec![2, 2, 2]);

        match res {
            DequeueManyResult::Removed { new_position } => assert_eq!(new_position, 0),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn move_items_when_nothing_playing_does_not_start_playback() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        assert_eq!(manager.queue_next, 0);

        let res = manager.move_items(vec![1, 3], 0);

        assert!(matches!(res, MoveItemsResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![2, 4, 1, 3, 5]);
    }

    #[test]
    fn move_items_keeps_relative_order_and_filters_duplicate_indices() {
        let mut manager =
            manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5), item(6)]);

        let res = manager.move_items(vec![4, 1, 1, 99, 3], 0);

        assert!(matches!(res, MoveItemsResult::Moved));
        assert_eq!(queue_ids(&manager), vec![2, 4, 5, 1, 3, 6]);
    }

    #[test]
    fn move_items_current_item_uses_its_offset_within_the_moved_block() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(3); // current = index 3 (item 4)

        let res = manager.move_items(vec![1, 3], 0);

        assert_eq!(res, MoveItemsResult::MovedCurrent { new_position: 1 });
        assert_eq!(manager.queue_next, 2);
        assert_eq!(queue_ids(&manager), vec![2, 4, 1, 3, 5]);
    }

    #[test]
    fn move_items_all_after_current_leaves_position_unchanged() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(1);
        let before_queue_next = manager.queue_next;

        let res = manager.move_items(vec![3, 4], 2);

        assert_eq!(res, MoveItemsResult::Moved);
        assert_eq!(manager.queue_next, before_queue_next);
        assert_eq!(queue_ids(&manager), vec![1, 2, 4, 5, 3]);
    }

    #[test]
    fn move_items_spanning_current_shifts_position() {
        let mut manager =
            manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5), item(6)]);
        manager.set_position(2);

        let res = manager.move_items(vec![0, 4], 1);

        assert_eq!(res, MoveItemsResult::MovedCurrent { new_position: 3 });
        assert_eq!(queue_ids(&manager), vec![2, 1, 5, 3, 4, 6]);
    }

    #[test]
    fn move_items_clamps_destination_to_end() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);

        let res = manager.move_items(vec![0, 1], 99);

        assert_eq!(res, MoveItemsResult::Moved);
        assert_eq!(queue_ids(&manager), vec![3, 4, 5, 1, 2]);
    }

    /// Queue items whose paths exist on disk, so `item_is_playable` (a
    /// `path.exists()` check for local files) accepts them and
    /// `next_playable_from` can advance onto them.
    fn playable_items(dir: &TestDir, ids: &[i64]) -> Vec<QueueItemData> {
        ids.iter()
            .map(|&id| {
                let name = format!("meliora-dequeue-{id}.flac");
                let path = dir.join(&name);
                std::fs::write(&path, b"").expect("write playable temp file");
                serde_json::from_value(json!({
                    "db_id": id,
                    "db_album_id": id / 10,
                    "path": path.to_string_lossy(),
                }))
                .expect("valid queue item")
            })
            .collect()
    }

    /// Regression: removing the current track together with items before it
    /// must move the cursor onto the track playback resumes on. A stale
    /// `queue_next` made `current_position()` return None (or the removed
    /// slot), so no QueuePositionChanged reached the UI and the info section
    /// kept the removed track's cleared metadata ("Unknown Track") while the
    /// audio already played the next item.
    #[test]
    fn dequeue_many_removing_current_with_earlier_items_keeps_cursor_on_played_track() {
        let dir = TestDir::new("meliora-queue-dequeue-many-current");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3, 4]));
        manager.set_position(2); // current = index 2 (item 3)

        let res = manager.dequeue_many(vec![0, 2]); // one before current + current

        match res {
            DequeueManyResult::RemovedCurrent { new_path } => {
                // the track after the removed current shifted into its slot
                let expected = {
                    let queue = manager.queue.read().expect("poisoned queue lock");
                    queue[1].get_path().clone()
                };
                assert_eq!(new_path.as_ref(), Some(&expected));
            }
            other => panic!("expected RemovedCurrent, got {other:?}"),
        }
        assert_eq!(manager.current_position(), Some(1));
    }

    /// Removing the current track must report the position of the track that
    /// was actually opened, so the UI resolves the right queue item.
    #[test]
    fn dequeue_removing_current_reports_position_of_the_opened_track() {
        let dir = TestDir::new("meliora-queue-dequeue-current");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_position(1); // current = index 1 (item 2)

        let res = manager.dequeue(1);

        match res {
            DequeueResult::RemovedCurrent { new_path } => {
                let expected = {
                    let queue = manager.queue.read().expect("poisoned queue lock");
                    queue[1].get_path().clone()
                };
                assert_eq!(new_path.as_ref(), Some(&expected));
            }
            other => panic!("expected RemovedCurrent, got {other:?}"),
        }
        assert_eq!(manager.current_position(), Some(1));
    }

    /// When the item that shifts into the removed slot is unplayable, the
    /// advance skips it; the cursor must follow onto the played item instead
    /// of parking on the unplayable slot.
    #[test]
    fn dequeue_removing_current_skips_unplayable_items_and_follows_the_played_track() {
        let dir = TestDir::new("meliora-queue-dequeue-unplayable");
        let mut items = playable_items(&dir, &[1, 2, 3]);
        // point the middle item at a file that does not exist
        let missing = dir.join("meliora-dequeue-missing.flac");
        items[1] = serde_json::from_value(json!({
            "db_id": 2,
            "db_album_id": 0,
            "path": missing.to_string_lossy(),
        }))
        .expect("valid queue item");

        let mut manager = manager_with_queue(items);
        manager.set_position(0); // current = index 0 (item 1)

        let res = manager.dequeue(0);

        match res {
            DequeueResult::RemovedCurrent { new_path } => {
                // item 2 is unplayable, so playback resumes on item 3 (index 1)
                let expected = {
                    let queue = manager.queue.read().expect("poisoned queue lock");
                    queue[1].get_path().clone()
                };
                assert_eq!(new_path.as_ref(), Some(&expected));
            }
            other => panic!("expected RemovedCurrent, got {other:?}"),
        }
        assert_eq!(manager.current_position(), Some(1));
    }

    /// 候选路径跨不可播项：导航的前向扫描必须跳过缺失文件（本地路径
    /// `path.exists()` 为假），落在其后第一个可播项上。三段化后该判定在
    /// 锁外快照上完成，断言游标算式与旧实现一致（cursor = 落点 index + 1）。
    #[test]
    fn next_skips_unplayable_candidates_and_lands_on_the_next_playable_track() {
        let dir = TestDir::new("meliora-queue-next-skip-unplayable");
        let mut items = playable_items(&dir, &[1]);
        // 中间两项指向不存在的文件（文件名与 playable_items 的实文件不重名）
        for id in [2_i64, 3] {
            let missing = dir.join(&format!("meliora-next-missing-{id}.flac"));
            items.push(
                serde_json::from_value(json!({
                    "db_id": id,
                    "db_album_id": 0,
                    "path": missing.to_string_lossy(),
                }))
                .expect("valid queue item"),
            );
        }
        items.extend(playable_items(&dir, &[4]));

        let mut manager = manager_with_queue(items);
        manager.set_position(0); // current = index 0 (item 1)

        let res = manager.next(true);

        match res {
            QueueNavigationResult::Changed { index, .. } => {
                // index 1/2 均不可播，前进必须落到 index 3（item 4）
                assert_eq!(index, 3);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
        assert_eq!(manager.current_position(), Some(3));
    }

    /// Regression: the take-based snapshot refactor emptied the queue Vec
    /// *before* `current_item` was read, so `clear(true)` always saw an empty
    /// queue and silently dropped the playing track it was supposed to keep.
    #[test]
    fn clear_keep_current_preserves_the_playing_track() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(2); // current = index 2 (item 3)

        manager.clear(true);

        assert_eq!(queue_ids(&manager), vec![3]);
        assert_eq!(manager.queue_next, 1);
        assert_eq!(manager.current_position(), Some(0));

        // shuffle mode: the kept item must also be registered in
        // original_queue so a later unshuffle can still find it
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);
        manager.toggle_shuffle();
        manager.undo_stack.clear();
        let current_id = {
            let queue = manager.queue.read().expect("poisoned queue lock");
            queue[1].get_db_id().expect("test items have db ids")
        };

        manager.clear(true);

        assert_eq!(queue_ids(&manager), vec![current_id]);
        assert_eq!(manager.original_queue.len(), 1);
    }

    /// Regression: `move_item` derived the current slot with
    /// `saturating_sub`, so moving items while nothing was playing fabricated
    /// a current track (queue_next 0 -> to+1 / 1) and the thread reported a
    /// bogus QueuePositionChanged. The `move_items` batch path already used
    /// `checked_sub`; the single-item path must match.
    #[test]
    fn move_item_when_nothing_playing_does_not_start_playback() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        assert_eq!(manager.queue_next, 0);

        // moving the first item used to take the `from == current` branch
        let res = manager.move_item(0, 2, true);
        assert!(matches!(res, MoveResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![2, 3, 1]);

        // moving any item to index 0 used to take the `to <= current` branch
        let res = manager.move_item(2, 0, true);
        assert!(matches!(res, MoveResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![1, 2, 3]);

        // the single-index `move_items` delegation must behave the same
        let res = manager.move_items(vec![1], 2);
        assert!(matches!(res, MoveItemsResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![1, 3, 2]);
    }

    /// Regression: toggling shuffle off while nothing was playing fabricated a
    /// current track (queue_next 0 -> 1): the manager then claimed item 0 was
    /// playing and the session snapshot persisted a bogus queue_position.
    #[test]
    fn toggle_shuffle_off_when_nothing_playing_keeps_no_current() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        assert_eq!(manager.queue_next, 0);

        manager.toggle_shuffle(); // on: nothing playing, whole queue shuffled
        assert_eq!(manager.queue_next, 0);
        manager.undo_stack.clear();

        manager.toggle_shuffle(); // off
        assert_eq!(manager.queue_next, 0);
        assert_eq!(manager.current_position(), None);
    }

    /// Regression: the repeat-all wrap reshuffles the whole (shuffled) queue,
    /// which invalidates every recorded undo entry's indices; applying them
    /// afterwards would remove or re-insert the wrong items. The wrap must
    /// drop the stale history.
    #[test]
    fn repeat_wrap_reshuffle_drops_stale_undo_entries() {
        let dir = TestDir::new("meliora-queue-wrap-undo");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3, 4]));
        manager.set_repeat(RepeatState::Repeating);
        manager.set_position(3); // on the last track
        manager.toggle_shuffle();
        manager.dequeue(0); // undo entry recorded in the pre-wrap ordering

        // EOF on the last track with repeat-all + shuffle wraps and reshuffles
        let res = manager.next(false);
        assert!(matches!(
            res,
            QueueNavigationResult::Changed {
                reshuffled: Reshuffled::Reshuffled,
                ..
            }
        ));

        // the reshuffled ordering invalidated the recorded inverse
        assert!(matches!(manager.undo_last_action(), UndoResult::None));
        // the cursor stays on a valid track after the wrap
        assert!(manager.current_position().is_some());
    }

    /// The thread resolves the opened item's slot via `jump_unshuffled`
    /// because a shuffle-on `queue_items` shuffles the appended batch, so the
    /// opened item no longer sits at `first_index + relative_idx`.
    #[test]
    fn jump_unshuffled_resolves_items_appended_by_a_shuffled_batch() {
        let dir = TestDir::new("meliora-queue-jump-unshuffled-batch");
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(0);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let first_index = manager.queue_items(playable_items(&dir, &[10, 11, 12]));

        // item 10 is the first appended item in the user's (original) order
        let res = manager.jump_unshuffled(first_index);
        match res {
            JumpResult::Jumped { .. } => {
                let pos = manager
                    .current_position()
                    .expect("jumped onto a valid slot");
                assert_eq!(
                    queue_ids(&manager)[pos],
                    10,
                    "jump_unshuffled(first_index) must land on the first appended item"
                );
            }
            other => panic!("expected Jumped, got {other:?}"),
        }
    }

    /// The repeat-all wrap without shuffle lands on the first playable slot
    /// (`next(false)`'s decision), so the gapless pre-open must see it too:
    /// `peek_next_path` used to return None past the last item and the wrap
    /// fell into a cold open — with a persisted online URL that cold open
    /// pays fail-then-refresh (~375 ms of audible underruns, log-verified
    /// 2026-10-06: index 19 → 0 advanced with no prepared stream).
    #[test]
    fn peek_next_path_covers_repeat_all_wrap_without_shuffle() {
        let dir = TestDir::new("meliora-queue-wrap-peek");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_repeat(RepeatState::Repeating);
        manager.set_position(2); // on the last track

        let peeked = manager
            .peek_next_path()
            .expect("wrap target is predictable");
        assert_eq!(
            peeked,
            manager.queue.read().expect("poisoned queue lock")[0]
                .get_path()
                .clone()
        );

        // lockstep: the advance lands on the peeked slot
        assert!(matches!(
            manager.next(false),
            QueueNavigationResult::Changed { index: 0, .. }
        ));
    }

    #[test]
    fn peek_next_path_stays_none_beyond_queue_end_without_repeat() {
        let dir = TestDir::new("meliora-queue-end-peek");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_position(2); // on the last track, repeat off

        assert!(manager.peek_next_path().is_none());
        assert!(matches!(
            manager.next(false),
            QueueNavigationResult::EndOfQueue
        ));
    }

    #[test]
    fn peek_next_path_stays_none_on_shuffle_wrap() {
        let dir = TestDir::new("meliora-queue-shuffle-wrap-peek");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_repeat(RepeatState::Repeating);
        manager.set_position(2);
        manager.toggle_shuffle();

        // the reshuffled landing order is not predictable
        assert!(manager.peek_next_path().is_none());
    }
}
