//! 流注册表（stream map）落盘的共享设施：去重基线（只写真变了的字节）+
//! 写闸门（串行化临时文件写与 rename，防 JSON 撕裂）。与具体 provider 无关，
//! 无条件编译，不依赖任何 provider feature。
//!
//! 关键约束：每个在线源必须各持一份 `static` 实例。
//! 基线是各注册表自己的状态——共享同一实例会让去重跳写与失败回滚
//! 覆盖/误伤另一份注册表的写入。

use crate::sync::RecoveredMutex;
use std::{path::PathBuf, sync::Mutex};

/// 注册表条目上限：LRU 语义，最新条目在头部。各在线源共用同一口径。
pub(crate) const STREAM_MAP_CAP: usize = 200;

/// 一个在线源的流注册表持久化设施。见模块文档：每个在线源各持一份。
pub struct StreamMapPersistence {
    /// 最后一次交给持久化任务的 stream map JSON 字节。留着它是为了在重复登记一份没变过的
    /// 注册表（例如重播同一首歌）时完全跳过磁盘写，而不是把相同的字节再写一遍。
    last: Mutex<Option<Vec<u8>>>,
    /// 串行化 stream map 的持久化。写入本身落到临时文件再 rename 覆盖目标，所以进程写到
    /// 一半死掉也绝不会留下截断的 JSON；这道门还挡住两个写者交错各自的临时文件。tokio 的
    /// 互斥锁按 poll 顺序（不是 spawn 顺序）公平，所以写者大致按交接顺序落盘；最后一次写
    /// 陈旧只多花下次启动的一次 URL 刷新（注册表是缓存）。
    gate: tokio::sync::Mutex<()>,
}

impl StreamMapPersistence {
    /// `const`，让每个 provider 直接持一个普通 `static` 实例（首次使用前
    /// 不可能有竞争，无需惰性初始化）。
    pub const fn new() -> Self {
        Self {
            last: Mutex::new(None),
            gate: tokio::sync::Mutex::const_new(()),
        }
    }

    /// 与最后一次落盘的字节不同就把 `json` 原样还回（并记为新基线），没变则返回 `None`、
    /// 写任务可以整个跳过。这把互斥锁只被持有一次 memcmp 的时间。
    pub fn persist_if_changed(&self, json: Vec<u8>) -> Option<Vec<u8>> {
        let mut last = self.last.lock_recovered();
        if last.as_deref() == Some(json.as_slice()) {
            return None;
        }
        *last = Some(json.clone());
        Some(json)
    }

    /// 在共享运行时上派生被门串行化的磁盘写。调用前提：
    /// [`Self::persist_if_changed`](Self::persist_if_changed) 刚返回了
    /// `Some(json)`（路径也只在此时构造，去重跳过时零额外开销）。
    /// `on_error` 负责打日志（文案含 provider 名）。
    pub fn spawn_persist(
        &'static self,
        path: PathBuf,
        json: Vec<u8>,
        on_error: impl FnOnce(&std::io::Error) + Send + 'static,
    ) {
        crate::RUNTIME.spawn(async move {
            let _gate = self.gate.lock().await;
            let tmp = path.with_extension("json.tmp");
            let result = match tokio::fs::write(&tmp, &json).await {
                Ok(()) => tokio::fs::rename(&tmp, &path).await,
                Err(err) => Err(err),
            };
            if let Err(err) = result {
                let _ = tokio::fs::remove_file(&tmp).await;
                // 回滚基线，让之后相同的一份快照是重试，而不是静默地把旧文件留在原地。
                // 仅当基线仍是自己的字节时才回滚：期间可能有更新的写者已经改过它。
                let mut last = self.last.lock_recovered();
                if last.as_deref() == Some(json.as_slice()) {
                    last.take();
                }
                on_error(&err);
            }
        });
    }

    /// 各在线源 `remember_online_track` 的持久化尾部：序列化 → 去重 → 门控写，
    /// 单点维护。`provider` 仅进失败日志文案。序列化失败按既有语义
    /// 静默跳过（缓存是优化，显示不依赖它）。
    pub fn persist_snapshot(
        &'static self,
        path: PathBuf,
        snapshot: &impl serde::Serialize,
        provider: &'static str,
    ) {
        let Ok(json) = serde_json::to_vec(snapshot) else {
            return;
        };
        if let Some(json) = self.persist_if_changed(json) {
            self.spawn_persist(path, json, move |err| {
                tracing::warn!(%err, "failed to persist {provider} stream map");
            });
        }
    }
}

#[cfg(test)]
mod persist_guard_tests {
    //! 持久化去重守卫的钉子（十五轮测试缺口矩阵中危档）：相同快照必须跳写、
    //! 变化必须放行，且回滚条件（基线仍是自己的字节）不在此函数内。
    use super::*;

