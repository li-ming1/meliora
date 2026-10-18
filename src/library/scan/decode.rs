use crate::library::scan::discover::{SidecarLyrics, sidecar_lyrics_path};
use std::{io::Cursor, sync::Arc};

use camino::Utf8Path;
use image::{DynamicImage, EncodableLayout, codecs::jpeg::JpegEncoder, imageops};
use xxhash_rust::xxh3::xxh3_64;

use crate::media::{
    errors::OpenError, lookup_table::try_open_media, metadata::Metadata,
    traits::MediaProviderFeatures,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtSource {
    Embedded,
    /// 曲目旁边的文件夹图片。排名数值小的胜出。
    Folder(u8),
}

impl ArtSource {
    pub(crate) fn db_value(self) -> i64 {
        match self {
            ArtSource::Embedded => 0,
            ArtSource::Folder(rank) => rank as i64,
        }
    }
}

// 同时存在多张可识别的文件夹图片时，排名数值小的胜出
const RANK_COVER: u8 = 1;
const RANK_FOLDER: u8 = 2;
const RANK_FRONT: u8 = 3;

/// 被认作文件夹封面的图片扩展名，匹配时忽略大小写。
const FOLDER_ART_EXTENSIONS: [&str; 3] = ["jpg", "jpeg", "png"];

pub(crate) fn folder_art_rank(stem: &str) -> Option<u8> {
    match stem.to_ascii_lowercase().as_str() {
        "cover" => Some(RANK_COVER),
        "folder" => Some(RANK_FOLDER),
        "front" => Some(RANK_FRONT),
        _ => None,
    }
}

/// 扩展名与文件名片段都表明它是文件夹封面时给出排名，否则 `None`。
pub(crate) fn is_folder_art_name(stem: &str, extension: &str) -> Option<u8> {
    if FOLDER_ART_EXTENSIONS
        .iter()
        .any(|supported| extension.eq_ignore_ascii_case(supported))
    {
        folder_art_rank(stem)
    } else {
        None
    }
}

/// 跳过隐藏/系统文件（属性位取自已读取的元数据，不额外 stat）。
/// Windows 上常会残留过期的 Folder.jpg。
pub(crate) fn is_hidden_file(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};

        const HIDDEN_OR_SYSTEM: u32 = FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0;
        metadata.file_attributes() & HIDDEN_OR_SYSTEM != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[derive(Debug)]
pub enum RawArt {
    Owned(Box<[u8]>),
    Shared(Arc<Vec<u8>>),
}

impl AsRef<[u8]> for RawArt {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

#[derive(Debug)]
pub enum ProcessedImage {
    Owned(Vec<u8>),
    Shared(Arc<Vec<u8>>),
}

impl AsRef<[u8]> for ProcessedImage {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

#[derive(Debug)]
pub struct ProcessedArt {
    pub image: ProcessedImage,
    pub thumb: Vec<u8>,
}

#[derive(Debug)]
pub struct ScannedArt {
    pub raw: Option<RawArt>,
    pub processed: Option<Arc<ProcessedArt>>,
    pub hash: u64,
    pub source: ArtSource,
}

impl ScannedArt {
    fn embedded(bytes: Box<[u8]>) -> Self {
        let hash = xxh3_64(&bytes);
        ScannedArt {
            raw: Some(RawArt::Owned(bytes)),
            processed: None,
            hash,
            source: ArtSource::Embedded,
        }
    }

    pub(crate) fn folder(bytes: Arc<Vec<u8>>, rank: u8) -> Self {
        Self {
            hash: xxh3_64(&bytes),
            raw: Some(RawArt::Shared(bytes)),
            processed: None,
            source: ArtSource::Folder(rank),
        }
    }
}

#[derive(Debug, Default)]
pub struct FileArt {
    pub embedded: Option<ScannedArt>,
    pub folder: Option<ScannedArt>,
    /// 该文件夹是否已检查过封面（首张/代表性曲目）。
    pub representative: bool,
}

/// 一个被扫描的文件：`(元数据, 曲目时长秒数, 目前发现的封面)`。
pub type FileInformation = (Metadata, u64, FileArt);

/// 一次失败的文件读取该如何更新扫描记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanReadError {
    /// 文件在发现之后消失了——从记录里删掉。
    Missing,
    /// 临时性错误（例如文件被锁）——不写入记录，好让下一次扫描重试。
    Transient,
    /// 损坏或读不了的码流——写入记录，直到该文件 mtime 变化。
    Corrupt,
}

fn classify_io_kind(kind: std::io::ErrorKind) -> ScanReadError {
    match kind {
        std::io::ErrorKind::NotFound => ScanReadError::Missing,
        _ => ScanReadError::Transient,
    }
}

fn classify_open_error(e: &anyhow::Error) -> ScanReadError {
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return classify_io_kind(io.kind());
    }
    match e.downcast_ref::<OpenError>() {
        Some(OpenError::Io(kind)) => classify_io_kind(*kind),
        Some(_) => ScanReadError::Corrupt,
        None => ScanReadError::Transient,
    }
}

fn duration_ms_or_else(
    hint: Option<u64>,
    fallback: impl FnOnce() -> Result<u64, ScanReadError>,
) -> Result<u64, ScanReadError> {
    match hint.filter(|duration| *duration > 0) {
        Some(duration) => Ok(duration),
        None => fallback(),
    }
}

fn scan_path(path: &Utf8Path) -> Result<FileInformation, ScanReadError> {
    let mut stream = try_open_media(
        path.as_std_path(),
        MediaProviderFeatures::PROVIDES_METADATA | MediaProviderFeatures::ALLOWS_INDEXING,
    )
    .map_err(|e| classify_open_error(&e))?
    // 不支持的格式或 provider 缺失——按损坏处理，免得每次扫描都重试
    .ok_or(ScanReadError::Corrupt)?;
    stream
        .start_playback()
        .map_err(|_| ScanReadError::Corrupt)?;
    let metadata = stream.read_metadata().map_err(|_| ScanReadError::Corrupt)?;
    let image = stream.read_image().map_err(|_| ScanReadError::Corrupt)?;
    let duration_hint = stream.duration_ms().ok();

    stream.close();

    let duration_ms = duration_ms_or_else(duration_hint, || {
        // 元数据里没有时长的 provider 仍需跑一遍解码器
        let mut decoder =
            try_open_media(path.as_std_path(), MediaProviderFeatures::PROVIDES_DECODER)
                .map_err(|e| classify_open_error(&e))?
                .ok_or(ScanReadError::Corrupt)?;
        decoder
            .start_playback()
            .map_err(|_| ScanReadError::Corrupt)?;
        let duration = decoder.duration_ms().map_err(|_| ScanReadError::Corrupt)?;
        decoder.close();
        Ok(duration)
    })?;
    let len = duration_ms / 1_000;

    let art = FileArt {
        embedded: image.map(ScannedArt::embedded),
        folder: None,
        representative: false,
    };
    Ok((metadata, len, art))
}

fn resolve_lyrics(
    path: &Utf8Path,
    sidecar_lyrics: SidecarLyrics,
    embedded_lyrics: Option<String>,
) -> Option<String> {
    let sidecar_lyrics = match sidecar_lyrics {
        // 目录列举已证明 sidecar 不存在——跳过那次探测
        SidecarLyrics::Absent => None,
        SidecarLyrics::Unknown | SidecarLyrics::Present => sidecar_lyrics_path(path)
            .and_then(|lrc_path| std::fs::read_to_string(lrc_path).ok())
            .filter(|content| !content.trim().is_empty()),
    };

    sidecar_lyrics.or(embedded_lyrics)
}

enum SourceImage {
    Owned(RawArt),
}

impl SourceImage {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(raw) => raw.as_ref(),
        }
    }

    fn into_processed(self) -> ProcessedImage {
        match self {
            Self::Owned(RawArt::Owned(bytes)) => ProcessedImage::Owned(bytes.into_vec()),
            Self::Owned(RawArt::Shared(bytes)) => ProcessedImage::Shared(bytes),
        }
    }
}

