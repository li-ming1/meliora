use tracing::{info, warn};

use crate::{
    devices::{
        cpal::CpalProvider,
        dummy::DummyDeviceProvider,
        errors::{FindError, OpenError, ResetError, StateError, SubmissionError},
        format::{ChannelSpec, FormatInfo},
        traits::{Device, DeviceProvider, OutputStream},
    },
    media::pipeline::ChannelConsumers,
};

// 分段"音量百分比 → 浮点缩放"函数所用的魔数
pub const LN_50: f64 = 3.91202300543_f64;
pub const LINEAR_SCALING_COEFFICIENT: f64 = 0.295751527165_f64;

/// 输入达到或超过该水平时，[`DeviceController::set_volume`] 直接锁到满音量。
const FULL_VOLUME_MIN_INPUT: f64 = 0.99;
/// 输入低于该水平时，[`DeviceController::set_volume`] 改用普通线性斜坡，
/// 不用指数曲线。
const LINEAR_SEGMENT_MAX_INPUT: f64 = 0.1;

/// 设备控制器操作的错误类型。
#[derive(Debug)]
pub enum DeviceError {
    NoProvider,
    NoDevice,
    NoStream,
    OpenError(OpenError),
    FindError(FindError),
    StateError(StateError),
    ResetError(ResetError),
    SubmissionError(SubmissionError),
}

impl From<OpenError> for DeviceError {
    fn from(e: OpenError) -> Self {
        DeviceError::OpenError(e)
    }
}

impl From<FindError> for DeviceError {
    fn from(e: FindError) -> Self {
        DeviceError::FindError(e)
    }
}

impl From<StateError> for DeviceError {
    fn from(e: StateError) -> Self {
        DeviceError::StateError(e)
    }
}

impl From<ResetError> for DeviceError {
    fn from(e: ResetError) -> Self {
        DeviceError::ResetError(e)
    }
}

impl From<SubmissionError> for DeviceError {
    fn from(e: SubmissionError) -> Self {
        DeviceError::SubmissionError(e)
    }
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceError::NoProvider => write!(f, "No device provider available"),
            DeviceError::NoDevice => write!(f, "No device available"),
            DeviceError::NoStream => write!(f, "No stream available"),
            DeviceError::OpenError(e) => write!(f, "Open error: {:?}", e),
            DeviceError::FindError(e) => write!(f, "Find error: {:?}", e),
            DeviceError::StateError(e) => write!(f, "State error: {:?}", e),
            DeviceError::ResetError(e) => write!(f, "Reset error: {:?}", e),
            DeviceError::SubmissionError(e) => write!(f, "Submission error: {:?}", e),
        }
    }
}

impl std::error::Error for DeviceError {}

/// 音频设备与流管理的控制器。
///
/// 本组件负责与设备提供者、设备、输出流的所有交互：选设备、建流、播放控制、
/// 音量管理。
pub struct DeviceController {
    device_provider: Option<Box<dyn DeviceProvider>>,
    device: Option<Box<dyn Device>>,
    stream: Option<Box<dyn OutputStream>>,
    current_format: Option<FormatInfo>,
    last_volume: f64,
    last_replaygain: f64,
}

impl DeviceController {
    pub fn new() -> Self {
        Self {
            device_provider: None,
            device: None,
            stream: None,
            current_format: None,
            last_volume: 1.0,
            last_replaygain: 1.0,
        }
    }

    /// 依据环境变量或平台默认值初始化设备提供者。
    pub fn initialize_provider(&mut self) {
        let requested_device_provider =
            std::env::var("DEVICE_PROVIDER").unwrap_or_else(|_| "cpal".to_string());

        self.initialize_provider_by_name(&requested_device_provider);
    }

    /// 按名称初始化指定的设备提供者。
    pub fn initialize_provider_by_name(&mut self, provider_name: &str) {
        let mut provider: Box<dyn DeviceProvider> = match provider_name {
            "cpal" => Box::new(CpalProvider::default()),
            "dummy" => Box::new(DummyDeviceProvider::new()),
            _ => {
                warn!("Unknown device provider: {}", provider_name);
                warn!("Falling back to CPAL");
                Box::new(CpalProvider::default())
            }
        };
        provider.initialize();
        self.device_provider = Some(provider);
    }

