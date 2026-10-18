use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tracing::{debug, info};

use crate::{
    devices::format::ChannelSpec,
    media::{
        errors::{
            ChannelRetrievalError, FrameDurationError, PlaybackReadError, PlaybackStartError,
            SeekError, TrackDurationError,
        },
        lookup_table::try_open_media,
        metadata::Metadata,
        pipeline::{ChannelProducers, DecodeResult},
        traits::{MediaProviderFeatures, MediaStream},
    },
};

pub struct CompleteMetadata {
    pub metadata: Box<Metadata>,
    pub album_art: Option<Arc<[u8]>>,
}

/// 在播放线程之外打开好的媒体流，曲目切换时可立即挂上（无缝切歌前进）。
pub struct PreparedMedia {
    pub(crate) path: PathBuf,
    pub(crate) stream: Box<dyn MediaStream>,
    pub(crate) duration_ms: Option<u64>,
}

/// 媒体流管理的控制器。
///
/// 本组件负责与媒体提供者及流的所有交互：开合文件、解码音频、读取元数据。
pub struct MediaController {
    media_stream: Option<Box<dyn MediaStream>>,
    current_path: Option<PathBuf>,
}

impl MediaController {
    pub fn new() -> Self {
        Self {
            media_stream: None,
            current_path: None,
        }
    }

    /// 当前是否已有媒体流处于打开状态。
    pub fn has_stream(&self) -> bool {
        self.media_stream.is_some()
    }

    /// 把媒体文件打开成独立的流，不触碰控制器状态；[`Self::open`]（播放线程）与
    /// 无缝切歌预备（后台线程）共用。`refresh` 给 HTTP 源装上流中 URL 刷新钩子
    /// （签名 URL 过期时在填充线程自愈）。
    pub(super) fn open_stream(
        path: &Path,
        refresh: Option<crate::media::http_source::MediaUrlRefresh>,
    ) -> Result<(Box<dyn MediaStream>, Option<u64>), PlaybackStartError> {
        // 远端 HTTP(S) 流绕过基于文件的提供者查找
        #[cfg(feature = "online_sources")]
        let src = if crate::media::is_http_path(path) {
            crate::media::http_source::open_http_media(path, refresh).map(Some)
        } else {
            try_open_media(path, MediaProviderFeatures::PROVIDES_DECODER)
        };
        #[cfg(not(feature = "online_sources"))]
        let _ = refresh;
        #[cfg(not(feature = "online_sources"))]
        let src = try_open_media(path, MediaProviderFeatures::PROVIDES_DECODER);

        let src =
            src.map_err(|e| PlaybackStartError::MediaError(format!("Unable to open media: {e}")))?;

        let Some(mut media_stream) = src else {
            return Err(PlaybackStartError::MediaError(
                "No media provider found".to_string(),
            ));
        };

        media_stream.start_playback().map_err(|e| {
            PlaybackStartError::MediaError(format!("Unable to start playback: {e}"))
        })?;

        media_stream
            .channels()
            .map_err(|e| PlaybackStartError::MediaError(format!("Unable to get channels: {e}")))?;

        let duration_ms = media_stream.duration_ms().ok();
        Ok((media_stream, duration_ms))
    }

    /// 打开媒体文件并准备好播放。
    ///
    /// 返回已知的曲目时长（毫秒），用于配置音频管线与设备。
    pub fn open(
        &mut self,
        path: &Path,
        refresh: Option<crate::media::http_source::MediaUrlRefresh>,
    ) -> Result<Option<u64>, PlaybackStartError> {
        debug!("Opening track '{}'", path.display());

        // 先关掉已有的流
        self.close();

        let (media_stream, duration_ms) = Self::open_stream(path, refresh)?;

        self.media_stream = Some(media_stream);
        self.current_path = Some(path.to_path_buf());

        info!(
            path = %path.display(),
            ?duration_ms,
            "media prepared for playback"
        );

        Ok(duration_ms)
    }

    /// 挂上由 [`AudioEngine::prepare_next`] 预先打开的流。预备阶段已完成全部校验
    /// （start + channels），所以这里不可能被网络阻塞。
    pub fn attach_prepared(
        &mut self,
        prepared: PreparedMedia,
    ) -> Result<Option<u64>, PlaybackStartError> {
        info!(
            path = %prepared.path.display(),
            "attaching pre-opened media stream"
        );
        self.close();

        let duration_ms = prepared.duration_ms;
        self.media_stream = Some(prepared.stream);
        self.current_path = Some(prepared.path);
        Ok(duration_ms)
    }

    /// 关闭当前媒体流（若有）。
    pub fn close(&mut self) {
        if let Some(mut stream) = self.media_stream.take() {
            stream.stop_playback();
            stream.close();
        }

        self.current_path = None;
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    /// 跳转定位到指定时刻，单位秒。
    pub fn seek(&mut self, time: f64) -> Result<(), SeekError> {
        if let Some(stream) = &mut self.media_stream {
            stream.seek(time)
        } else {
            Err(SeekError::InvalidState)
        }
    }

    /// 把音频样本解码进给定的环形缓冲生产者。
    pub fn decode_into(
        &mut self,
        output: &mut ChannelProducers,
    ) -> Result<DecodeResult, PlaybackReadError> {
        let stream = self
            .media_stream
            .as_mut()
            .ok_or(PlaybackReadError::NeverStarted)?;

        stream.decode_into(output)
    }

    /// 检查元数据是否更新，有则返回。
    ///
    /// 有更新时返回 (metadata, 可选封面图) 二元组；没有更新则返回 None。
    pub fn check_metadata_update(&mut self) -> Option<CompleteMetadata> {
        let stream = self.media_stream.as_mut()?;

        if !stream.metadata_updated() {
            return None;
        }

        let mut metadata = stream.read_metadata().ok()?;
        if let Some(path) = &self.current_path
            && !crate::media::is_http_path(path)
        {
            metadata.fill_from_filename(path);
        }
        let image = stream.read_image().ok().flatten().map(Arc::from);

        Some(CompleteMetadata {
            metadata: Box::new(metadata),
            album_art: image,
        })
    }

    pub fn position_ms(&self) -> Result<u64, TrackDurationError> {
        self.media_stream
            .as_ref()
            .ok_or(TrackDurationError::NeverStarted)?
            .position_ms()
    }

    pub fn channels(&self) -> Result<ChannelSpec, ChannelRetrievalError> {
        self.media_stream
            .as_ref()
            .ok_or(ChannelRetrievalError::NeverStarted)?
            .channels()
    }

    pub fn frame_duration(&self) -> Result<u64, FrameDurationError> {
        self.media_stream
            .as_ref()
            .ok_or(FrameDurationError::NeverStarted)?
            .frame_duration()
    }

    pub fn sample_rate(&self) -> Result<u32, ChannelRetrievalError> {
        self.media_stream
            .as_ref()
            .ok_or(ChannelRetrievalError::NeverStarted)?
            .sample_rate()
    }

    pub fn set_looping(&mut self, enabled: bool) {
        if let Some(stream) = &mut self.media_stream {
            stream.set_looping(enabled);
        }
    }
}
