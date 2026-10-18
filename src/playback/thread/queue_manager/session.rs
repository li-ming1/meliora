//! Session 持久化防抖：队列快照/播放位置的节流持久化与挂起快照冲刷。
//! 自 queue_manager.rs 原样搬出；经 `use super::*` 白盒访问 QueueManager
//! 的私有字段与关联函数。内部方法放宽为 `pub(super)`（宿主子树可见），
//! `flush_pending_session` 维持 `pub`（thread.rs 主循环经原路径调用）。

use crate::sync::RecoveredRwLock;
use std::time::{Duration, Instant};

use super::*;

/// 向会话存储 worker 发送队列快照的最小间隔。一次发送会深拷贝整条队列
/// （见 `send_session_with_queue`），所以突发变更（批量删除、快速跳曲）会
/// 合并成每个窗口一次快照。
const SESSION_PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

impl QueueManager {
    /// 仅播放状态变化时（切歌前进、跳转定位、循环/随机切换）持久化队列会话。
    ///
    /// 按共用的防抖窗口（`last_session_send`）三分支处理：
    ///
    /// - `session_dirty` 已置（有队列快照待发）：立刻升级为完整发送。这是正确性
    ///   要求——待发快照早于本次状态变化，在其上只打状态补丁可能持久化出指向
    ///   旧队列内容的位置（"新位置、旧队列"）。完整快照同时带上全新队列与本次
    ///   状态变化。
    /// - 窗口之内：只置 `state_dirty`；`flush_pending_session` 把突发（快速跳曲）
    ///   合并成一次仅状态发送。延后是安全的：watch 值始终在已存队列快照之上保持
    ///   最新状态，连续状态更新因此无损合并，崩溃暴露窗口（位置最多陈旧 500ms）
    ///   与队列变更本来就已接受的范围一致。
    /// - 窗口之外：立即发送（前导沿、仅状态的低成本路径——已存队列快照原样复用）
    ///   并占用窗口，紧随其后的队列变更因此合并成一次快照，而不是拆成状态写加
    ///   完整写。
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

    /// 仅状态的会话发送：复用已存队列快照，刷新 position/shuffle/repeat，然后
    /// 清除 `state_dirty` 并占用防抖窗口。发送侧远比 `send_session_with_queue`
    /// 便宜（不再抖动 queue/original_queue 的引用计数）；存储 worker 仍是每次
    /// 通知重写一次文件，而这正是窗口要合并掉的开销。
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

    /// 队列内容或顺序发生变化时持久化队列会话。
    ///
    /// 发送受防抖：空闲窗口之后的第一次变更立即发送（前导沿），窗口内的后续变更
    /// 只把会话标脏，由播放主循环调用 `flush_pending_session` 冲刷。发送要对整条
    /// 队列做快照，因此这把批量变更压成每窗口一次快照，而不是每项一次。
    ///
    /// 窗口锚点与仅状态路径（`persist_session_state`）共用：状态发送同样占用窗口，
    /// 紧随其后的队列变更会被延后合并成一次快照，而不是拆成状态写加完整写。
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

    /// 冲刷挂起的会话发送（若有），否则是廉价空操作；播放主循环每轮都调用。
    /// 按顺序分三档判断：
    ///
    /// 1. `session_dirty` —— 欠一次完整队列快照（其前导沿发送被窗口挡住）。
    ///    快照同时带上当前状态，因此挂着的 `state_dirty` 被并入并一并清除。
    /// 2. 仅 `state_dirty` —— 窗口内播放状态有变；仅状态发送在已存队列快照上
    ///    刷新 position/shuffle/repeat。
    /// 3. 两者皆无 —— 无事可做。
    pub fn flush_pending_session(&mut self) {
        if self.session_dirty {
            self.send_session_with_queue();
        } else if self.state_dirty {
            self.send_session_state_only();
        }
    }

    /// 取一份一致的队列快照并发送给会话存储 worker。
    ///
    /// 队列是写时复制的，因此读锁内只是两次引用计数递增——worker 在锁释放后
    /// 用它自己那份引用做序列化。调用点靠防抖把它挡在突发路径之外。
    ///
    /// 完整快照必然覆盖播放状态，所以两个脏标志都在这里清除，不会有挂起的仅状态
    /// 更新活过它。
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