    #[test]
    fn persist_if_changed_dedupes_and_admits_changes() {
        let guard = StreamMapPersistence::new();

        // 首次：放行
        let first = guard.persist_if_changed(b"snapshot-1".to_vec());
        assert_eq!(first.as_deref(), Some(b"snapshot-1".as_slice()));

        // 相同快照：跳写
        assert_eq!(guard.persist_if_changed(b"snapshot-1".to_vec()), None);

        // 变化：放行并成为新基线
        assert_eq!(
            guard.persist_if_changed(b"snapshot-2".to_vec()).as_deref(),
            Some(b"snapshot-2".as_slice())
        );
        assert_eq!(guard.persist_if_changed(b"snapshot-2".to_vec()), None);

        // 回到旧快照也算变化（基线是最近一次，不是历史去重）
        assert_eq!(
            guard.persist_if_changed(b"snapshot-1".to_vec()).as_deref(),
            Some(b"snapshot-1".as_slice())
        );
    }
}

#[cfg(test)]
mod persist_tests {
    //! 持久化去重与回滚守卫（十五轮测试缺口矩阵中危档）：
    //! - persist_if_changed 去重语义：相同跳写、变化放行、基线始终最近一次
    //! - spawn_persist 写失败回滚基线：相同快照是重试而非静默跳过；
    //!   不匹配的失败不得覆盖更新的写者
    use super::*;
    use crate::test_support::TestDir;
    use std::time::Duration;

    #[test]
    fn persist_if_changed_dedupes_and_admits_changes() {
        let guard = StreamMapPersistence::new();
        // 首次放行
        assert_eq!(
            guard.persist_if_changed(b"v1".to_vec()).as_deref(),
            Some(b"v1".as_slice())
        );
        // 重复跳过
        assert_eq!(guard.persist_if_changed(b"v1".to_vec()), None);
        // 变化放行
        assert_eq!(
            guard.persist_if_changed(b"v2".to_vec()).as_deref(),
            Some(b"v2".as_slice())
        );
        // 基线是最近一次（v2），不是历史去重——回到 v1 也是变化
        assert_eq!(
            guard.persist_if_changed(b"v1".to_vec()).as_deref(),
            Some(b"v1".as_slice())
        );
    }

    #[test]
    fn rollback_admits_retry_only_when_bytes_match() {
        let guard = StreamMapPersistence::new();
        guard.persist_if_changed(b"v1".to_vec());

        // 模拟写失败 + 回滚（spawn_persist 失败分支的 last.take()），
        // 基线仍是 v1 → 清除 → 重试放行
        {
            let mut last = guard.last.lock_recovered();
            if last.as_deref() == Some(b"v1".as_slice()) {
                last.take();
            }
        }
        assert_eq!(
            guard.persist_if_changed(b"v1".to_vec()).as_deref(),
            Some(b"v1".as_slice()),
            "回滚后重试必须放行"
        );

        // 基线 = v2；模拟 v1 的失败回滚尝试（不匹配 → 不覆盖）
        guard.persist_if_changed(b"v2".to_vec());
        {
            let mut last = guard.last.lock_recovered();
            if last.as_deref() == Some(b"v1".as_slice()) {
                last.take();
            }
        }
        assert_eq!(
            guard.persist_if_changed(b"v2".to_vec()),
            None,
            "v2 基线不受 v1 失败影响"
        );
    }

    #[test]
    fn spawn_persist_failure_rolls_back_for_retry() {
        // 端到端：好路径建立基线，坏路径写失败 → 基线回滚 → 相同快照重试放行
        let guard: &'static StreamMapPersistence = Box::leak(Box::new(StreamMapPersistence::new()));
        let dir = TestDir::new("stream-persist-rollback");

        // 建基线 v1 并成功写盘
        assert_eq!(
            guard.persist_if_changed(b"v1".to_vec()).as_deref(),
            Some(b"v1".as_slice())
        );
        let good = dir.join("good.json");
        crate::RUNTIME.block_on(async {
            guard.spawn_persist(good.clone(), b"v1".to_vec(), |_| {});
            tokio::time::sleep(Duration::from_millis(500)).await;
        });
        assert!(good.exists(), "好路径应成功写盘");
        assert_eq!(guard.persist_if_changed(b"v1".to_vec()), None);

        // 坏路径发 v2（放行，基线变 v2）→ 写失败 → 基线回滚 → v2 重试放行
        let bad = dir.join("no-such-dir/bad.json");
        assert_eq!(
            guard.persist_if_changed(b"v2".to_vec()).as_deref(),
            Some(b"v2".as_slice())
        );
        crate::RUNTIME.block_on(async {
            let g = guard;
            let b = bad.clone();
            let j = b"v2".to_vec();
            guard.spawn_persist(b, j, |_| {});
            tokio::time::sleep(Duration::from_millis(500)).await;
        });
        assert_eq!(
            guard.persist_if_changed(b"v2".to_vec()).as_deref(),
            Some(b"v2".as_slice()),
            "写失败后基线回滚，重试放行"
        );
    }
}
