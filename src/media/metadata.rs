use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use smallvec::{SmallVec, smallvec};
use std::path::Path;
use std::str::FromStr;

pub(crate) fn parse_rg_float_str(value: &str) -> Option<f64> {
    value.trim().parse().ok()
}

pub(crate) fn parse_rg_gain_str(value: &str) -> Option<f64> {
    let s = value.trim();
    // 按字节探测后缀：字节比较对任何输入都安全，只有它命中后，下面的 str
    // 切片才一定落在字符边界上——以多字节字符结尾的标签解析为 None，
    // 而不是 panic（与 strip_quality_suffix 的契约相同）。
    let ends_in_db = s.len() >= 2 && s.as_bytes()[s.len() - 2..].eq_ignore_ascii_case(b"db");
    let s = if ends_in_db {
        s[..s.len() - 2].trim()
    } else {
        s
    };
    s.parse().ok()
}

pub(crate) fn parse_r128_gain_str(value: &str) -> Option<f64> {
    let v: i16 = value.trim().parse().ok()?;
    Some(v as f64 / 256.0)
}

#[derive(Debug, Clone)]
pub enum MetadataTag {
    Name(String),
    Artist(String),
    Artists(String),
    AlbumArtist(String),
    AlbumArtists(String),
    OriginalArtist(String),
    Composer(String),
    Album(String),
    Genre(String),
    Grouping(String),
    Bpm(u64),
    Compilation(bool),
    Date(String),
    TrackNumber(String),
    TrackTotal(u64),
    DiscNumber(String),
    DiscTotal(u64),
    Label(String),
    Catalog(String),
    Isrc(String),
    SortAlbum(String),
    ArtistSort(String),
    AlbumArtistSort(String),
    MbidAlbum(String),
    Lyrics(String),
    ReplayGainTrackGain(String),
    ReplayGainTrackPeak(String),
    ReplayGainAlbumGain(String),
    ReplayGainAlbumPeak(String),
    R128TrackGain(String),
    R128AlbumGain(String),
    DiscSubtitle(String),
    LoopStart(f64),
    LoopEnd(f64),
}

