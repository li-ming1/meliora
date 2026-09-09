use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReplayGainMode {
    #[default]
    Off,
    Track,
    Album,
    Auto,
}

/// Hint for Auto mode - determines whether track or album gain is preferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainAutoHint {
    PreferTrack,
    PreferAlbum,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct ReplayGainSettings {
    pub mode: ReplayGainMode,
    /// Pre-amp in dB, applied on top of RG gain. Range: -6.0 to +6.0
    pub preamp_db: f64,
    /// Fallback pre-amp in dB, applied when track has no RG data. Range: -6.0 to +6.0
    pub fallback_preamp_db: f64,
}

/// Calculate the linear gain multiplier for a track. Returns the multiplier to apply to audio samples.
///
/// `track_peak`/`album_peak` are the tagged sample peaks (0.0..1.0+). When the
/// selected gain would push the tagged peak past full scale, the gain is
/// capped at `1.0 / peak` (ReplayGain 1.0-style clipping prevention); the
/// device-side soft limiter remains as the transient safety net.
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
            ReplayGainAutoHint::PreferAlbum => (
                album_gain.or(track_gain),
                album_peak.or(track_peak),
            ),
        },
    };

    let gain_db = match selected_gain {
        Some(gain) => gain + settings.preamp_db,
        None => settings.fallback_preamp_db,
    };

    // Convert dB to linear: 10^(dB/20)
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
        assert_eq!(calculate_gain(&SETTINGS_OFF, ReplayGainAutoHint::PreferTrack, Some(-3.0), None, Some(0.5), None), 1.0);
    }

    #[test]
    fn gain_applies_preamp() {
        let gain = calculate_gain(&track_settings(0.0), ReplayGainAutoHint::PreferTrack, Some(-6.0), None, None, None);
        assert!((gain - 10.0_f64.powf(-6.0 / 20.0)).abs() < 1e-12);
    }

    #[test]
    fn peak_caps_boost_but_not_attenuation() {
        // -3 dB tag on a 0.9-peak track + 6 dB pre-amp → +3 dB net (1.41×),
        // which would clip the 0.9 peak → capped at 1/0.9.
        let capped = calculate_gain(&track_settings(6.0), ReplayGainAutoHint::PreferTrack, Some(-3.0), None, Some(0.9), None);
        assert!((capped - 1.0 / 0.9).abs() < 1e-12);
        // -9 dB tag + 3 dB pre-amp → net attenuation, untouched.
        let attenuated = calculate_gain(&track_settings(3.0), ReplayGainAutoHint::PreferTrack, Some(-9.0), None, Some(0.9), None);
        assert!((attenuated - 10.0_f64.powf(-6.0 / 20.0)).abs() < 1e-12);
    }

    #[test]
    fn missing_or_degenerate_peak_leaves_gain_uncapped() {
        let expected = 10.0_f64.powf(3.0 / 20.0);
        for peak in [None, Some(0.0)] {
            let gain = calculate_gain(&track_settings(3.0), ReplayGainAutoHint::PreferTrack, Some(0.0), None, peak, None);
            assert!((gain - expected).abs() < 1e-12);
        }
    }
}
