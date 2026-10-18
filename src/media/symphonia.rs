use smallvec::SmallVec;
use std::{ffi::OsStr, fs::File};
use symphonia::{
    core::{
        audio::{Audio, GenericAudioBufferRef},
        codecs::{
            audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions},
            registry::CodecRegistry,
        },
        errors::Error,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType, probe::Hint},
        io::{MediaSource, MediaSourceStream},
        meta::{MetadataOptions, StandardTag, Tag, Visual},
        units::{Time, TimeBase, Timestamp},
    },
    default::codecs::{
        AdpcmDecoder, AlacDecoder, FlacDecoder, MpaDecoder, PcmDecoder, VorbisDecoder,
    },
};
use symphonia_adapter_fdk_aac::AacDecoder;
use tracing::error;

use symphonia_adapter_libopus::OpusDecoder;

use crate::{
    devices::{
        channels::{ChannelLabel, ChannelLayout, ChannelPosition},
        format::ChannelSpec,
        resample::{SampleInto, i24_saturating, u24_saturating},
    },
    media::{
        errors::{
            ChannelRetrievalError, FrameDurationError, MetadataError, OpenError, PlaybackReadError,
            PlaybackStartError, SeekError, TrackDurationError,
        },
        metadata::{Metadata, MetadataTag, apply_tag},
        pipeline::{ChannelProducers, DecodeResult, WriteError},
        traits::{MediaProvider, MediaProviderFeatures, MediaStream},
    },
};

fn time_to_millis(time: Time) -> u64 {
    (time.as_secs_f64() * 1000.0) as u64
}

/// 让一次上游 symphonia 调用豁免于测试分配守卫，因为 symphonia 的分配方式
/// 我们无法控制。
#[inline]
fn symphonia_alloc_exempt<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    {
        crate::test_support::alloc_guard::exempt(f)
    }
    #[cfg(not(test))]
    {
        f()
    }
}

#[inline]
fn next_packet(
    format: &mut dyn FormatReader,
) -> symphonia::core::errors::Result<Option<symphonia::core::packet::Packet>> {
    symphonia_alloc_exempt(|| format.next_packet())
}

fn map_write_error(e: WriteError) -> PlaybackReadError {
    match e {
        WriteError::ChannelMismatch(m) => PlaybackReadError::ChannelCountChanged(m.got.max(1)),
        other => PlaybackReadError::Unknown(format!("pipeline write failed: {other:?}")),
    }
}

fn classify_next_packet_error(err: Error) -> Result<DecodeResult, PlaybackReadError> {
    match err {
        Error::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            Ok(DecodeResult::Eof)
        }
        Error::IoError(io) => {
            error!("I/O error while reading audio packets: {io}");
            Err(PlaybackReadError::DecodeFatal(format!("I/O error: {io}")))
        }
        other => {
            error!("error while reading audio packets: {other}");
            Err(PlaybackReadError::DecodeFatal(other.to_string()))
        }
    }
}

/// probe 失败时把 I/O 错误种类透出，好让扫描器区分瞬时读取失败与文件损坏。
fn map_probe_error(err: Error) -> OpenError {
    match err {
        Error::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            OpenError::UnsupportedFormat
        }
        Error::IoError(io) => OpenError::Io(io.kind()),
        _ => OpenError::UnsupportedFormat,
    }
}

/// LOOP_START / LOOP_END 的原始标签值是微秒；循环跳转定位时换算成秒。
const LOOP_TAG_MICROS_PER_SECOND: f64 = 1_000_000.0;

