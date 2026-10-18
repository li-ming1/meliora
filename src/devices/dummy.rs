use std::{
    env,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tracing::{debug, info, warn};

use crate::sync::RecoveredMutex;
use crate::{
    devices::{
        errors::{
            CloseError, FindError, InfoError, OpenError, ResetError, StateError, SubmissionError,
        },
        format::{BufferSize, ChannelSpec, FormatInfo, SampleFormat},
        traits::{Device, DeviceProvider, OutputStream},
    },
    media::pipeline::ChannelConsumers,
};

pub type CapturedPlanes = Arc<Mutex<Vec<Vec<f64>>>>;

static CAPTURE: Mutex<Option<CapturedPlanes>> = Mutex::new(None);

/// 安装一个接收器，记录到达 dummy 设备层的每个样本。测试用它抓取管线的最终结果。
#[cfg_attr(not(test), allow(dead_code))]
pub fn install_capture() -> CapturedPlanes {
    let sink: CapturedPlanes = Arc::new(Mutex::new(Vec::new()));
    *CAPTURE.lock_recovered() = Some(Arc::clone(&sink));
    sink
}

/// 移除 [`install_capture`] 安装接收器（若有）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn uninstall_capture() {
    *CAPTURE.lock_recovered() = None;
}

/// 被建模的设备死亡一旦触发就置位，于是重建出的流不会再次死掉。
static DEVICE_ALREADY_DIED: AtomicBool = AtomicBool::new(false);

/// 复位这个一次性闩锁，让下一个到达 `HB_DUMMY_DIE_AFTER_FRAMES` 的流故障一次。
#[cfg_attr(not(test), allow(dead_code))]
pub fn arm_device_death() {
    DEVICE_ALREADY_DIED.store(false, Ordering::SeqCst);
}

fn capture_samples<T: Copy + Into<f64>>(staging: &[Vec<T>], read: usize) {
    let guard = CAPTURE.lock_recovered();
    let Some(sink) = guard.as_ref() else { return };

    let mut planes = sink.lock_recovered();
    if planes.len() < staging.len() {
        planes.resize_with(staging.len(), Vec::new);
    }
    for (plane, channel) in planes.iter_mut().zip(staging) {
        plane.extend(channel[..read].iter().map(|&sample| sample.into()));
    }
}

/// dummy 设备提供方仅供测试使用：它不播放任何真实音频，向它造出的设备提交样本
/// 是 no-op。
///
/// 该提供方给出的流在提交样本时永不阻塞，也就是说播放线程往往会立刻提交下一批
/// 样本。
///
/// 以下环境变量用于配置 dummy 设备：
/// - `HB_DUMMY_SAMPLE_RATE`：dummy 设备的采样率。默认 44100。
/// - `HB_DUMMY_BIT_FORMAT`：dummy 设备的位格式。默认 `S16`。
/// - `HB_DUMMY_CHANNELS`：dummy 设备的声道数。默认 2。
/// - `HB_DUMMY_BUFFER_SIZE`：dummy 设备的缓冲区大小。默认 4096。
///   - 它其实不起任何作用：样本从不真正装进缓冲区。
/// - `HB_DUMMY_BOUNDED_FRAMES`：被建模的有界硬件环形缓冲容量。无默认值：一旦设置，
///   `open_device` 改返回 [`BoundedDummyStream`]，用于建模背压。
/// - `HB_DUMMY_DRAIN_FRAMES`：每次 `consume_from` 调用有界环排空的帧数。默认 1024。
/// - `HB_DUMMY_DIE_AFTER_FRAMES`：消费到这么多帧后让流故障一次（见
///   [`arm_device_death`]）。无默认值：未设置即流永不死。
pub struct DummyDeviceProvider {}

impl DummyDeviceProvider {
    pub fn new() -> Self {
        Self {}
    }
}

impl DeviceProvider for DummyDeviceProvider {
    fn initialize(&mut self) {
        info!("DummyDeviceProvider initialized");
        warn!("This device provider WILL not play any actual audio.");
    }

    fn get_default_device(&mut self) -> Result<Box<dyn Device>, FindError> {
        debug!("Creating new dummy device");
        Ok(Box::new(DummyDevice {}) as Box<dyn Device>)
    }
}

/// `HB_DUMMY_*` 环境变量的解析结果；变量未设置或无法解析为 `T` 时返回 `None`。
fn env_parsed<T: FromStr>(name: &str) -> Option<T> {
    env::var(name).ok().and_then(|s| s.parse().ok())
}

pub struct DummyDevice {}

impl DummyDevice {
    pub fn get_sample_rate() -> u32 {
        env_parsed("HB_DUMMY_SAMPLE_RATE").unwrap_or(44100)
    }

    pub fn get_bit_format() -> Result<SampleFormat, InfoError> {
        let var = env::var("HB_DUMMY_BIT_FORMAT");
        let format = match var.as_deref() {
            Ok("F64") => SampleFormat::Float64,
            Ok("F32") => SampleFormat::Float32,
            Ok("S32") => SampleFormat::Signed32,
            Ok("U32") => SampleFormat::Unsigned32,
            Ok("S24") => SampleFormat::Signed24,
            Ok("U24") => SampleFormat::Unsigned24,
            Ok("S16") => SampleFormat::Signed16,
            Ok("U16") => SampleFormat::Unsigned16,
            Ok("S8") => SampleFormat::Signed8,
            Ok("U8") => SampleFormat::Unsigned8,
            Err(env::VarError::NotPresent) => SampleFormat::Signed16,
            Ok(other) => return Err(InfoError::SampleFmt(other.to_string())),
            Err(env::VarError::NotUnicode(os)) => {
                return Err(InfoError::SampleFmt(os.to_string_lossy().into_owned()));
            }
        };
        Ok(format)
    }

