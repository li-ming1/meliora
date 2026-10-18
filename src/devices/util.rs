use std::sync::atomic::AtomicU64;

use super::resample::{SampleFrom, SampleInto};

pub trait Scale: Sized {
    fn scale(self, factor: f64) -> Self;
}

impl<T> Scale for T
where
    T: SampleInto<f64> + SampleFrom<f64> + Copy,
{
    fn scale(self, factor: f64) -> T {
        // 超过 1.0 或低于 -1.0 的值都会被钳住，因为它们越界
        let scaled = (self.sample_into() * factor).clamp(-1.0, 1.0);
        T::sample_from(scaled)
    }
}

/// 软限幅器开始介入的输入电平（约 -0.45 dBFS）。
/// 低于该点时函数是逐位精确的直通，因此普通素材原样不动，只有被增益抬起来的
/// 峰值才被曲线压向满幅。
const LIMITER_KNEE: f64 = 0.95;

/// 平滑饱和到 ±1.0：拐点以下透明，拐点处 C1 连续，拐点以上单调且渐近于 ±1.0。
/// 量化前最后的满精度一级——取代硬钳位，让被抬升的峰值走曲线而不是碎成奇次谐波。
#[inline]
pub fn soft_limit(x: f64) -> f64 {
    let magnitude = x.abs();
    if magnitude <= LIMITER_KNEE {
        return x;
    }
    let knee_span = 1.0 - LIMITER_KNEE;
    let limited = LIMITER_KNEE + knee_span * ((magnitude - LIMITER_KNEE) / knee_span).tanh();
    x.signum() * limited
}

/// 整数目标的量化步长（以满幅的比例表示），只列出浅到能听出抖动的位深；
/// float 与 ≥24-bit 目标返回 `None`——那里量化本身听不见，加抖动只会添噪声。
pub trait DitherLsb: Sized {
    const DITHER_LSB: Option<f64>;
}

macro_rules! impl_dither_shallow {
    ($($t:ty => $lsb:expr),* $(,)?) => {
        $(impl DitherLsb for $t {
            const DITHER_LSB: Option<f64> = Some($lsb);
        })*
    };
}

macro_rules! impl_dither_none {
    ($($t:ty),* $(,)?) => {
        $(impl DitherLsb for $t {
            const DITHER_LSB: Option<f64> = None;
        })*
    };
}

impl_dither_shallow!(i8 => 1.0 / 128.0, i16 => 1.0 / 32_768.0, u8 => 1.0 / 128.0, u16 => 1.0 / 32_768.0);
impl_dither_none!(i32, u32, f32, f64);

/// 下一个 TPDF 抖动偏移，以目标 LSB 为单位：两个均匀分布之和减 1.0，得到
/// [-1, 1) 上的三角密度。抽样自每设备一份的 xorshift 状态；交织消费顺带把
/// 各声道去相关。
#[inline]
pub fn tpdf(state: &mut u64) -> f64 {
    // xorshift64*：每个样本一次乘异或步进
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    let bits = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
    let a = (bits >> 40) as f64 / 16_777_216.0; // 高 24 位
    let b = ((bits >> 16) & 0xFF_FFFF) as f64 / 16_777_216.0;
    a + b - 1.0
}

pub struct AtomicF64 {
    inner: AtomicU64,
}

impl AtomicF64 {
    pub fn new(value: f64) -> Self {
        let as_u64 = value.to_bits();
        Self {
            inner: AtomicU64::new(as_u64),
        }
    }

    pub fn store(&self, value: f64, ordering: std::sync::atomic::Ordering) {
        let as_u64 = value.to_bits();
        self.inner.store(as_u64, ordering)
    }

    pub fn load(&self, ordering: std::sync::atomic::Ordering) -> f64 {
        let as_u64 = self.inner.load(ordering);
        f64::from_bits(as_u64)
    }
}

pub const GAIN_RAMP_MS: f64 = 15.0;

/// 线性增益斜坡。
///
/// 每个回调调用一次 `apply` 即可平滑过渡增益。当前增益按固定斜率向 `target`
/// 逼近，斜率由采样率导出，因此一次满量程斜坡（0.0 到 1.0）无论采样率多少都耗时
/// [`GAIN_RAMP_MS`]。
pub struct GainRamp {
    current: f64,
    step: f64,
    frame_pos: usize,
}

impl GainRamp {
    pub fn new(sample_rate_hz: u32) -> Self {
        let ramp_frames = (sample_rate_hz as f64 * GAIN_RAMP_MS / 1000.0).max(1.0);
        Self {
            current: 0.0,
            step: 1.0 / ramp_frames,
            frame_pos: 0,
        }
    }

    fn advance_toward_target(&mut self, target: f64) {
        if self.current < target {
            self.current = (self.current + self.step).min(target);
        } else {
            self.current = (self.current - self.step).max(target);
        }
    }

