//! Undo 子系统：撤销动作记录（UndoAction）、栈容量治理与逆操作回放
//! （undo_last_action）。自 queue_manager.rs 原样搬出；经 `use super::*`
//! 白盒访问宿主的私有常量与 QueueManager 私有字段/关联函数。

use tracing::info;

use super::*;

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
    /// Items were inserted into the queue. Inserts are always contiguous, so
    /// the undo payload is a `(start, len)` range instead of an index list.
    Inserted {
        queue_range: (usize, usize),
        original_queue_range: (usize, usize),
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

impl QueueManager {
    fn undo_result_from_state(
        queue: &[QueueItemData],
        queue_next: usize,
        shuffle: bool,
    ) -> UndoResult {
        if let Some(current_idx) = Self::cursor_index(queue.len(), queue_next) {
            UndoResult::Ok {
                current_idx,
                current_path: queue[current_idx].get_path().clone(),
                shuffle,
            }
        } else {
            UndoResult::OkNoCurrent { shuffle }
        }
    }

    /// Undo a removal: merge the removed `(index, item)` pairs back in one
    /// ascending pass（逐对 insert 是 k 次 O(n) memmove，k 大的多选删除在
    /// undo 时会退回 dequeue_many 修掉的那类冻结；归并重建只付一遍）.
    fn undo_reinsert_items(
        queue: &mut Vec<QueueItemData>,
        removed: SmallVec<[(usize, QueueItemData); 1]>,
    ) {
        let mut removed = removed.into_vec();
        removed.sort_unstable_by_key(|(idx, _)| *idx);
        let extra = removed.len();
        let mut removed = removed.into_iter().peekable();
        let mut restored = Vec::with_capacity(queue.len() + extra);
        for q_item in queue.drain(..) {
            while removed
                .peek()
                .is_some_and(|(idx, _)| *idx <= restored.len())
            {
                let (_, item) = removed.next().expect("just peeked");
                restored.push(item);
            }
            restored.push(q_item);
        }
        for (idx, item) in removed {
            if idx <= restored.len() {
                restored.push(item);
            }
        }
        *queue = restored;
    }

    /// Undo an insertion: drain the recorded contiguous range in one move
    /// （逐索引 remove 是 O(k×n) memmove，范围 drain 只需一次）.
    fn undo_remove_items(queue: &mut Vec<QueueItemData>, (start, len): (usize, usize)) {
        queue.drain(start..start + len);
    }

    pub(super) fn push_undo_action(&mut self, action: UndoAction) {
        // 超限批删不入栈且整栈清空，取舍见 UNDO_REMOVED_ITEM_CAP 的注释。
        if let UndoAction::Removed { queue_items, .. } = &action
            && queue_items.len() > UNDO_REMOVED_ITEM_CAP
        {
            info!(
                items = queue_items.len(),
                cap = UNDO_REMOVED_ITEM_CAP,
                "batch removal exceeds the undo item cap; clearing undo history, this removal is not undoable"
            );
            self.undo_stack.clear();
            return;
        }

        // 淘汰只从最旧端进行，保证剩余撤销历史连续（撤销按 LIFO 逐条回放，
        // 从中间挖洞会让后续 undo 跳过中间步骤的逆操作）。
        let is_snapshot = matches!(action, UndoAction::Replaced { .. });
        while self.undo_stack.len() >= UNDO_STACK_CAPACITY
            || (is_snapshot
                && self
                    .undo_stack
                    .iter()
                    .filter(|entry| matches!(entry, UndoAction::Replaced { .. }))
                    .count()
                    >= UNDO_SNAPSHOT_CAPACITY)
        {
            if self.undo_stack.pop_front().is_none() {
                break;
            }
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
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                *queue = old_queue;
                self.original_queue = old_original_queue;
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
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                let queue = Arc::make_mut(&mut queue);
                Self::undo_reinsert_items(queue, queue_items);

                let original_queue = Arc::make_mut(&mut self.original_queue);
                Self::undo_reinsert_items(original_queue, original_queue_items);

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Inserted {
                queue_range,
                original_queue_range,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                let queue = Arc::make_mut(&mut queue);
                Self::undo_remove_items(queue, queue_range);

                let original_queue = Arc::make_mut(&mut self.original_queue);
                Self::undo_remove_items(original_queue, original_queue_range);

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Moved {
                original,
                new,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                let queue = Arc::make_mut(&mut queue);

                let item = queue.remove(new);
                queue.insert(original, item);

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::MovedMany {
                items,
                destination,
                previous_queue_next,
                previous_shuffle,
            }) => {
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                let queue = Arc::make_mut(&mut queue);

                // 单遍重建替代逐项 remove+insert（与 dequeue_many/move_items
                // 同型）：旧实现对每个移动项各做一次 O(n) memmove（k 项即
                // O(k×n)），全程持有 UI 队列面板/歌词面板每帧读取的同一把写
                // 锁。move_items 记录 pristine 升序 (idx, item)：一次 drain
                // 重排——目标块内的克隆件让位，payload 原件按 pristine 索引
                // 归位，其余项照序保留，整体 O(n)。
                let mut moved = items.into_vec();
                moved.sort_unstable_by_key(|(idx, _)| *idx);
                let block_end = destination.saturating_add(moved.len());
                let mut moved = moved.into_iter().peekable();
                let mut restored = Vec::with_capacity(queue.len());
                for (post_idx, q_item) in queue.drain(..).enumerate() {
                    while moved.peek().is_some_and(|(idx, _)| *idx <= restored.len()) {
                        let (_, item) = moved.next().expect("just peeked");
                        restored.push(item);
                    }
                    if post_idx >= destination && post_idx < block_end {
                        continue;
                    }
                    restored.push(q_item);
                }
                // 整块落在队尾等场景下 pristine 索引尚未轮到的余项在此按序
                // 补齐；越界项沿用旧实现的守卫语义丢弃（不 panic）。
                for (idx, item) in moved {
                    if idx <= restored.len() {
                        restored.push(item);
                    }
                }
                *queue = restored;

                self.queue_next = previous_queue_next;
                self.shuffle = previous_shuffle;

                Self::undo_result_from_state(queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Shuffled) => {
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                *queue = take(&mut self.original_queue);
                self.shuffle = false;

                Self::undo_result_from_state(&queue, self.queue_next, self.shuffle)
            }
            Some(UndoAction::Unshuffled {
                shuffled_queue,
                previous_queue_next,
            }) => {
                let mut queue = self.queue.write().unwrap_or_else(|e| e.into_inner());
                self.original_queue = std::mem::replace(&mut *queue, shuffled_queue);
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{
        DequeueManyResult, MoveItemsResult, QueueManager,
        tests::{QueueManagerState, item, manager_with_queue, queue_ids, snapshot},
    };
    use super::UndoResult;

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
        use super::super::UNDO_STACK_CAPACITY;

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

    /// 超过 UNDO_REMOVED_ITEM_CAP 的批删不记录 undo 且整栈清空（取舍见
    /// UNDO_REMOVED_ITEM_CAP 注释）：先记一条小动作，验证它随大删除一并
    /// 丢弃——LIFO 纪律下旧动作不能叠在不可撤销删除之后被单独 undo。
    #[test]
    fn dequeue_many_above_undo_item_cap_clears_the_undo_stack() {
        use super::super::UNDO_REMOVED_ITEM_CAP;

        let count = UNDO_REMOVED_ITEM_CAP as i64 + 1;
        let items: Vec<_> = (1..=count).map(item).collect();
        let mut manager = manager_with_queue(items);
        manager.set_position(0);

        manager.insert_item(0, item(10_000));
        assert_eq!(manager.undo_stack.len(), 1);

        let res = manager.dequeue_many((0..count as usize).collect());

        match res {
            DequeueManyResult::RemovedCurrent { new_path } => assert_eq!(new_path, None),
            other => panic!("expected RemovedCurrent, got {other:?}"),
        }
        assert!(manager.undo_stack.is_empty());
        assert!(matches!(manager.undo_last_action(), UndoResult::None));
    }

    /// 恰好等于上限的批删仍正常记录、正常撤销（严格大于上限才触发清栈）。
    #[test]
    fn dequeue_many_at_undo_item_cap_is_still_undoable() {
        use super::super::UNDO_REMOVED_ITEM_CAP;

        let count = UNDO_REMOVED_ITEM_CAP as i64;
        let items: Vec<_> = (1..=count).map(item).collect();
        let mut manager = manager_with_queue(items);

        let res = manager.dequeue_many((0..count as usize).collect());

        match res {
            DequeueManyResult::Removed { new_position } => assert_eq!(new_position, 0),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert_eq!(manager.undo_stack.len(), 1);
        assert!(matches!(
            manager.undo_last_action(),
            UndoResult::OkNoCurrent { .. }
        ));
        assert_eq!(manager.len(), count as usize);
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

    /// 回归：shuffle 批量删除时 original_queue 的 undo 记录必须是 pristine
    /// 位置。旧实现记录随前序删除左移的演化位置：在本用例的乱序排列下
    /// （original [1,2,3,4]、queue [2,3,1,4]、按降序队列索引处理 id1→id3
    /// →id2），id2 的记录由 pristine 1 坍缩成演化 0，undo 的升序插入会把
    /// 还原的 original_queue 重排成 [2,3,1,4]。
    #[test]
    fn undo_dequeue_many_in_shuffle_mode_restores_pristine_original_positions() {
        let mut manager = manager_with_queue(vec![item(2), item(3), item(1), item(4)]);
        // tests 模块直写私有字段构造确定性乱序排列（toggle_shuffle 的随机
        // 洗牌覆盖不到上述特定顺序）。
        manager.original_queue = Arc::new(vec![item(1), item(2), item(3), item(4)]);
        manager.shuffle = true;
        manager.undo_stack.clear();

        let before = snapshot(&manager);

        manager.dequeue_many(vec![0, 1, 2]);

        assert_eq!(manager.len(), 1);
        assert_eq!(manager.original_queue.len(), 1);
        assert_undo_round_trip(&mut manager, before);
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

    /// 多段（不相邻索引）批量移动的 undo 必须逐项还原：内容与顺序和移动
    /// 前完全一致（MovedMany 还原改单遍重建后的回归断言）。
    #[test]
    fn undo_move_items_many_segments_restores_every_item_in_order() {
        let original: Vec<_> = (1..=8).map(item).collect();
        let mut manager = manager_with_queue(original.clone());
        manager.set_position(5); // current = index 5 (item 6)

        let res = manager.move_items(vec![0, 3, 7], 2);

        assert_eq!(res, MoveItemsResult::MovedCurrent { new_position: 6 });
        assert_eq!(queue_ids(&manager), vec![2, 3, 1, 4, 8, 5, 6, 7]);

        let undo = manager.undo_last_action();
        assert!(matches!(undo, UndoResult::Ok { .. }));

        assert_eq!(queue_ids(&manager), (1..=8).collect::<Vec<i64>>());
        let queue = manager.queue.read().expect("poisoned queue lock");
        assert_eq!(queue.len(), original.len());
        for (idx, (restored, expected)) in queue.iter().zip(original.iter()).enumerate() {
            assert_eq!(restored, expected, "restored item at index {idx} differs");
        }
        drop(queue);
        assert_eq!(manager.queue_next, 6);
        assert!(!manager.shuffle);
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
