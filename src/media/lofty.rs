use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Seek},
};

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::id3::v2::Id3v2Version;
use lofty::picture::PictureType;
use lofty::prelude::ItemKey;
use lofty::tag::{ItemValue, Tag, TagItem, TagType};

use crate::media::text::token_key;
use crate::media::{
    errors::{
        ChannelRetrievalError, FrameDurationError, MetadataError, OpenError, PlaybackReadError,
        PlaybackStartError, SeekError, TrackDurationError,
    },
    metadata::{Metadata, MetadataTag, apply_tag},
    pipeline::{ChannelProducers, DecodeResult},
    traits::{MediaProvider, MediaProviderFeatures, MediaStream},
};

fn item_value_to_string(value: &ItemValue) -> Option<String> {
    let value = match value {
        ItemValue::Text(s) | ItemValue::Locator(s) => Some(s.as_str()),
        ItemValue::Binary(v) => str::from_utf8(v).ok(),
    };
    let value = value?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn item_value_to_bool(value: &ItemValue) -> Option<bool> {
    match value {
        ItemValue::Text(v) => Some(matches!(v.as_str(), "1" | "true")),
        _ => None,
    }
}

fn item_value_to_u64(value: &ItemValue) -> Option<u64> {
    match value {
        ItemValue::Text(v) => v.parse().ok(),
        // 试着按无符号整数解析，可能解不出来，但总比没有强
        ItemValue::Binary(v) => match v.len() {
            0 => None,
            2 => Some(u16::from_ne_bytes(v[0..2].try_into().ok()?) as u64),
            4 => Some(u32::from_ne_bytes(v[0..4].try_into().ok()?) as u64),
            8 => Some(u64::from_ne_bytes(v[0..8].try_into().ok()?)),
            _ => None,
        },
        _ => None,
    }
}

/// 标签里有正面封面就取它，否则取第一张任意类型的图片。
fn extract_cover(tag: &Tag) -> Option<Box<[u8]>> {
    let picture = tag
        .pictures()
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    Some(picture.data().to_vec().into_boxed_slice())
}

fn map_standard_tag(item: &TagItem) -> Option<MetadataTag> {
    let value = item.value();

    match item.key() {
        ItemKey::TrackTitle => item_value_to_string(value).map(MetadataTag::Name),
        ItemKey::TrackArtist => item_value_to_string(value).map(MetadataTag::Artist),
        ItemKey::TrackArtists => item_value_to_string(value).map(MetadataTag::Artists),
        ItemKey::AlbumArtist => item_value_to_string(value).map(MetadataTag::AlbumArtist),
        ItemKey::AlbumArtists => item_value_to_string(value).map(MetadataTag::AlbumArtists),
        ItemKey::OriginalArtist => item_value_to_string(value).map(MetadataTag::OriginalArtist),
        ItemKey::Composer => item_value_to_string(value).map(MetadataTag::Composer),
        ItemKey::AlbumTitle => item_value_to_string(value).map(MetadataTag::Album),
        ItemKey::Genre => item_value_to_string(value).map(MetadataTag::Genre),
        ItemKey::ContentGroup => item_value_to_string(value).map(MetadataTag::Grouping),
        ItemKey::Bpm => item_value_to_u64(value).map(MetadataTag::Bpm),
        ItemKey::FlagCompilation => item_value_to_bool(value).map(MetadataTag::Compilation),
        ItemKey::TrackNumber => item_value_to_string(value).map(MetadataTag::TrackNumber),
        ItemKey::TrackTotal => item_value_to_u64(value).map(MetadataTag::TrackTotal),
        ItemKey::DiscNumber => item_value_to_string(value).map(MetadataTag::DiscNumber),
        ItemKey::DiscTotal => item_value_to_u64(value).map(MetadataTag::DiscTotal),
        ItemKey::Year
        | ItemKey::RecordingDate
        | ItemKey::ReleaseDate
        | ItemKey::OriginalReleaseDate => item_value_to_string(value).map(MetadataTag::Date),
        ItemKey::Label => item_value_to_string(value).map(MetadataTag::Label),
        ItemKey::CatalogNumber => item_value_to_string(value).map(MetadataTag::Catalog),
        ItemKey::Isrc => item_value_to_string(value).map(MetadataTag::Isrc),
        ItemKey::AlbumTitleSortOrder => item_value_to_string(value).map(MetadataTag::SortAlbum),
        ItemKey::AlbumArtistSortOrder => {
            item_value_to_string(value).map(MetadataTag::AlbumArtistSort)
        }
        ItemKey::TrackArtistSortOrder => item_value_to_string(value).map(MetadataTag::ArtistSort),
        ItemKey::MusicBrainzReleaseId => item_value_to_string(value).map(MetadataTag::MbidAlbum),
        ItemKey::Lyrics => item_value_to_string(value).map(MetadataTag::Lyrics),
        ItemKey::ReplayGainTrackGain => {
            item_value_to_string(value).map(MetadataTag::ReplayGainTrackGain)
        }
        ItemKey::ReplayGainTrackPeak => {
            item_value_to_string(value).map(MetadataTag::ReplayGainTrackPeak)
        }
        ItemKey::ReplayGainAlbumGain => {
            item_value_to_string(value).map(MetadataTag::ReplayGainAlbumGain)
        }
        ItemKey::ReplayGainAlbumPeak => {
            item_value_to_string(value).map(MetadataTag::ReplayGainAlbumPeak)
        }
        ItemKey::SetSubtitle => item_value_to_string(value).map(MetadataTag::DiscSubtitle),
        _ => None,
    }
}

struct TagsFromFile {
    tagged_file: TaggedFile,
    metadata: Metadata,
    duration: Option<u64>,
}

/// 需要 ID3v2 版本，是因为 v2.3 标签会把多个名字用 `/` 挤进一个值；
/// 统一的标签视图会丢掉版本号，所以必须从文件里把它取回来。位于偏移 0 的
/// 标签（MPEG/AAC 的常见布局）靠嗅探 10 字节头回答，而不是重解析整个文件；
/// 其余情形（藏在 AIFF/WAV 的 RIFF/FORM 魔数之后的 ID3 chunk、标签前的垃圾
/// 字节、不受支持的 v2.2 头）回退到关掉 properties 的完整 lofty 解析，
/// 于是返回值与那次解析本会报告的完全一致。
fn read_id3v2_version(file: &mut File, file_type: FileType) -> Option<Id3v2Version> {
    use lofty::aac::AacFile;
    use lofty::iff::{aiff::AiffFile, wav::WavFile};
    use lofty::mpeg::MpegFile;

    // 不以自身 ID3 chunk 开头的容器永远走不到下面的头嗅探，所以一律走
    // 重解析回退；其它文件类型本来就不做重解析，直接报 None。
    if matches!(file_type, FileType::Mpeg | FileType::Aac) {
        file.rewind().ok()?;
        let mut header = [0u8; 10];
        let header_len = file.read(&mut header).ok()?;
        if header_len >= 4 && &header[..3] == b"ID3" {
            match header[3] {
                3 => return Some(Id3v2Version::V3),
                4 => return Some(Id3v2Version::V4),
                _ => {}
            }
        }
    }

    // 开头没有 ID3v2 头（或版本不受支持）；标签仍可能住在容器的别处。
    // 按原来的方式重新解析一次。
    file.rewind().ok()?;
    let options = ParseOptions::new().read_properties(false);

    macro_rules! read_version {
        ($file_ty:ty) => {
            <$file_ty>::read_from(file, options)
                .ok()
                .and_then(|f| f.id3v2().map(|tag| tag.original_version()))
        };
    }

    match file_type {
        FileType::Mpeg => read_version!(MpegFile),
        FileType::Aac => read_version!(AacFile),
        FileType::Aiff => read_version!(AiffFile),
        FileType::Wav => read_version!(WavFile),
        _ => None,
    }
}

/// 把原始的 artist 值拆成名字列表：每个值一项；只有 v2.3 时代的标签例外，
/// 它们用 `/` 把多个名字挤进一个值。
fn split_artist_names(value: &str, split_slash: bool) -> Vec<String> {
    let raw: Vec<&str> = if split_slash {
        value.split('/').collect()
    } else {
        vec![value]
    };
    raw.into_iter()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// 往一个拼接列表字段追加一个名字，重复的只加一次。一个值本身就可能用同一个
/// 分隔符拼了好几个名字（多个标签系统都会写 "A, B" 这种多歌手标签），
/// 所以它的每一部分都各自与已在场的部分去重。
fn push_unique_name(field: &mut Option<String>, name: &str, separator: &str) {
    let parts: Vec<&str> = name
        .split(separator)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return;
    }
    let existing: Vec<&str> = field
        .as_deref()
        .map(|value| value.split(separator).collect())
        .unwrap_or_default();
    let missing: Vec<&str> = parts
        .into_iter()
        .filter(|part| !existing.contains(part))
        .collect();
    if missing.is_empty() {
        return;
    }
    let missing = missing.join(separator);
    if let Some(existing) = field {
        existing.push_str(separator);
        existing.push_str(&missing);
    } else {
        *field = Some(missing);
    }
}

/// 应用标签期间收集到的歌手名。`tpe1` 与展示字段同步，`artists_tag` 存
/// ARTISTS 标签的逐条署名，`album_artists_tag` 存 ALBUMARTISTS 标签的逐条署名。
#[derive(Default)]
struct ArtistNames {
    tpe1: Vec<String>,
    artists_tag: Vec<String>,
    album_artists_tag: Vec<String>,
}

fn push_unique_vec(names: &mut (impl AsRef<[String]> + Extend<String>), name: &str) {
    if !name.is_empty() && !names.as_ref().iter().any(|n| n == name) {
        names.extend([name.to_string()]);
    }
}

fn apply_tag_items(
    tag: &Tag,
    split_artists: bool,
    metadata: &mut Metadata,
    artist_names: &mut ArtistNames,
) {
    for item in tag.items() {
        let Some(meta_tag) = map_standard_tag(item) else {
            continue;
        };
        match meta_tag {
            // 不按 `/` 拆分——v2.3 文件的挤名不可靠，而字面斜杠很常见
            // （AC/DC）
            MetadataTag::Artist(value) => {
                push_unique_name(&mut metadata.artist, &value, ", ");
                push_unique_vec(&mut artist_names.tpe1, value.trim());
            }
            // 展示值用 ", " 拼接；也是断言/keys 列表的种子，之后 TSO2 会替换 keys
            // （见 `finalize_album_artist_keys`）
            MetadataTag::AlbumArtist(value) => {
                for name in split_artist_names(&value, split_artists) {
                    push_unique_name(&mut metadata.album_artist, &name, ", ");
                    push_unique_vec(&mut metadata.album_artist_keys, &name);
                }
            }
            // ALBUMARTISTS 标签承载逐条的专辑歌手署名
            MetadataTag::AlbumArtists(value) => {
                for name in split_artist_names(&value, split_artists) {
                    push_unique_vec(&mut artist_names.album_artists_tag, &name);
                }
            }
            // ARTISTS 标签承载逐条署名
            MetadataTag::Artists(value) => {
                for name in split_artist_names(&value, split_artists) {
                    push_unique_vec(&mut artist_names.artists_tag, &name);
                }
            }
            MetadataTag::AlbumArtistSort(value) => {
                push_unique_name(&mut metadata.album_artist_sort, &value, " & ");
            }
            MetadataTag::ArtistSort(value) => {
                push_unique_name(&mut metadata.artist_sort, &value, " & ");
            }
            _ => apply_tag(meta_tag, metadata),
        }
    }
}

/// 匹配列表优先取 ARTISTS 的逐条署名；TPE1 常常是一整条拼接署名，作为兜底。
/// 所有标签都应用完之后调用一次。
fn finalize_track_artists(metadata: &mut Metadata, tpe1: Vec<String>, artists_tag: Vec<String>) {
    let names = if artists_tag.is_empty() {
        tpe1
    } else {
        artists_tag
    };
    metadata.artists = names.into_iter().collect();
}

/// 断言/keys 列表：有逐条 ALBUMARTISTS 署名时用它；否则当 TSO2 的每一部分
/// 都声称是某位已署名的曲目歌手时按 "&" 切分 TSO2；再否则用
/// `apply_tag_items` 收集到的原始 TPE2 值。所有标签应用完之后调用一次。
fn finalize_album_artist_keys(metadata: &mut Metadata, album_artists: &[String]) {
    if !album_artists.is_empty() {
        metadata.album_artist_keys = album_artists.iter().cloned().collect();
        // 逐条署名同时也顶替缺失的拼接展示值
        if metadata
            .album_artist
            .as_deref()
            .is_none_or(|d| d.trim().is_empty())
        {
            metadata.album_artist = Some(album_artists.join(", "));
        }
    }
    if let Some(sort) = metadata
        .album_artist_sort
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        let parts: Vec<&str> = sort
            .split('&')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        // 多段排序名只有在每一段都声称是某位曲目歌手时才可信；单个歌手的排序名
        // 自身就可能含 "&"（Simon & Garfunkel）；单段排序名只在没有逐条署名
        // 可以反驳它时才可信。
        // keys 列表只为多段路径构建——常见的单段专辑保住它的零分配快路径
        let multi_part_all_claimed = parts.len() > 1 && {
            let artist_keys: Vec<String> = metadata
                .artists
                .iter()
                .map(|name| token_key(name))
                .collect();
            let claimed = |part: &str| {
                let part_key = token_key(part);
                artist_keys.contains(&part_key)
            };
            parts.iter().all(|part| claimed(part))
        };
        let single_part_without_credits = parts.len() == 1 && album_artists.is_empty();
        // 两条规则都蕴含 `parts` 非空，所以无需再额外判空
        let trusted = multi_part_all_claimed || single_part_without_credits;
        if trusted {
            metadata.album_artist_keys = parts.into_iter().map(str::to_string).collect();
        }
    }
}