/// 把 symphonia 的标准标签映射到我们的标签模型；未收录的标准直接忽略。
fn map_standard_tag(std_tag: &StandardTag) -> Option<MetadataTag> {
    match std_tag {
        StandardTag::TrackTitle(s) => Some(MetadataTag::Name((**s).clone())),
        StandardTag::Artist(s) => Some(MetadataTag::Artist((**s).clone())),
        StandardTag::AlbumArtist(s) => Some(MetadataTag::AlbumArtist((**s).clone())),
        StandardTag::OriginalArtist(s) => Some(MetadataTag::OriginalArtist((**s).clone())),
        StandardTag::Composer(s) => Some(MetadataTag::Composer((**s).clone())),
        StandardTag::Album(s) => Some(MetadataTag::Album((**s).clone())),
        StandardTag::Genre(s) => Some(MetadataTag::Genre((**s).clone())),
        StandardTag::Grouping(s) => Some(MetadataTag::Grouping((**s).clone())),
        StandardTag::Bpm(n) => Some(MetadataTag::Bpm(*n)),
        StandardTag::CompilationFlag(b) => Some(MetadataTag::Compilation(*b)),
        StandardTag::ReleaseDate(s) => Some(MetadataTag::Date((**s).clone())),
        StandardTag::TrackNumber(n) => Some(MetadataTag::TrackNumber(n.to_string())),
        StandardTag::TrackTotal(n) => Some(MetadataTag::TrackTotal(*n)),
        StandardTag::DiscNumber(n) => Some(MetadataTag::DiscNumber(n.to_string())),
        StandardTag::DiscTotal(n) => Some(MetadataTag::DiscTotal(*n)),
        StandardTag::Label(s) => Some(MetadataTag::Label((**s).clone())),
        StandardTag::IdentCatalogNumber(s) => Some(MetadataTag::Catalog((**s).clone())),
        StandardTag::IdentIsrc(s) => Some(MetadataTag::Isrc((**s).clone())),
        StandardTag::SortAlbum(s) => Some(MetadataTag::SortAlbum((**s).clone())),
        StandardTag::SortAlbumArtist(s) => Some(MetadataTag::ArtistSort((**s).clone())),
        StandardTag::MusicBrainzAlbumId(s) => Some(MetadataTag::MbidAlbum((**s).clone())),
        StandardTag::Lyrics(s) => Some(MetadataTag::Lyrics((**s).clone())),
        StandardTag::ReplayGainTrackGain(s) => {
            Some(MetadataTag::ReplayGainTrackGain((**s).clone()))
        }
        StandardTag::ReplayGainTrackPeak(s) => {
            Some(MetadataTag::ReplayGainTrackPeak((**s).clone()))
        }
        StandardTag::ReplayGainAlbumGain(s) => {
            Some(MetadataTag::ReplayGainAlbumGain((**s).clone()))
        }
        StandardTag::ReplayGainAlbumPeak(s) => {
            Some(MetadataTag::ReplayGainAlbumPeak((**s).clone()))
        }
        StandardTag::DiscSubtitle(s) => Some(MetadataTag::DiscSubtitle((**s).clone())),
        _ => None,
    }
}

/// 把原始（非标准）标签键映射到我们的标签模型：ReplayGain/R128 一族、
/// MusicBrainz 专辑 id，以及循环点。未知键直接忽略。
fn map_raw_tag(tag: &Tag) -> Option<MetadataTag> {
    let key = tag.raw.key.trim_start_matches("TXXX:").to_ascii_lowercase();
    let value = &tag.raw.value;
    match key.as_str() {
        "replaygain_track_gain" => Some(MetadataTag::ReplayGainTrackGain(value.to_string())),
        "replaygain_track_peak" => Some(MetadataTag::ReplayGainTrackPeak(value.to_string())),
        "replaygain_album_gain" => Some(MetadataTag::ReplayGainAlbumGain(value.to_string())),
        "replaygain_album_peak" => Some(MetadataTag::ReplayGainAlbumPeak(value.to_string())),
        "r128_track_gain" => Some(MetadataTag::R128TrackGain(value.to_string())),
        "r128_album_gain" => Some(MetadataTag::R128AlbumGain(value.to_string())),
        "musicbrainz album id" => Some(MetadataTag::MbidAlbum(value.to_string())),
        "loop_start" => value
            .to_string()
            .parse::<f64>()
            .ok()
            .map(|v| MetadataTag::LoopStart(v / LOOP_TAG_MICROS_PER_SECOND)),
        "loop_end" => value
            .to_string()
            .parse::<f64>()
            .ok()
            .map(|v| MetadataTag::LoopEnd(v / LOOP_TAG_MICROS_PER_SECOND)),
        _ => None,
    }
}

#[derive(Default)]
pub struct SymphoniaProvider;

