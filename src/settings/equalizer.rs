use serde::{Deserialize, Deserializer, Serialize};

use crate::playback::dsp::equalizer::{
    MAX_FREQUENCY, MAX_GAIN_DB, MAX_Q, MIN_FREQUENCY, MIN_Q, clamp_finite,
};

/// 均衡器频段的最大数量。
pub const MAX_EQ_BANDS: usize = 16;

/// 单个均衡器频段的滤波器类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EqBandKind {
    /// Peaking 滤波器，唯一使用 `gain_db` 的类型。
    #[default]
    Bell,
    LowPass,
    HighPass,
    BandPass,
    Notch,
}

impl EqBandKind {
    /// 该类型的频段是否会施加它的 `gain_db`。
    pub fn has_gain(self) -> bool {
        matches!(self, Self::Bell)
    }

    /// 该类型频段的初始 Q；通带用 Butterworth。
    pub fn default_q(self) -> f64 {
        match self {
            Self::Bell => 1.0,
            Self::LowPass | Self::HighPass => std::f64::consts::FRAC_1_SQRT_2,
            Self::BandPass | Self::Notch => 4.0,
        }
    }
}

/// 单个参数化均衡器频段。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBandSettings {
    pub kind: EqBandKind,
    /// 中心/转折频率，单位 Hz。
    pub frequency: f64,
    /// 增益，单位 dB。只有 `Bell` 用到，但无论如何都序列化，好让切换类型时能往返。
    pub gain_db: f64,
    pub q: f64,
    pub enabled: bool,
}

impl Default for EqBandSettings {
    fn default() -> Self {
        Self {
            kind: EqBandKind::Bell,
            frequency: 1_000.0,
            gain_db: 0.0,
            q: 1.0,
            enabled: true,
        }
    }
}

/// 参数化均衡器设置。既是给 DSP 的线格式，也是 UI 的工作态。
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct EqualizerSettings {
    /// 全局旁路。
    pub enabled: bool,
    /// 按曲线的峰值提升量对输出做衰减，让频段增益无法造成削波。
    pub volume_compensation: bool,
    pub bands: Vec<EqBandSettings>,
}

impl EqualizerSettings {
    pub fn sanitize(&mut self) {
        for band in &mut self.bands {
            band.frequency =
                clamp_finite(band.frequency, MIN_FREQUENCY, MAX_FREQUENCY, MIN_FREQUENCY);
            band.gain_db = clamp_finite(band.gain_db, -MAX_GAIN_DB, MAX_GAIN_DB, 0.0);
            band.q = clamp_finite(band.q, MIN_Q, MAX_Q, 1.0);
        }
    }
}

impl<'de> Deserialize<'de> for EqualizerSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Raw {
            enabled: bool,
            volume_compensation: bool,
            bands: Vec<EqBandSettings>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut settings = Self {
            enabled: raw.enabled,
            volume_compensation: raw.volume_compensation,
            bands: raw.bands,
        };
        // 手工编辑过的文件可能带着越界值
        settings.sanitize();
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_into_dsp_ranges() {
        let mut config = EqualizerSettings {
            enabled: true,
            bands: vec![EqBandSettings {
                frequency: -5.0,
                gain_db: 90.0,
                q: -1.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        config.sanitize();
        let band = &config.bands[0];
        assert_eq!(band.frequency, MIN_FREQUENCY);
        assert_eq!(band.gain_db, MAX_GAIN_DB);
        assert_eq!(band.q, MIN_Q);
    }

    #[test]
    fn sanitize_repairs_non_finite_values() {
        let mut config = EqualizerSettings {
            enabled: true,
            bands: vec![EqBandSettings {
                frequency: f64::NAN,
                gain_db: f64::INFINITY,
                q: f64::NEG_INFINITY,
                ..Default::default()
            }],
            ..Default::default()
        };
        config.sanitize();
        let band = &config.bands[0];
        assert_eq!(band.frequency, MIN_FREQUENCY);
        assert_eq!(band.gain_db, 0.0);
        assert_eq!(band.q, 1.0);
    }

    #[test]
    fn deserialize_sanitizes_bands() {
        let config: EqualizerSettings =
            serde_json::from_str(r#"{"enabled":true,"bands":[{"frequency":-5.0,"q":-1.0}]}"#)
                .unwrap();
        let band = &config.bands[0];
        assert_eq!(band.frequency, MIN_FREQUENCY);
        assert_eq!(band.q, MIN_Q);
    }

    #[test]
    fn deserialize_defaults_missing_fields() {
        let config: EqualizerSettings = serde_json::from_str("{}").unwrap();
        assert!(!config.enabled);
        assert!(!config.volume_compensation);
        assert!(config.bands.is_empty());
    }

    #[test]
    fn volume_compensation_round_trips() {
        let config = EqualizerSettings {
            volume_compensation: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: EqualizerSettings = serde_json::from_str(&json).unwrap();
        assert!(parsed.volume_compensation);
    }
}
