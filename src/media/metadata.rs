use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use smallvec::{SmallVec, smallvec};
use std::path::Path;
use std::str::FromStr;

pub fn parse_rg_float_str(value: &str) -> Option<f64> {
    value.trim().parse().ok()
}

pub fn parse_rg_gain_str(value: &str) -> Option<f64> {
    let s = value.trim();
    // Probe the suffix as bytes: the byte check is safe on any input, and only
    // once it matches is the str slice below guaranteed to sit on a char
    // boundary — a tag ending in a multi-byte char parses as None instead of
    // panicking (same contract as strip_quality_suffix).
    let ends_in_db = s.len() >= 2 && s.as_bytes()[s.len() - 2..].eq_ignore_ascii_case(b"db");
    let s = if ends_in_db {
        s[..s.len() - 2].trim()
    } else {
        s
    };
    s.parse().ok()
}

pub fn parse_r128_gain_str(value: &str) -> Option<f64> {
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
    /// Album artist claim parts: TSO2 split on "&" when every part claims a credited track artist,
    /// the individual ALBUMARTISTS credits, otherwise the raw TPE2 values.
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
    /// Release date metadata. Only one of `date`, `year_month`, or `year` should be set.
    pub date: Option<DateTime<Utc>>,
    /// Release year/month metadata for partial dates like `1995-06`.
    pub year_month: Option<(u16, u8)>,
    /// Optional year field. If the date or year_month field is filled, the year field will be
    /// empty. This field exists because some tagging software uses the date field as a year field,
    /// which cannot be handled properly as a date.
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
    /// Fills title/artist from the filename when the file carries no title tag.
    /// Download tools commonly name files "<artist> - <title>[_MQ].mp3"; the
    /// artist prefix is split off and a trailing quality marker is dropped.
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

/// Downloaders append quality markers like `_MQ`/`_hq` to filenames; drop one
/// so "天若有情_MQ" displays as "天若有情".
fn strip_quality_suffix(stem: &str) -> &str {
    const MARKERS: [&str; 8] = ["_MQ", "_HQ", "_SQ", "_ZQ", "_mq", "_hq", "_sq", "_zq"];
    for marker in MARKERS {
        // strip_suffix is char-boundary safe: returns None when the marker
        // wouldn't align (e.g. the stem ends in a multi-byte char)
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

/// Parses `value` as a `T` only when it is exactly `len` ASCII digits.
fn parse_fixed_digits<T: FromStr>(value: &str, len: usize) -> Option<T> {
    (value.len() == len && value.chars().all(|c| c.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

/// Parses exact ISO release dates before the generic parser.
///
/// This preserves the original precision for `YYYY`, `YYYY-MM`, and `YYYY-MM-DD` values.
/// If a value is not ISO-like, we return `Ok(None)` so the generic parser can still handle
/// free-form tags like `May 25, 2021`. If a value does look ISO-like but is invalid, we return
/// `Err(())` so the generic parser does not silently invent a day or otherwise change precision.
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
            parse_fixed_digits::<u16>(first, 4).ok_or(())?;
            parse_fixed_digits::<u8>(month, 2).ok_or(())?;
            parse_fixed_digits::<u8>(day, 2).ok_or(())?;

            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map(|date| Some(ParsedReleaseDate::FullDate(utc_midnight(date))))
                .map_err(|_| ())
        }
        (None, Some(_)) => Err(()),
    }
}

pub fn parse_release_date(value: &str) -> Option<ParsedReleaseDate> {
    match parse_iso_release_date(value) {
        Ok(Some(date)) => Some(date),
        Err(()) => None,
        Ok(None) => {
            // Non-ISO dates still go through the generic parser, but we pin date-only values to
            // UTC midnight so they do not pick up local-current-time defaults.
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

/// Trailing run of ASCII digits in `s`.
fn trailing_digits(s: &str) -> &str {
    let start = s.rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    &s[start..]
}

/// Leading run of ASCII digits in `s`.
fn leading_digits(s: &str) -> &str {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    &s[..end]
}

/// `"3/12"` (anywhere in the string) -> `(3, Some(12))`.
fn parse_slash_pair(value: &str) -> Option<(u64, Option<u64>)> {
    let (before, after) = value.split_once('/')?;
    let track = trailing_digits(before);
    let total = leading_digits(after);
    if track.is_empty() || total.is_empty() {
        return None;
    }
    Some((track.parse().ok()?, total.parse().ok()))
}

pub fn parse_track_number(value: &str) -> Option<ParsedTrackNumber> {
    let mut parsed = ParsedTrackNumber::default();

    // check for vinyl style numbers: one letter side designator + optional number
    let mut chars = value.chars();
    if let Some(side) = chars.next()
        && side.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_digit())
    {
        parsed.disc = Some((side.to_ascii_uppercase() as u64) - ('A' as u64) + 1);
        parsed.is_vinyl = true;
        parsed.track = trailing_digits(value).parse().ok().unwrap_or(1);
        Some(parsed)
    // check for MP3-style numbers
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

pub fn parse_disc_number(value: &str) -> Option<ParsedDiscNumber> {
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
        // regression: byte-slicing the quality-marker check panicked on stems
        // ending in a multi-byte char ("...耳机") and killed the playback thread
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

        // real tags are never overwritten
        let mut metadata = Metadata {
            name: Some("Real Title".to_string()),
            ..Metadata::default()
        };
        metadata.fill_from_filename(Path::new("D:\\music\\云狗蛋 - 天若有情_MQ.mp3"));
        assert_eq!(metadata.name.as_deref(), Some("Real Title"));
    }

    #[test]
    fn rg_gain_handles_multibyte_suffix() {
        // regression: byte-slicing the "db" suffix check panicked on tag values
        // ending in a multi-byte char instead of yielding None
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