/// 超过这个边长的全尺寸封面会被重缩放，更小的原样保留。
const MAX_ART_DIMENSION: u32 = 1024;
/// 与全尺寸图一并生成的方形 BMP 缩略图边长。
const THUMBNAIL_SIZE: u32 = 70;
/// 重缩放后全尺寸图的 JPEG 质量（仅在图被缩小时用到）。
const RESIZED_JPEG_QUALITY: u8 = 70;

fn process_source_image(image: SourceImage) -> anyhow::Result<(ProcessedImage, Vec<u8>)> {
    let decoded = image::ImageReader::new(Cursor::new(image.bytes()))
        .with_guessed_format()?
        .decode()?
        .into_rgb8();

    let thumb_rgb = imageops::thumbnail(&decoded, THUMBNAIL_SIZE, THUMBNAIL_SIZE);

    let mut thumb_buf: Vec<u8> = Vec::new();
    DynamicImage::ImageRgb8(thumb_rgb)
        .write_to(&mut Cursor::new(&mut thumb_buf), image::ImageFormat::Bmp)?;

    // 小图原样放过，大图缩到能装进 1024x1024
    let (w, h) = decoded.dimensions();
    let resized = if w <= MAX_ART_DIMENSION && h <= MAX_ART_DIMENSION {
        image.into_processed()
    } else {
        let scale = MAX_ART_DIMENSION as f32 / (w.max(h) as f32);
        let new_w = (w as f32 * scale).round().max(1.0) as u32;
        let new_h = (h as f32 * scale).round().max(1.0) as u32;

        let resized_img = imageops::resize(
            &decoded,
            new_w,
            new_h,
            image::imageops::FilterType::Lanczos3,
        );
        let mut buf: Vec<u8> = Vec::new();
        JpegEncoder::new_with_quality(&mut buf, RESIZED_JPEG_QUALITY).encode(
            resized_img.as_bytes(),
            resized_img.width(),
            resized_img.height(),
            image::ExtendedColorType::Rgb8,
        )?;

        ProcessedImage::Owned(buf)
    };

    Ok((resized, thumb_buf))
}

/// 处理封面；不需要缩放时复用它的原始分配。
pub fn process_owned_album_art(image: RawArt) -> anyhow::Result<(ProcessedImage, Vec<u8>)> {
    process_source_image(SourceImage::Owned(image))
}

/// 读取元数据与内嵌封面，并标出适合被指派文件夹封面的曲目。
pub fn read_metadata_for_path(
    path: &Utf8Path,
    sidecar_lyrics: SidecarLyrics,
) -> Result<FileInformation, ScanReadError> {
    let (mut metadata, len, mut art) = scan_path(path)?;

    let is_representative = metadata.track_current.is_none_or(|t| matches!(t, 0 | 1))
        && metadata.disc_current.is_none_or(|d| matches!(d, 0 | 1));
    art.representative = is_representative;

    metadata.lyrics = resolve_lyrics(path, sidecar_lyrics, metadata.lyrics.take());

    Ok((metadata, len, art))
}

#[cfg(test)]
#[path = "decode/tests.rs"]
mod tests;