/// 一个已打开的 symphonia 文件。初始为空（`Default`）；由
/// [`SymphoniaProvider::open_source`] 填入 format reader 并读取首批元数据。
#[derive(Default)]
pub struct SymphoniaStream {
    format: Option<Box<dyn FormatReader>>,
    current_metadata: Metadata,
    current_track: u32,
    current_duration: u64,
    current_length: Option<u64>,
    current_position_ms: u64,
    current_timebase: Option<TimeBase>,
    decoder: Option<Box<dyn AudioDecoder>>,
    pending_metadata_update: bool,
    last_image: Option<Visual>,
    conversion_buffer: Vec<Vec<f64>>,
    looping: bool,
    loop_start_seconds: Option<f64>,
    loop_end_seconds: Option<f64>,
    pending_loop_seek: bool,
    needs_loop_start_trim: bool,
}

impl SymphoniaStream {
    /// 刻意写成自由函数：好让解码循环在 format reader（它持有元数据日志）
    /// 仍处于可变借用时，就能把流中途的元数据修订折叠进来。
    fn apply_tags(tags: &[Tag], metadata: &mut Metadata) {
        for tag in tags {
            let meta_tag = match &tag.std {
                Some(std_tag) => map_standard_tag(std_tag),
                None => map_raw_tag(tag),
            };
            if let Some(meta_tag) = meta_tag {
                apply_tag(meta_tag, metadata);
            }
        }
    }

    fn read_base_metadata(&mut self, format: &mut dyn FormatReader) {
        self.current_metadata = Metadata::default();
        self.last_image = None;

        let mut meta_queue = format.metadata();

        // 只有确实读到有用的内容时才更新元数据
        let found_metadata = if let Some(metadata) = meta_queue.skip_to_latest() {
            Self::apply_tags(&metadata.media.tags, &mut self.current_metadata);
            if !metadata.media.visuals.is_empty() {
                self.last_image = Some(metadata.media.visuals[0].clone());
            }
            !metadata.media.tags.is_empty() || !metadata.media.visuals.is_empty()
        } else {
            false
        };

        self.pending_metadata_update = found_metadata;
    }

    /// 第一个带编解码参数的轨道的音频参数。
    fn audio_params(&self) -> Result<&AudioCodecParameters, ChannelRetrievalError> {
        let format = self
            .format
            .as_ref()
            .ok_or(ChannelRetrievalError::InvalidState)?;
        let track = format
            .tracks()
            .iter()
            .find(|track| track.codec_params.is_some())
            .ok_or(ChannelRetrievalError::NothingToPlay)?;
        let codec_params = track.codec_params.as_ref().unwrap();
        codec_params
            .audio()
            .ok_or(ChannelRetrievalError::NothingToPlay)
    }

    fn loop_seek_if_pending(&mut self) -> Result<(), PlaybackReadError> {
        if !self.pending_loop_seek {
            return Ok(());
        }
        let Some(format) = self.format.as_mut() else {
            return Err(PlaybackReadError::InvalidState);
        };
        if let Some(loop_start) = self.loop_start_seconds
            && format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time: Time::try_from_secs_f64(loop_start).unwrap_or(Time::ZERO),
                        track_id: Some(self.current_track),
                    },
                )
                .is_err()
        {
            return Err(PlaybackReadError::Eof);
        }
        self.pending_loop_seek = false;
        self.needs_loop_start_trim = true;
        Ok(())
    }

    fn try_loop_on_eof(&mut self) -> bool {
        if self.looping && self.loop_start_seconds.is_some() {
            self.pending_loop_seek = true;
            true
        } else {
            false
        }
    }

    fn compute_loop_start_offset(
        loop_start_seconds: Option<f64>,
        timebase: Option<TimeBase>,
        packet_pts: Timestamp,
        rate: u32,
    ) -> usize {
        let (Some(loop_start), Some(tb)) = (loop_start_seconds, timebase) else {
            return 0;
        };
        let current_secs = tb
            .calc_time(packet_pts)
            .map(|t| t.as_secs_f64())
            .unwrap_or(0.0);
        if current_secs < loop_start {
            ((loop_start - current_secs) * rate as f64) as usize
        } else {
            0
        }
    }

    fn compute_loop_window(
        looping: bool,
        loop_end_seconds: Option<f64>,
        timebase: Option<TimeBase>,
        packet_pts: Timestamp,
        start_offset: usize,
        after_start: usize,
        rate: u32,
    ) -> (usize, bool) {
        if !looping {
            return (after_start, false);
        }
        let (Some(loop_end), Some(tb)) = (loop_end_seconds, timebase) else {
            return (after_start, false);
        };
        let current_secs = tb
            .calc_time(packet_pts)
            .map(|t| t.as_secs_f64())
            .unwrap_or(0.0);
        let frame_start = current_secs + start_offset as f64 / rate as f64;
        let frame_secs = after_start as f64 / rate as f64;
        if frame_start + frame_secs > loop_end {
            let keep = ((loop_end - frame_start).max(0.0) * rate as f64) as usize;
            (keep, true)
        } else {
            (after_start, false)
        }
    }
}