    fn advance_frame_pos(&mut self, samples: usize, channels: usize) {
        self.frame_pos = (self.frame_pos + samples) % channels;
    }

    /// 将斜坡原地作用于交织样本缓冲区。
    ///
    /// `data` 是交织排列 `[ch0_f0, ch1_f0, ..., ch0_f1, ch1_f1, ...]`
    /// `target` 是要逼近的目标增益
    ///
    /// - 单位增益稳态（current == target == 1.0）：样本直接透传
    /// - 非单位增益稳态：对所有样本施加同一个平坦增益
    /// - 斜坡过程中：增益按帧步进，并施加到该帧内的每个声道
    pub fn apply<T: Scale + Copy>(&mut self, data: &mut [T], channels: usize, target: f64) {
        if channels == 0 || data.is_empty() {
            return;
        }

        // 只认精确的 1.0：低于单位增益必须真的缩放，而斜坡正好落在 target 上，
        // 所以稳态单位增益总会走这条路径
        if self.current == 1.0 && target == 1.0 {
            self.advance_frame_pos(data.len(), channels);
            return;
        }

        if (self.current - target).abs() < f64::EPSILON {
            let gain = self.current;
            for sample in data.iter_mut() {
                *sample = sample.scale(gain);
            }
            self.advance_frame_pos(data.len(), channels);
            return;
        }

        for sample in data.iter_mut() {
            if self.frame_pos == 0 {
                self.advance_toward_target(target);
            }

            *sample = sample.scale(self.current);
            self.frame_pos = (self.frame_pos + 1) % channels;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GainRamp, LIMITER_KNEE, soft_limit, tpdf};

    fn assert_approx_eq(lhs: f32, rhs: f32) {
        assert!((lhs - rhs).abs() < 1e-6, "left={lhs}, right={rhs}");
    }

    #[test]
    fn gain_ramp_keeps_partial_frames_consistent_across_calls() {
        let mut ramp = GainRamp::new(1000);

        let mut first = [1.0_f32, 1.0, 1.0];
        ramp.apply(&mut first, 2, 1.0);

        assert_approx_eq(first[0], 1.0 / 15.0);
        assert_approx_eq(first[1], 1.0 / 15.0);
        assert_approx_eq(first[2], 2.0 / 15.0);

        let mut second = [1.0_f32, 1.0];
        ramp.apply(&mut second, 2, 1.0);

        assert_approx_eq(second[0], 2.0 / 15.0);
        assert_approx_eq(second[1], 3.0 / 15.0);
    }

    #[test]
    fn unity_fast_path_preserves_frame_alignment() {
        let mut ramp = GainRamp::new(1000);
        ramp.current = 1.0;
        ramp.frame_pos = 1;

        let mut steady = [1.0_f32];
        ramp.apply(&mut steady, 2, 1.0);

        assert_approx_eq(steady[0], 1.0);
        assert_eq!(ramp.frame_pos, 0);

        let mut faded = [1.0_f32];
        ramp.apply(&mut faded, 2, 0.0);

        assert_approx_eq(faded[0], 14.0 / 15.0);
        assert_eq!(ramp.frame_pos, 1);
    }

    #[test]
    fn soft_limit_is_transparent_below_the_knee() {
        for x in [0.0, 0.5, LIMITER_KNEE, -LIMITER_KNEE, -0.25] {
            assert_eq!(soft_limit(x), x, "must be bit-exact at {x}");
        }
    }

    #[test]
    fn soft_limit_curves_into_full_scale_without_clipping() {
        let mut previous = LIMITER_KNEE;
        // 单调、封顶 1.0，且在拐点连续
        for step in [1.0, 1.05, 1.2, 2.0, 8.0] {
            let x = LIMITER_KNEE + step * 0.1;
            let limited = soft_limit(x);
            assert!(limited > previous && limited < 1.0, "at {x} -> {limited}");
            previous = limited;
        }
        assert_approx_eq(soft_limit(LIMITER_KNEE + 1e-9) as f32, LIMITER_KNEE as f32);
        // 对称性
        assert_eq!(soft_limit(1.3), -soft_limit(-1.3));
    }

    #[test]
    fn tpdf_stays_within_one_lsb_and_averages_to_zero() {
        let mut state = 0x9E37_79B9_7F4A_7C15;
        let (mut sum, mut min, mut max) = (0.0f64, f64::INFINITY, f64::NEG_INFINITY);
        for _ in 0..100_000 {
            let d = tpdf(&mut state);
            assert!((-1.0..1.0).contains(&d), "offset out of range: {d}");
            sum += d;
            min = min.min(d);
            max = max.max(d);
        }
        assert!(min < -0.9, "should reach both tails, min={min}");
        assert!(max > 0.9, "should reach both tails, max={max}");
        assert!((sum / 100_000.0).abs() < 0.01, "mean must be ~0");
    }
}
