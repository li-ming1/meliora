use std::collections::VecDeque;

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use intx::{I24, U24};
use rubato::{Fft, FixedSync, Resampler as RubatoResampler};
use tracing::{error, info};

use crate::media::pipeline::{ChannelConsumers, DEFAULT_BUFFER_FRAMES};

/// 对称缩放下 24-bit 样本的一个 LSB 步长：2^23。
const I24_SCALE: f64 = 8_388_608.0;

pub trait SampleInto<T> {
    fn sample_into(self) -> T;
}

impl SampleInto<f64> for U24 {
    fn sample_into(self) -> f64 {
        f64::from(u32::from(self) as i32 - 0x80_0000) / I24_SCALE
    }
}

impl SampleInto<f64> for I24 {
    fn sample_into(self) -> f64 {
        f64::from(i32::from(self)) / I24_SCALE
    }
}

impl SampleInto<f64> for f32 {
    fn sample_into(self) -> f64 {
        self as f64
    }
}

impl SampleInto<f64> for f64 {
    fn sample_into(self) -> f64 {
        self
    }
}

/// 把缩放后的 `i32` 转成 [`I24`]：样本落在 24-bit 范围外时饱和取值而非 panic
/// （例如增益 > 1.0 或样本间过冲）。
#[inline]
pub(crate) fn i24_saturating(scaled: i32) -> I24 {
    let clamped = scaled.clamp(i32::from(I24::MIN), i32::from(I24::MAX));
    I24::try_from(clamped).unwrap_or(I24::MAX)
}

/// 把缩放后的 `u32` 转成 [`U24`]：饱和收进 24-bit 范围。
#[inline]
pub(crate) fn u24_saturating(scaled: u32) -> U24 {
    U24::try_from(scaled.min(u32::from(U24::MAX))).unwrap_or(U24::MAX)
}

/// 从另一种样本类型转换而来的 trait。
pub trait SampleFrom<T> {
    fn sample_from(value: T) -> Self;
}

impl SampleFrom<f64> for U24 {
    fn sample_from(value: f64) -> Self {
        // 先在带符号域钳位，再重新居中到 [0, 2^24)
        let signed = i32::from(I24::sample_from(value));
        u24_saturating((signed + 0x80_0000) as u32)
    }
}

impl SampleFrom<f64> for I24 {
    fn sample_from(value: f64) -> Self {
        // `as` 转换会在 i32 边界饱和，并把 NaN 映射为 0
        i24_saturating((value * I24_SCALE).round() as i32)
    }
}

impl SampleFrom<f64> for f32 {
    fn sample_from(value: f64) -> Self {
        value as f32
    }
}

impl SampleFrom<f64> for f64 {
    fn sample_from(value: f64) -> Self {
        value
    }
}

// 带符号整数按 2^(N-1) 对称缩放：双向都精确（2 的幂做乘除），数字静音落在 0.0，
// 且 i::MIN 正好映射到 -1.0。float -> int 取最近舍入；`as` 转换对越界值饱和、
// 把 NaN 映射为 0，因此转换是全域的。
macro_rules! impl_signed_sample_f64 {
    ($t:ty, $scale:expr) => {
        impl SampleInto<f64> for $t {
            fn sample_into(self) -> f64 {
                f64::from(self) / $scale
            }
        }

        impl SampleFrom<f64> for $t {
            fn sample_from(value: f64) -> $t {
                (value * $scale).round() as $t
            }
        }
    };
}

impl_signed_sample_f64!(i8, 128.0);
impl_signed_sample_f64!(i16, 32_768.0);
impl_signed_sample_f64!(i32, 2_147_483_648.0);

// 无符号整数在整数域里重新居中（翻转符号位，精确且零成本），并复用带符号的缩放，
// 因此中位点（数字静音）恰好映射到 0.0。
macro_rules! impl_unsigned_sample_f64 {
    ($t:ty, $signed:ty) => {
        impl SampleInto<f64> for $t {
            fn sample_into(self) -> f64 {
                ((self ^ (<$signed>::MIN as $t)) as $signed).sample_into()
            }
        }

        impl SampleFrom<f64> for $t {
            fn sample_from(value: f64) -> $t {
                (<$signed>::sample_from(value) as $t) ^ (<$signed>::MIN as $t)
            }
        }
    };
}

impl_unsigned_sample_f64!(u8, i8);
impl_unsigned_sample_f64!(u16, i16);
impl_unsigned_sample_f64!(u32, i32);

