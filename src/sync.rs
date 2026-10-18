//! 锁的毒化恢复策略集中在这里。
//!
//! 本应用遇到 poison 时**不 panic**：走到 `into_inner` 意味着持锁线程已经死
//! 在半路，而一个早已消失的线程不该把整个播放器带走。数据是否仍然可信由各
//! 调用点自己的守卫（generation / epoch / 不可变快照）判定，而不是由锁决定。
//!
//! 三种守卫各一个方法名，读起来就是意图本身，不必在每个调用点重读一遍
//! `unwrap_or_else(|poisoned| poisoned.into_inner())`。方法体就是原来那句
//! 展开写法，单态化后由 LTO 内联回调用点（形态与改写前一致）。

use std::sync::{
    Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult,
};
use std::time::Duration;

pub(crate) trait RecoveredMutex<T> {
    fn lock_recovered(&self) -> MutexGuard<'_, T>;
}

impl<T> RecoveredMutex<T> for Mutex<T> {
    fn lock_recovered(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(crate) trait RecoveredRwLock<T> {
    fn read_recovered(&self) -> RwLockReadGuard<'_, T>;
    fn write_recovered(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RecoveredRwLock<T> for RwLock<T> {
    fn read_recovered(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_recovered(&self) -> RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `Condvar` 的等待同样返回 `LockResult`：被唤醒时锁可能已经因持锁线程
/// panic 而毒化，等待方拿回守卫继续判断状态即可（谓词都在自己手里）。
pub(crate) trait RecoveredCondvar {
    fn wait_recovered<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T>;
    fn wait_timeout_recovered<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult);
}

impl RecoveredCondvar for Condvar {
    fn wait_recovered<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait_timeout_recovered<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        self.wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