impl SymphoniaProvider {
    /// 探测任意可 seek 的字节源（本地文件、HTTP range 流……），
    /// 并在其之上构建 [`SymphoniaStream`]。
    pub fn open_source(
        &self,
        source: Box<dyn MediaSource>,
        ext: Option<&OsStr>,
    ) -> Result<Box<dyn MediaStream>, OpenError> {
        let mss = MediaSourceStream::new(source, Default::default());
        let meta_opts = MetadataOptions::default();
        let fmt_opts = FormatOptions::default();

        let mut hint = Hint::new();
        if let Some(ext) = ext.and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let mut format: Box<dyn FormatReader> = symphonia::default::get_probe()
            .probe(&hint, mss, fmt_opts, meta_opts)
            .map_err(map_probe_error)?;

        let mut stream = SymphoniaStream::default();
        stream.read_base_metadata(&mut *format);
        stream.format = Some(format);

        Ok(Box::new(stream))
    }
}

impl MediaProvider for SymphoniaProvider {
    fn open(&self, file: File, ext: Option<&OsStr>) -> Result<Box<dyn MediaStream>, OpenError> {
        self.open_source(Box::new(file), ext)
    }

    fn supported_extensions(&self) -> &[&str] {
        super::SUPPORTED_EXTENSIONS
    }

    fn supported_features(&self) -> MediaProviderFeatures {
        MediaProviderFeatures::ALLOWS_INDEXING
            | MediaProviderFeatures::PROVIDES_DECODER
            | MediaProviderFeatures::PROVIDES_METADATA
    }
}

impl MediaStream for SymphoniaStream {
    fn close(&mut self) {
        self.stop_playback();
        self.current_metadata = Metadata::default();
        self.format = None;
    }

    fn start_playback(&mut self) -> Result<(), PlaybackStartError> {
        let Some(format) = &self.format else {
            return Err(PlaybackStartError::InvalidState);
        };
        let track = format
            .first_track_known_codec(TrackType::Audio)
            .ok_or(PlaybackStartError::NothingToPlay)?;

        let codec_params = track
            .codec_params
            .as_ref()
            .ok_or(PlaybackStartError::NothingToPlay)?;
        let audio_params = codec_params
            .audio()
            .ok_or(PlaybackStartError::NothingToPlay)?;

        // 对我们打开的所有格式，timebase 在 probe 时就是已知的（容器没给时
        // 由采样率推导），而 `num_frames` 不是：OGG/Opus 要读到最后一页才知道。
        // 过去把两者绑定判断，会让 OGG/Opus 的 `current_timebase` 一直为空，
        // 于是 packet 的 PTS 永远无法换算，播放位置恒为 0。
        self.current_timebase = track.time_base;
        if let (Some(frame_count), Some(tb)) = (track.num_frames, track.time_base)
            && let Some(t) = tb.calc_time(Timestamp::new(frame_count as i64))
        {
            self.current_length = Some(time_to_millis(t));
        }

        let channel_count = audio_params
            .channels
            .as_ref()
            .map(|c| c.count())
            .unwrap_or(2);
        let frame_capacity = audio_params.max_frames_per_packet.unwrap_or(8192) as usize;
        // 首次解码之前先报出最坏 packet 大小：管线按 `frame_duration()`
        // （见 setup_pipeline）来定解码环的尺寸，于是一个完整的大 packet 装得下。
        // 第一个被解码的 packet 会用解码器的真实容量覆盖这个值。
        self.current_duration = frame_capacity as u64;

        self.conversion_buffer = (0..channel_count)
            .map(|_| Vec::with_capacity(frame_capacity))
            .collect();

        self.current_track = track.id;

        let dec_opts = AudioDecoderOptions::default();
        self.decoder = Some({
            let mut codecs = CodecRegistry::new();
            codecs.register_audio_decoder::<MpaDecoder>();
            codecs.register_audio_decoder::<PcmDecoder>();
            codecs.register_audio_decoder::<AlacDecoder>();
            codecs.register_audio_decoder::<FlacDecoder>();
            codecs.register_audio_decoder::<VorbisDecoder>();
            codecs.register_audio_decoder::<AdpcmDecoder>();
            codecs.register_audio_decoder::<OpusDecoder>();
            codecs.register_audio_decoder::<AacDecoder>();

            codecs
                .make_audio_decoder(audio_params, &dec_opts)
                .map_err(|_| PlaybackStartError::Undecodable)?
        });

        Ok(())
    }