pub struct Resampler {
    resampler: Fft<f64>,
    duration: u64,
    input_buffer: Vec<VecDeque<f64>>,
    temp_input: Vec<Vec<f64>>,
    temp_output: Vec<Vec<f64>>,
    channels: usize,
    source_rate: u32,
    target_rate: u32,
    /// 自上次 reset 以来喂入重采样器的总帧数。
    frames_in: u64,
    /// 自上次 reset 以来重采样器吐出的总帧数。
    frames_out: u64,
    /// [`Self::flush_into`] 排空尾部后置位；重采样器必须先 reset 才能继续处理。
    flushed: bool,
}

impl Resampler {
    /// `Err` 携带可展示的 rubato 失败信息（畸形文件带来的坏速率比/时长）；
    /// 调用方将其视为致命的解码步骤。
    pub fn new(
        orig_rate: u32,
        target_rate: u32,
        duration: u64,
        channels: u16,
    ) -> Result<Self, String> {
        // 引擎无条件构造 Resampler，速率相同时自行走直通，所以只在真的需要转换时
        // 打日志。
        if orig_rate != target_rate {
            info!(
                "Resampling required, resampling from {:?} to {:?} (duration {:?})",
                orig_rate, target_rate, duration
            );
        }

        let resampler = Fft::<f64>::new(
            orig_rate as usize,
            target_rate as usize,
            duration as usize,
            channels as usize,
            FixedSync::Input,
        )
        .map_err(|e| e.to_string())?;

        let channels_usize = channels as usize;
        let output_frames_max = resampler.output_frames_max();

        Ok(Resampler {
            resampler,
            duration,
            // 容量按一个周期能喂进来的最大读取量再加一块未排空的分片来定，
            // 因此稳态处理过程中永不扩容
            input_buffer: (0..channels_usize)
                .map(|_| VecDeque::with_capacity(DEFAULT_BUFFER_FRAMES + duration as usize))
                .collect(),
            temp_input: (0..channels_usize)
                .map(|_| Vec::with_capacity(duration as usize))
                .collect(),
            temp_output: (0..channels_usize)
                .map(|_| vec![0.0; output_frames_max])
                .collect(),
            channels: channels_usize,
            source_rate: orig_rate,
            target_rate,
            frames_in: 0,
            frames_out: 0,
            flushed: false,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 重采样器的延迟，以输出帧计。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn output_delay(&self) -> usize {
        self.resampler.output_delay()
    }

    /// 单次 process 调用最多能吐出的帧数。
    pub fn output_frames_max(&self) -> usize {
        self.resampler.output_frames_max()
    }

    pub fn needs_resampling(&self) -> bool {
        self.source_rate != self.target_rate
    }

    /// 该重采样器能否不重建就服务这些参数的流。
    ///
    /// `duration`（解码器的分块大小）刻意不参与比较：即使速率/声道完全相同，
    /// 它也因文件而异，一旦纳入就会导致几乎每次切歌都整体重建
    /// （FFT 计划 + 声道缓冲区）。分块大小不同是无害的：`process_into`
    /// 始终按本重采样器构建时的分块大小喂料，调用方的交接缓冲区也是动态定尺。
    pub fn matches_params(&self, source_rate: u32, target_rate: u32, channels: usize) -> bool {
        self.source_rate == source_rate
            && self.target_rate == target_rate
            && self.channels == channels
    }

    fn input_available(&self) -> usize {
        self.input_buffer.iter().map(|b| b.len()).min().unwrap_or(0)
    }

    pub fn reset(&mut self) {
        for buf in &mut self.input_buffer {
            buf.clear();
        }
        for buf in &mut self.temp_input {
            buf.clear();
        }
        for buf in &mut self.temp_output {
            buf.fill(0.0);
        }
        self.resampler.reset();
        self.frames_in = 0;
        self.frames_out = 0;
        self.flushed = false;
    }

    pub fn process_into(
        &mut self,
        input: &mut ChannelConsumers,
        output: &mut [Vec<f64>],
        max_input_samples: usize,
    ) -> usize {
        if !self.needs_resampling() {
            return Self::passthrough_direct(input, output, max_input_samples);
        }

        let read = Self::read_into_buffers(input, &mut self.input_buffer, max_input_samples);
        self.frames_in += read as u64;

        if read == 0 {
            return 0;
        }

        let duration = self.duration as usize;
        if self.input_available() < duration {
            return 0; // 输入还不够一个分块
        }

        let mut total_output = 0;
        while self.input_available() >= duration {
            let Some(frames_written) = self.pump_resampler(duration, None) else {
                error!("resampler error while processing; chunk truncated");
                break;
            };
            self.append_frames(output, frames_written);
            total_output += frames_written;
        }

        self.frames_out += total_output as u64;
        total_output
    }

    /// 把重采样器输入缓冲区里的内容冲入 `output`。仅在重采样器即将被丢弃时使用
    /// （无间隙播放过程中不要用），因为它可能往输出里掺进一些静音。
    pub fn flush_into(&mut self, output: &mut [Vec<f64>]) -> usize {
        if self.flushed || !self.needs_resampling() || self.frames_in == 0 {
            return 0;
        }
        self.flushed = true;

        let ratio = f64::from(self.target_rate) / f64::from(self.source_rate);
        let expected_total =
            (self.frames_in as f64 * ratio).ceil() as u64 + self.resampler.output_delay() as u64;

        let mut written = 0;
        // 第一次调用带上缓冲区里那截不完整分片，之后的调用灌零
        let mut partial = self.input_available();

        // 每次 pump 正常吐出约 duration*ratio 帧，所以这么多轮必然够；
        // 只有停止产帧的重采样器才会超出该轮数
        let per_cycle = (self.duration as f64 * ratio).max(1.0) as u64;
        let max_cycles = expected_total.div_ceil(per_cycle) + 8;
        let mut cycles = 0;

        while self.frames_out < expected_total {
            cycles += 1;
            if cycles > max_cycles {
                error!("resampler stopped producing frames while flushing; tail truncated");
                break;
            }

            let indexing = rubato::Indexing {
                input_offset: 0,
                output_offset: 0,
                active_channels_mask: None,
                partial_len: Some(partial),
            };
            let Some(frames_written) = self.pump_resampler(partial, Some(&indexing)) else {
                error!("resampler error while flushing; tail truncated");
                break;
            };

            // 截掉最后一块，让 flush 恰好在输入结束处收尾，
            // 而不是多追加一段静音
            let keep = frames_written.min((expected_total - self.frames_out) as usize);
            self.append_frames(output, keep);
            self.frames_out += keep as u64;
            written += keep;
            partial = 0;
        }

        written
    }

    /// 从输入缓冲区最多取 `max_frames` 帧搬进 `temp_input`，对其跑一遍重采样，
    /// 返回产出的帧数。`indexing` 携带 flush 时的不完整分片长度；稳态处理传
    /// `None`。重采样器出错时返回 `None`——日志与截断策略由调用方决定。
    fn pump_resampler(
        &mut self,
        max_frames: usize,
        indexing: Option<&rubato::Indexing>,
    ) -> Option<usize> {
        for ch in 0..self.channels {
            let drain_count = max_frames.min(self.input_buffer[ch].len());
            let temp = &mut self.temp_input[ch];
            temp.clear();
            // VecDeque 的物理存储是环形的：`as_slices` 给出按逻辑顺序的首尾
            // 两段，用两次 `extend_from_slice` 做 bulk 拷贝，取代逐元素
            // `extend(drain(..))`——44.1k→48k 的重采样每次播放都走这里，
            // 每分片每声道上千次逐元素 push。随后的 `drain(..n)` 对 f64
            // 无 Drop 负担，只是头指针前移。
            let (front, back) = self.input_buffer[ch].as_slices();
            let front_take = drain_count.min(front.len());
            temp.extend_from_slice(&front[..front_take]);
            if drain_count > front_take {
                temp.extend_from_slice(&back[..drain_count - front_take]);
            }
            self.input_buffer[ch].drain(..drain_count);
        }

        let input_frames = self.temp_input.first().map(|v| v.len()).unwrap_or(0);
        // 与下面的 process_into_buffer 同一套失败策略：adapter 畸形（声道数不符、
        // 平面长短不一）时跳过这一遍，而不是在音频路径上 panic。等长且声道齐全的
        // 不变量由上面的缓冲区构建保证，所以这条分支实际不可达——
        // 但音频路径不能拿 unwrap 去依赖它。
        let input_adapter =
            SequentialSliceOfVecs::new(&self.temp_input, self.channels, input_frames).ok()?;
        let output_frames_max = self.temp_output.first().map(|v| v.len()).unwrap_or(0);
        let mut output_adapter =
            SequentialSliceOfVecs::new_mut(&mut self.temp_output, self.channels, output_frames_max)
                .ok()?;

        let Ok((_, frames_written)) =
            self.resampler
                .process_into_buffer(&input_adapter, &mut output_adapter, indexing)
        else {
            return None;
        };
        Some(frames_written)
    }

    /// 把每个中转声道的前 `frames` 个样本追加到 `output`。
    fn append_frames(&self, output: &mut [Vec<f64>], frames: usize) {
        for (out_buf, temp_ch) in output
            .iter_mut()
            .zip(self.temp_output.iter())
            .take(self.channels)
        {
            out_buf.extend_from_slice(&temp_ch[..frames]);
        }
    }

    pub fn passthrough_direct(
        input: &mut ChannelConsumers,
        output: &mut [Vec<f64>],
        max_samples: usize,
    ) -> usize {
        let read = input.try_read_to_staging(max_samples);
        if read == 0 {
            return 0;
        }

        let staging = input.staging();
        for (ch, channel) in staging.iter().enumerate() {
            if let Some(buf) = output.get_mut(ch) {
                buf.extend_from_slice(&channel[..read]);
            }
        }
        read
    }

    /// 从 f64 声道消费者把样本读进内部缓冲区
    fn read_into_buffers(
        input: &mut ChannelConsumers,
        buffers: &mut [VecDeque<f64>],
        max_samples: usize,
    ) -> usize {
        let read = input.try_read_to_staging(max_samples);
        if read == 0 {
            return 0;
        }

        let staging = input.staging();
        for (ch, channel) in staging.iter().enumerate() {
            // 与 `passthrough_direct` 同一道防线：解码器突然报出多于管线构建时的
            // 声道数，绝不能让整个进程中止（release 是 `panic = "abort"`）。
            if let Some(buffer) = buffers.get_mut(ch) {
                buffer.extend(&channel[..read]);
            }
        }
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::pipeline::ChannelBuffers;

    /// 按环形缓冲区大小分片，把 `frames` 帧恒为 `value` 的样本推过重采样器，
    /// 输出收集到 `out`。
    fn feed_constant(resampler: &mut Resampler, frames: usize, value: f64, out: &mut [Vec<f64>]) {
        let (mut producers, mut consumers) = ChannelBuffers::new(2, 8192).split();
        let mut remaining = frames;
        while remaining > 0 {
            let piece = remaining.min(4096);
            let planes = vec![vec![value; piece], vec![value; piece]];
            producers.write_vecs_nonblocking(&planes).unwrap();
            resampler.process_into(&mut consumers, out, 8192);
            remaining -= piece;
        }
        // 把最后一次写入留在环形缓冲里的部分也取出来
        resampler.process_into(&mut consumers, out, 8192);
    }

    #[test]
    fn flush_emits_exact_expected_stream_length() {
        let mut resampler = Resampler::new(44_100, 48_000, 1024, 2).unwrap();
        let mut out = vec![Vec::new(), Vec::new()];

        // 刻意不取分块大小的整数倍，好让一截不完整尾巴在“EOF”时留在重采样器内部
        let frames = 10_000;
        feed_constant(&mut resampler, frames, 1.0, &mut out);
        let before_flush = out[0].len();

        let flushed = resampler.flush_into(&mut out);
        assert!(flushed > 0, "flush produced no frames");

        let expected =
            (frames as f64 * 48_000.0 / 44_100.0).ceil() as usize + resampler.output_delay();
        for (ch, plane) in out.iter().enumerate() {
            assert_eq!(
                plane.len(),
                expected,
                "channel {ch}: expected {expected} total frames \
                 ({before_flush} before flush + tail)"
            );
        }

        // 稳态内容一直保真到尾巴之前（最后 output_delay 帧会向零填充衰减）
        let steady_end = expected - resampler.output_delay() - 16;
        assert!(
            (out[0][steady_end] - 1.0).abs() < 1e-3,
            "tail content missing: sample at {steady_end} is {}",
            out[0][steady_end]
        );

        // 未 reset 前再次 flush 是空操作
        assert_eq!(resampler.flush_into(&mut out), 0);

        // reset 之后重采样器恢复正常处理
        resampler.reset();
        let mut out2 = vec![Vec::new(), Vec::new()];
        feed_constant(&mut resampler, 4096, 0.5, &mut out2);
        assert!(!out2[0].is_empty());
    }

    #[test]
    fn flush_without_input_is_a_no_op() {
        let mut resampler = Resampler::new(44_100, 48_000, 1024, 2).unwrap();
        let mut out = vec![Vec::new(), Vec::new()];
        assert_eq!(resampler.flush_into(&mut out), 0);
        assert!(out[0].is_empty());
    }
}