    /// 当前是否已有流处于打开状态。
    pub fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    /// 按指定的声道配置创建新流。
    ///
    /// `channels` 为 None 时使用设备的默认格式；返回实际打开的格式。
    pub fn create_stream(
        &mut self,
        channels: Option<ChannelSpec>,
    ) -> Result<FormatInfo, DeviceError> {
        self.close_stream();

        let device_provider = self
            .device_provider
            .as_mut()
            .ok_or(DeviceError::NoProvider)?;

        let mut device = device_provider.get_default_device()?;

        let default_format = device
            .get_default_format()
            .map_err(|_| DeviceError::NoDevice)?;

        // 在设备默认格式之上叠加所请求的声道布局
        let requested = channels.map(|ch| FormatInfo {
            channels: ch,
            ..default_format.clone()
        });

        let (mut stream, opened_format) = if let Some(req) = requested {
            match device.open_device(req.clone()) {
                Ok(stream) => (stream, req),
                Err(e) => {
                    warn!(
                        ?default_format,
                        "Failed to open device with requested format: {:?}", e
                    );
                    warn!("Falling back to default format");
                    (device.open_device(default_format.clone())?, default_format)
                }
            }
        } else {
            (device.open_device(default_format.clone())?, default_format)
        };

        // 在新流上恢复上一次生效的音量 / ReplayGain
        stream.set_volume(self.last_volume).ok();
        stream.set_replaygain(self.last_replaygain).ok();

        self.stream = Some(stream);
        self.current_format = Some(opened_format.clone());
        self.device = Some(device);

        info!(
            "Opened device: {:?}, format: {:?}, rate: {}, channel_count: {}",
            self.device.as_ref().and_then(|d| d.get_name().ok()),
            opened_format.sample_type,
            opened_format.sample_rate,
            opened_format.channels.count()
        );

        Ok(opened_format)
    }

    /// 关闭当前流。
    pub fn close_stream(&mut self) {
        if let Some(mut stream) = self.stream.take()
            && let Err(e) = stream.close_stream()
        {
            warn!("Failed to close stream: {:?}", e);
        }
        self.current_format = None;
    }

    /// 在当前流上开始播放。
    pub fn play(&mut self) -> Result<(), DeviceError> {
        let stream = self.stream.as_mut().ok_or(DeviceError::NoStream)?;
        Ok(stream.play()?)
    }

    /// 暂停当前流的播放。
    pub fn pause(&mut self) -> Result<(), DeviceError> {
        let stream = self.stream.as_mut().ok_or(DeviceError::NoStream)?;
        Ok(stream.pause()?)
    }

    /// 推进流的延后工作（例如完成异步暂停淡出）。没有流时是空操作。
    pub fn poll(&mut self) -> Result<(), DeviceError> {
        if let Some(stream) = &mut self.stream {
            stream.poll()?;
        }
        Ok(())
    }

    /// 流是否有延后工作待办、因此必须尽快跑 `poll`（见
    /// `OutputStream::has_deferred_work`）。没有流即没有延后工作。
    pub fn has_deferred_work(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|stream| stream.has_deferred_work())
    }

    /// 重置流缓冲。
    pub fn reset(&mut self) -> Result<(), DeviceError> {
        let stream = self.stream.as_mut().ok_or(DeviceError::NoStream)?;
        Ok(stream.reset()?)
    }

    /// 从环形缓冲消费者取走样本并提交给设备。
    pub fn consume_from(&mut self, input: &mut ChannelConsumers) -> Result<usize, DeviceError> {
        let stream = self.stream.as_mut().ok_or(DeviceError::NoStream)?;
        Ok(stream.consume_from(input)?)
    }

    /// 设置播放音量（0.0 到 1.0，已完成缩放）。
    pub fn set_volume(&mut self, volume: f64) -> Result<(), DeviceError> {
        let volume_scaled = if volume >= FULL_VOLUME_MIN_INPUT {
            1.0
        } else if volume > LINEAR_SEGMENT_MAX_INPUT {
            f64::exp(LN_50 * volume) / 50.0
        } else {
            volume * LINEAR_SCALING_COEFFICIENT
        };

        self.last_volume = volume_scaled;

        if let Some(stream) = &mut self.stream {
            stream.set_volume(volume_scaled)?;
        }

        Ok(())
    }

    /// 设置 ReplayGain 倍数（线性）。
    pub fn set_replaygain(&mut self, gain: f64) -> Result<(), DeviceError> {
        self.last_replaygain = gain;

        if let Some(stream) = &mut self.stream {
            stream.set_replaygain(gain)?;
        }

        Ok(())
    }

    /// 取当前流格式，前提是有流处于打开状态。
    pub fn current_format(&self) -> Option<&FormatInfo> {
        self.current_format.as_ref()
    }
}