fn tags_by_priority(tags: &[Tag], has_id3v2: bool) -> Vec<&Tag> {
    if !has_id3v2 {
        return tags.iter().collect();
    }

    let is_text_tag = |tag: &&Tag| matches!(tag.tag_type(), TagType::RiffInfo | TagType::AiffText);
    tags.iter()
        .filter(|tag| is_text_tag(tag))
        .chain(tags.iter().filter(|tag| !is_text_tag(tag)))
        .collect()
}

fn read_tags_from_file(mut file: File) -> Result<TagsFromFile, OpenError> {
    let tagged_file = lofty::read_from(&mut file).map_err(|_| OpenError::UnsupportedFormat)?;

    let mut metadata = Metadata::default();

    let has_better_tag = tagged_file
        .tags()
        .iter()
        .any(|tag| tag.tag_type() != TagType::Id3v1);

    let has_id3v2 = tagged_file
        .tags()
        .iter()
        .any(|tag| tag.tag_type() == TagType::Id3v2);
    let id3v2_version = has_id3v2
        .then(|| read_id3v2_version(&mut file, tagged_file.file_type()))
        .flatten();

    let mut artist_names = ArtistNames::default();
    for tag in tags_by_priority(tagged_file.tags(), has_id3v2) {
        if has_better_tag && tag.tag_type() == TagType::Id3v1 {
            continue;
        }

        // 缘由见 `read_id3v2_version` 里的注释
        let split_artists =
            tag.tag_type() == TagType::Id3v2 && id3v2_version == Some(Id3v2Version::V3);
        apply_tag_items(tag, split_artists, &mut metadata, &mut artist_names);
    }

    finalize_track_artists(&mut metadata, artist_names.tpe1, artist_names.artists_tag);
    finalize_album_artist_keys(&mut metadata, &artist_names.album_artists_tag);

    let duration = tagged_file.properties().duration();
    let duration_ms = (!duration.is_zero()).then_some(duration.as_millis() as u64);

    Ok(TagsFromFile {
        tagged_file,
        metadata,
        duration: duration_ms,
    })
}

