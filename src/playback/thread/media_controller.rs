use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tracing::info;

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

/// A media stream opened off the playback thread, ready to be attached
/// instantly at a track transition (gapless advance).
pub struct PreparedMedia {
    pub(crate) path: PathBuf,
    pub(crate) stream: Box<dyn MediaStream>,
    pub(crate) duration_ms: Option<u64>,
}

/// Controller for media stream management.
///
/// This component handles all interactions with media providers and streams,
/// including opening/closing files, decoding audio, and retrieving metadata.
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

    /// Check if a media stream is currently open.
    pub fn has_stream(&self) -> bool {
        self.media_stream.is_some()
    }

    /// Open a media file into a standalone stream without touching controller
    /// state; shared by [`Self::open`] (playback thread) and gapless prepare
    /// (background thread).
    pub(super) fn open_stream(
        path: &Path,
    ) -> Result<(Box<dyn MediaStream>, Option<u64>), PlaybackStartError> {
        // remote HTTP(S) streams bypass the file-based provider lookup
        #[cfg(feature = "online_sources")]
        let src = if crate::media::is_http_path(path) {
            crate::media::http_source::open_http_media(path).map(Some)
        } else {
            try_open_media(path, MediaProviderFeatures::PROVIDES_DECODER)
        };
        #[cfg(not(feature = "online_sources"))]
        let src = try_open_media(path, MediaProviderFeatures::PROVIDES_DECODER);

        let src =
            src.map_err(|e| PlaybackStartError::MediaError(format!("Unable to open media: {e}")))?;

        let Some(mut media_stream) = src else {
            return Err(PlaybackStartError::MediaError(
                "No media provider found".to_string(),
            ));
        };

        media_stream
            .start_playback()
            .map_err(|e| PlaybackStartError::MediaError(format!("Unable to start playback: {e}")))?;

        media_stream
            .channels()
            .map_err(|e| PlaybackStartError::MediaError(format!("Unable to get channels: {e}")))?;

        let duration_ms = media_stream.duration_ms().ok();
        Ok((media_stream, duration_ms))
    }

    /// Open a media file and prepare it for playback.
    ///
    /// Returns the track duration in milliseconds if known, used to configure
    /// the audio pipeline and device.
    pub fn open(&mut self, path: &Path) -> Result<Option<u64>, PlaybackStartError> {
        info!("Opening track '{}'", path.display());

        // Close any existing stream
        self.close();

        let (media_stream, duration_ms) = Self::open_stream(path)?;

        self.media_stream = Some(media_stream);
        self.current_path = Some(path.to_path_buf());

        info!(
            path = %path.display(),
            ?duration_ms,
            "media prepared for playback"
        );

        Ok(duration_ms)
    }

    /// Install a stream pre-opened by [`AudioEngine::prepare_next`]. The
    /// stream was fully validated (start + channels) at prepare time, so this
    /// cannot block on the network.
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

    /// Close the current media stream, if any.
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

    /// Seek to the specified time in seconds.
    pub fn seek(&mut self, time: f64) -> Result<(), SeekError> {
        if let Some(stream) = &mut self.media_stream {
            stream.seek(time)
        } else {
            Err(SeekError::InvalidState)
        }
    }

    /// Decode audio samples into the provided ring buffer producers.
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

    /// Check for metadata updates and return them if available.
    ///
    /// Returns a tuple of (metadata, optional album art) if there's an update,
    /// or None if there's no update.
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
