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
    /// 队列位置发生变化。
    Changed {
        index: usize,
        path: PathBuf,
        reshuffled: Reshuffled,
    },
    /// 当前曲目应当重播（RepeatOne 模式）。
    Unchanged { path: PathBuf },
    /// 已到达队列末尾。
    EndOfQueue,
}

#[derive(Debug, Clone)]
pub enum DequeueResult {
    /// 移除了一个条目，队列位置已相应调整。
    Removed { new_position: usize },
    /// 移除了当前正在播放的条目。
    RemovedCurrent {
        /// 接下来要播的曲目路径（若有）。
        new_path: Option<PathBuf>,
    },
    /// 什么都没变（下标越界）。
    Unchanged,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DequeueManyResult {
    /// 移除了若干条目，队列位置已相应调整。
    Removed { new_position: usize },
    /// 移除了当前正在播放的条目。
    RemovedCurrent { new_path: Option<PathBuf> },
    /// 什么都没变（下标集合为空，或全部越界）。
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveResult {
    Moved,
    /// 条目已移动，且当前位置发生变化。
    MovedCurrent {
        new_position: usize,
    },
    /// 什么都没变（位置相同或非法）。
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
    /// 条目已插入，当前位置不变。
    Inserted { first_index: usize },
    /// 条目已插入，且当前位置被推移。
    InsertedMovedCurrent {
        first_index: usize,
        new_position: usize,
    },
    /// 什么都没变（位置非法）。
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShuffleResult {
    /// 随机播放已开启。
    Shuffled,
    /// 随机播放已关闭，附带在未洗牌队列中的新位置。
    Unshuffled { new_position: usize },
}

/// 短生命周期的操作结果；大小不对称无关紧要，因为调用方查看完取值就立刻丢弃。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ReplaceResult {
    /// 队列已替换，含要播放的第一个条目。
    Replaced { first_item: Option<QueueItemData> },
    /// 替换之后队列为空。
    Empty,
}

#[derive(Debug, Clone)]
pub enum JumpResult {
    Jumped { path: PathBuf },
    OutOfBounds,
}

/// 播放队列状态的管理者。
///
/// 负责全部队列操作：导航、随机播放、循环模式与队列变更；不处理打开曲目、
/// 发事件这类副作用——那些归 PlaybackThread。
pub struct QueueManager {
    playback_settings: PlaybackSettings,
    /// 当前队列，与 UI 线程共享用于展示。内层 `Arc` 让会话快照与 undo 快照
    /// 只是计数抓取：变更走 `Arc::make_mut`，因此只有快照确实还活着时才复制
    /// （写时复制）。
    queue: Arc<RwLock<Arc<Vec<QueueItemData>>>>,
    /// 随机播放开启时保存未洗牌的原序。
    original_queue: Arc<Vec<QueueItemData>>,
    /// 随机播放是否开启。
    shuffle: bool,
    /// 下一首要播曲目的下标：queue_next == 1 表示当前在第 0 项，
    /// queue_next == queue.len() 表示当前在最后一项。
    queue_next: usize,
    repeat: RepeatState,
    storage_tx: tokio::sync::watch::Sender<PlaybackSessionData>,
    undo_stack: VecDeque<UndoAction>,
    /// 自上次发出会话快照后，队列内容有变化。
    session_dirty: bool,
    /// 自上次发送后播放状态（position/shuffle/repeat）有变化，正等待防抖后的
    /// 仅状态刷新。与 `session_dirty`（队列内容）平行，共用同一个
    /// `last_session_send` 窗口锚点。见 `session.rs`。
    state_dirty: bool,
    /// 上次发送会话快照的时刻（防抖锚点）——全量快照路径与仅状态路径共用该
    /// 锚点，所以一次状态发送同样占用这个窗口。
    last_session_send: Option<Instant>,
}

impl QueueManager {
    /// 光标（`queue_next`）所在的队列下标，仅当它落在真实槽位上
    /// （没在播放、或光标已越过末尾时为 `None`）。
    fn cursor_index(queue_len: usize, queue_next: usize) -> Option<usize> {
        queue_next
            .checked_sub(1)
            .filter(|current_idx| *current_idx < queue_len)
    }

    /// 光标所在槽位的队列项，仅当它落在真实槽位上。
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
        // 复用共享的 path_playable 助手，让远程 HTTP(S) 流（酷狗）也可播，
        // 而不只认已存在的本地文件
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

    /// 单曲循环模式下光标所在曲目重播而不前进，返回光标槽位下标。非单曲循环、
    /// 或槽位已消失/不可播时返回 `None`。
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

    /// 一次导航步进是否重排了队列（随机播放开启时的列表循环回绕）。
    fn reshuffle_state(shuffle: bool) -> Reshuffled {
        if shuffle {
            Reshuffled::Reshuffled
        } else {
            Reshuffled::NotReshuffled
        }
    }

    /// 导航后的持久化：重排式回绕改动了队列内容（全量快照），普通前进只移动
    /// 光标。
    fn persist_after_navigation(&mut self, result: &QueueNavigationResult) {
        if let QueueNavigationResult::Changed { reshuffled, .. } = result {
            if *reshuffled == Reshuffled::Reshuffled {
                self.persist_session_with_queue();
            } else {
                self.persist_session_state();
            }
        }
    }

    /// 随机播放模式：把 `item` 追加到 `original_queue`，返回这次追加的 undo
    /// 区间（未洗牌时 `(0, 0)`——那种情况下 `original_queue` 按定义始终为空）。
    fn append_to_original_queue(&mut self, item: &QueueItemData) -> (usize, usize) {
        if self.shuffle {
            let start = self.original_queue.len();
            Arc::make_mut(&mut self.original_queue).push(item.clone());
            (start, 1)
        } else {
            (0, 0)
        }
    }

    /// 随机播放模式：把 `items` 追加到 `original_queue`，返回这次追加的 undo
    /// 区间（未洗牌时 `(0, 0)`）。
    fn append_all_to_original_queue(&mut self, items: &[QueueItemData]) -> (usize, usize) {
        if self.shuffle {
            let start = self.original_queue.len();
            Arc::make_mut(&mut self.original_queue).extend(items.iter().cloned());
            (start, items.len())
        } else {
            (0, 0)
        }
    }

    /// 判定插入对光标的影响：插在当前槽位之前的项会把光标右移 `cursor_shift`。
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

    /// 当前队列位置（0 起始）；没在播放时返回 None。
    pub fn current_position(&self) -> Option<usize> {
        Self::cursor_index(self.len(), self.queue_next)
    }

    /// 当前播放条目的在线提供方身份（仅当它是 HTTP 流时），用于在持久化的
    /// 链接 403 时重新取一个新 URL。
    #[cfg(feature = "online_sources")]
    pub fn current_online_identity(&self) -> Option<super::super::queue::OnlineIdentity> {
        let position = self.current_position()?;
        let queue = self.queue.read_recovered();
        queue.get(position)?.online_identity().cloned()
    }

    /// 当前播放条目的持久化展示元数据 `(name, artist, duration, cover)`，
    /// 供流登记表重新注册使用。
    #[cfg(feature = "online_sources")]
    pub fn current_display(&self) -> Option<PersistedDisplay> {
        let position = self.current_position()?;
        let queue = self.queue.read_recovered();
        queue.get(position)?.persisted_display()
    }

    /// 当前播放条目的路径（若有）。
    #[cfg(feature = "online_sources")]
    pub fn current_path(&self) -> Option<PathBuf> {
        let position = self.current_position()?;
        let queue = self.queue.read_recovered();
        Some(queue.get(position)?.get_path().clone())
    }

    /// 交换当前播放条目所存储的路径（其过期的流 URL 被新取回的一个替换时使用）。
    #[cfg(feature = "online_sources")]
    pub fn replace_current_path(&self, path: PathBuf) -> bool {
        // 在取写锁之前先把位置算出来：算位置要读队列，而持写锁期间再读会自死锁。
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

    /// 交换 `peek_next_path` 解析到的那条目所存储的路径——无缝预开在后台线程上
    /// 取回下一条目过期流 URL 之后的写回。只有该槽位仍持有 `expected`（发起
    /// 预开时用的路径）才生效：从请求到完成之间解析出的"下一首"可能移动
    /// （切换循环模式会把 `next` 重新指向当前槽位），把新 URL 盖到别的条目上
    /// 会写坏那个条目的路径。任何不匹配都静默丢弃新 URL——该次转换自身的同步
    /// 刷新重试仍会把它取回。与 [`Self::replace_current_path`] 一样，交换本身
    /// 不把会话标脏；刷新后的 URL 随下一次快照一起带走。
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

    /// 当前循环状态。
    pub fn repeat_state(&self) -> RepeatState {
        self.repeat
    }

    /// 队列长度。
    pub fn len(&self) -> usize {
        self.queue.read_recovered().len()
    }

    /// 随机播放开启时返回 true。
    pub fn is_shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    /// 队列里每个条目都属于同一个已知专辑时返回 true。
    pub fn all_items_same_album(&self) -> bool {
        let queue = self.queue.read_recovered();
        let Some(first_album) = queue.first().and_then(QueueItemData::get_db_album_id) else {
            return false;
        };

        queue
            .iter()
            .all(|item| item.get_db_album_id() == Some(first_album))
    }

    /// 首个可播条目的路径与下标，不 clone 条目本身。
    pub fn first_playable_path(&self) -> Option<(PathBuf, usize)> {
        let queue = self.queue.read_recovered();
        Self::first_playable_index_in(&queue).map(|index| (queue[index].get_path().clone(), index))
    }

    /// 最后一个可播条目的路径与下标，不 clone 条目本身。
    pub fn last_playable_path(&self) -> Option<(PathBuf, usize)> {
        let queue = self.queue.read_recovered();
        Self::last_playable_index(&queue).map(|index| (queue[index].get_path().clone(), index))
    }

    /// 首个可播条目的下标，给只要槽位的调用方用。
    pub fn first_playable_index(&self) -> Option<usize> {
        let queue = self.queue.read_recovered();
        Self::first_playable_index_in(&queue)
    }

    /// 直接设置队列位置（打开曲目之后使用）。
    pub fn set_position(&mut self, index: usize) {
        self.queue_next = index + 1;
        self.persist_session_state();
    }

    /// 设置循环状态。
    pub fn set_repeat(&mut self, state: RepeatState) {
        self.repeat = Self::normalize_repeat_state(&self.playback_settings, state);
        self.persist_session_state();
    }

    /// 更新播放设置。
    pub fn update_settings(&mut self, settings: PlaybackSettings) {
        self.playback_settings = settings;

        if self.playback_settings.always_repeat && self.repeat == RepeatState::NotRepeating {
            self.repeat = RepeatState::Repeating;
            self.persist_session_state();
        }
    }

    /// 前进到队列里的下一首。
    ///
    /// 返回接下来该播哪一首、或者应当停止播放的信息。
    ///
    /// 导航分三段：读锁下取队列快照 → 在所有锁之外判定 → 应用。可播性扫描会
    /// 调 `path.exists()`——卡死的 UNC/SMB 挂载上这次 stat 可达 ~100 ms，
    /// 绝不能跑在 UI 队列面板/歌词面板每帧读取的那把写锁里。拆分之所以安全：
    /// 播放线程是队列唯一变更方，快照与应用之间状态不会漂移，所以在快照上定出的
    /// 下标对活队列依然有效。
    pub fn next(&mut self, user_initiated: bool) -> QueueNavigationResult {
        let (queue, queue_next) = {
            let queue = self.queue.read_recovered();
            (Arc::clone(&queue), self.queue_next)
        };

        // 在所有锁之外判定。
        if !user_initiated
            && let Some(index) = Self::repeat_one_target(self.repeat, &queue, queue_next)
        {
            return QueueNavigationResult::Unchanged {
                path: queue[index].get_path().clone(),
            };
        }

        // （落点下标 + 结果重排标志，外加一份待在应用阶段写回的重排后队列）
        let (new_queue, landing) = if let Some(index) = Self::next_playable_from(&queue, queue_next)
        {
            (None, Some((index, Reshuffled::NotReshuffled)))
        } else if self.repeat == RepeatState::Repeating {
            // 列表循环回绕。随机播放开启时先重排整个队列：这里对本地副本洗牌、
            // 下面再写回（`replace_queue` 的 快照 → 锁外运算 → 写回 模式）。
            // 可播性与顺序无关，所以落点扫描在那份副本上做，不必在写锁里重新 stat。
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

        // 应用，保持旧的锁内顺序：先队列内容，再 undo 历史，最后光标。
        if let Some(new_queue) = new_queue {
            let mut queue = self.queue.write_recovered();
            *queue = new_queue;
            drop(queue);
            // 回绕把整个队列重排了，所以每条已记录 undo 项的下标都指向过期顺序；
            // 之后照它们执行会删错项或错插项。
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

    /// `peek_next_path` 解析到的队列槽位，判定基于给定的队列快照：单曲循环生效时
    /// 取单曲循环目标的槽位，否则取下一个可播项，再否则（随机播放关闭时）取列表
    /// 循环回绕的落点槽位（首个可播项）。与 `next(false)` 的落点判定保持同步，
    /// 这样无缝预开请求、它的 URL 刷新钩子与刷新后路径的写回指向同一条目。
    /// 前进不可预测时返回 `None`（无循环时到队列末尾；随机播放的回绕要重排）。
    fn peek_next_index(&self, queue: &[QueueItemData]) -> Option<usize> {
        Self::repeat_one_target(self.repeat, queue, self.queue_next)
            .or_else(|| Self::next_playable_from(queue, self.queue_next))
            // 随机播放关闭的列表循环回绕：`next(false)` 不重排任何东西、落在首个
            // 可播槽位，所以这次前进可预测，无缝预开也必须覆盖它——回绕以前会掉进
            // 冷打开，用的持久化 URL 可能已过期，那是在线播放里唯一残留的可闻曲目间隙。
            .or_else(|| {
                if self.repeat == RepeatState::Repeating && !self.shuffle {
                    Self::first_playable_index_in(queue)
                } else {
                    None
                }
            })
    }

    /// `next(false)` 将要打开的路径，不改动任何状态（供无缝预开使用）。前进不可
    /// 预测时返回 `None`（队列末尾、以及随机播放的回绕要重排）。
    pub fn peek_next_path(&self) -> Option<PathBuf> {
        let queue = self.queue.read_recovered();
        self.peek_next_index(&queue)
            .map(|index| queue[index].get_path().clone())
    }

    /// `peek_next_path` 会返回的那条目的在线提供方身份（若它带有）。这让无缝
    /// 预开能给出错钩子装上下一条（而非当前那条）的 URL 刷新。
    #[cfg(feature = "online_sources")]
    pub fn peek_next_online_identity(&self) -> Option<super::super::queue::OnlineIdentity> {
        let queue = self.queue.read_recovered();
        let index = self.peek_next_index(&queue)?;
        queue.get(index)?.online_identity().cloned()
    }

    /// `peek_next_path` 会返回的那条目的持久化展示元数据
    /// `(name, artist, duration, cover)`，供预开刷新 URL 时重新注册进流登记表。
    #[cfg(feature = "online_sources")]
    pub fn peek_next_display(&self) -> Option<PersistedDisplay> {
        let queue = self.queue.read_recovered();
        let index = self.peek_next_index(&queue)?;
        queue.get(index)?.persisted_display()
    }

    /// 回到队列里的上一首。
    ///
    /// 三段化同 `next()`：读锁下取快照，在所有锁之外判定，再应用。
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
            // 与 `next()` 里回绕同样的 undo 过期问题：已记录的下标指向洗牌前的
            // 顺序。在应用阶段随写回一起清空。
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

    /// 跳转到队列里的指定下标。
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

    /// 跳转到原序（未洗牌）队列里的某个下标。未洗牌时行为等同普通跳转。
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

    /// 在队列末尾追加单个条目，返回条目被加入的下标。
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

    /// 在队列末尾追加多个条目。随机播放开启时新条目先洗牌再追加，
    /// 返回首个加入条目的下标。
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

    /// 在指定位置插入单个条目。
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

    /// 在指定位置插入多个条目。
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

    /// 移除队列里指定下标的条目。
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
                // 下一首会顶进被移除的槽位。跳过不可播项，把光标留在实际会播的那首
                // 上：`current_position()` 就是播放线程经 QueuePositionChanged 上报的
                // 值，必须与已打开的路径一致，否则 UI 会为新曲目解析出错误的（或没有）
                // 队列条目。
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
                // 没在播放（queue_next == 0），或被移除的下标在当前槽位之后：只上报
                // 解析出的槽位，不凭空造光标。与 `dequeue_many` 的 `checked_sub` 语义
                // 一致（a632cef）——停止状态下单条路径不得声称有当前曲目。
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
            // 移除当前曲目之前的项会把它所在的槽位左移，所以光标必须跟上
            // （current - items_before_current），随后再跳过不可播项。留着过期的
            // `queue_next` 会让 `current_position()` 报出那个已被移除的槽位（或
            // None），于是 QueuePositionChanged 根本传不到 UI，信息区仍保留被移除
            // 曲目被清空后的元数据（"Unknown Track"）。
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

    /// 把一个条目从一个位置移到另一个位置。需要记入 undo 历史时把
    /// `user_initiated` 置为 `true`。
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
                // 移动的是当前曲目
                self.queue_next = to + 1;
                MoveResult::MovedCurrent { new_position: to }
            } else if from < current && to >= current {
                // 从当前之前移到当前之后
                self.queue_next -= 1;
                MoveResult::MovedCurrent {
                    new_position: self.queue_next - 1,
                }
            } else if from > current && to <= current {
                // 从当前之后移到当前之前
                self.queue_next += 1;
                MoveResult::MovedCurrent {
                    new_position: self.queue_next.saturating_sub(1),
                }
            } else {
                MoveResult::Moved
            }
        } else {
            // 没在播放（queue_next == 0）：移动条目不得凭空造出当前曲目。与已经用了
            // `checked_sub` 的 `move_items` 批量路径保持一致。
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

    /// 把多个条目移到同一个目标位置。
    ///
    /// `indices`（升序）处的条目先被移除，再从 `to` 起连续插回。目标 `to` 指的是
    /// 移除之后的最终位置（即首个被移动条目应当落到的下标）。
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

        // `to` 是移除后队列里的插入位置（调用方会为超出主源下标的额外移除做补偿）
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

        // 校准当前位置
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

    /// 用新条目整体替换队列。随机播放开启时会洗牌（但原序仍被保留）。
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

        // 快照靠把活的 Arc 取走而不是 clone：整个交换期间都持着写锁，读者不会观察到
        // 中间的空队列状态，undo 栈拿到独占所有权（于是下次变更的 `make_mut`
        // 也不会深拷贝）。
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

    /// 清空队列。`keep_current` 为 true 时保留当前播放的曲目，队列里只剩那一首。
    pub fn clear(&mut self, keep_current: bool) {
        let previous_queue_next = self.queue_next;
        let previous_shuffle = self.shuffle;

        let mut queue = self.queue.write_recovered();

        // 必须在下面那些 take 式快照之前读当前条目：`take` 会把 Vec 清空，之后再读
        // `queue.len()` 永远看到空队列，`clear(true)` 就静默丢掉了本该保留的播放中
        // 曲目。
        let current_item = if keep_current {
            Self::cursor_item(&queue, self.queue_next)
        } else {
            None
        };

        // take 式快照，缘由与 replace_queue 相同：不做整队拷贝
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

    /// 随机播放当前是否开启。
    pub fn shuffled(&self) -> bool {
        self.shuffle
    }

    /// 切换随机播放模式。
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

                // 把洗牌后的 Vec 直接 take 成 undo 快照而不 clone：内容无论如何都要
                // 搬进 Arc，这里只是省掉那次拷贝。
                let shuffled_queue = take(&mut *queue);
                *queue = take(&mut self.original_queue);
                // 只有原本存在当前曲目时才重指光标；没在播放（或光标是越过末尾的过期
                // 值）就保持 queue_next 不变，取消洗牌绝不凭空造出当前条目。
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
        // queue_next 保持 0（没在播放）。
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
        // queue_next 保持 0（没在播放）。
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
        manager.set_position(1); // 当前 = 下标 1（item 2）

        let res = manager.dequeue_many(vec![1, 3]);

        // new_path 取决于文件是否真的存在于磁盘上；这里只断言变体。
        assert!(matches!(res, DequeueManyResult::RemovedCurrent { .. }));
        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn dequeue_many_removes_items_before_current_shifts_position() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(3); // 当前 = 下标 3（item 4）

        let res = manager.dequeue_many(vec![0, 1]);

        match res {
            DequeueManyResult::Removed { new_position } => assert_eq!(new_position, 1),
            other => panic!("expected Removed, got {other:?}"),
        }
        // 当前项（item 4）现在在下标 1；queue_next 指向它之后。
        assert_eq!(manager.queue_next, 2);
    }

    #[test]
    fn dequeue_many_removes_items_after_current_keeps_position() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4), item(5)]);
        manager.set_position(1); // 当前 = 下标 1（item 2）
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
        manager.set_position(3); // 当前 = 下标 3（item 4）

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

    /// 路径确实存在于磁盘上的队列项：`item_is_playable`（本地文件走
    /// `path.exists()` 判定）因此认它们，`next_playable_from` 也能推进到它们上面。
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

    /// 回归：连同当前曲目之前的条目一起移除时，光标必须落到播放实际续播的那首上。
    /// 过期的 `queue_next` 会让 `current_position()` 返回 None（或那个已被移除的
    /// 槽位），于是 QueuePositionChanged 根本传不到 UI，音频已经在播下一项了，信息区
    /// 却仍保留被移除曲目清空后的元数据（"Unknown Track"）。
    #[test]
    fn dequeue_many_removing_current_with_earlier_items_keeps_cursor_on_played_track() {
        let dir = TestDir::new("meliora-queue-dequeue-many-current");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3, 4]));
        manager.set_position(2); // 当前 = 下标 2（item 3）

