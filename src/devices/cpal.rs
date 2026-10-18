use crate::sync::RecoveredMutex;
use crate::{
    devices::{
        errors::{
            CloseError, FindError, InfoError, OpenError, ResetError, StateError, SubmissionError,
        },
        format::{BufferSize, ChannelSpec, FormatInfo, SampleFormat},
        resample::SampleFrom,
        traits::{Device, DeviceProvider, OutputStream},
        util::{AtomicF64, DitherLsb, GainRamp, Scale, soft_limit, tpdf},
    },
    media::pipeline::{
        ChannelConsumers, DEFAULT_BUFFER_FRAMES, read_available, write_bounded_planar,
    },
};
use cpal::{
    Host, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rtrb::{Producer, RingBuffer};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tracing::{info, warn};

macro_rules! make_unknown_error {
    ($from:ty, $to:ty) => {
        impl From<$from> for $to {
            fn from(value: $from) -> Self {
                <$to>::Unknown(value.to_string())
            }
        }
    };
}

/// 请求淡出到真正暂停流之间的等待时长。必须超过增益斜坡长度（15 ms）再加几个
/// 回调周期，才能保证音频线程以零增益把缓冲区排空。
const PAUSE_FADE_WAIT: Duration = Duration::from_millis(50);
/// 目标回调缓冲区大小。
const DEVICE_BUFFER_TARGET: Duration = Duration::from_millis(20);
/// 内部环形缓冲目标。它必须大于设备缓冲区，解码器才能吸收调度抖动而不至于把流
/// 拖到欠载。100 ms 只要生产者停顿超过一个缓冲区就会欠载（开曲、在线流抓取、
/// 封面解码突发）；250 ms 能吸收这些而代价很小（约 1 MB）——seek/pause 路径经
/// `reset()` 重建流，所以这段余量里不会播出陈旧音频。
const RING_BUFFER_TARGET: Duration = Duration::from_millis(250);
/// 生产者停顿达到该时长即记录日志。设备环形缓冲是 250 ms，接近这个长度的停顿
/// 正是饿死实时回调的那个；量出它，远好过面对一行光秃秃的 `underran` 猜原因。
const PRODUCER_STALL_REPORT_MS: u64 = 100;

/// 实时错误回调写入的固定长度错误消息槽容量。后端消息过长会被截断。
const DEVICE_ERROR_MESSAGE_CAP: usize = 256;
/// 欠载日志行的最小间隔；同一窗口内的突发折叠成一条报告。
const UNDERRUN_LOG_INTERVAL: Duration = Duration::from_secs(1);
/// 逐样本 TPDF 抖动的非零 xorshift 种子（64 位黄金比例常数；任何非零值都行）。
const DITHER_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// 最近一次 cpal 错误：由实时错误回调写入，由生产者读出（并打日志）。回调只
/// 拷贝字节——实时线程上不分配；文本在生产者侧重建。
struct DeviceErrorMessage {
    /// 错误类别，用于后端没附消息的情形（其 `Display` 是静态描述）。
    kind: cpal::ErrorKind,
    /// 后端附带消息的字节数。
    len: usize,
    bytes: [u8; DEVICE_ERROR_MESSAGE_CAP],
}

impl DeviceErrorMessage {
    fn record(err: &cpal::Error) -> Self {
        let mut slot = Self {
            kind: err.kind(),
            len: 0,
            bytes: [0; DEVICE_ERROR_MESSAGE_CAP],
        };
        if let Some(message) = err.message() {
            let n = message.len().min(slot.bytes.len());
            // 刻意按字节切片：按 str 切片会在截断点落在字符中间时 panic，而这段
            // 跑在实时回调线程上；生产侧用有损方式重建文本（`text`）
            slot.bytes[..n].copy_from_slice(&message.as_bytes()[..n]);
            slot.len = n;
        }
        slot
    }

    /// 在（非实时的）生产侧重建供人阅读的文本。
    fn text(&self) -> std::borrow::Cow<'_, str> {
        if self.len > 0 {
            String::from_utf8_lossy(&self.bytes[..self.len])
        } else {
            std::borrow::Cow::Owned(self.kind.to_string())
        }
    }
}