pub fn apply_tag(tag: MetadataTag, metadata: &mut Metadata) {
    match tag {
        MetadataTag::Name(v) => metadata.name = Some(v),
        MetadataTag::Artist(v) => metadata.artist = Some(v),
        MetadataTag::Artists(v) => push_unique(&mut metadata.artists, v),
        MetadataTag::AlbumArtist(v) => metadata.album_artist = Some(v),
        MetadataTag::AlbumArtists(v) => push_unique(&mut metadata.album_artist_keys, v),
        MetadataTag::OriginalArtist(v) => metadata.original_artist = Some(v),
        MetadataTag::Composer(v) => metadata.composer = Some(v),
        MetadataTag::Album(v) => metadata.album = Some(v),
        MetadataTag::Genre(v) => metadata.genre = Some(v),
        MetadataTag::Grouping(v) => metadata.grouping = Some(v),
        MetadataTag::Bpm(v) => metadata.bpm = Some(v),
        MetadataTag::Compilation(v) => metadata.compilation = v,
        MetadataTag::Date(v) => match parse_release_date(&v) {
            Some(ParsedReleaseDate::FullDate(date)) => {
                metadata.date = Some(date);
                metadata.year_month = None;
                metadata.year = None;
            }
            Some(ParsedReleaseDate::YearMonth(year, month)) if metadata.date.is_none() => {
                metadata.year_month = Some((year, month));
                metadata.year = None;
            }
            Some(ParsedReleaseDate::Year(year))
                if metadata.date.is_none() && metadata.year_month.is_none() =>
            {
                metadata.year = Some(year);
            }
            _ => {}
        },
        MetadataTag::TrackNumber(v) => {
            if let Some(parsed) = parse_track_number(&v) {
                metadata.track_current = Some(parsed.track);
                metadata.vinyl_numbering = parsed.is_vinyl;
                if let Some(disc) = parsed.disc {
                    metadata.disc_current = Some(disc);
                }
                if let Some(max) = parsed.track_max {
                    metadata.track_max = Some(max);
                }
            }
        }
        MetadataTag::TrackTotal(v) => metadata.track_max = Some(v),
        MetadataTag::DiscNumber(v) => {
            if let Some(parsed) = parse_disc_number(&v) {
                metadata.disc_current = Some(parsed.disc);
                if let Some(total) = parsed.disc_max {
                    metadata.disc_max = Some(total);
                }
                if let Some(subtitle) = parsed.disc_subtitle {
                    metadata.disc_subtitle = Some(subtitle);
                }
            }
        }
        MetadataTag::DiscTotal(v) => metadata.disc_max = Some(v),
        MetadataTag::Label(v) => metadata.label = Some(v),
        MetadataTag::Catalog(v) => metadata.catalog = Some(v),
        MetadataTag::Isrc(v) => metadata.isrc = Some(v),
        MetadataTag::SortAlbum(v) => metadata.sort_album = Some(v),
        MetadataTag::ArtistSort(v) => metadata.artist_sort = Some(v),
        MetadataTag::AlbumArtistSort(v) => metadata.album_artist_sort = Some(v),
        MetadataTag::MbidAlbum(v) => metadata.mbid_album = Some(v),
        MetadataTag::Lyrics(v) => metadata.lyrics = Some(v),
        MetadataTag::ReplayGainTrackGain(v) => {
            metadata.replaygain_track_gain = parse_rg_gain_str(&v);
        }
        MetadataTag::ReplayGainTrackPeak(v) => {
            metadata.replaygain_track_peak = parse_rg_float_str(&v);
        }
        MetadataTag::ReplayGainAlbumGain(v) => {
            metadata.replaygain_album_gain = parse_rg_gain_str(&v);
        }
        MetadataTag::ReplayGainAlbumPeak(v) => {
            metadata.replaygain_album_peak = parse_rg_float_str(&v);
        }
        MetadataTag::DiscSubtitle(v) => metadata.disc_subtitle = Some(v),
        MetadataTag::R128TrackGain(v) => {
            metadata.replaygain_track_gain = parse_r128_gain_str(&v);
        }
        MetadataTag::R128AlbumGain(v) => {
            metadata.replaygain_album_gain = parse_r128_gain_str(&v);
        }
        MetadataTag::LoopStart(v) => metadata.loop_start = Some(v),
        MetadataTag::LoopEnd(v) => metadata.loop_end = Some(v),
    }
}