#[derive(Default)]
pub struct LoftyProvider;

pub struct LoftyStream {
    metadata: Metadata,
    tagged_file: TaggedFile,
    duration_ms: Option<u64>,
    started: bool,
}

impl MediaProvider for LoftyProvider {
    fn open(&self, file: File, _ext: Option<&OsStr>) -> Result<Box<dyn MediaStream>, OpenError> {
        let tags = read_tags_from_file(file)?;

        Ok(Box::new(LoftyStream {
            metadata: tags.metadata,
            tagged_file: tags.tagged_file,
            duration_ms: tags.duration,
            started: false,
        }))
    }

    fn supported_extensions(&self) -> &[&str] {
        super::SUPPORTED_EXTENSIONS
    }

    fn supported_features(&self) -> MediaProviderFeatures {
        MediaProviderFeatures::ALLOWS_INDEXING | MediaProviderFeatures::PROVIDES_METADATA
    }
}

impl MediaStream for LoftyStream {
    fn close(&mut self) {
        self.started = false;
    }

    fn start_playback(&mut self) -> Result<(), PlaybackStartError> {
        self.started = true;
        Ok(())
    }

    fn stop_playback(&mut self) {
        self.started = false;
    }

    fn seek(&mut self, _time: f64) -> Result<(), SeekError> {
        Err(SeekError::InvalidState)
    }

