use thiserror::Error;

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum OpenError {
    #[error("Format not supported by decoder")]
    UnsupportedFormat,
    #[error("I/O error: `{0:?}`")]
    Io(std::io::ErrorKind),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum PlaybackStartError {
    /// 表示不论出于什么原因，解码器的初始化以一种本不应可能的方式失败了。
    /// 通用解码错误不要用这个（应使用 Undecodable）。
    #[error("The media file is not valid and cannot be played")]
    InvalidState,
    #[error("Media is open but has no audio")]
    NothingToPlay,
    #[error("Media is undecodable")]
    Undecodable,
    #[error("Failed to process media: {0}")]
    MediaError(String),
    #[error("Audio stream error: {0}")]
    StreamError(String),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum PlaybackReadError {
    /// 表示不论出于什么原因，解码器的初始化以一种本不应可能的方式失败了。
    /// 通用解码错误不要用这个（应使用 DecodeFatal）。
    #[error("The media file is not valid and cannot be played")]
    InvalidState,
    #[error("Media is open but was never started")]
    NeverStarted,
    #[error("End of file reached")]
    Eof,
    #[error("Channel count changed to {0}")]
    ChannelCountChanged(usize),
    #[error("Unknown media provider error: `{0}`")]
    Unknown(String),
    #[error("Decode error: `{0}`")]
    DecodeFatal(String),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum MetadataError {
    #[error("The media file is not valid and cannot be played")]
    InvalidState,
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum FrameDurationError {
    #[error("Frame length requested before decoding")]
    NeverStarted,
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum TrackDurationError {
    #[error("Media is open but was never started")]
    NeverStarted,
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum SeekError {
    #[error("The media file is not valid and cannot be played")]
    InvalidState,
    #[error("Unknown media provider error: `{0}`")]
    Unknown(String),
}

#[derive(PartialEq, Eq, Debug, Clone, Error)]
pub enum ChannelRetrievalError {
    #[error("The media file is not valid and cannot be played")]
    InvalidState,
    #[error("Media is open but was never started")]
    NeverStarted,
    #[error("Media is open but has no audio")]
    NothingToPlay,
}