fn push_unique(values: &mut SmallVec<[String; 2]>, value: String) {
    let value = value.trim();
    if !value.is_empty() && !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

#[derive(Debug, Default, PartialEq, Clone)]
pub struct Metadata {
    pub name: Option<String>,
    pub artist: Option<String>,
    pub artists: SmallVec<[String; 2]>,
    pub album_artist: Option<String>,
    /// 专辑歌手断言项：当每个部分都声称为已署名的曲目歌手时，TSO2 按 "&"
    /// 切分；否则是逐条的 ALBUMARTISTS 署名，再否则是原始的 TPE2 值。
    pub album_artist_keys: SmallVec<[String; 2]>,
    pub artist_sort: Option<String>,
    pub album_artist_sort: Option<String>,
    pub original_artist: Option<String>,
    pub composer: Option<String>,
    pub album: Option<String>,
    pub sort_album: Option<String>,
    pub genre: Option<String>,
    pub grouping: Option<String>,
    pub bpm: Option<u64>,
    pub compilation: bool,
    /// 发行日期元数据。`date`、`year_month`、`year` 三者只应有一个被设置。
    pub date: Option<DateTime<Utc>>,
    /// 发行年/月元数据，用于 `1995-06` 这类不完整的日期。
    pub year_month: Option<(u16, u8)>,
    /// 可选的年份字段。若 date 或 year_month 已有值，year 保持为空。
    /// 之所以存在这个字段，是因为有些打标签软件把 date 字段当年份用，
    /// 而这按日期无法正确处理。
    pub year: Option<u16>,

    pub track_current: Option<u64>,
    pub track_max: Option<u64>,
    pub disc_current: Option<u64>,
    pub disc_max: Option<u64>,
    pub disc_subtitle: Option<String>,
    pub vinyl_numbering: bool,

    pub label: Option<String>,
    pub catalog: Option<String>,
    pub isrc: Option<String>,

    pub mbid_album: Option<String>,

    pub replaygain_track_gain: Option<f64>,
    pub replaygain_track_peak: Option<f64>,
    pub replaygain_album_gain: Option<f64>,
    pub replaygain_album_peak: Option<f64>,

    pub lyrics: Option<String>,

    pub loop_start: Option<f64>,
    pub loop_end: Option<f64>,
}

impl Metadata {
    /// 文件没有标题标签时，用文件名补 标题/歌手。
    /// 下载工具常把文件命名为 "<artist> - <title>[_MQ].mp3"；这里拆掉歌手
    /// 前缀并丢弃末尾的质量标记。
    pub fn fill_from_filename(&mut self, path: &Path) {
        if self.name.is_some() {
            return;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return;
        };
        let stem = strip_quality_suffix(stem).trim();
        if stem.is_empty() {
            return;
        }
        if let Some((artist, title)) = stem.split_once(" - ") {
            let title = title.trim();
            if !title.is_empty() {
                self.name = Some(title.to_string());
                let artist = artist.trim();
                if self.artist.is_none() && self.artists.is_empty() {
                    self.artist = Some(artist.to_string());
                    self.artists = smallvec![artist.to_string()];
                }
                return;
            }
        }
        self.name = Some(stem.to_string());
    }
}

/// 下载器会在文件名后追加 `_MQ`/`_hq` 这类质量标记；去掉一个，好让
/// "天若有情_MQ" 显示成 "天若有情"。
fn strip_quality_suffix(stem: &str) -> &str {
    const MARKERS: [&str; 8] = ["_MQ", "_HQ", "_SQ", "_ZQ", "_mq", "_hq", "_sq", "_zq"];
    for marker in MARKERS {
        // strip_suffix 是字符边界安全的：标记对不齐时返回 None
        // （例如 stem 以多字节字符结尾）
        if let Some(base) = stem.strip_suffix(marker) {
            return base;
        }
    }
    stem
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedReleaseDate {
    FullDate(DateTime<Utc>),
    YearMonth(u16, u8),
    Year(u16),
}

fn utc_midnight(date: NaiveDate) -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(date.and_time(NaiveTime::MIN), Utc)
}

/// 仅当 `value` 恰好是 `len` 位 ASCII 数字时，才把它按 `T` 解析。
fn parse_fixed_digits<T: FromStr>(value: &str, len: usize) -> Option<T> {
    (value.len() == len && value.chars().all(|c| c.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

/// 先于通用解析器尝试精确的 ISO 发行日期。
///
/// 这样能为 `YYYY`、`YYYY-MM`、`YYYY-MM-DD` 保留原始精度。
/// 若值不像 ISO 格式，返回 `Ok(None)`，让通用解析器仍能处理
/// `May 25, 2021` 这类自由格式标签。若值看起来像 ISO 但不合法，返回
/// `Err(())`，这样通用解析器就不会擅自补出一个日或改变精度。
fn parse_iso_release_date(value: &str) -> Result<Option<ParsedReleaseDate>, ()> {
    let value = value.trim();

    if !value.bytes().all(|byte| {
        byte.is_ascii_digit() || byte == b'-' || byte == b'.' || byte == b'/' || byte == b'_'
    }) {
        return Ok(None);
    }

    let mut parts = value.split(['-', '.', '/', '_']);
    let first = parts.next().ok_or(())?;
    let second = parts.next();
    let third = parts.next();

    if parts.next().is_some() {
        return Err(());
    }

    match (second, third) {
        (None, None) => parse_fixed_digits::<u16>(first, 4)
            .map(ParsedReleaseDate::Year)
            .map(Some)
            .ok_or(()),
        (Some(month), None) => {
            let year = parse_fixed_digits::<u16>(first, 4).ok_or(())?;
            let month = match parse_fixed_digits::<u8>(month, 2) {
                Some(month @ 1..=12) => month,
                _ => return Err(()),
            };

            Ok(Some(ParsedReleaseDate::YearMonth(year, month)))
        }
        (Some(month), Some(day)) => {
            let year = parse_fixed_digits::<u16>(first, 4).ok_or(())?;
            let month = parse_fixed_digits::<u8>(month, 2).ok_or(())?;
            let day = parse_fixed_digits::<u8>(day, 2).ok_or(())?;

            NaiveDate::from_ymd_opt(year.into(), month.into(), day.into())
                .map(|date| Some(ParsedReleaseDate::FullDate(utc_midnight(date))))
                .ok_or(())
        }
        (None, Some(_)) => Err(()),
    }
}

pub(crate) fn parse_release_date(value: &str) -> Option<ParsedReleaseDate> {
    match parse_iso_release_date(value) {
        Ok(Some(date)) => Some(date),
        Err(()) => None,
        Ok(None) => {
            // 非 ISO 日期仍走通用解析器，但把纯日期值钉在 UTC 零点上，
            // 以免它们取到本地当前时间的默认值。
            dateparser::parse_with(value.trim(), &Utc, NaiveTime::MIN)
                .ok()
                .map(ParsedReleaseDate::FullDate)
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ParsedTrackNumber {
    pub disc: Option<u64>,
    pub track: u64,
    pub track_max: Option<u64>,
    pub is_vinyl: bool,
}

/// `s` 末尾连续的 ASCII 数字。
///
/// `trim_end_matches` 返回的是字符边界，不像 `rfind` 加一再切片得到的
/// 字节偏移：最后一个非数字字符可能是多字节的（"一/12"），
/// 从它起始处 +1 切片会落在字符内部而 panic。
fn trailing_digits(s: &str) -> &str {
    let start = s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    &s[start..]
}

/// `s` 开头连续的 ASCII 数字。
fn leading_digits(s: &str) -> &str {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    &s[..end]
}

/// `"3/12"`（出现在字符串任意位置）-> `(3, Some(12))`。
fn parse_slash_pair(value: &str) -> Option<(u64, Option<u64>)> {
    let (before, after) = value.split_once('/')?;
    let track = trailing_digits(before);
    let total = leading_digits(after);
    if track.is_empty() || total.is_empty() {
        return None;
    }
    Some((track.parse().ok()?, total.parse().ok()))
}

/// 音轨号解析。先走语法化文法（语义对齐上游 hummingbird numbering.rs，不引
/// 入 regex 依赖）：`[面字母][数字][[.-]数字][/数字]`。文法不认的脏值回退到
/// 旧链（`parse_track_number_lenient`），保证存量可解析值一个不少。
///
/// 三类此前整号丢失的形态由文法补齐：
/// - `"1-2"` 的 section 后缀（古典/现场专辑常见）——单列编号模型没有存储位，
///   解析通过但只落轨号，不再整号返回 None；
/// - 黑胶单曲的面字母连写（`"AA"` = 面 A 第 2 轨）；
/// - 黑胶面 + section（`"A1.2"`）。
pub(crate) fn parse_track_number(value: &str) -> Option<ParsedTrackNumber> {
    parse_track_number_grammar(value).or_else(|| parse_track_number_lenient(value))
}

/// 语法化文法：`[面字母*][数字*][[.-]数字][/数字]`，全串必须消费完毕。
fn parse_track_number_grammar(value: &str) -> Option<ParsedTrackNumber> {
    let value = value.trim();
    let letter_end = value
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(value.len());
    let (letters, rest) = value.split_at(letter_end);
    let digit_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, rest) = rest.split_at(digit_end);

    // `[.-]` 后必须紧跟数字，否则整条文法不成立（"1-A"、"1."）
    let (section, rest) = match rest.strip_prefix(['.', '-']) {
        Some(rest) => {
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            (Some(&rest[..end]), &rest[end..])
        }
        None => (None, rest),
    };
    let (track_max, rest) = match rest.strip_prefix('/') {
        Some(rest) => {
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            (Some(&rest[..end]), &rest[end..])
        }
        None => (None, rest),
    };
    // 尾巴有杂字符（"3/12 (CD 1)"）交给 lenient 链
    if !rest.is_empty() {
        return None;
    }
    // section 在单列模型里无存储位（见 `parse_track_number` 文档），
    // 消费它只为守住文法边界
    let _ = section;
    let track_max = track_max.and_then(|digits| digits.parse().ok());

    let mut parsed = ParsedTrackNumber {
        track_max,
        ..ParsedTrackNumber::default()
    };
    match (letters.is_empty(), digits.is_empty()) {
        // 纯数字："1"、"1/12"、"1-2"（section 丢弃，轨号保住）
        (true, false) => parsed.track = digits.parse().ok()?,
        // 黑胶单曲：面字母逐轨连写（"A"、"AA"、"BBB.1"），轨号 = 连写长度
        (false, true) => {
            let (disc, track) = decode_side_run(letters)?;
            parsed.disc = Some(disc);
            parsed.track = track;
            parsed.is_vinyl = true;
        }
        // 黑胶面 + 轨号（"A3"、"A1.2"）：面字母必须单个
        (false, false) => {
            let mut chars = letters.chars();
            let side = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            parsed.disc = Some((side.to_ascii_uppercase() as u64) - ('A' as u64) + 1);
            parsed.track = digits.parse().ok()?;
            parsed.is_vinyl = true;
        }
        (true, true) => return None,
    }
    Some(parsed)
}

/// 黑胶单曲的面字母连写（"A"=1、"AA"=2）：所有字母大小写不敏感地相同，
/// 长度封顶防病态输入（上游 MAX_VINYL_SINGLE_TRACK = 64）。
fn decode_side_run(letters: &str) -> Option<(u64, u64)> {
    const MAX_VINYL_SINGLE_TRACK: usize = 64;
    let side = letters.chars().next()?.to_ascii_uppercase();
    if letters.len() > MAX_VINYL_SINGLE_TRACK
        || !letters
            .chars()
            .all(|candidate| candidate.to_ascii_uppercase() == side)
    {
        return None;
    }
    Some(((side as u64) - ('A' as u64) + 1, letters.len() as u64))
}

/// 旧链兜底：黑胶「面字母+轨号」与任意位置的斜杠对。文法先跑，这里只接
/// 文法不认的脏值（"3/12 (CD 1)"、"Track 3/12"），存量可解析值零回退。
fn parse_track_number_lenient(value: &str) -> Option<ParsedTrackNumber> {
    let mut parsed = ParsedTrackNumber::default();

    // 检查黑胶式编号：一个字母的面标识 + 可选数字
    let mut chars = value.chars();
    if let Some(side) = chars.next()
        && side.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_digit())
    {
        parsed.disc = Some((side.to_ascii_uppercase() as u64) - ('A' as u64) + 1);
        parsed.is_vinyl = true;
        parsed.track = trailing_digits(value).parse().ok().unwrap_or(1);
        Some(parsed)
    // 检查 MP3 式编号
    } else if let Some((track, track_max)) = parse_slash_pair(value) {
        parsed.track = track;
        parsed.track_max = track_max;
        Some(parsed)
    } else {
        parsed.track = value.parse().ok()?;
        Some(parsed)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ParsedDiscNumber {
    pub disc: u64,
    pub disc_max: Option<u64>,
    pub disc_subtitle: Option<String>,
}

pub(crate) fn parse_disc_number(value: &str) -> Option<ParsedDiscNumber> {
    if let Some((disc, disc_max)) = parse_slash_pair(value) {
        return Some(ParsedDiscNumber {
            disc,
            disc_max,
            disc_subtitle: None,
        });
    }

    // `"Disc 2 - Bonus Tracks"` / `"2 - Bonus Tracks"` / `"2—Bonus Tracks"`
    let rest = value.strip_prefix("Disc ").unwrap_or(value);
    let digits = leading_digits(rest);
    if let Ok(disc) = digits.parse::<u64>() {
        let tail = &rest[digits.len()..];
        let tail = tail.strip_prefix(' ').unwrap_or(tail);
        if let Some(after_dash) = tail.strip_prefix(['-', '\u{2014}']) {
            let subtitle = after_dash.strip_prefix(' ').unwrap_or(after_dash);
            if !subtitle.is_empty() {
                return Some(ParsedDiscNumber {
                    disc,
                    disc_max: None,
                    disc_subtitle: Some(subtitle.to_string()),
                });
            }
        }
    }

    Some(ParsedDiscNumber {
        disc: value.parse().ok()?,
        disc_max: None,
        disc_subtitle: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{Metadata, ParsedReleaseDate, parse_release_date, parse_rg_gain_str};
    use chrono::{NaiveTime, TimeZone, Timelike, Utc};
    use std::path::Path;

    #[test]
    fn filename_fallback_handles_multibyte_stems() {
        // 回归用例：质量标记检查曾用字节切片，在结尾为多字节字符的 stem
        // （"...耳机"）上 panic，并打死播放线程
        let mut metadata = Metadata::default();
        metadata.fill_from_filename(Path::new("D:\\music\\木蜘蛛 - 我试着把孤独藏进耳机.mp3"));
        assert_eq!(metadata.name.as_deref(), Some("我试着把孤独藏进耳机"));
        assert_eq!(metadata.artist.as_deref(), Some("木蜘蛛"));

        let mut metadata = Metadata::default();
        metadata.fill_from_filename(Path::new("D:\\music\\云狗蛋 - 天若有情_MQ.mp3"));
        assert_eq!(metadata.name.as_deref(), Some("天若有情"));
        assert_eq!(metadata.artist.as_deref(), Some("云狗蛋"));

        let mut metadata = Metadata::default();
        metadata.fill_from_filename(Path::new("D:\\music\\遗失的心跳-白允y.mp3"));
        assert_eq!(metadata.name.as_deref(), Some("遗失的心跳-白允y"));

        // 真实标签绝不会被覆盖
        let mut metadata = Metadata {
            name: Some("Real Title".to_string()),
            ..Metadata::default()
        };
        metadata.fill_from_filename(Path::new("D:\\music\\云狗蛋 - 天若有情_MQ.mp3"));
        assert_eq!(metadata.name.as_deref(), Some("Real Title"));
    }

    #[test]
    fn rg_gain_handles_multibyte_suffix() {
        // 回归用例："db" 后缀检查曾用字节切片，在结尾为多字节字符的标签值上
        // 会 panic，而不是返回 None
        assert_eq!(parse_rg_gain_str(" -6.5 dB"), Some(-6.5));
        assert_eq!(parse_rg_gain_str("+3.1db"), Some(3.1));
        assert_eq!(parse_rg_gain_str("2.0 DB"), Some(2.0));
        assert_eq!(parse_rg_gain_str("a文"), None);
        assert_eq!(parse_rg_gain_str("文"), None);
        assert_eq!(parse_rg_gain_str("not a gain"), None);
    }

    #[test]
    fn parses_year_only_release_dates() {
        assert_eq!(
            parse_release_date("1995"),
            Some(ParsedReleaseDate::Year(1995))
        );
    }

    #[test]
    fn parses_year_month_release_dates() {
        assert_eq!(
            parse_release_date("1995-06"),
            Some(ParsedReleaseDate::YearMonth(1995, 6))
        );
    }

    #[test]
    fn parses_full_release_dates() {
        assert_eq!(
            parse_release_date("1995-06-24"),
            Some(ParsedReleaseDate::FullDate(
                Utc.with_ymd_and_hms(1995, 6, 24, 0, 0, 0).single().unwrap(),
            ))
        );
    }

    #[test]
    fn parses_full_release_dates_with_alternate_separators() {
        let expected = Some(ParsedReleaseDate::FullDate(
            Utc.with_ymd_and_hms(1995, 6, 24, 0, 0, 0).single().unwrap(),
        ));

        assert_eq!(parse_release_date("1995.06.24"), expected);
        assert_eq!(parse_release_date("1995/06/24"), expected);
        assert_eq!(parse_release_date("1995_06_24"), expected);
    }

    #[test]
    fn rejects_invalid_full_release_dates() {
        assert_eq!(parse_release_date("1995-13-01"), None);
        assert_eq!(parse_release_date("1995-06-31"), None);
    }

    #[test]
    fn rejects_invalid_partial_release_dates() {
        assert_eq!(parse_release_date("1995-13"), None);
    }

    #[test]
    fn rejects_malformed_release_dates() {
        assert_eq!(parse_release_date("not-a-date"), None);
    }

    #[test]
    fn generic_release_date_fallback_uses_utc_midnight() {
        let date = Utc.with_ymd_and_hms(2021, 5, 25, 0, 0, 0).single().unwrap();

        assert_eq!(
            parse_release_date("May 25, 2021"),
            Some(ParsedReleaseDate::FullDate(date))
        );
        assert_eq!(date.time(), NaiveTime::MIN);
        assert_eq!(date.time().nanosecond(), 0);
    }

    fn track_number_parses(
        value: &str,
        e_disc: Option<u64>,
        e_track: u64,
        e_track_max: Option<u64>,
        e_is_vinyl: bool,
    ) {
        assert_eq!(
            super::parse_track_number(value),
            Some(super::ParsedTrackNumber {
                disc: e_disc,
                track: e_track,
                track_max: e_track_max,
                is_vinyl: e_is_vinyl,
            })
        );
    }

    #[test]
    fn parse_track_number_rejects_invalid_numbers() {
        assert_eq!(super::parse_track_number("Intro"), None);
        assert_eq!(super::parse_track_number("Side A"), None);
        assert_eq!(super::parse_track_number(""), None);
    }

    #[test]
    fn parse_slash_pair_does_not_slice_inside_multibyte_chars() {
        // 斜杠前的部分以一个多字节字符结尾；旧的字节偏移切片会 panic
        // 而不是返回 None。
        assert_eq!(super::parse_track_number("一/12"), None);
        assert_eq!(super::parse_disc_number("Disc 一/12"), None);
        assert_eq!(super::parse_track_number("A一/12"), None);
    }

    #[test]
    fn parse_track_number_parses_vinyl_numbers() {
        track_number_parses("A0", Some(1), 0, None, true);
        track_number_parses("A1", Some(1), 1, None, true);
        track_number_parses("A2", Some(1), 2, None, true);
        track_number_parses("A3", Some(1), 3, None, true);
        track_number_parses("B1", Some(2), 1, None, true);
        track_number_parses("B2", Some(2), 2, None, true);
        track_number_parses("B3", Some(2), 3, None, true);
        track_number_parses("Z999", Some(26), 999, None, true);
    }

    #[test]
    fn parse_track_number_parses_vinyl_disc_only() {
        track_number_parses("A", Some(1), 1, None, true);
        track_number_parses("B", Some(2), 1, None, true);
        track_number_parses("Z", Some(26), 1, None, true);
    }

    #[test]
    fn parse_track_number_handles_sections_and_vinyl_singles() {
        // 上游 hummingbird numbering.rs 补齐的三类此前整号丢失的形态
        track_number_parses("1-2", None, 1, None, false);
        track_number_parses("A1.2", Some(1), 1, None, true);
        track_number_parses("AA", Some(1), 2, None, true);
        track_number_parses("BBB.1", Some(2), 3, None, true);
        // 文法外的脏值仍拒绝：混合面字母、连写带轨号、section 位置非数字
        assert_eq!(super::parse_track_number("AB"), None);
        assert_eq!(super::parse_track_number("AA1"), None);
        assert_eq!(super::parse_track_number("1-A"), None);
        assert_eq!(super::parse_track_number("1.1.1"), None);
        assert_eq!(super::parse_track_number("Intro"), None);
    }

    #[test]
    fn parse_track_number_lenient_chain_still_covers_dirty_slash_values() {
        // 文法要求全串消费完毕；尾巴带杂字符的斜杠对回退旧链提取
        track_number_parses("3/12 (CD 1)", None, 3, Some(12), false);
        track_number_parses("Track 3/12", None, 3, Some(12), false);
    }

    #[test]
    fn parse_track_number_parses_id3_set() {
        track_number_parses("0/1", None, 0, Some(1), false);
        track_number_parses("1/12", None, 1, Some(12), false);
        track_number_parses("2/12", None, 2, Some(12), false);
        track_number_parses("3/12", None, 3, Some(12), false);
        track_number_parses("9999/9999", None, 9999, Some(9999), false);
    }

    #[test]
    fn parse_track_number_parses_normal() {
        track_number_parses("0", None, 0, None, false);
        track_number_parses("1", None, 1, None, false);
        track_number_parses("2", None, 2, None, false);
        track_number_parses("3", None, 3, None, false);
        track_number_parses("9999", None, 9999, None, false);
    }

    fn disc_number_parses(
        value: &str,
        e_disc: u64,
        e_disc_max: Option<u64>,
        e_disc_subtitle: Option<&str>,
    ) {
        assert_eq!(
            super::parse_disc_number(value),
            Some(super::ParsedDiscNumber {
                disc: e_disc,
                disc_max: e_disc_max,
                disc_subtitle: e_disc_subtitle.map(|s| s.to_string())
            })
        );
    }

    #[test]
    fn parse_disc_number_parses_id3_set() {
        disc_number_parses("0/1", 0, Some(1), None);
        disc_number_parses("1/12", 1, Some(12), None);
        disc_number_parses("2/12", 2, Some(12), None);
        disc_number_parses("3/12", 3, Some(12), None);
        disc_number_parses("9999/9999", 9999, Some(9999), None);
    }

    #[test]
    fn parse_disc_number_parses_disc_subtitles() {
        disc_number_parses("Disc 1 - Subtitle", 1, None, Some("Subtitle"));
        disc_number_parses("Disc 2 - Subtitle", 2, None, Some("Subtitle"));
        disc_number_parses("Disc 3 - Subtitle", 3, None, Some("Subtitle"));
        disc_number_parses("Disc 9999 - Subtitle", 9999, None, Some("Subtitle"));
        disc_number_parses("1 - Subtitle", 1, None, Some("Subtitle"));
        disc_number_parses("1-Subtitle", 1, None, Some("Subtitle"));
        disc_number_parses("1—Subtitle", 1, None, Some("Subtitle"));
        disc_number_parses("1-Subtitle", 1, None, Some("Subtitle"));
        disc_number_parses(
            "Disc 1 - My Very Cool and Unique Subtitle",
            1,
            None,
            Some("My Very Cool and Unique Subtitle"),
        );
    }

    #[test]
    fn parse_disc_number_parses_normal() {
        disc_number_parses("1", 1, None, None);
        disc_number_parses("2", 2, None, None);
        disc_number_parses("3", 3, None, None);
        disc_number_parses("9999", 9999, None, None);
    }
}