    fn stop_playback(&mut self) {
        self.current_track = 0;
        self.decoder = None;
    }

    fn frame_duration(&self) -> Result<u64, FrameDurationError> {
        if self.decoder.is_none() || self.current_duration == 0 {
            return Err(FrameDurationError::NeverStarted);
        }
        Ok(self.current_duration)
    }

    fn read_metadata(&mut self) -> Result<Metadata, MetadataError> {
        self.pending_metadata_update = false;

        if self.format.is_none() {
            return Err(MetadataError::InvalidState);
        }
        // 是克隆而非取走——流中标签更新时播放侧会重新读取元数据
        Ok(self.current_metadata.clone())
    }

    fn metadata_updated(&self) -> bool {
        self.pending_metadata_update
    }

    fn read_image(&mut self) -> Result<Option<Box<[u8]>>, MetadataError> {
        if self.format.is_none() {
            return Err(MetadataError::InvalidState);
        }
        // 封面图只交付一次：take 在返回的同时把它清掉
        Ok(self.last_image.take().map(|visual| visual.data))
    }

    fn duration_ms(&self) -> Result<u64, TrackDurationError> {
        if self.decoder.is_none() {
            return Err(TrackDurationError::NeverStarted);
        }
        self.current_length.ok_or(TrackDurationError::NeverStarted)
    }

    fn position_ms(&self) -> Result<u64, TrackDurationError> {
        // 位置只要求解码器已在运行：若还要求时长已知，那些帧数到 EOF 才确定
        // 的格式（OGG/Opus）明明 PTS 换算正常，却整首都报不出位置。
        if self.decoder.is_none() {
            return Err(TrackDurationError::NeverStarted);
        }
        Ok(self.current_position_ms)
    }

