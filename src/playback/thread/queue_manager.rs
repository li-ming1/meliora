use std::{
    collections::VecDeque,
    mem::take,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use rand::{rng, seq::SliceRandom};
use smallvec::{SmallVec, smallvec};

use crate::{
    playback::{events::RepeatState, queue::QueueItemData, session_storage::PlaybackSessionData},
    settings::playback::PlaybackSettings,
};

const UNDO_STACK_CAPACITY: usize = 30;

/// Minimum interval between queue-snapshot sends to the session storage worker.
/// A send deep-copies the whole queue (see `send_session_with_queue`), so burst
/// mutations (batch drops, rapid skips) coalesce into one snapshot per window.
const SESSION_PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

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

#[derive(Debug, Clone)]
/// Type storing the inverse of various queue mutations, for undoing queue changes.
pub enum UndoAction {
    /// The queue was replaced with a new set of items. Contains the old queue state.
    /// Snapshots are `Arc`-shared: cloning into the undo stack must not deep-copy a
    /// 100k-item queue 30 times over, and undo may still need to hand out a fresh
    /// `Vec` if the snapshot is shared elsewhere.
    Replaced {
        old_queue: Arc<Vec<QueueItemData>>,
        old_original_queue: Arc<Vec<QueueItemData>>,
        previous_queue_next: usize,
        previous_shuffle: bool,
    },
    /// The queue was shuffled (shuffle toggled on). The pre-shuffle queue is recoverable
    /// by taking `original_queue` at undo time, so no payload is needed.
    Shuffled,
    /// The queue was unshuffled (shuffle toggled off). Stores the pre-unshuffle (shuffled)
    /// queue; the pre-unshuffle `original_queue` is recoverable from the queue at undo time.
    Unshuffled {
        shuffled_queue: Arc<Vec<QueueItemData>>,
        previous_queue_next: usize,
    },
    /// Items were removed from the queue. Contains a list of removed items and their indices.
    Removed {
        queue_items: SmallVec<[(usize, QueueItemData); 1]>,
        original_queue_items: SmallVec<[(usize, QueueItemData); 1]>,
        previous_queue_next: usize,
        previous_shuffle: bool,
    },
    /// Items were inserted into the queue. Contains a list of inserted item indices.
    Inserted {
        queue_indices: SmallVec<[usize; 1]>,
        original_queue_indices: SmallVec<[usize; 1]>,
        previous_queue_next: usize,
        previous_shuffle: bool,
    },
    /// Items were moved within the queue. Contains the original and new indices.
    Moved {
        original: usize,
        new: usize,
        previous_queue_next: usize,
        previous_shuffle: bool,
    },
    MovedMany {
        items: SmallVec<[(usize, QueueItemData); 2]>,
        destination: usize,
        previous_queue_next: usize,
        previous_shuffle: bool,
    },
}

#[derive(Debug, Clone)]
pub enum UndoResult {
    /// The last action was undone successfully. Contains the current index and path.
    Ok {
        current_idx: usize,
        current_path: PathBuf,
        shuffle: bool,
    },
    /// The last action was undone successfully, but no current track is selected.
    OkNoCurrent { shuffle: bool },
    /// No action to undo.
    None,
}

/// Manages the playback queue state.
///
/// This component handles all queue operations including navigation, shuffling,
/// repeat modes, and queue mutations. It does NOT handle side effects like
/// opening tracks or emitting events - those are the responsibility of the
/// PlaybackThread.
pub struct QueueManager {
    playback_settings: PlaybackSettings,
    /// The current queue. Shared with the UI thread for display.
    queue: Arc<RwLock<Vec<QueueItemData>>>,
    /// If shuffled, this holds the original (unshuffled) queue order.
    original_queue: Vec<QueueItemData>,
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
    /// Instant of the last session snapshot send (debounce anchor).
    last_session_send: Option<Instant>,
}

impl QueueManager {
    fn undo_result_from_state(
        queue: &[QueueItemData],
        queue_next: usize,
        shuffle: bool,
    ) -> UndoResult {
        if let Some(current_idx) = queue_next
            .checked_sub(1)
            .filter(|current_idx| *current_idx < queue.len())
        {
            UndoResult::Ok {
                current_idx,
                current_path: queue[current_idx].get_path().clone(),
                shuffle,
            }
        } else {
            UndoResult::OkNoCurrent { shuffle }
        }
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

    fn first_playable_index(queue: &[QueueItemData]) -> Option<usize> {
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

    fn push_undo_action(&mut self, action: UndoAction) {
        if self.undo_stack.len() >= UNDO_STACK_CAPACITY {
            self.undo_stack.pop_front();
        }

        self.undo_stack.push_back(action);
    }

    pub fn undo_last_action(&mut self) -> UndoResult {
        let action = self.undo_stack.pop_back();

        let result = match action {
            Some(UndoAction::Replaced {
                old_queue,
                old_original_queue,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");
                *queue = Arc::try_unwrap(old_queue).unwrap_or_else(|shared| (*shared).clone());
                self.original_queue =
                    Arc::try_unwrap(old_original_queue).unwrap_or_else(|shared| (*shared).clone());
                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Removed {
                queue_items,
                original_queue_items,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");

                let mut queue_items = queue_items.into_vec();
                queue_items.sort_unstable_by_key(|(idx, _)| *idx);
                for (idx, item) in queue_items {
                    queue.insert(idx, item);
                }

                let mut original_queue_items = original_queue_items.into_vec();
                original_queue_items.sort_unstable_by_key(|(idx, _)| *idx);
                for (idx, item) in original_queue_items {
                    self.original_queue.insert(idx, item);
                }

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Inserted {
                queue_indices,
                original_queue_indices,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");

                let mut queue_indices = queue_indices.into_vec();
                queue_indices.sort_unstable_by(|a, b| b.cmp(a));
                for idx in queue_indices {
                    queue.remove(idx);
                }

                let mut original_queue_indices = original_queue_indices.into_vec();
                original_queue_indices.sort_unstable_by(|a, b| b.cmp(a));
                for idx in original_queue_indices {
                    self.original_queue.remove(idx);
                }

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Moved {
                original,
                new,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");

                let item = queue.remove(new);
                queue.insert(original, item);

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::MovedMany {
                items,
                destination,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");

                for _ in 0..items.len() {
                    if destination < queue.len() {
                        queue.remove(destination);
                    }
                }

                for (original_idx, item) in items {
                    if original_idx <= queue.len() {
                        queue.insert(original_idx, item);
                    }
                }

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Shuffled) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");
                *queue = take(&mut self.original_queue);
                self.shuffle = false;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Unshuffled {
                shuffled_queue,
                previous_queue_next,
            }) => {
                let mut queue = self.queue.write().expect("poisoned queue lock");
                let restored =
                    Arc::try_unwrap(shuffled_queue).unwrap_or_else(|shared| (*shared).clone());
                self.original_queue = std::mem::replace(&mut *queue, restored);
                self.queue_next = previous_queue_next;
                self.shuffle = true;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            None => UndoResult::None,
        };

        if !matches!(result, UndoResult::None) {
            self.persist_session_with_queue();
        }

        result
    }

    pub fn new(
        queue: Arc<RwLock<Vec<QueueItemData>>>,
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
            let queue = queue.read().expect("poisoned queue lock");
            let queue_len = queue.len();
            let original_queue = if shuffle && session_original_queue.len() == queue_len {
                session_original_queue
            } else if shuffle {
                queue.clone()
            } else {
                Vec::new()
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
            last_session_send: None,
        }
    }

    /// Get the current queue position (0-indexed).
    /// Returns None if no track is playing.
    pub fn current_position(&self) -> Option<usize> {
        let position = self.queue_next.checked_sub(1)?;
        (position < self.len()).then_some(position)
    }

    /// Online-provider identity of the currently playing item, if it is an
    /// HTTP stream (used to re-fetch a fresh URL when a persisted link 403s).
    #[cfg(feature = "online_sources")]
    pub fn current_online_identity(&self) -> Option<super::super::queue::OnlineIdentity> {
        let position = self.current_position()?;
        let queue = self.queue.read().ok()?;
        queue.get(position)?.online_identity().cloned()
    }

    /// Persisted display metadata `(name, artist, duration, cover)` of the
    /// currently playing item, for stream-registry re-registration.
    #[cfg(feature = "online_sources")]
    pub fn current_display(
        &self,
    ) -> Option<(Option<String>, Option<String>, Option<i64>, Option<String>)> {
        let position = self.current_position()?;
        let queue = self.queue.read().ok()?;
        queue.get(position)?.persisted_display()
    }

    /// Path of the currently playing item, if any.
    pub fn current_path(&self) -> Option<PathBuf> {
        let position = self.current_position()?;
        let queue = self.queue.read().ok()?;
        Some(queue.get(position)?.get_path().clone())
    }

    /// Swaps the stored path of the currently playing item (used when its
    /// expired stream URL is replaced by a freshly fetched one).
    pub fn replace_current_path(&self, path: PathBuf) -> bool {
        // Resolve the position before taking the write lock: computing it
        // reads the queue, and a read inside a held write lock self-deadlocks.
        let Some(position) = self.current_position() else {
            return false;
        };
        let Ok(mut guard) = self.queue.write() else {
            return false;
        };
        match guard.get_mut(position) {
            Some(item) => {
                item.replace_path(path);
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
        self.queue.read().expect("poisoned queue lock").len()
    }

    /// Returns true when shuffle mode is enabled.
    pub fn is_shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    /// Returns true if every queued item belongs to the same known album.
    pub fn all_items_same_album(&self) -> bool {
        let queue = self.queue.read().expect("poisoned queue lock");
        let Some(first_album) = queue.first().and_then(QueueItemData::get_db_album_id) else {
            return false;
        };

        queue
            .iter()
            .all(|item| item.get_db_album_id() == Some(first_album))
    }

    /// Get the first playable item in the queue along with its index.
    pub fn first_with_index(&self) -> Option<(QueueItemData, usize)> {
        self.queue
            .read()
            .expect("poisoned queue lock")
            .iter()
            .enumerate()
            .find(|(_, item)| Self::item_is_playable(item))
            .map(|(idx, item)| (item.clone(), idx))
    }

    /// Get the last item in the queue along with its index, if the queue is non-empty.
    pub fn last_with_index(&self) -> Option<(QueueItemData, usize)> {
        let queue = self.queue.read().expect("poisoned queue lock");
        Self::last_playable_index(&queue).map(|index| (queue[index].clone(), index))
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
    pub fn next(&mut self, user_initiated: bool) -> QueueNavigationResult {
        let result = {
            let mut queue = self.queue.write().expect("poisoned queue lock");

            if self.repeat == RepeatState::RepeatingOne
                && !user_initiated
                && let Some(path) = queue.get(self.queue_next.saturating_sub(1))
                && Self::item_is_playable(path)
            {
                return QueueNavigationResult::Unchanged {
                    path: path.get_path().clone(),
                };
            }

            if let Some(index) = Self::next_playable_from(&queue, self.queue_next) {
                self.queue_next = index + 1;
                QueueNavigationResult::Changed {
                    index,
                    path: queue[index].get_path().clone(),
                    reshuffled: Reshuffled::NotReshuffled,
                }
            } else if self.repeat == RepeatState::Repeating {
                if self.shuffle {
                    queue.shuffle(&mut rng());
                }
                if let Some(index) = Self::first_playable_index(&queue) {
                    self.queue_next = index + 1;
                    QueueNavigationResult::Changed {
                        index,
                        path: queue[index].get_path().clone(),
                        reshuffled: if self.shuffle {
                            Reshuffled::Reshuffled
                        } else {
                            Reshuffled::NotReshuffled
                        },
                    }
                } else {
                    QueueNavigationResult::EndOfQueue
                }
            } else {
                QueueNavigationResult::EndOfQueue
            }
        };

        if let QueueNavigationResult::Changed { reshuffled, .. } = &result {
            if *reshuffled == Reshuffled::Reshuffled {
                self.persist_session_with_queue();
            } else {
                self.persist_session_state();
            }
        }

        result
    }

    /// The path `next(false)` would open, without advancing any state (used
    /// by gapless pre-open). Returns `None` when the advance is not
    /// predictable (queue end, repeat-wrap with shuffle reshuffles).
    pub fn peek_next_path(&self) -> Option<PathBuf> {
        let queue = self.queue.read().expect("poisoned queue lock");

        if self.repeat == RepeatState::RepeatingOne
            && let Some(path) = queue.get(self.queue_next.saturating_sub(1))
            && Self::item_is_playable(path)
        {
            return Some(path.get_path().clone());
        }

        Self::next_playable_from(&queue, self.queue_next).map(|index| queue[index].get_path().clone())
    }

    /// Go to the previous track in the queue.
    pub fn previous(&mut self) -> QueueNavigationResult {
        let result = {
            let mut queue = self.queue.write().expect("poisoned queue lock");

            if self.queue_next > 1
                && let Some(index) = Self::prev_playable_before(&queue, self.queue_next - 1)
            {
                self.queue_next = index + 1;
                QueueNavigationResult::Changed {
                    index,
                    path: queue[index].get_path().clone(),
                    reshuffled: Reshuffled::NotReshuffled,
                }
            } else if self.repeat == RepeatState::Repeating
                && !queue.is_empty()
                && let Some(index) = {
                    if self.shuffle {
                        queue.shuffle(&mut rng());
                    }
                    Self::last_playable_index(&queue)
                }
            {
                self.queue_next = index + 1;
                QueueNavigationResult::Changed {
                    index,
                    path: queue[index].get_path().clone(),
                    reshuffled: if self.shuffle {
                        Reshuffled::Reshuffled
                    } else {
                        Reshuffled::NotReshuffled
                    },
                }
            } else {
                QueueNavigationResult::EndOfQueue
            }
        };

        if let QueueNavigationResult::Changed { reshuffled, .. } = &result {
            if *reshuffled == Reshuffled::Reshuffled {
                self.persist_session_with_queue();
            } else {
                self.persist_session_state();
            }
        }

        result
    }

    /// Jump to a specific index in the queue.
    pub fn jump(&mut self, index: usize) -> JumpResult {
        let queue = self.queue.read().expect("poisoned queue lock");

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

        let original_item = match self.original_queue.get(index) {
            Some(item) => item.clone(),
            None => return JumpResult::OutOfBounds,
        };

        let queue = self.queue.read().expect("poisoned queue lock");
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

        let mut queue = self.queue.write().expect("poisoned queue lock");
        let mut original_queue_indices = SmallVec::new();

        if self.shuffle {
            original_queue_indices.push(self.original_queue.len());
            self.original_queue.push(item.clone());
        }

        queue.push(item);

        let index = queue.len() - 1;

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_indices: smallvec![index],
            original_queue_indices,
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

        let mut queue = self.queue.write().expect("poisoned queue lock");
        let first_index = queue.len();
        let items_len = items.len();
        let mut original_queue_indices = SmallVec::new();

        if self.shuffle {
            let original_start = self.original_queue.len();
            self.original_queue.extend(items.clone());
            original_queue_indices.extend(original_start..original_start + items_len);

            let mut shuffled = items;
            shuffled.shuffle(&mut rng());
            queue.extend(shuffled);
        } else {
            queue.extend(items);
        }

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_indices: (first_index..first_index + items_len).collect(),
            original_queue_indices,
            previous_queue_next,
            previous_shuffle,
        });

        first_index
    }

    /// Insert a single item at a specific position.
    pub fn insert_item(&mut self, position: usize, item: QueueItemData) -> InsertResult {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write().expect("poisoned queue lock");
        let mut original_queue_indices = SmallVec::new();

        let insert_pos = position.min(queue.len());

        if self.shuffle {
            original_queue_indices.push(self.original_queue.len());
            self.original_queue.push(item.clone());
        }

        queue.insert(insert_pos, item);

        drop(queue);

        let result = if insert_pos < self.queue_next {
            self.queue_next += 1;
            InsertResult::InsertedMovedCurrent {
                first_index: insert_pos,
                new_position: self.queue_next.saturating_sub(1),
            }
        } else {
            InsertResult::Inserted {
                first_index: insert_pos,
            }
        };

        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_indices: smallvec![insert_pos],
            original_queue_indices,
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

        let mut queue = self.queue.write().expect("poisoned queue lock");

        let insert_pos = position.min(queue.len());
        let items_len = items.len();
        let mut original_queue_indices = SmallVec::new();

        if self.shuffle {
            let original_start = self.original_queue.len();
            self.original_queue.extend(items.clone());
            original_queue_indices.extend(original_start..original_start + items_len);
        }

        queue.splice(insert_pos..insert_pos, items);

        drop(queue);

        let result = if insert_pos < self.queue_next {
            self.queue_next += items_len;
            InsertResult::InsertedMovedCurrent {
                first_index: insert_pos,
                new_position: self.queue_next.saturating_sub(1),
            }
        } else {
            InsertResult::Inserted {
                first_index: insert_pos,
            }
        };

        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Inserted {
            queue_indices: (insert_pos..insert_pos + items_len).collect(),
            original_queue_indices,
            previous_queue_next,
            previous_shuffle,
        });

        result
    }

    /// Remove an item from the queue at the specified index.
    pub fn dequeue(&mut self, index: usize) -> DequeueResult {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write().expect("poisoned queue lock");

        if index >= queue.len() {
            return DequeueResult::Unchanged;
        }

        let removed = queue.remove(index);
        let mut original_queue_items = SmallVec::new();

        if self.shuffle
            && let Some(pos) = self.original_queue.iter().position(|item| item == &removed)
        {
            let original_removed = self.original_queue.remove(pos);
            original_queue_items.push((pos, original_removed));
        }

        let current = self.queue_next.saturating_sub(1);

        let res = if index == current {
            let new_path = Self::next_playable_from(&queue, current)
                .and_then(|idx| queue.get(idx))
                .map(|v| v.get_path().clone());
            DequeueResult::RemovedCurrent { new_path }
        } else if index < current {
            self.queue_next -= 1;
            DequeueResult::Removed {
                new_position: self.queue_next.saturating_sub(1),
            }
        } else {
            DequeueResult::Removed {
                new_position: current,
            }
        };

        drop(queue);
        self.persist_session_with_queue();

        self.push_undo_action(UndoAction::Removed {
            queue_items: smallvec![(index, removed)],
            original_queue_items,
            previous_queue_next,
            previous_shuffle,
        });

        res
    }

    pub fn dequeue_many(&mut self, mut indices: Vec<usize>) -> DequeueManyResult {
        if indices.is_empty() {
            return DequeueManyResult::Unchanged;
        }

        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write().expect("poisoned queue lock");

        indices.retain(|idx| *idx < queue.len());
        indices.sort_unstable();
        indices.dedup();

        if indices.is_empty() {
            return DequeueManyResult::Unchanged;
        }

        let mut removed_queue_items: SmallVec<[(usize, QueueItemData); 1]> = SmallVec::new();
        let mut removed_original_items: SmallVec<[(usize, QueueItemData); 1]> = SmallVec::new();

        for &idx in indices.iter().rev() {
            let item = queue.remove(idx);

            if self.shuffle
                && let Some(pos) = self.original_queue.iter().position(|q| q == &item)
            {
                let orig_item = self.original_queue.remove(pos);
                removed_original_items.push((pos, orig_item));
            }

            removed_queue_items.push((idx, item));
        }

        let current = self.queue_next.checked_sub(1);
        let removed_current = current.is_some_and(|c| indices.binary_search(&c).is_ok());
        let items_before_current = current
            .map(|c| indices.iter().filter(|&&idx| idx < c).count())
            .unwrap_or(0);

        let res = if removed_current {
            let current = current.expect("removed_current implies current is Some");
            let new_path = Self::next_playable_from(&queue, current - items_before_current)
                .and_then(|idx| queue.get(idx))
                .map(|v| v.get_path().clone());
            DequeueManyResult::RemovedCurrent { new_path }
        } else if self.queue_next > 0 {
            self.queue_next -= items_before_current;
            DequeueManyResult::Removed {
                new_position: self.queue_next - 1,
            }
        } else {
            DequeueManyResult::Removed { new_position: 0 }
        };

        drop(queue);
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

        let mut queue = self.queue.write().expect("poisoned queue lock");

        if from >= queue.len() || to >= queue.len() {
            return MoveResult::Unchanged;
        }

        let item = queue.remove(from);
        queue.insert(to, item);

        let current = self.queue_next.saturating_sub(1);

        let res = if from == current {
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

        let mut queue = self.queue.write().expect("poisoned queue lock");

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

        // Record original positions and items, then remove in reverse order
        let mut original_items: SmallVec<[(usize, QueueItemData); 2]> = SmallVec::new();
        for &idx in indices.iter().rev() {
            let item = queue.remove(idx);
            original_items.push((idx, item));
        }
        // original_items is in reverse order, reverse to get ascending order
        original_items.reverse();

        // Insert all items at the destination
        for (i, (_, item)) in original_items.iter().enumerate() {
            queue.insert(insert_at + i, item.clone());
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

        let mut queue = self.queue.write().expect("poisoned queue lock");

        // Snapshot by taking the live Vecs out instead of cloning: the write lock is held for the
        // whole swap so readers never observe the empty intermediate state, and the undo stack
        // gets sole ownership (so `Arc::try_unwrap` at undo time never deep-copies either).
        let old_queue = Arc::new(take(&mut *queue));
        let old_original_queue = Arc::new(take(&mut self.original_queue));

        if self.shuffle {
            // Consume `items` by value: cloning it twice per replace put three
            // full-queue copies inside the write lock on large libraries.
            let mut shuffled = items.clone();
            shuffled.shuffle(&mut rng());

            self.original_queue = items;
            *queue = shuffled;
        } else {
            self.original_queue.clear();
            *queue = items;
        }

        let first_item = Self::first_playable_index(&queue).map(|idx| queue[idx].clone());

        drop(queue);

        self.push_undo_action(UndoAction::Replaced {
            old_queue,
            old_original_queue,
            previous_queue_next,
            previous_shuffle,
        });

        self.queue_next = 0;
        self.persist_session_with_queue();

        match first_item {
            Some(first) => ReplaceResult::Replaced {
                first_item: Some(first),
            },
            None => ReplaceResult::Empty,
        }
    }

    /// Clear the queue.
    ///
    /// If `keep_current` is true, the currently playing track will be preserved and the
    /// queue will contain only that track.
    pub fn clear(&mut self, keep_current: bool) {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write().expect("poisoned queue lock");

        // take-based snapshots, same rationale as replace_queue: no full-queue copy
        let queue_clone = Arc::new(take(&mut *queue));
        let old_original_queue = Arc::new(take(&mut self.original_queue));

        let current_item = keep_current
            .then(|| {
                (self.queue_next > 0 && self.queue_next <= queue.len())
                    .then(|| queue[self.queue_next - 1].clone())
            })
            .flatten();

        queue.clear();
        self.original_queue.clear();

        if let Some(current_item) = current_item {
            queue.push(current_item.clone());
            self.queue_next = 1;

            if self.shuffle {
                self.original_queue.push(current_item);
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
            let mut queue = self.queue.write().expect("poisoned queue lock");

            self.shuffle = !self.shuffle;

            if self.shuffle {
                self.original_queue = queue.clone();

                let start = self.queue_next.min(queue.len());
                if start < queue.len() {
                    queue[start..].shuffle(&mut rng());
                }

                drop(queue);

                self.push_undo_action(UndoAction::Shuffled);

                ShuffleResult::Shuffled
            } else {
                let current_item = if self.queue_next > 0 && self.queue_next <= queue.len() {
                    Some(queue[self.queue_next - 1].clone())
                } else {
                    None
                };

                let new_position = current_item
                    .and_then(|target_item| {
                        self.original_queue
                            .iter()
                            .position(|item| item == &target_item)
                    })
                    .unwrap_or(0);

                // take the shuffled Vec as the undo snapshot instead of cloning it:
                // its contents move into the Arc either way, this just skips the copy
                let shuffled_queue = Arc::new(take(&mut *queue));
                *queue = take(&mut self.original_queue);
                self.queue_next = new_position + 1;

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

    /// Persist the queue session when only playback state changed.
    ///
    /// This reuses the stored queue snapshot and updates fields like
    /// the current position, shuffle mode, and repeat mode. A pending
    /// debounced queue snapshot is upgraded to a full send so the stored
    /// position never references stale queue contents.
    fn persist_session_state(&mut self) {
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
    fn persist_session_with_queue(&mut self) {
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
    /// This deep-copies the queue (and original queue) under the read lock —
    /// the price of handing the worker a consistent snapshot of a live `Vec`.
    /// Call sites debounce to keep it off burst paths.
    fn send_session_with_queue(&mut self) {
        let queue = self.queue.read().expect("poisoned queue lock");
        let queue_snapshot = queue.clone();
        let queue_position = self
            .queue_next
            .checked_sub(1)
            .filter(|position| *position < queue.len());
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use serde_json::json;
    use tokio::sync::watch;

    use super::{DequeueManyResult, MoveItemsResult, QueueManager, UndoResult};
    use crate::{
        playback::{queue::QueueItemData, session_storage::PlaybackSessionData},
        settings::playback::PlaybackSettings,
    };

    #[derive(Debug, Clone, PartialEq)]
    struct QueueManagerState {
        queue: Vec<QueueItemData>,
        original_queue: Vec<QueueItemData>,
        queue_next: usize,
        shuffle: bool,
    }

    fn item(id: i64) -> QueueItemData {
        serde_json::from_value(json!({
            "db_id": id,
            "db_album_id": id / 10,
            "path": format!("/tmp/meliora-undo-{id}.flac"),
        }))
        .expect("valid queue item")
    }

    fn manager_with_queue(items: Vec<QueueItemData>) -> QueueManager {
        let queue = Arc::new(RwLock::new(items));
        let (storage_tx, _storage_rx) = watch::channel(PlaybackSessionData::default());

        QueueManager::new(
            queue,
            PlaybackSettings::default(),
            PlaybackSessionData::default(),
            storage_tx,
        )
    }

    fn snapshot(manager: &QueueManager) -> QueueManagerState {
        QueueManagerState {
            queue: manager.queue.read().expect("poisoned queue lock").clone(),
            original_queue: manager.original_queue.clone(),
            queue_next: manager.queue_next,
            shuffle: manager.shuffle,
        }
    }

    fn queue_ids(manager: &QueueManager) -> Vec<i64> {
        manager
            .queue
            .read()
            .expect("poisoned queue lock")
            .iter()
            .map(|item| item.get_db_id().expect("test items have db ids"))
            .collect()
    }

    fn assert_undo_round_trip(manager: &mut QueueManager, before: QueueManagerState) {
        let undo = manager.undo_last_action();
        assert!(
            !matches!(undo, UndoResult::None),
            "expected an undoable action"
        );
        assert_eq!(snapshot(manager), before);
    }

    #[test]
    fn undo_insert_item_restores_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);

        let before = snapshot(&manager);

        manager.insert_item(0, item(9));

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_queue_items_in_shuffle_mode_restores_original_queue() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let before = snapshot(&manager);

        manager.queue_items(vec![item(10), item(11), item(12)]);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_dequeue_in_shuffle_mode_restores_original_queue_indexes() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(2);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let before = snapshot(&manager);

        manager.dequeue(1);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_move_restores_previous_track_position() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(2);

        let before = snapshot(&manager);

        manager.move_item(0, 3, true);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_replace_queue_restores_full_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let before = snapshot(&manager);

        manager.replace_queue(vec![item(7), item(8)]);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_clear_keep_current_restores_full_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(2);

        let before = snapshot(&manager);

        manager.clear(true);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_toggle_shuffle_restores_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(1);

        let before = snapshot(&manager);

        manager.toggle_shuffle();

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_twice_restores_state_before_both_actions() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);

        let before = snapshot(&manager);

        manager.insert_item(0, item(9));
        manager.move_item(0, 2, true);

        assert!(!matches!(manager.undo_last_action(), UndoResult::None));
        assert!(!matches!(manager.undo_last_action(), UndoResult::None));
        assert_eq!(snapshot(&manager), before);
    }

    #[test]
    fn undo_on_empty_stack_returns_none() {
        let mut manager = manager_with_queue(vec![item(1), item(2)]);
        let before = snapshot(&manager);

        assert!(matches!(manager.undo_last_action(), UndoResult::None));
        assert_eq!(snapshot(&manager), before);
    }

    #[test]
    fn undo_stack_evicts_oldest_when_exceeding_capacity() {
        use super::UNDO_STACK_CAPACITY;

        let mut manager = manager_with_queue(vec![item(1)]);

        manager.insert_item(0, item(100));
        let after_first_insert = snapshot(&manager);

        for i in 0..UNDO_STACK_CAPACITY {
            manager.insert_item(0, item(200 + i as i64));
        }

        assert_eq!(manager.undo_stack.len(), UNDO_STACK_CAPACITY);

        for _ in 0..UNDO_STACK_CAPACITY {
            assert!(!matches!(manager.undo_last_action(), UndoResult::None));
        }

        assert!(matches!(manager.undo_last_action(), UndoResult::None));
        assert_eq!(snapshot(&manager), after_first_insert);
    }

    #[test]
    fn undo_toggle_shuffle_off_restores_shuffled_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(1);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let before = snapshot(&manager);
        assert!(before.shuffle);

        manager.toggle_shuffle();
        assert!(!manager.shuffle);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_insert_items_restores_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(1);

        let before = snapshot(&manager);

        manager.insert_items(1, vec![item(10), item(11), item(12)]);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_clear_without_keep_current_restores_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(2);

        let before = snapshot(&manager);

        manager.clear(false);

        assert_undo_round_trip(&mut manager, before);
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
    fn undo_dequeue_many_restores_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(2);

        let before = snapshot(&manager);

        manager.dequeue_many(vec![0, 2, 4]);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_dequeue_many_in_shuffle_mode_restores_original_queue() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(1);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let before = snapshot(&manager);

        manager.dequeue_many(vec![0, 2]);

        assert_undo_round_trip(&mut manager, before);
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
    fn undo_move_items_restores_previous_state() {
        let mut manager =
            manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5), item(6)]);
        manager.set_position(4);

        let before = snapshot(&manager);

        manager.move_items(vec![1, 3, 4], 0);

        assert_undo_round_trip(&mut manager, before);
    }

    #[test]
    fn undo_move_items_in_shuffle_mode_restores_previous_state() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(2);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let before = snapshot(&manager);

        manager.move_items(vec![0, 2], 1);

        assert_undo_round_trip(&mut manager, before);
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

    #[test]
    fn undo_shuffle_then_insert_unwinds_in_lifo_order() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(0);

        let before = snapshot(&manager);

        manager.toggle_shuffle();
        let after_shuffle = snapshot(&manager);

        manager.insert_item(1, item(99));

        assert!(!matches!(manager.undo_last_action(), UndoResult::None));
        assert_eq!(snapshot(&manager), after_shuffle);

        assert!(!matches!(manager.undo_last_action(), UndoResult::None));
        assert_eq!(snapshot(&manager), before);
    }
}