pub struct CpalProvider {
    host: Host,
}

impl Default for CpalProvider {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

impl DeviceProvider for CpalProvider {
    fn initialize(&mut self) {
        self.host = cpal::default_host();

        info!("Using cpal host {}", self.host.id().name());
    }

    fn get_default_device(&mut self) -> Result<Box<dyn Device>, FindError> {
        self.host
            .default_output_device()
            .ok_or(FindError::DeviceDoesNotExist)
            .map(|dev| Box::new(CpalDevice::from(dev)) as Box<dyn Device>)
    }
}

struct CpalDevice {
    device: cpal::Device,
}

impl From<cpal::Device> for CpalDevice {
    fn from(value: cpal::Device) -> Self {
        CpalDevice { device: value }
    }
}

impl TryFrom<cpal::SampleFormat> for SampleFormat {
    type Error = InfoError;

    fn try_from(value: cpal::SampleFormat) -> Result<Self, Self::Error> {
        match value {
            cpal::SampleFormat::I8 => Ok(SampleFormat::Signed8),
            cpal::SampleFormat::I16 => Ok(SampleFormat::Signed16),
            cpal::SampleFormat::I32 => Ok(SampleFormat::Signed32),
            cpal::SampleFormat::U8 => Ok(SampleFormat::Unsigned8),
            cpal::SampleFormat::U16 => Ok(SampleFormat::Unsigned16),
            cpal::SampleFormat::U32 => Ok(SampleFormat::Unsigned32),
            cpal::SampleFormat::F32 => Ok(SampleFormat::Float32),
            cpal::SampleFormat::F64 => Ok(SampleFormat::Float64),
            unsupported => Err(InfoError::SampleFmt(unsupported.to_string())),
        }
    }
}

fn cpal_config_from_info(format: &FormatInfo) -> Result<cpal::StreamConfig, ()> {
    if format.originating_provider != "cpal" {
        return Err(());
    }

    let target_frames = frames_for_duration(format.sample_rate, DEVICE_BUFFER_TARGET);
    let buffer_size = match format.buffer_size {
        BufferSize::Range(min, max) => cpal::BufferSize::Fixed(target_frames.clamp(min, max)),
        BufferSize::Fixed(size) => cpal::BufferSize::Fixed(size),
        BufferSize::Unknown => cpal::BufferSize::Default,
    };

    Ok(cpal::StreamConfig {
        channels: format.channels.count(),
        sample_rate: format.sample_rate,
        buffer_size,
    })
}

fn frames_for_duration(sample_rate: u32, duration: Duration) -> u32 {
    ((u64::from(sample_rate) * duration.as_micros() as u64) / 1_000_000).max(1) as u32
}

trait CpalSample: SizedSample + Default + Send + Sized + 'static + Scale {}

impl<T> CpalSample for T where T: SizedSample + Default + Send + Sized + 'static + Scale {}

fn create_stream_internal<T: CpalSample + SampleFrom<f64>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    buffer_size: usize,
    target_gain: Arc<AtomicF64>,
    underruns: Arc<AtomicU64>,
    primed: Arc<AtomicBool>,
    device_error_message: Arc<Mutex<Option<DeviceErrorMessage>>>,
) -> Result<(cpal::Stream, Producer<T>, Arc<AtomicBool>), OpenError> {
    let (prod, mut cons) = RingBuffer::<T>::new(buffer_size);
    let channels = config.channels as usize;
    let mut ramp = GainRamp::new(config.sample_rate);

    let device_errored = Arc::new(AtomicBool::new(false));
    let error_flag = device_errored.clone();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            // 实时回调线程的 MMCSS 登记（cpal 自建线程无法在创建侧做）：
            // 每线程一次，后续只剩 thread_local 读，实时路径安全。见
            // devices::mmcss 模块文档（2026-10-04 调度饥饿事故的应用侧
            // 防线）。
            crate::devices::mmcss::register_audio_thread_once();
            let read = read_available(&mut cons, data);
            if read < data.len() {
                // 尾部用数字静音填满，这样欠载时播的是静音，而不是设备缓冲区里
                // 残留的陈旧样本。填的是量化后的 0.0 而非 `T::default()`：无符号
                // 格式的静音点在中位，而 0 回读成全幅负 DC。
                data[read..].fill(T::sample_from(0.0));
                // 生产者首次提交之前，这段静音是 open->prime 之间的预期空档
                // （设备流已在运行，管线还在准备第一条曲目），不是欠载——
                // 把它计入会在每次会话启动刷出约 20 次回调的虚假突发
                // （2026-09 日志）。用 primed 设门，让计数器只报告已喂料流的
                // 真实饥饿。
                if primed.load(Ordering::Relaxed) {
                    underruns.fetch_add(1, Ordering::Relaxed);
                }
            }

            let target = target_gain.load(Ordering::Relaxed);
            ramp.apply(data, channels, target);
        },
        move |err| {
            // 实时线程：只记录错误。打日志（文件 IO 与文本格式化）留给生产者
            // 观察到旗标时再做。槽位拷贝逐字节进行——这里不分配。
            //
            // 用 `try_lock` 而不是 `lock`：生产者也锁这同一把互斥锁，而实时线程
            // 绝不能在它上面阻塞。若生产者恰好在并发排空，这条消息就被丢弃——
            // 错误风暴期间丢消息可以接受，且下面的旗标无论如何都会触发生产者侧
            // 的错误路径。
            if let Ok(mut slot) = device_error_message.try_lock() {
                *slot = Some(DeviceErrorMessage::record(&err));
            }
            error_flag.store(true, Ordering::Relaxed);
        },
        None,
    )?;

    Ok((stream, prod, device_errored))
}

