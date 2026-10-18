use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReplayGainMode {
    #[default]
    Off,
    Track,
    Album,
    Auto,
}

/// Auto 模式的提示——决定优先取曲目增益还是专辑增益。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainAutoHint {
    PreferTrack,
    PreferAlbum,
}

// `serde(default)` 覆盖手工编辑的部分对象：没有它，像
// `{"replaygain": {"mode": "track"}}` 这样的 JSON 会让字段反序列化失败，整个
// settings.json 被判为 Corrupt，并在下次保存时抹掉其他所有已存设置。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReplayGainSettings {
    pub mode: ReplayGainMode,
    /// 前置增益，单位 dB，叠加在 RG 增益之上。范围：-6.0 到 +6.0
    pub preamp_db: f64,
    /// 回退前置增益，单位 dB，曲目无 RG 数据时使用。范围：-6.0 到 +6.0
    pub fallback_preamp_db: f64,
}

/// 计算曲目的线性增益倍数。返回要施加到音频采样上的倍数。
///
/// `track_peak`/`album_peak` 是标签里的采样峰值（0.0..1.0+）。当所选增益会把标签
/// 峰值推过满量程时，增益被钳到 `1.0 / peak`（ReplayGain 1.0 式的防削波）；设备侧
/// 软限幅器仍然作为瞬态的安全网。
pub fn calculate_gain(
    settings: &ReplayGainSettings,
    auto_hint: ReplayGainAutoHint,
    track_gain: Option<f64>,
    album_gain: Option<f64>,
    track_peak: Option<f64>,
    album_peak: Option<f64>,
) -> f64 {
    let (selected_gain, selected_peak) = match settings.mode {
        ReplayGainMode::Off => return 1.0,
        ReplayGainMode::Track => (track_gain, track_peak),
        ReplayGainMode::Album => (album_gain.or(track_gain), album_peak.or(track_peak)),
        ReplayGainMode::Auto => match auto_hint {
            ReplayGainAutoHint::PreferTrack => (track_gain, track_peak),
            ReplayGainAutoHint::PreferAlbum => {
                (album_gain.or(track_gain), album_peak.or(track_peak))
            }
        },
    };

    let gain_db = match selected_gain {
        Some(gain) => gain + settings.preamp_db,
        None => settings.fallback_preamp_db,
    };

    // dB 转线性：10^(dB/20)
    let mut gain = 10.0_f64.powf(gain_db / 20.0);

    if let Some(peak) = selected_peak.filter(|peak| *peak > 0.0) {
        gain = gain.min(1.0 / peak);
    }
    gain
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS_OFF: ReplayGainSettings = ReplayGainSettings {
        mode: ReplayGainMode::Off,
        preamp_db: 0.0,
        fallback_preamp_db: 0.0,
    };

    fn track_settings(preamp_db: f64) -> ReplayGainSettings {
        ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db,
            fallback_preamp_db: 0.0,
        }
    }

    #[test]
    fn off_mode_is_unity() {
        assert_eq!(
            calculate_gain(
                &SETTINGS_OFF,
                ReplayGainAutoHint::PreferTrack,
                Some(-3.0),
                None,
                Some(0.5),
                None
            ),
            1.0
        );
    }

    #[test]
    fn gain_applies_preamp() {
        let gain = calculate_gain(
            &track_settings(0.0),
            ReplayGainAutoHint::PreferTrack,
            Some(-6.0),
            None,
            None,
            None,
        );
        assert!((gain - 10.0_f64.powf(-6.0 / 20.0)).abs() < 1e-12);
    }

    #[test]
    fn peak_caps_boost_but_not_attenuation() {
        // 峰值 0.9 的曲目带 -3 dB 标签 + 6 dB 前置 → 净 +3 dB（1.41×），会把 0.9
        // 峰值削顶 → 钳到 1/0.9。
        let capped = calculate_gain(
            &track_settings(6.0),
            ReplayGainAutoHint::PreferTrack,
            Some(-3.0),
            None,
            Some(0.9),
            None,
        );
        assert!((capped - 1.0 / 0.9).abs() < 1e-12);
        // -9 dB 标签 + 3 dB 前置 → 净衰减，不做钳位。
        let attenuated = calculate_gain(
            &track_settings(3.0),
            ReplayGainAutoHint::PreferTrack,
            Some(-9.0),
            None,
            Some(0.9),
            None,
        );
        assert!((attenuated - 10.0_f64.powf(-6.0 / 20.0)).abs() < 1e-12);
    }

    #[test]
    fn missing_or_degenerate_peak_leaves_gain_uncapped() {
        let expected = 10.0_f64.powf(3.0 / 20.0);
        for peak in [None, Some(0.0)] {
            let gain = calculate_gain(
                &track_settings(3.0),
                ReplayGainAutoHint::PreferTrack,
                Some(0.0),
                None,
                peak,
                None,
            );
            assert!((gain - expected).abs() < 1e-12);
        }
    }
}
