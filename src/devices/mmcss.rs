//! MMCSS（Windows 多媒体类计划程序服务）线程登记：给音频实时回调与
//! 解码泵以优先调度，作为 CPU 竞争场景的应用侧防线。2026-10-04 实测：
//! 后台重构建把解码泵饿穿、250ms 环排空，单会话 696 次 callback
//! underrun（producer stall 计量从未触发 = 纯调度饥饿而非 IO 停摆）——
//! 当时的缓解只有给构建侧降优先级，本模块是对称的应用侧防线。
//!
//! 登记失败静默退回默认调度——与未引入本模块时完全相同，无害；线程
//! 退出时 MMCSS 登记随之失效，无需显式 AvRevert（回调线程随流重建/
//! 销毁，泵线程与进程同寿）。

use std::cell::Cell;

use windows::{
    Win32::System::Threading::{
        AVRT_PRIORITY_HIGH, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
    },
    core::PCWSTR,
};

const PRO_AUDIO: PCWSTR = windows::core::w!("Pro Audio");
const PLAYBACK: PCWSTR = windows::core::w!("Playback");

thread_local! {
    /// 实时回调路径的每线程一次性登记标记：登记后置位，后续调用只剩
    /// 一次 thread_local 读。
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

/// 登记调用线程到 MMCSS 任务类并提到 HIGH 子优先级（文档推荐的大多数
/// 媒体应用档位，CRITICAL 会与内核最关键线程争抢，无必要）。
fn register(task: PCWSTR) -> bool {
    unsafe {
        let Ok(handle) = AvSetMmThreadCharacteristicsW(task, &mut 0) else {
            return false;
        };
        // 子优先级设置失败可忽略：登记本身已获得 MMCSS 调度待遇。
        let _ = AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH);
        true
    }
}

/// 实时音频回调内调用：本线程首次执行时登记一次，后续调用零系统调用。
/// cpal 的回调线程由 cpal 自行创建，无法在创建侧登记，只能在回调首帧
/// 补登记。
pub fn register_audio_thread_once() {
    REGISTERED.with(|registered| {
        if !registered.get() {
            registered.set(register(PRO_AUDIO));
        }
    });
}

/// A/V 生产者线程（解码泵）在线程启动时调用一次。
pub fn register_playback_thread() {
    register(PLAYBACK);
}
