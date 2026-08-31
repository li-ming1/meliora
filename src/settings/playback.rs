use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::equalizer::EqualizerSettings;
use super::replaygain::ReplayGainSettings;

fn default_keep_current_on_queue_clear() -> bool {
    true
}

/// KuGou online playback format/bitrate. Higher tiers generally need VIP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OnlineQuality {
    #[default]
    Standard,
    High,
    Lossless,
    HiRes,
}

impl OnlineQuality {
    /// The `quality` value passed to the KuGou playback-URL endpoint.
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

/// NetEase online playback quality `level`. Higher tiers generally need VIP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NeteaseQuality {
    #[default]
    Standard,
    Higher,
    Exhigh,
    Lossless,
    HiRes,
}

impl NeteaseQuality {
    /// The `level` value passed to the NetEase playback-URL endpoint.
    #[cfg(feature = "netease")]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Higher => "higher",
            Self::Exhigh => "exhigh",
            Self::Lossless => "lossless",
            Self::HiRes => "hires",
        }
    }
}

/// User-set playback settings, to be passed to the playback thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackSettings {
    /// Whether or not the playback thread should allow for repeating to be disabled.
    ///
    /// If the option is false (the default), requests to set the RepeatState to NotRepeating will
    /// be processed normally. If the option is set to true, the playback thread will turn all
    /// requests to disable repeating into requests to repeat. In practice, this means that the
    /// repeat mode selection button will cycle from Repeating -> RepeatingOne instead of
    /// NotRepeating -> Repeating -> RepeatingOne.
    ///
    /// Defaults to false.
    #[serde(default)]
    pub always_repeat: bool,

    /// Determines whether or not the playback thread should handle previous track requests by
    /// jumping to the beginning of the track if the current track has been played for more than
    /// 5 seconds.
    ///
    /// If the option is false, requests to go to the previous track always result in the previous
    /// track in the queue being played. If the option is true, requests to go to the previous
    /// track will follow the previously described behavior.
    ///
    /// Currently defaults to false - this may change in the future as this appears to be fairly
    /// controversial (out of everyone I've asked it's been exactly 50/50 whether or not they
    /// prefer this behavior)
    #[serde(default)]
    pub prev_track_jump_first: bool,

    /// Determines whether or not clearing the queue should preserve the currently playing track.
    ///
    /// If the option is false, clearing the queue removes all tracks and stops playback. If the
    /// option is true, the currently playing track will be preserved in the queue when clearing.
    ///
    /// Defaults to true.
    #[serde(default = "default_keep_current_on_queue_clear")]
    pub keep_current_on_queue_clear: bool,

    /// ReplayGain settings.
    #[serde(default)]
    pub replaygain: ReplayGainSettings,

    /// Parametric equalizer settings.
    #[serde(default)]
    pub equalizer: EqualizerSettings,

    /// Whether to prevent the system screensaver and sleep while playing.
    #[serde(default)]
    pub prevent_idle: bool,

    /// When enabled, removes the current track from the queue when it finishes playing.
    ///
    /// Defaults to false.
    #[serde(default)]
    pub consume: bool,

    /// KuGou online playback quality. Defaults to standard 128 kbps.
    #[serde(default)]
    pub online_quality: OnlineQuality,

    /// NetEase online playback quality. Defaults to standard 128 kbps.
    #[serde(default)]
    pub netease_quality: NeteaseQuality,

    /// Directory online (KuGou/NetEase) downloads are written to. `None` uses
    /// the platform's default Downloads folder.
    #[serde(default)]
    pub download_dir: Option<String>,
}

impl PlaybackSettings {
    /// The configured download directory, or the platform default when unset.
    #[cfg_attr(not(feature = "online_sources"), allow(dead_code))]
    pub fn effective_download_dir(&self) -> PathBuf {
        self.download_dir
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_download_dir)
    }
}

/// The platform's default downloads folder (`~/Downloads` on all desktop OSes).
#[cfg_attr(not(feature = "online_sources"), allow(dead_code))]
pub fn default_download_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Downloads")
}

#[allow(clippy::derivable_impls)]
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
            netease_quality: NeteaseQuality::default(),
            download_dir: None,
        }
    }
}
