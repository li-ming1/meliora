//! MMCSS（Windows 多媒体类计划程序服务）线程登记：给音频实时回调与
//! 解码泵以优先调度，作为 CPU 竞争场景的应用侧防线。2026-10-04 实测：
//! 后台重构建把解码泵饿穿、250ms 环排空，单会话 696 次 callback
//! underrun（producer stall 计量从未触发 = 纯调度饥饿而非 IO 停摆）——
//! 当时的缓解只有给构建侧降优先级，本模块是对称的应用侧防线。
//!
//! 登记失败静默退回默认调度——与未引入本模块时完全相同，无害。
//!
//! 句柄纪律（MSDN 语义，AvSetMmThreadCharacteristicsW /
//! AvRevertMmThreadCharacteristics）：线程终止本身会终结该线程的 MMCSS
//! 调度关联，但登记返回的句柄必须成对释放，否则随线程泄漏（media-prepare
//! 一次性线程、在线填充的逐曲目线程都会持续重建）。除下述 cpal 例外，所有
//! 登记都经 [`MmcssRegistration`] 守卫绑定到线程闭包存活期，Drop 时成对归还：
//! - 解码泵线程（thread.rs）与进程同寿，守卫永不 Drop，无害；
//! - media-prepare 预备线程（audio_engine.rs）一次性，随结果发送退出；
//! - 在线填充线程（prefetch.rs）逐曲目重建，线程退出即归还。
//!
//! 例外（刻意保留现状）：cpal 回调线程经 [`register_audio_thread_once`] 在
//! 回调首帧补登记。它由 cpal 自建、没有退出钩子可用，且回调内 revert 会
//! 撤销正在使用的调度关联；句柄随流重建线性增长的量级有界（一次性小句柄），
//! 接受不归还。

use std::cell::Cell;

use windows::{
    Win32::Foundation::HANDLE,
    Win32::System::Threading::{
        AVRT_PRIORITY_HIGH, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
        AvSetMmThreadPriority,
    },
    core::PCWSTR,
};

const PRO_AUDIO: PCWSTR = windows::core::w!("Pro Audio");
const PLAYBACK: PCWSTR = windows::core::w!("Playback");

thread_local! {
    /// 实时回调路径的每线程尝试标记：首次尝试后置位（无论成败——缓存的
    /// 是『已尝试』而非『已成功』，失败重试只会在每帧回调里重付系统调用），
    /// 后续调用只剩一次 thread_local 读。
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

/// MMCSS 登记句柄守卫：Drop 时对合法句柄调用 `AvRevertMmThreadCharacteristics`
/// 成对释放。调用方必须把它绑定到登记线程的闭包存活期——线程退出即归还，
/// 逐曲目/一次性的线程重建才不会泄漏内核句柄。
pub struct MmcssRegistration {
    handle: HANDLE,
}

impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        // 登记失败时 register 返回非法句柄哨兵，此时无物可释放。
        if !self.handle.is_invalid() {
            // 释放失败无补救路径：登记关联已随线程终止失效。
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(self.handle);
            }
        }
    }
}

/// 登记调用线程到 MMCSS 任务类并提到 HIGH 子优先级（文档推荐的大多数
/// 媒体应用档位，CRITICAL 会与内核最关键线程争抢，无必要）。返回登记句柄；
/// 失败时返回非法句柄（[`MmcssRegistration`] 对它不做系统调用）。
fn register(task: PCWSTR) -> HANDLE {
    unsafe {
        let Ok(handle) = AvSetMmThreadCharacteristicsW(task, &mut 0) else {
            return HANDLE::default();
        };
        // 子优先级设置失败可忽略：登记本身已获得 MMCSS 调度待遇。
        let _ = AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH);
        handle
    }
}

/// 实时音频回调内调用：本线程至多尝试登记一次（无论成败都置位，失败
/// 静默退回默认调度且不重试），后续调用零系统调用。cpal 的回调线程由
/// cpal 自行创建，无法在创建侧登记，只能在回调首帧补登记（句柄不归还的
/// 理由见模块文档）。
pub fn register_audio_thread_once() {
    REGISTERED.with(|registered| {
        if !registered.get() {
            register(PRO_AUDIO);
            registered.set(true);
        }
    });
}

/// A/V 生产者线程（解码泵）在线程启动时调用一次。返回的守卫必须绑定到
/// 线程闭包存活期。
pub fn register_playback_thread() -> MmcssRegistration {
    MmcssRegistration {
        handle: register(PLAYBACK),
    }
}
