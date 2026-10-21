pub mod channels;
pub mod cpal;
pub mod dummy;
pub mod errors;
pub mod format;
pub mod mix;
#[cfg(windows)]
pub mod mmcss;
/// 非 Windows 平台的 MMCSS 桩：调用点无需平台门控。音频线程优先级是
/// Windows 调度器特有的机制，其它平台保持默认调度。
#[cfg(not(windows))]
pub mod mmcss {
    pub fn register_audio_thread_once() {}

    pub fn register_playback_thread() {}
}
pub mod resample;
pub mod traits;
pub mod util;
