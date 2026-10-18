use std::{ffi::OsStr, fs::File};

use bitflags::bitflags;

use crate::devices::format::ChannelSpec;

use super::{
    errors::{
        ChannelRetrievalError, FrameDurationError, MetadataError, OpenError, PlaybackReadError,
        PlaybackStartError, SeekError, TrackDurationError,
    },
    metadata::Metadata,
    pipeline::{ChannelProducers, DecodeResult},
};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq)]
    /// 媒体 provider 的 feature 位标志。
    pub struct MediaProviderFeatures: u8 {
        /// 表示该 provider 应用于读取元数据。
        const PROVIDES_METADATA        = 0b00000001;
        /// 表示该 provider 应用于解码媒体文件。
        const PROVIDES_DECODER         = 0b00000010;
        /// 表示扫描时应把该 provider 纳入文件索引的候选。
        const ALLOWS_INDEXING          = 0b00000100;
    }
}

/// MediaProvider trait 定义了与媒体 provider 交互所需的方法。媒体 provider
/// 是 [MediaStream] 对象的工厂，由后者负责从媒体文件里解码与读取元数据。
///
/// 该 trait 设计得足够灵活，provider 可以只实现元数据读取、只实现解码，或
/// 两者都实现。这样解码型 provider 无需把文件打开两次就能取到编解码器内
/// 的元数据。
pub trait MediaProvider: Send + Sync {
    /// 请求 provider 打开指定文件。文件以 File 对象给出，扩展名以
    /// Option<&OsStr> 给出。若未提供扩展名，provider 会依据文件内容推断类型。
    fn open(&self, file: File, ext: Option<&OsStr>) -> Result<Box<dyn MediaStream>, OpenError>;

    /// 返回该插件支持的文件扩展名列表。
    fn supported_extensions(&self) -> &[&str];

    /// 返回该插件支持的媒体 provider feature 位标志列表。
    /// 详见 `MediaProviderFeatures`。
    fn supported_features(&self) -> MediaProviderFeatures;
}

/// MediaStream trait 定义了与已打开的媒体流交互所需的方法。媒体流负责从
/// 媒体文件读取采样与元数据。
///
/// 当前的播放管线顺序如下：
/// Create -> Open -> Start -> Metadata -> Read -> Read -> ... -> Close
///
/// 注意：若你的 provider 支持元数据读取，在音乐库索引期间它会被要求连续
/// 多次快速地 open、start、read metadata。这是正常且预期的行为，你的插件
/// 必须能应对。
///
/// `Send` 是为了让流能在后台线程打开后交给播放线程（无缝播放的预打开）。
pub trait MediaStream: Send {
    /// 告知 provider 当前打开的文件不再需要。若已有文件处于打开状态，
    /// 不保证在 open 之前调用本函数。
    fn close(&mut self);

    /// 告知 provider 播放即将开始。
    fn start_playback(&mut self) -> Result<(), PlaybackStartError>;

    /// 告知 provider 播放已结束，不再读取采样或元数据。
    fn stop_playback(&mut self);

    /// 请求 provider 在当前文件中跳转定位到指定时刻，时间以秒给出。
    /// 若没有文件处于打开状态，本函数应返回错误。
    fn seek(&mut self, time: f64) -> Result<(), SeekError>;

    /// 返回当前打开文件下 provider 给出的 PlaybackFrames 的常规时长；没有
    /// 文件打开时应返回错误。注意 PlaybackFrame 可以比这个时长短，但绝不应
    /// 更长。
    fn frame_duration(&self) -> Result<u64, FrameDurationError>;

    /// 返回当前打开文件的元数据并接管其所有权。若没有文件打开，或 provider
    /// 不支持元数据读取，本函数应返回错误。
    fn read_metadata(&mut self) -> Result<Metadata, MetadataError>;

    /// 返回自上次调用 read_metadata 以来元数据是否发生过更新。
    fn metadata_updated(&self) -> bool;

    /// 从曲目元数据中取当前封面图（若存在）。若没有文件打开，或 provider
    /// 不支持图像读取，本函数应返回错误。
    fn read_image(&mut self) -> Result<Option<Box<[u8]>>, MetadataError>;

    /// 返回当前打开文件的时长（毫秒）。若没有文件打开，或播放尚未开始，
    /// 本函数应返回错误。播放一开始本函数就应可用，且不要求读取任何采样。
    fn duration_ms(&self) -> Result<u64, TrackDurationError>;

    /// 返回当前播放位置（毫秒）。若没有文件打开，或播放尚未开始，本函数
    /// 应返回错误。播放一开始本函数就应可用，且不要求读取任何采样。
    fn position_ms(&self) -> Result<u64, TrackDurationError>;

    /// 返回被解码曲目使用的声道规格。播放一开始本函数就应可用，且不要求
    /// 读取任何采样。
    ///
    /// 播放线程用它判断当前设备能否处理该曲目的通道数，若能则切换通道数。
    fn channels(&self) -> Result<ChannelSpec, ChannelRetrievalError>;

    /// 返回被解码曲目的采样率（Hz）。播放一开始本函数就应可用，且不要求
    /// 读取任何采样。
    ///
    /// 播放线程用它确定源采样率与设备采样率不一致时重采样的正确目标率。
    fn sample_rate(&self) -> Result<u32, ChannelRetrievalError>;

    /// 解码一个 packet/frame，并把采样以 f64 直接写入给定的环形缓冲 producer。
    /// 解码器负责从原生采样格式转换为 f64。
    fn decode_into(
        &mut self,
        output: &mut ChannelProducers,
    ) -> Result<DecodeResult, PlaybackReadError>;

    /// 媒体流是否应尝试使用其内置的循环处理。Symphonia 在启用循环时，会在
    /// EOF 或循环结束点跳回循环起始点。
    ///
    /// 这件事由媒体流自身而非音频引擎处理，好让媒体流的实现能原生处理循环
    /// 行为（例如 tracker module 原生支持循环时）。
    fn set_looping(&mut self, enabled: bool);
}