    pub fn get_channels() -> u16 {
        env_parsed("HB_DUMMY_CHANNELS").unwrap_or(2)
    }

    pub fn get_buffer_size() -> u32 {
        env_parsed("HB_DUMMY_BUFFER_SIZE").unwrap_or(4096)
    }

    pub fn get_bounded_frames() -> Option<usize> {
        env_parsed("HB_DUMMY_BOUNDED_FRAMES")
    }

    pub fn get_drain_frames() -> usize {
        env_parsed("HB_DUMMY_DRAIN_FRAMES").unwrap_or(1024)
    }

    pub fn get_die_after_frames() -> Option<usize> {
        env_parsed("HB_DUMMY_DIE_AFTER_FRAMES")
    }
}

impl Device for DummyDevice {
    fn open_device(&mut self, _format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError> {
        if let Some(capacity) = DummyDevice::get_bounded_frames() {
            let drain = DummyDevice::get_drain_frames();
            return Ok(Box::new(BoundedDummyStream::new(capacity, drain)) as Box<dyn OutputStream>);
        }
        let device = DummyStream {
            die_after: DummyDevice::get_die_after_frames(),
            consumed: 0,
        };
        Ok(Box::new(device) as Box<dyn OutputStream>)
    }

    fn get_default_format(&self) -> Result<FormatInfo, InfoError> {
        Ok(FormatInfo {
            originating_provider: "dummy",
            sample_type: DummyDevice::get_bit_format()?,
            sample_rate: DummyDevice::get_sample_rate(),
            buffer_size: BufferSize::Fixed(DummyDevice::get_buffer_size()),
            channels: ChannelSpec::Count(DummyDevice::get_channels()),
        })
    }

    fn get_name(&self) -> Result<String, InfoError> {
        Ok("Meliora Dummy Audio Device".to_string())
    }
}

pub struct DummyStream {
    /// 若设置，消费到这么多帧后故障一次（见 [`arm_device_death`]）。
    die_after: Option<usize>,
    consumed: usize,
}

impl DummyStream {
    /// 本流此刻是否应当故障（为真时消耗掉一次性闩锁）。
    fn should_die(&self) -> bool {
        match self.die_after {
            Some(threshold) if self.consumed >= threshold => DEVICE_ALREADY_DIED
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            _ => false,
        }
    }
}

impl OutputStream for DummyStream {
    fn close_stream(&mut self) -> Result<(), CloseError> {
        debug!("Stream closed.");
        Ok(())
    }

    fn play(&mut self) -> Result<(), StateError> {
        debug!("Stream resumed.");
        Ok(())
    }

    fn pause(&mut self) -> Result<(), StateError> {
        debug!("Stream paused.");
        Ok(())
    }

    fn reset(&mut self) -> Result<(), ResetError> {
        debug!("Stream reset.");
        Ok(())
    }

    fn set_volume(&mut self, volume: f64) -> Result<(), StateError> {
        debug!("Volume set to {}.", volume);
        Ok(())
    }

    fn consume_from(&mut self, input: &mut ChannelConsumers) -> Result<usize, SubmissionError> {
        // 在读取之前先故障，这样不丢任何帧
        if self.should_die() {
            return Err(SubmissionError::DeviceError);
        }

        let available = input.potentially_available();
        if available == 0 {
            return Ok(0);
        }

        // 把样本排空即可，不做任何处理
        let read = input.try_read_to_staging(available);
        capture_samples(input.staging(), read);
        self.consumed += read;
        debug!("Consumed {} samples from ring buffer (dummy device)", read);
        Ok(read)
    }
}

/// 建模真实设备有界硬件环的 dummy 流：最多容纳 `capacity` 帧，每次
/// `consume_from` 调用“播放”其中 `drain` 帧（推进播放），因此只接收与剩余
/// 空间等量的新输入。用于测试那些只在背压下才出现的特定缺陷。
pub struct BoundedDummyStream {
    capacity: usize,
    drain: usize,
    /// 当前缓冲在被建模硬件环中的帧数。
    fill: usize,
}

impl BoundedDummyStream {
    fn new(capacity: usize, drain: usize) -> Self {
        Self {
            capacity,
            drain: drain.max(1),
            fill: 0,
        }
    }
}

impl OutputStream for BoundedDummyStream {
    fn close_stream(&mut self) -> Result<(), CloseError> {
        Ok(())
    }

    fn play(&mut self) -> Result<(), StateError> {
        Ok(())
    }

    fn pause(&mut self) -> Result<(), StateError> {
        Ok(())
    }

    fn reset(&mut self) -> Result<(), ResetError> {
        self.fill = 0;
        Ok(())
    }

    fn set_volume(&mut self, _volume: f64) -> Result<(), StateError> {
        Ok(())
    }

    fn consume_from(&mut self, input: &mut ChannelConsumers) -> Result<usize, SubmissionError> {
        // 推进播放：从环里排空至多 `drain` 帧
        self.fill = self.fill.saturating_sub(self.drain);

        let free = self.capacity - self.fill;
        let available = input.potentially_available().min(free);
        if available == 0 {
            return Ok(0);
        }

        let read = input.try_read_to_staging(available);
        capture_samples(input.staging(), read);
        self.fill += read;
        Ok(read)
    }
}
