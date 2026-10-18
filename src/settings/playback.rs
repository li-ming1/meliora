use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::equalizer::EqualizerSettings;
use super::replaygain::ReplayGainSettings;

fn default_keep_current_on_queue_clear() -> bool {
    true
}

/// 酷狗在线播放的格式/码率。更高的档位一般需要 VIP。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OnlineQuality {
    #[default]
    Standard,
    High,
    Lossless,
    HiRes,
}

impl OnlineQuality {
    /// 传给酷狗播放地址接口的 `quality` 值。
    #[cfg(feature = "kugou")]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "128",
            Self::High => "320",
            Self::Lossless => "flac",
            Self::HiRes => "high",
        }
    }
}

/// 用户设置的播放选项，会传给播放线程。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackSettings {
    /// 播放线程是否应允许"关闭循环"。
    ///
    /// 如果该选项为 false（默认），把 RepeatState 设为 NotRepeating 的请求会正常
    /// 处理。如果设为 true，播放线程会把所有"关闭循环"的请求变成"开启循环"。
    /// 实际效果是循环模式选择按钮从 Repeating -> RepeatingOne 之间循环，而不是
    /// NotRepeating -> Repeating -> RepeatingOne。
    ///
    /// 默认为 false。
    #[serde(default)]
    pub always_repeat: bool,

    /// 决定播放线程处理"上一曲"请求时，是否在当前曲目已播放超过 5 秒后改为跳到
    /// 本曲开头。
    ///
    /// 如果为 false，"上一曲"请求总是播放队列里的上一首。如果为 true，"上一曲"
    /// 请求按上面描述的行为处理。
    ///
    /// 目前默认 false——未来可能会变，因为这个行为看来颇有争议（问到我的人里正好
    /// 一半一半，各半是否喜欢这个行为）
    #[serde(default)]
    pub prev_track_jump_first: bool,

    /// 决定清空队列时是否保留正在播放的曲目。
    ///
    /// 如果为 false，清空队列会移除全部曲目并停止播放。如果为 true，清空时队列里
    /// 会保留当前播放的曲目。
    ///
    /// 默认为 true。
    #[serde(default = "default_keep_current_on_queue_clear")]
    pub keep_current_on_queue_clear: bool,

    /// ReplayGain 设置。
    #[serde(default)]
    pub replaygain: ReplayGainSettings,

    /// 参数化均衡器设置。
    #[serde(default)]
    pub equalizer: EqualizerSettings,

    /// 播放时是否阻止系统屏保与睡眠。
    #[serde(default)]
    pub prevent_idle: bool,

    /// 启用后，当前曲目播放完毕即从队列移除。
    ///
    /// 默认为 false。
    #[serde(default)]
    pub consume: bool,

    /// 酷狗在线播放音质。默认标准 128 kbps。
    #[serde(default)]
    pub online_quality: OnlineQuality,

    /// 在线（酷狗）下载写入的目录。`None` 表示用平台的默认 Downloads 文件夹。
    #[serde(default)]
    pub download_dir: Option<String>,
}

impl PlaybackSettings {
    /// 已配置的下载目录；未设置时用平台默认目录。
    #[cfg_attr(not(feature = "online_sources"), allow(dead_code))]
    pub fn effective_download_dir(&self) -> PathBuf {
        self.download_dir
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_download_dir)
    }
}

/// 平台的默认 Downloads 文件夹（所有桌面系统都是 `~/Downloads`）。
#[cfg_attr(not(feature = "online_sources"), allow(dead_code))]
pub fn default_download_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Downloads")
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            always_repeat: false,
            prev_track_jump_first: false,
            keep_current_on_queue_clear: true,
            replaygain: ReplayGainSettings::default(),
            equalizer: EqualizerSettings::default(),
            prevent_idle: false,
            consume: false,
            online_quality: OnlineQuality::default(),
            download_dir: None,
        }
    }
}