        let res = manager.dequeue_many(vec![0, 2]); // 当前之前的一项 + 当前项

        match res {
            DequeueManyResult::RemovedCurrent { new_path } => {
                // 被移除当前项之后那首顶进了它的槽位
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

    /// 移除当前曲目时必须上报实际被打开那首的位置，UI 才能解析到正确的队列条目。
    #[test]
    fn dequeue_removing_current_reports_position_of_the_opened_track() {
        let dir = TestDir::new("meliora-queue-dequeue-current");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_position(1); // 当前 = 下标 1（item 2）

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

    /// 顶进被移除槽位的那一项不可播时，前进会跳过它；光标必须跟到真正被播放的那项
    /// 上，而不是停在不可播的槽位里。
    #[test]
    fn dequeue_removing_current_skips_unplayable_items_and_follows_the_played_track() {
        let dir = TestDir::new("meliora-queue-dequeue-unplayable");
        let mut items = playable_items(&dir, &[1, 2, 3]);
        // 把中间那项指向一个并不存在的文件
        let missing = dir.join("meliora-dequeue-missing.flac");
        items[1] = serde_json::from_value(json!({
            "db_id": 2,
            "db_album_id": 0,
            "path": missing.to_string_lossy(),
        }))
        .expect("valid queue item");

        let mut manager = manager_with_queue(items);
        manager.set_position(0); // 当前 = 下标 0（item 1）

        let res = manager.dequeue(0);

        match res {
            DequeueResult::RemovedCurrent { new_path } => {
                // item 2 不可播，所以播放从 item 3（下标 1）续上
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
        manager.set_position(0); // 当前 = 下标 0（item 1）

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

    /// 回归：take 式快照重构在读取 `current_item` 之前就把队列 Vec 清空了，于是
    /// `clear(true)` 永远看到空队列，静默丢掉本该保留的播放中曲目。
    #[test]
    fn clear_keep_current_preserves_the_playing_track() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3), item(4)]);
        manager.set_position(2); // 当前 = 下标 2（item 3）

        manager.clear(true);

        assert_eq!(queue_ids(&manager), vec![3]);
        assert_eq!(manager.queue_next, 1);
        assert_eq!(manager.current_position(), Some(0));

        // 随机播放模式：被保留的项也要登记进 original_queue，
        // 这样之后取消洗牌时仍能找到它
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

    /// 回归：`move_item` 用 `saturating_sub` 推导当前槽位，所以在没播放任何东西时移动
    /// 条目会凭空造出当前曲目（queue_next 0 -> to+1 / 1），线程随之上报一个假的
    /// QueuePositionChanged。`move_items` 批量路径已经用了 `checked_sub`，
    /// 单条路径必须与它一致。
    #[test]
    fn move_item_when_nothing_playing_does_not_start_playback() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        assert_eq!(manager.queue_next, 0);

        // 移动第一项原先会走 `from == current` 分支
        let res = manager.move_item(0, 2, true);
        assert!(matches!(res, MoveResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![2, 3, 1]);

        // 把任意项移到下标 0 原先会走 `to <= current` 分支
        let res = manager.move_item(2, 0, true);
        assert!(matches!(res, MoveResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![1, 2, 3]);

        // 单下标的 `move_items` 委托也必须行为一致
        let res = manager.move_items(vec![1], 2);
        assert!(matches!(res, MoveItemsResult::Moved));
        assert_eq!(manager.queue_next, 0);
        assert_eq!(queue_ids(&manager), vec![1, 3, 2]);
    }

    /// 回归：没在播放时关闭随机播放会凭空造出当前曲目（queue_next 0 -> 1）：管理器
    /// 于是声称 item 0 正在播放，会话快照还把假的 queue_position 持久化了下去。
    #[test]
    fn toggle_shuffle_off_when_nothing_playing_keeps_no_current() {
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        assert_eq!(manager.queue_next, 0);

        manager.toggle_shuffle(); // 开启：没在播放，整个队列被洗牌
        assert_eq!(manager.queue_next, 0);
        manager.undo_stack.clear();

        manager.toggle_shuffle(); // 关闭
        assert_eq!(manager.queue_next, 0);
        assert_eq!(manager.current_position(), None);
    }

    /// 回归：列表循环回绕会把整个（已洗牌的）队列重新洗牌，这让每条已记录 undo 项的
    /// 下标都失效；之后照它们执行会删错项或错插项。回绕必须丢掉这份过期历史。
    #[test]
    fn repeat_wrap_reshuffle_drops_stale_undo_entries() {
        let dir = TestDir::new("meliora-queue-wrap-undo");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3, 4]));
        manager.set_repeat(RepeatState::Repeating);
        manager.set_position(3); // 停在最后一首
        manager.toggle_shuffle();
        manager.dequeue(0); // undo 项记录的是回绕前的顺序

        // 最后一首遇到末尾时，列表循环 + 随机播放会回绕并重新洗牌
        let res = manager.next(false);
        assert!(matches!(
            res,
            QueueNavigationResult::Changed {
                reshuffled: Reshuffled::Reshuffled,
                ..
            }
        ));

        // 重洗后的顺序让已记录的逆操作失效
        assert!(matches!(manager.undo_last_action(), UndoResult::None));
        // 回绕之后光标仍停在一个有效曲目上
        assert!(manager.current_position().is_some());
    }

    /// 线程经 `jump_unshuffled` 解析被打开条目的槽位，因为随机播放开启时
    /// `queue_items` 会把追加的那一批洗牌，被打开的项已不再坐在
    /// `first_index + relative_idx` 上。
    #[test]
    fn jump_unshuffled_resolves_items_appended_by_a_shuffled_batch() {
        let dir = TestDir::new("meliora-queue-jump-unshuffled-batch");
        let mut manager = manager_with_queue(vec![item(1), item(2), item(3)]);
        manager.set_position(0);
        manager.toggle_shuffle();
        manager.undo_stack.clear();

        let first_index = manager.queue_items(playable_items(&dir, &[10, 11, 12]));

        // item 10 是用户原序里首个被追加的项
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

    /// 随机播放关闭的列表循环回绕落在首个可播槽位（`next(false)` 的判定），所以无缝
    /// 预开也必须看到它：`peek_next_path` 以前在最后一项之后返回 None，于是回绕掉进
    /// 冷打开——带着持久化的在线 URL 时那次冷打开要付 fail-then-refresh
    /// （~375 ms 的可闻欠载，2026-10-06 日志核实：下标 19 → 0 前进时没有已预备的流）。
    #[test]
    fn peek_next_path_covers_repeat_all_wrap_without_shuffle() {
        let dir = TestDir::new("meliora-queue-wrap-peek");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_repeat(RepeatState::Repeating);
        manager.set_position(2); // 停在最后一首

        let peeked = manager
            .peek_next_path()
            .expect("wrap target is predictable");
        assert_eq!(
            peeked,
            manager.queue.read().expect("poisoned queue lock")[0]
                .get_path()
                .clone()
        );

        // 保持同步：前进正好落在被 peek 的那个槽位上
        assert!(matches!(
            manager.next(false),
            QueueNavigationResult::Changed { index: 0, .. }
        ));
    }

    #[test]
    fn peek_next_path_stays_none_beyond_queue_end_without_repeat() {
        let dir = TestDir::new("meliora-queue-end-peek");
        let mut manager = manager_with_queue(playable_items(&dir, &[1, 2, 3]));
        manager.set_position(2); // 停在最后一首，循环关闭

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

        // 重洗后的落点顺序不可预测
        assert!(manager.peek_next_path().is_none());
    }
}