    fn frame_duration(&self) -> Result<u64, FrameDurationError> {
        Err(FrameDurationError::NeverStarted)
    }

    fn read_metadata(&mut self) -> Result<Metadata, MetadataError> {
        Ok(std::mem::take(&mut self.metadata))
    }

    fn metadata_updated(&self) -> bool {
        false
    }

    fn read_image(&mut self) -> Result<Option<Box<[u8]>>, MetadataError> {
        // 封面按需从 tags 现场提取，避免读取标签时无条件拷贝整份图片字节；
        // 提取后清空 tags 释放图片驻留，重复调用返回 None（一次取走的语义不变）
        let has_id3v2 = self
            .tagged_file
            .tags()
            .iter()
            .any(|tag| tag.tag_type() == TagType::Id3v2);
        let image = tags_by_priority(self.tagged_file.tags(), has_id3v2)
            .into_iter()
            .find_map(extract_cover);
        self.tagged_file.clear();
        Ok(image)
    }

    fn duration_ms(&self) -> Result<u64, TrackDurationError> {
        if !self.started {
            return Err(TrackDurationError::NeverStarted);
        }
        self.duration_ms.ok_or(TrackDurationError::NeverStarted)
    }

    fn position_ms(&self) -> Result<u64, TrackDurationError> {
        Err(TrackDurationError::NeverStarted)
    }

    fn channels(&self) -> Result<crate::devices::format::ChannelSpec, ChannelRetrievalError> {
        Err(ChannelRetrievalError::NothingToPlay)
    }

    fn sample_rate(&self) -> Result<u32, ChannelRetrievalError> {
        Err(ChannelRetrievalError::NothingToPlay)
    }

    fn decode_into(
        &mut self,
        _output: &mut ChannelProducers,
    ) -> Result<DecodeResult, PlaybackReadError> {
        Err(PlaybackReadError::InvalidState)
    }

    fn set_looping(&mut self, _enabled: bool) {}
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::test_support::TestDir;

    /// 把合成出来的测试素材写进临时目录，再经 provider 打开它，
    /// 与读取真实文件的路径完全一致。
    fn read_fixture(dir: &TestDir, name: &str) -> (Metadata, bool) {
        let path = dir.join(name);
        std::fs::write(&path, crate::test_support::audio_fixtures::fixture(name)).unwrap();
        let file = File::open(&path).unwrap_or_else(|err| panic!("failed to open {name}: {err}"));
        let mut stream = LoftyProvider
            .open(file, path.extension())
            .unwrap_or_else(|err| panic!("failed to read {name}: {err}"));

        stream.start_playback().unwrap();
        let metadata = stream.read_metadata().unwrap();
        let has_image = stream.read_image().unwrap().is_some();
        assert!(stream.read_image().unwrap().is_none());

        (metadata, has_image)
    }

    fn fixture_bytes(dir: &TestDir, name: &str) -> File {
        let path = dir.join(name);
        std::fs::write(&path, crate::test_support::audio_fixtures::fixture(name)).unwrap();
        File::open(&path).unwrap()
    }

    const RICH_METADATA_FIXTURES: &[&str] = &[
        "fixture.mp3",
        "fixture.flac",
        "fixture.ogg",
        "fixture.m4a",
        "fixture.wav",
        "fixture.aiff",
        "fixture.opus",
        "fixture.aac",
    ];