    fn seek(&mut self, time: f64) -> Result<(), SeekError> {
        let timebase = self.current_timebase;
        let Some(format) = &mut self.format else {
            return Err(SeekError::InvalidState);
        };

        self.pending_loop_seek = false;
        self.needs_loop_start_trim = false;

        let seek = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::try_from_secs_f64(time).unwrap_or(Time::ZERO),
                    track_id: None,
                },
            )
            .map_err(|e| SeekError::Unknown(e.to_string()))?;

        if let Some(timebase) = timebase
            && let Some(t) = timebase.calc_time(seek.actual_ts)
        {
            self.current_position_ms = time_to_millis(t);
        }

        Ok(())
    }

    fn channels(&self) -> Result<ChannelSpec, ChannelRetrievalError> {
        use symphonia::core::audio::{ChannelLabel as SymLabel, Channels as SymChannels};

        let audio_params = self.audio_params()?;
        let sym_channels = audio_params.channels.clone().unwrap_or(SymChannels::None);

        let fallback_discrete =
            |index: usize| ChannelLabel::Discrete(index.min(usize::from(u16::MAX)) as u16);

        let spec = match sym_channels {
            SymChannels::Positioned(pos) => match ChannelPosition::from_bits(pos.bits()) {
                Some(position) => ChannelSpec::Layout(ChannelLayout::Positioned(position)),
                None => ChannelSpec::Count(pos.bits().count_ones() as u16),
            },
            SymChannels::Discrete(n) => ChannelSpec::Layout(ChannelLayout::Discrete(n)),
            SymChannels::Custom(labels) => {
                let our_labels: Vec<ChannelLabel> = labels
                    .iter()
                    .enumerate()
                    .map(|(index, label)| match label {
                        SymLabel::Positioned(p) => ChannelPosition::from_bits(p.bits())
                            .filter(|position| position.bits().is_power_of_two())
                            .map(ChannelLabel::Positioned)
                            .unwrap_or_else(|| fallback_discrete(index)),
                        SymLabel::Discrete(n) | SymLabel::Ambisonic(n) => {
                            ChannelLabel::Discrete(*n)
                        }
                        _ => fallback_discrete(index),
                    })
                    .collect();
                let layout = crate::devices::mix::layout_from_labels(our_labels);
                ChannelSpec::Layout(layout)
            }
            SymChannels::Ambisonic(order) => {
                let count = (1 + usize::from(order)) * (1 + usize::from(order));
                ChannelSpec::Count(count as u16)
            }
            _ => ChannelSpec::Count(2),
        };

        Ok(spec)
    }

    fn sample_rate(&self) -> Result<u32, ChannelRetrievalError> {
        let audio_params = self.audio_params()?;
        audio_params
            .sample_rate
            .ok_or(ChannelRetrievalError::NothingToPlay)
    }

    fn decode_into(
        &mut self,
        output: &mut ChannelProducers,
    ) -> Result<DecodeResult, PlaybackReadError> {
        if self.format.is_none() {
            return Err(PlaybackReadError::InvalidState);
        }

        // 畸形文件可能每个 packet 都失败；出错就提前返回，
        // 不要再空转整条流却始终产不出音频
        const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 256;
        let mut consecutive_decode_errors = 0u32;

        loop {
            self.loop_seek_if_pending()?;

            let format = self.format.as_mut().expect("format presence checked above");

            let packet = match next_packet(format.as_mut()) {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    if self.try_loop_on_eof() {
                        continue;
                    }
                    return Ok(DecodeResult::Eof);
                }
                Err(err) => return classify_next_packet_error(err),
            };

            // demuxer 可能在流中途推入新的元数据修订（例如链式 OGG 流宣告新标签）。
            // 常见情形只花一次 len 比较；发生修订变化时消费掉日志，把最新修订的标签
            // 折叠进当前元数据，并让播放侧经 check_metadata_update 重新读取。
            if !format.metadata().is_latest() {
                let mut meta_queue = format.metadata();
                if let Some(revision) = meta_queue.skip_to_latest() {
                    Self::apply_tags(&revision.media.tags, &mut self.current_metadata);
                    if !revision.media.visuals.is_empty() {
                        self.last_image = Some(revision.media.visuals[0].clone());
                    }
                    self.pending_metadata_update = true;
                }
            }

            if packet.track_id != self.current_track {
                continue;
            }

            let Some(decoder) = &mut self.decoder else {
                return Err(PlaybackReadError::NeverStarted);
            };

            match symphonia_alloc_exempt(|| decoder.decode(&packet)) {
                Ok(decoded) => {
                    consecutive_decode_errors = 0;
                    let spec = decoded.spec();
                    let rate = spec.rate();
                    let channel_count = spec.channels().count();
                    self.current_duration = decoded.capacity() as u64;

                    if let Some(tb) = &self.current_timebase
                        && let Some(t) = tb.calc_time(packet.pts)
                    {
                        self.current_position_ms = time_to_millis(t);
                    }

                    let start_offset = if self.needs_loop_start_trim {
                        self.needs_loop_start_trim = false;
                        Self::compute_loop_start_offset(
                            self.loop_start_seconds,
                            self.current_timebase,
                            packet.pts,
                            rate,
                        )
                    } else {
                        0
                    };

                    let after_start = decoded.frames().saturating_sub(start_offset);
                    if after_start == 0 {
                        continue;
                    }

                    let (max_samples, needs_loop_seek) = Self::compute_loop_window(
                        self.looping,
                        self.loop_end_seconds,
                        self.current_timebase,
                        packet.pts,
                        start_offset,
                        after_start,
                        rate,
                    );

                    if needs_loop_seek && max_samples == 0 {
                        self.pending_loop_seek = true;
                        continue;
                    }

                    if channel_count != output.channel_count() {
                        return Err(PlaybackReadError::ChannelCountChanged(channel_count));
                    }

                    // 提示值有时会不准，要以实际容量为准校验
                    let frame_capacity = decoded.capacity();
                    while self.conversion_buffer.len() < channel_count {
                        self.conversion_buffer
                            .push(Vec::with_capacity(frame_capacity));
                    }

                    for buf in &mut self.conversion_buffer[..channel_count] {
                        buf.clear();
                        if buf.capacity() < frame_capacity {
                            buf.reserve(frame_capacity);
                        }
                    }

                    macro_rules! convert_chan {
                        ($v:ident, $convert:expr) => {{
                            for ch in 0..channel_count {
                                if let Some(plane) = $v.plane(ch) {
                                    self.conversion_buffer[ch].extend(
                                        plane
                                            .iter()
                                            .skip(start_offset)
                                            .take(max_samples)
                                            .map($convert),
                                    );
                                }
                            }
                        }};
                    }

                    match decoded {
                        GenericAudioBufferRef::U8(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::U16(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::U24(v) => {
                            convert_chan!(v, |s| u24_saturating(s.0).sample_into())
                        }
                        GenericAudioBufferRef::U32(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::S8(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::S16(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::S24(v) => {
                            convert_chan!(v, |s| i24_saturating(s.0).sample_into())
                        }
                        GenericAudioBufferRef::S32(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::F32(v) => convert_chan!(v, |&s| s.sample_into()),
                        GenericAudioBufferRef::F64(v) => {
                            let counts: SmallVec<[&[f64]; 8]> = (0..channel_count)
                                .filter_map(|ch| {
                                    v.plane(ch).map(|plane| {
                                        &plane[start_offset..start_offset + max_samples]
                                    })
                                })
                                .collect();
                            // 非阻塞：consumer（重采样器）在同一个播放线程上抽取，
                            // 阻塞式重试写入会对着一个「只有等我们返回才能运行」的
                            // consumer 空转
                            if let Err(e) = output.write_slices_nonblocking(&counts) {
                                return Err(map_write_error(e));
                            }
                            if needs_loop_seek {
                                self.pending_loop_seek = true;
                            }
                            return Ok(DecodeResult::Decoded {
                                frames: max_samples,
                                rate,
                            });
                        }
                    }

                    if let Err(e) =
                        output.write_vecs_nonblocking(&self.conversion_buffer[..channel_count])
                    {
                        return Err(map_write_error(e));
                    }

                    if needs_loop_seek {
                        self.pending_loop_seek = true;
                    }

                    return Ok(DecodeResult::Decoded {
                        frames: max_samples,
                        rate,
                    });
                }
                Err(Error::IoError(_)) | Err(Error::DecodeError(_)) => {
                    consecutive_decode_errors += 1;
                    if consecutive_decode_errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
                        return Err(PlaybackReadError::DecodeFatal(
                            "too many consecutive decode errors".to_string(),
                        ));
                    }
                    continue;
                }
                Err(e) => {
                    return Err(PlaybackReadError::DecodeFatal(e.to_string()));
                }
            }
        }
    }

    fn set_looping(&mut self, enabled: bool) {
        self.looping = enabled;
        self.pending_loop_seek = false;
        self.needs_loop_start_trim = false;
        if enabled {
            self.loop_start_seconds = self.current_metadata.loop_start;
            self.loop_end_seconds = self.current_metadata.loop_end;
        } else {
            self.loop_start_seconds = None;
            self.loop_end_seconds = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn map_probe_error_treats_truncated_file_as_corrupt() {
        let err = Error::IoError(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "eof",
        ));
        assert_eq!(map_probe_error(err), OpenError::UnsupportedFormat);
    }

    #[test]
    fn map_probe_error_preserves_io_kind() {
        let err = Error::IoError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert_eq!(
            map_probe_error(err),
            OpenError::Io(std::io::ErrorKind::PermissionDenied)
        );
    }

    fn open_fixture(dir: &TestDir, name: &str) -> Box<dyn MediaStream> {
        let path = dir.join(name);
        std::fs::write(&path, crate::test_support::audio_fixtures::fixture(name)).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        SymphoniaProvider.open(file, path.extension()).unwrap()
    }

    #[test]
    fn flagged_metadata_update_only_when_metadata_was_read() {
        // Symphonia 对 WAV 不暴露任何标签（它的 RIFF reader 从不挂元数据日志），
        // 因此打开 WAV 不能标记为有更新：发布空元数据会把 UI 已经从音乐库
        // 或其它 provider 拿到的更好的元数据冲掉
        let dir = TestDir::new("symphonia-fixture-test");
        assert!(!open_fixture(&dir, "fixture.wav").metadata_updated());
        assert!(open_fixture(&dir, "fixture.flac").metadata_updated());
    }
}
