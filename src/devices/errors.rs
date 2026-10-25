use thiserror::Error;

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum SubmissionError {
    #[error("Stream stopped consuming samples (device died?)")]
    WriteTimeout,
    #[error("The audio device reported a fatal error (disconnected?)")]
    DeviceError,
    // 流按 `stream_channels` 声道交织消费，而管线生产者是 `pipeline_channels`
    // 个平面——继续提交会把样本拆进错误的声道（采样率漂移则是变调变速）。
    // 修复引擎侧漂移检测后此错误结构上不可达，纯纵深守卫。
    #[error(
        "Pipeline channel count {pipeline_channels} does not match the open stream ({stream_channels})"
    )]
    FormatMismatch {
        stream_channels: usize,
        pipeline_channels: usize,
    },
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum FindError {
    #[error("Requested device does not exist")]
    DeviceDoesNotExist,
    #[error("Unknown device provider error: `{0}`")]
    Unknown(String),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum InfoError {
    #[error("Unsupported sample format `{0}` requested")]
    SampleFmt(String),
    #[error("Unknown device error: `{0}`")]
    Unknown(String),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum OpenError {
    #[error(
        "The supplied sample format is from a different device provider than the requested device"
    )]
    InvalidConfigProvider,
    #[error("The supplied sample format is not supported by the device")]
    InvalidSampleFormat,
    #[error("Unknown device error: `{0}`")]
    Unknown(String),
}

/// 不可实例化：两个内置 provider 关闭流均不会失败。
#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum CloseError {}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum StateError {
    #[error("Unknown stream error: `{0}`")]
    Unknown(String),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum ResetError {
    #[error("Unknown stream error: `{0}`")]
    Unknown(String),
}