    const DATE_FIXTURES: &[&str] = &[
        "fixture.flac",
        "fixture.ogg",
        "fixture.m4a",
        "fixture.wav",
        "fixture.aiff",
        "fixture.opus",
    ];

    // WAV 与 AIFF 素材带 ID3 标签，也暴露富字段与日期，但 Lofty 不会把它们的
    // USLT 帧归一化成 Lyrics。歌词断言只保留会暴露它的格式。
    const LYRICS_FIXTURES: &[&str] =
        &["fixture.flac", "fixture.ogg", "fixture.m4a", "fixture.opus"];

    fn assert_rich_metadata(metadata: &Metadata, context: &str) {
        assert_eq!(
            metadata.name.as_deref(),
            Some("Test Track"),
            "name in {context}"
        );
        assert_eq!(
            metadata.artist.as_deref(),
            Some("Test Artist"),
            "artist in {context}"
        );
        assert_eq!(
            metadata.album_artist.as_deref(),
            Some("Test Album Artist"),
            "album artist in {context}"
        );
        assert_eq!(
            metadata.album.as_deref(),
            Some("Test Album"),
            "album in {context}"
        );
        assert_eq!(
            metadata.genre.as_deref(),
            Some("Test Genre"),
            "genre in {context}"
        );
        assert_eq!(metadata.track_current, Some(2), "track in {context}");
        assert_eq!(metadata.track_max, Some(9), "track total in {context}");
        assert_eq!(metadata.disc_current, Some(1), "disc in {context}");
        assert_eq!(metadata.disc_max, Some(3), "disc total in {context}");
        assert_eq!(
            metadata.isrc.as_deref(),
            Some("QZHB12400001"),
            "isrc in {context}"
        );
        assert_eq!(
            metadata.mbid_album.as_deref(),
            Some("12345678-1234-4234-9234-123456789abc"),
            "mbid in {context}"
        );
        assert_eq!(
            metadata.replaygain_track_gain,
            Some(-3.21),
            "rg track gain in {context}"
        );
        assert_eq!(
            metadata.replaygain_track_peak,
            Some(0.987654),
            "rg track peak in {context}"
        );
        assert_eq!(
            metadata.replaygain_album_gain,
            Some(-4.56),
            "rg album gain in {context}"
        );
        assert_eq!(
            metadata.replaygain_album_peak,
            Some(0.876543),
            "rg album peak in {context}"
        );
    }

