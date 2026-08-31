use thiserror::Error;

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum SubmissionError {
    #[error("Stream stopped consuming samples (device died?)")]
    WriteTimeout,
    #[error("The audio device reported a fatal error (disconnected?)")]
    DeviceError,
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

/// Uninhabited: both in-tree providers close streams infallibly.
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