impl CpalDevice {
    fn create_stream<T>(&mut self, format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError>
    where
        T: CpalSample + SampleFrom<f64> + DitherLsb,
    {
        let config =
            cpal_config_from_info(&format).map_err(|_| OpenError::InvalidConfigProvider)?;
        let channels = format.channels.count();
        let ring_buffer_frames = frames_for_duration(config.sample_rate, RING_BUFFER_TARGET);
        let buffer_size = ring_buffer_frames as usize * channels as usize;
        info!(
            "audio ring: {ring_buffer_frames} frames ({} ms) × {channels} ch, \
            CPAL buffer {buffer_size} samples; device buffer target {} ms",
            RING_BUFFER_TARGET.as_millis(),
            DEVICE_BUFFER_TARGET.as_millis()
        );
        let target_gain = Arc::new(AtomicF64::new(1.0));
        let underruns = Arc::new(AtomicU64::new(0));
        let primed = Arc::new(AtomicBool::new(false));
        let device_error_message = Arc::new(Mutex::new(None::<DeviceErrorMessage>));
        let (stream, prod, device_errored) = create_stream_internal::<T>(
            &self.device,
            config,
            buffer_size,
            target_gain.clone(),
            underruns.clone(),
            primed.clone(),
            device_error_message.clone(),
        )?;

        Ok(Box::new(CpalStream {
            ring_buf: prod,
            stream,
            config,
            buffer_size,
            device: self.device.clone(),
            target_gain,
            last_user_volume: 1.0,
            replaygain: 1.0,
            dither: DITHER_SEED,
            // 最坏情况：一整个管线中转缓冲区，交织排列
            interleave_buffer: Vec::with_capacity(DEFAULT_BUFFER_FRAMES * channels as usize),
            underruns,
            primed,
            underruns_reported: 0,
            last_underrun_log: Instant::now(),
            stream_started_at: Instant::now(),
            idle_since: None,
            logged_first_submit: false,
            device_errored,
            device_error_message,
            pause_at: None,
        }))
    }
}

impl Device for CpalDevice {
    fn open_device(&mut self, format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError> {
        if format.originating_provider != "cpal" {
            return Err(OpenError::InvalidConfigProvider);
        }

        match format.sample_type {
            SampleFormat::Signed8 => self.create_stream::<i8>(format),
            SampleFormat::Signed16 => self.create_stream::<i16>(format),
            SampleFormat::Signed32 => self.create_stream::<i32>(format),
            SampleFormat::Unsigned8 => self.create_stream::<u8>(format),
            SampleFormat::Unsigned16 => self.create_stream::<u16>(format),
            SampleFormat::Unsigned32 => self.create_stream::<u32>(format),
            SampleFormat::Float32 => self.create_stream::<f32>(format),
            SampleFormat::Float64 => self.create_stream::<f64>(format),
            _ => Err(OpenError::InvalidSampleFormat),
        }
    }

    fn get_default_format(&self) -> Result<FormatInfo, InfoError> {
        let format = self.device.default_output_config()?;
        Ok(FormatInfo {
            originating_provider: "cpal",
            sample_type: format.sample_format().try_into()?,
            sample_rate: format.sample_rate(),
            buffer_size: match format.buffer_size() {
                &cpal::SupportedBufferSize::Range { min, max } => BufferSize::Range(min, max),
                cpal::SupportedBufferSize::Unknown => BufferSize::Unknown,
            },
            channels: ChannelSpec::Count(format.channels()),
        })
    }

    fn get_name(&self) -> Result<String, InfoError> {
        self.device
            .description()
            .map_err(Into::into)
            .map(|description| description.name().to_string())
    }
}

struct CpalStream<T>
where
    T: SizedSample + Default,
{
    pub ring_buf: Producer<T>,
    pub stream: cpal::Stream,
    pub config: cpal::StreamConfig,
    pub device: cpal::Device,
    pub buffer_size: usize,
    pub target_gain: Arc<AtomicF64>,
    /// 用户最近一次要求的音量。它与 `target_gain` 分开跟踪，因为 pause-fade 会
    /// 临时把共享原子写成 0.0；`play()` 从该字段恢复。
    pub last_user_volume: f64,
    pub replaygain: f64,
    /// 喂养逐样本 TPDF 抖动的 xorshift 状态（非零）。
    pub dither: u64,
    pub interleave_buffer: Vec<T>,
    pub underruns: Arc<AtomicU64>,
    /// 在生产者首次成功提交之前一直是清除态（`reset` 时再次清除）：实时回调
    /// 只在该位为真时统计欠载，于是 open->prime 之间的静音空档不算饥饿。
    pub primed: Arc<AtomicBool>,
    /// 已经报告过的欠载次数，避免同一突发被反复记录。
    underruns_reported: u64,
    last_underrun_log: Instant,
    /// 本流开始消费的时刻，用于计量 open -> 首个音频之间的空档。
    stream_started_at: Instant,
    /// 生产者上次无数据可提交的时刻，首次成功提交时清除。只在生产者侧使用，
    /// 因此完全不触碰实时回调。
    idle_since: Option<Instant>,
    /// 本流是否已经打过 open -> 首次提交 的日志行。
    logged_first_submit: bool,
    device_errored: Arc<AtomicBool>,
    /// 最近一次 cpal 错误：由实时错误回调写入，由生产者读出（并打日志）。
    /// 定容量字节槽：把分配和日志都挪出音频线程。
    device_error_message: Arc<Mutex<Option<DeviceErrorMessage>>>,
    /// 表示流正在淡出，需在该时刻之前暂停。
    pause_at: Option<Instant>,
}

impl<T> CpalStream<T>
where
    T: SizedSample + Default,
{
    fn report_underruns(&mut self) {
        let total = self.underruns.load(Ordering::Relaxed);
        if total > self.underruns_reported
            && self.last_underrun_log.elapsed() >= UNDERRUN_LOG_INTERVAL
        {
            warn!(
                "audio callback underran {} time(s) ({} total)",
                total - self.underruns_reported,
                total
            );
            self.underruns_reported = total;
            self.last_underrun_log = Instant::now();
        }
    }
}

impl<T> OutputStream for CpalStream<T>
where
    T: CpalSample + SampleFrom<f64> + DitherLsb,
{
    fn close_stream(&mut self) -> Result<(), CloseError> {
        Ok(())
    }

    fn play(&mut self) -> Result<(), StateError> {
        self.pause_at = None;
        self.target_gain
            .store(self.last_user_volume, Ordering::Relaxed);
        self.stream.play().map_err(|v| v.into())
    }

    fn pause(&mut self) -> Result<(), StateError> {
        self.target_gain.store(0.0, Ordering::Relaxed);
        self.pause_at = Some(Instant::now() + PAUSE_FADE_WAIT);
        Ok(())
    }

    fn poll(&mut self) -> Result<(), StateError> {
        if let Some(deadline) = self.pause_at
            && Instant::now() >= deadline
        {
            self.pause_at = None;
            return self.stream.pause().map_err(|v| v.into());
        }
        Ok(())
    }

    fn has_deferred_work(&self) -> bool {
        self.pause_at.is_some()
    }

    fn reset(&mut self) -> Result<(), ResetError> {
        let pause_pending = self.pause_at.take().is_some();
        let (stream, prod, device_errored) = create_stream_internal::<T>(
            &self.device,
            self.config,
            self.buffer_size,
            self.target_gain.clone(),
            self.underruns.clone(),
            self.primed.clone(),
            self.device_error_message.clone(),
        )?;

        self.stream = stream;
        self.ring_buf = prod;
        self.device_errored = device_errored;
        self.interleave_buffer.clear();
        self.stream_started_at = Instant::now();
        self.idle_since = None;
        self.logged_first_submit = false;
        // 重建后的流尚未布防，直到样本重新流动。
        self.primed.store(false, Ordering::Relaxed);

        if pause_pending && let Err(e) = self.stream.pause() {
            return Err(ResetError::Unknown(e.to_string()));
        }

        Ok(())
    }

    fn set_volume(&mut self, volume: f64) -> Result<(), StateError> {
        self.last_user_volume = volume;
        if self.pause_at.is_none() {
            self.target_gain.store(volume, Ordering::Relaxed);
        }
        Ok(())
    }

    fn set_replaygain(&mut self, gain: f64) -> Result<(), StateError> {
        self.replaygain = gain;
        Ok(())
    }

    #[allow(clippy::needless_range_loop)]
    fn consume_from(&mut self, input: &mut ChannelConsumers) -> Result<usize, SubmissionError> {
        if self.device_errored.load(Ordering::Relaxed) {
            // 在锁内取走消息、在锁外打日志：实时错误回调共用这把互斥锁，
            // 绝不能让它排在文件 IO 后面等待。毒化锁也要恢复，这样别处持锁时
            // panic 永远不会让消息无从取回。
            let message = self.device_error_message.lock_recovered().take();
            if let Some(msg) = message {
                warn!("cpal stream error: {}", msg.text());
            }
            return Err(SubmissionError::DeviceError);
        }

        self.report_underruns();

        let capacity_frames = self.interleave_buffer.capacity() / input.channel_count().max(1);
        let available = input.potentially_available().min(capacity_frames);
        if available == 0 {
            self.idle_since.get_or_insert_with(Instant::now);
            return Ok(0);
        }

        let read = input.try_read_to_staging(available);
        if read == 0 {
            self.idle_since.get_or_insert_with(Instant::now);
            return Ok(0);
        }

        // 生产者无料可交给设备环形缓冲了多久，这才是真正饿死实时回调的量，
        // 也是光秃秃一行 `underran` 唯一不给的数字。只在 idle -> 有料的转换处
        // 记录，稳定播放期间日志保持安静。
        if let Some(since) = self.idle_since.take() {
            let idle_ms = since.elapsed().as_millis() as u64;
            if idle_ms >= PRODUCER_STALL_REPORT_MS {
                info!(
                    idle_ms,
                    underruns = self.underruns.load(Ordering::Relaxed),
                    "[audio] producer had nothing to submit"
                );
            }
        }
        if !self.logged_first_submit {
            self.logged_first_submit = true;
            info!(
                elapsed_ms = self.stream_started_at.elapsed().as_millis() as u64,
                "[audio] first samples submitted after stream start"
            );
        }

        let staging = input.staging();

        let channel_count = staging.len();
        let rg = self.replaygain;
        let mut dither = self.dither;

        self.interleave_buffer.clear();
        debug_assert!(
            read * channel_count <= self.interleave_buffer.capacity(),
            "interleave buffer under-sized at stream creation"
        );

        for i in 0..read {
            for ch in 0..channel_count {
                // 量化之前先做软限幅：让增益（ReplayGain / EQ 提升）曲线趋近
                // 满幅，而不是硬削顶。
                let sample = soft_limit(staging[ch][i] * rg);
                // TPDF 抖动打散浅位整数目标的量化误差噪声相关；float 与
                // ≥24-bit 原样转换。
                let sample = match T::DITHER_LSB {
                    Some(lsb) => sample + tpdf(&mut dither) * lsb,
                    None => sample,
                };
                self.interleave_buffer.push(T::sample_from(sample));
            }
        }
        self.dither = dither;

        write_bounded_planar(
            std::slice::from_mut(&mut self.ring_buf),
            &[&self.interleave_buffer],
            self.interleave_buffer.len(),
        )
        .map_err(|e| {
            // 超时路径透传实际写入量：诊断停滞场景时不再只看到总量口径。
            warn!(written = e.written, "ring write timed out");
            SubmissionError::WriteTimeout
        })?;
        // 样本已进环形缓冲：此后回调饥饿就是真欠载，要被计数。
        self.primed.store(true, Ordering::Relaxed);

        Ok(read)
    }
}

make_unknown_error!(OpenError, ResetError);
make_unknown_error!(cpal::Error, StateError);
make_unknown_error!(cpal::Error, InfoError);
make_unknown_error!(cpal::Error, OpenError);
make_unknown_error!(cpal::Error, FindError);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::resample::SampleInto;

    /// 欠载尾部填充必须在每个被派发的样本格式下都回读为数字静音。
    /// `T::default()` 只对带符号与 float 目标成立：无符号格式把静音放在中位，
    /// 那里 0 回读成全幅负 DC，所以取值必须来自对 0.0 做量化
    /// （`SampleFrom`/`SampleInto` 里的符号位翻转）。
    #[test]
    fn underrun_silence_fill_reads_zero_in_every_format() {
        fn assert_silent<T: CpalSample + SampleFrom<f64> + SampleInto<f64>>() {
            // 先填入陈旧缓冲区内容，再执行回调实际做的那次填充
            let mut data = [T::default(); 8];
            data.fill(T::sample_from(0.0));
            for (i, sample) in data.iter().enumerate() {
                let value = sample.sample_into();
                assert_eq!(
                    value,
                    0.0,
                    "{} sample {i} reads {value}",
                    std::any::type_name::<T>()
                );
            }
        }

        assert_silent::<u8>();
        assert_silent::<u16>();
        assert_silent::<u32>();
        assert_silent::<i8>();
        assert_silent::<i16>();
        assert_silent::<i32>();
        assert_silent::<f32>();
        assert_silent::<f64>();
    }
}