    #[test]
    fn riff_info_only_fields_survive_alongside_id3v2() {
        let mut riff = Tag::new(TagType::RiffInfo);
        assert!(riff.insert_text(ItemKey::TrackTitle, "Avril 14th".to_string()));
        assert!(riff.insert_text(ItemKey::RecordingDate, "2001-10-22".to_string()));

        let mut id3 = Tag::new(TagType::Id3v2);
        assert!(id3.insert_text(ItemKey::TrackTitle, "Avril 14th (ID3)".to_string()));

        let tags = [riff, id3];
        let ordered = tags_by_priority(&tags, true);
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].tag_type(), TagType::RiffInfo);

        let mut metadata = Metadata::default();
        let mut artist_names = ArtistNames::default();
        for tag in ordered {
            apply_tag_items(tag, false, &mut metadata, &mut artist_names);
        }

        // 共有字段上 ID3v2 胜出，只存在于 INFO 里的日期也照样落地
        assert_eq!(metadata.name.as_deref(), Some("Avril 14th (ID3)"));
        let expected_date = Utc.with_ymd_and_hms(2001, 10, 22, 0, 0, 0).unwrap();
        assert_eq!(metadata.date, Some(expected_date));
    }

    #[test]
    fn reads_rich_metadata_from_tagged_fixtures() {
        let dir = TestDir::new("lofty-fixture-test");
        for name in RICH_METADATA_FIXTURES {
            let (metadata, has_image) = read_fixture(&dir, name);
            assert_rich_metadata(&metadata, name);
            assert!(has_image, "expected embedded image in {name}");
        }
    }

    #[test]
    fn reads_dates_from_fixtures_that_expose_them() {
        let dir = TestDir::new("lofty-fixture-test");
        let expected_date = Utc.with_ymd_and_hms(1995, 6, 24, 0, 0, 0).unwrap();

        for name in DATE_FIXTURES {
            let (metadata, _) = read_fixture(&dir, name);
            assert_eq!(
                metadata.date,
                Some(expected_date),
                "date mismatch in {name}"
            );
        }
    }

    #[test]
    fn reads_lyrics_from_fixtures_that_expose_them() {
        let dir = TestDir::new("lofty-fixture-test");
        for name in LYRICS_FIXTURES {
            let (metadata, _) = read_fixture(&dir, name);
            assert_eq!(
                metadata.lyrics.as_deref(),
                Some("[00:00.00] Test lyrics"),
                "lyrics mismatch in {name}"
            );
        }
    }

    #[test]
    fn detects_id3v2_version_from_concrete_files() {
        let dir = TestDir::new("lofty-fixture-test");
        let cases = [
            ("fixture.mp3", FileType::Mpeg, Some(Id3v2Version::V3)),
            ("fixture.aac", FileType::Aac, Some(Id3v2Version::V3)),
            ("fixture.aiff", FileType::Aiff, Some(Id3v2Version::V4)),
            ("fixture.flac", FileType::Flac, None),
        ];
        for (name, file_type, expected) in cases {
            let mut file = fixture_bytes(&dir, name);
            assert_eq!(
                read_id3v2_version(&mut file, file_type),
                expected,
                "version mismatch in {name}"
            );
        }
    }

    #[test]
    fn sniffs_id3v2_version_from_synthetic_header_bytes() {
        let dir = TestDir::new("lofty-id3v2-version-test");
        // 最简 10 字节 ID3v2 头："ID3" + major + minor + flags + size
        let header = |major: u8| [b'I', b'D', b'3', major, 0, 0, 0, 0, 0, 0];

        let version_of = |name: &str, bytes: &[u8], file_type: FileType| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let mut file = File::open(&path).unwrap();
            read_id3v2_version(&mut file, file_type)
        };

        // 前置的标签头只靠这 10 个头部字节就能回答
        assert_eq!(
            version_of("v23.bin", &header(3), FileType::Mpeg),
            Some(Id3v2Version::V3)
        );
        assert_eq!(
            version_of("v24.bin", &header(4), FileType::Mpeg),
            Some(Id3v2Version::V4)
        );
        assert_eq!(
            version_of("v23-aac.bin", &header(3), FileType::Aac),
            Some(Id3v2Version::V3)
        );

        // 其它容器本来就不做重解析；它们依旧报 None
        assert_eq!(
            version_of("flac-with-header.bin", &header(3), FileType::Flac),
            None
        );
        // AIFF/WAV 的 ID3 chunk 藏在 RIFF/FORM 魔数之后，所以一个位于开头的
        // 头会落到重解析路径，而重解析不认这个光秃秃的头
        assert_eq!(
            version_of("wav-with-header.bin", &header(3), FileType::Wav),
            None
        );

        // 没有 ID3v2 头：跑 lofty 回退，它也没找到标签
        assert_eq!(
            version_of("no-header.bin", b"just some bytes", FileType::Mpeg),
            None
        );
        // 被截断的头没有版本字节，同样走回退
        assert_eq!(version_of("truncated.bin", b"ID3", FileType::Mpeg), None);
    }

    fn tag_with_artists(tag_type: TagType, artists: &str) -> Tag {
        let mut tag = Tag::new(tag_type);
        assert!(tag.insert_text(ItemKey::TrackArtists, artists.to_string()));
        tag
    }

    fn names(values: &[&str]) -> smallvec::SmallVec<[String; 2]> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn tag_with_items(tag_type: TagType, key: ItemKey, values: &[&str]) -> Tag {
        let mut tag = Tag::new(tag_type);
        for value in values {
            assert!(tag.push(TagItem::new(key, ItemValue::Text(value.to_string()))));
        }
        tag
    }

    fn applied_metadata(tag: &Tag, split: bool) -> Metadata {
        let mut metadata = Metadata::default();
        let mut artist_names = ArtistNames::default();
        apply_tag_items(tag, split, &mut metadata, &mut artist_names);
        finalize_track_artists(&mut metadata, artist_names.tpe1, artist_names.artists_tag);
        finalize_album_artist_keys(&mut metadata, &artist_names.album_artists_tag);
        metadata
    }

    #[test]
    fn splits_slash_joined_artists_for_id3v23() {
        let tag = tag_with_artists(TagType::Id3v2, "Artist A/Artist B");
        let metadata = applied_metadata(&tag, true);
        assert_eq!(metadata.artists, names(&["Artist A", "Artist B"]));
    }

    #[test]
    fn keeps_literal_slash_in_artists_for_other_formats() {
        let tag = tag_with_artists(TagType::VorbisComments, "AC/DC");
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artists, names(&["AC/DC"]));
    }

    #[test]
    fn null_joined_track_artists_feed_display_and_matching() {
        let tag = tag_with_items(
            TagType::Id3v2,
            ItemKey::TrackArtist,
            &["Artist 1", "Artist 2"],
        );
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artist.as_deref(), Some("Artist 1, Artist 2"));
        assert_eq!(metadata.artists, names(&["Artist 1", "Artist 2"]));
    }

    #[test]
    fn duplicate_artist_across_tag_systems_is_added_once() {
        let tag = tag_with_items(
            TagType::Id3v2,
            ItemKey::TrackArtist,
            &["Test Artist", "Test Artist"],
        );
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artist.as_deref(), Some("Test Artist"));
        assert_eq!(metadata.artists, names(&["Test Artist"]));
    }

    #[test]
    fn duplicate_joined_track_artist_values_feed_display_once() {
        let tag = tag_with_items(
            TagType::VorbisComments,
            ItemKey::TrackArtist,
            &["Tracy feat. 築山さえ", "Tracy feat. 築山さえ"],
        );
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artist.as_deref(), Some("Tracy feat. 築山さえ"));
        assert_eq!(metadata.artists, names(&["Tracy feat. 築山さえ"]));
    }

    #[test]
    fn duplicate_joined_album_artist_values_feed_display_and_keys_once() {
        // beets 产出的文件会同时带 ALBUM ARTIST 与 ALBUMARTIST，于是同一人出现两次，
        // 我们得确认自己把这种重复忽略掉了
        let tag = tag_with_items(
            TagType::VorbisComments,
            ItemKey::AlbumArtist,
            &["Halozy, 556ミリメートル", "Halozy, 556ミリメートル"],
        );
        let metadata = applied_metadata(&tag, false);
        assert_eq!(
            metadata.album_artist.as_deref(),
            Some("Halozy, 556ミリメートル")
        );
        assert_eq!(
            metadata.album_artist_keys,
            names(&["Halozy, 556ミリメートル"])
        );
    }

    #[test]
    fn joined_album_artist_value_dedupes_against_individual_names() {
        // 拼接值与逐条列出的名字描述的是同一批歌手
        let tag = tag_with_items(
            TagType::VorbisComments,
            ItemKey::AlbumArtist,
            &["Halozy, 556ミリメートル", "Halozy", "556ミリメートル"],
        );
        let metadata = applied_metadata(&tag, false);
        assert_eq!(
            metadata.album_artist.as_deref(),
            Some("Halozy, 556ミリメートル")
        );
    }

    #[test]
    fn album_artists_individual_credits_feed_claim_keys() {
        // 拼接的 ALBUM ARTIST 值喂给展示字段，逐条的 ALBUMARTISTS 署名是断言部分，
        // 即便存在拼接的排序标签也是如此
        let mut tag = tag_with_items(
            TagType::VorbisComments,
            ItemKey::AlbumArtist,
            &["Halozy, 556ミリメートル"],
        );
        for value in ["Halozy", "556ミリメートル"] {
            assert!(tag.push(TagItem::new(
                ItemKey::AlbumArtists,
                ItemValue::Text(value.to_string())
            )));
        }
        assert!(tag.push(TagItem::new(
            ItemKey::AlbumArtistSortOrder,
            ItemValue::Text("Halozy, 556 Millimetre".to_string())
        )));
        let metadata = applied_metadata(&tag, false);
        assert_eq!(
            metadata.album_artist.as_deref(),
            Some("Halozy, 556ミリメートル")
        );
        assert_eq!(
            metadata.album_artist_keys,
            names(&["Halozy", "556ミリメートル"])
        );
    }

    #[test]
    fn album_artists_credits_alone_feed_display_and_keys() {
        // 没有拼接的 ALBUM ARTIST 值时，逐条署名同时顶替两者
        let mut tag = Tag::new(TagType::VorbisComments);
        for value in ["Halozy", "556ミリメートル"] {
            assert!(tag.push(TagItem::new(
                ItemKey::AlbumArtists,
                ItemValue::Text(value.to_string())
            )));
        }
        let metadata = applied_metadata(&tag, false);
        assert_eq!(
            metadata.album_artist.as_deref(),
            Some("Halozy, 556ミリメートル")
        );
        assert_eq!(
            metadata.album_artist_keys,
            names(&["Halozy", "556ミリメートル"])
        );
    }

    #[test]
    fn fully_claimed_sort_replaces_individual_album_artist_credits() {
        // 用 "&" 拼接、且每一部分都声称是某位已署名歌手的排序名，仍是最优断言列表
        let mut tag = tag_with_items(
            TagType::VorbisComments,
            ItemKey::AlbumArtist,
            &["Mark Pritchard and Thom Yorke"],
        );
        for value in ["Mark Pritchard", "Thom Yorke"] {
            assert!(tag.push(TagItem::new(
                ItemKey::AlbumArtists,
                ItemValue::Text(value.to_string())
            )));
        }
        for value in ["Pritchard, Mark", "Yorke, Thom"] {
            assert!(tag.push(TagItem::new(
                ItemKey::AlbumArtistSortOrder,
                ItemValue::Text(value.to_string())
            )));
        }
        let mut metadata = Metadata::default();
        let mut artist_names = ArtistNames::default();
        apply_tag_items(&tag, false, &mut metadata, &mut artist_names);
        metadata.artists = names(&["Mark Pritchard", "Thom Yorke"]);
        finalize_album_artist_keys(&mut metadata, &artist_names.album_artists_tag);
        assert_eq!(
            metadata.album_artist_keys,
            names(&["Pritchard, Mark", "Yorke, Thom"])
        );
    }

    #[test]
    fn duplicate_joined_album_artist_sort_values_feed_once() {
        let tag = tag_with_items(
            TagType::VorbisComments,
            ItemKey::AlbumArtistSortOrder,
            &["Halozy, 556 Millimetre", "Halozy, 556 Millimetre"],
        );
        let metadata = applied_metadata(&tag, false);
        assert_eq!(
            metadata.album_artist_sort.as_deref(),
            Some("Halozy, 556 Millimetre")
        );
    }

    #[test]
    fn repeated_album_artist_item_with_existing_part_is_skipped() {
        // 已被前面某部分覆盖的值不得再扩展展示字段
        let mut metadata = Metadata::default();
        let mut artist_names = ArtistNames::default();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.push(TagItem::new(
            ItemKey::AlbumArtist,
            ItemValue::Text("Band 1".to_string()),
        ));
        apply_tag_items(&tag, false, &mut metadata, &mut artist_names);
        tag.push(TagItem::new(
            ItemKey::AlbumArtist,
            ItemValue::Text("Band 1, Band 2".to_string()),
        ));
        apply_tag_items(&tag, false, &mut metadata, &mut artist_names);
        assert_eq!(metadata.album_artist.as_deref(), Some("Band 1, Band 2"));
    }

    #[test]
    fn single_track_artist_keeps_its_name() {
        let tag = tag_with_items(TagType::VorbisComments, ItemKey::TrackArtist, &["Artist"]);
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(metadata.artists, names(&["Artist"]));
    }

    #[test]
    fn keeps_literal_slash_in_track_artist_for_id3v23() {
        // TPE1 从不拆分，v2.3 的斜杠挤名太不可靠，无法反推（AC/DC）
        let tag = tag_with_items(TagType::Id3v2, ItemKey::TrackArtist, &["Artist A/Artist B"]);
        let metadata = applied_metadata(&tag, true);
        assert_eq!(metadata.artist.as_deref(), Some("Artist A/Artist B"));
        assert_eq!(metadata.artists, names(&["Artist A/Artist B"]));
    }

    #[test]
    fn keeps_literal_slash_in_track_artist_for_id3v24() {
        let tag = tag_with_items(TagType::Id3v2, ItemKey::TrackArtist, &["AC/DC"]);
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artist.as_deref(), Some("AC/DC"));
        assert_eq!(metadata.artists, names(&["AC/DC"]));
    }

    #[test]
    fn artists_tag_wins_matching_over_tpe1() {
        // Picard 把 TPE1 写成一条拼接署名，把 ARTISTS 写成逐条名字
        let mut tag = tag_with_items(
            TagType::Id3v2,
            ItemKey::TrackArtist,
            &["Artist A and Artist B"],
        );
        for value in ["Artist A", "Artist B"] {
            assert!(tag.push(TagItem::new(
                ItemKey::TrackArtists,
                ItemValue::Text(value.to_string())
            )));
        }
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.artist.as_deref(), Some("Artist A and Artist B"));
        assert_eq!(metadata.artists, names(&["Artist A", "Artist B"]));
    }

    #[test]
    fn null_joined_album_artists_feed_display_and_keys() {
        let tag = tag_with_items(TagType::Id3v2, ItemKey::AlbumArtist, &["Band 1", "Band 2"]);
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.album_artist.as_deref(), Some("Band 1, Band 2"));
        assert_eq!(metadata.album_artist_keys, names(&["Band 1", "Band 2"]));
    }

    #[test]
    fn album_artist_sort_keys_replace_tpe2_keys() {
        let tag = tag_with_items(
            TagType::Id3v2,
            ItemKey::AlbumArtist,
            &["Mark Pritchard and Thom Yorke"],
        );
        let mut metadata = applied_metadata(&tag, false);
        metadata.artists = names(&["Mark Pritchard", "Thom Yorke"]);
        metadata.album_artist_sort = Some("Pritchard, Mark & Yorke, Thom".to_string());
        finalize_album_artist_keys(&mut metadata, &[]);
        assert_eq!(
            metadata.album_artist_keys,
            names(&["Pritchard, Mark", "Yorke, Thom"])
        );
    }

    #[test]
    fn ampersand_in_single_artist_sort_keeps_tpe2_keys() {
        // Simon & Garfunkel 的排序名里含一个字面的 "&"，它不得被拆成断言
        let tag = tag_with_items(TagType::Id3v2, ItemKey::AlbumArtist, &["Simon & Garfunkel"]);
        let mut metadata = applied_metadata(&tag, false);
        metadata.artists = names(&["Simon & Garfunkel"]);
        metadata.album_artist_sort = Some("Simon & Garfunkel".to_string());
        finalize_album_artist_keys(&mut metadata, &[]);
        assert_eq!(metadata.album_artist_keys, names(&["Simon & Garfunkel"]));
    }

    #[test]
    fn multi_value_album_artist_sort_accumulates() {
        // v2.4 用 null 分隔的 TSO2 会以重复条目到达，它们全部构成断言部分
        let mut tag = tag_with_items(
            TagType::Id3v2,
            ItemKey::TrackArtist,
            &["Mark Pritchard", "Thom Yorke"],
        );
        for value in ["Pritchard, Mark", "Yorke, Thom"] {
            assert!(tag.push(TagItem::new(
                ItemKey::AlbumArtistSortOrder,
                ItemValue::Text(value.to_string())
            )));
        }
        let metadata = applied_metadata(&tag, false);
        assert_eq!(
            metadata.album_artist_sort.as_deref(),
            Some("Pritchard, Mark & Yorke, Thom")
        );
        assert_eq!(
            metadata.album_artist_keys,
            names(&["Pritchard, Mark", "Yorke, Thom"])
        );
    }

    #[test]
    fn empty_album_artist_values_produce_no_keys() {
        let tag = tag_with_items(TagType::Id3v2, ItemKey::AlbumArtist, &["   "]);
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.album_artist, None);
        assert!(metadata.album_artist_keys.is_empty());
    }

    #[test]
    fn blank_value_does_not_copy_through() {
        let tag = tag_with_items(TagType::Id3v2, ItemKey::Label, &["   "]);
        let metadata = applied_metadata(&tag, false);
        assert_eq!(metadata.label, None);
    }
}
