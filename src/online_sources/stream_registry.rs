//! 流注册表（stream map）落盘的共享设施：去重基线（只写真变了的字节）+
//! 写闸门（串行化 truncate+write，防 JSON 撕裂）。与具体 provider 无关，
//! 无条件编译，不依赖任何 provider feature。
//!
//! 关键约束：每个 provider（kugou / netease）必须各持一份 `static` 实例。
//! 基线是各注册表自己的状态——共享同一实例会让一侧的去重跳写与失败回滚
//! 覆盖/误伤另一侧的写入。

use std::{path::PathBuf, sync::Mutex};

/// 一个 provider 的流注册表持久化设施。见模块文档：每个 provider 各持一份。
pub struct StreamMapPersistence {
    /// Bytes of the stream-map JSON last handed to the persistence task. Kept
    /// so re-recording an unchanged registry (e.g. replaying the same song)
    /// skips the disk write entirely instead of re-writing identical bytes.
    last: Mutex<Option<Vec<u8>>>,
    /// Serializes stream-map persistence. Two overlapping truncate+write on
    /// the same file can interleave into torn, unparseable JSON — which would
    /// drop the whole registry on next launch — so every write takes this
    /// gate first. tokio's mutex is fair by poll order (not spawn order), so
    /// writers land roughly in hand-off order; a stale final write only costs
    /// a URL refresh on next launch (the registry is a cache).
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

    /// Returns `json` back when it differs from the last persisted bytes (and
    /// records it as the new baseline), or `None` when it is unchanged and the
    /// write task can be skipped. The mutex is only ever held for a memcmp.
    pub fn persist_if_changed(&self, json: Vec<u8>) -> Option<Vec<u8>> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
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
            if let Err(err) = tokio::fs::write(&path, &json).await {
                // Roll the baseline back so an identical later snapshot
                // retries instead of silently leaving the old file in place.
                // Only when the baseline is still our own bytes: a newer
                // writer may have updated it meanwhile.
                let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
                if last.as_deref() == Some(json.as_slice()) {
                    last.take();
                }
                on_error(&err);
            }
        });
    }
}
