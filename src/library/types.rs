pub mod table;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use gpui::{IntoElement, SharedString};
use sqlx::{Database, Decode, Sqlite, Type, encode::IsNull, error::BoxDynError};

#[derive(sqlx::FromRow)]
pub struct Artist {
    pub name: Option<DBString>,
}

/// A cheaply clonable string (`SharedString`) as read from and written to the
/// library DB, directly usable as GPUI text.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct DBString(pub SharedString);

impl From<String> for DBString {
    fn from(data: String) -> Self {
        Self(SharedString::from(data))
    }
}

impl From<&str> for DBString {
    fn from(data: &str) -> Self {
        Self(SharedString::from(data.to_string()))
    }
}

impl IntoElement for DBString {
    type Element = <SharedString as IntoElement>::Element;

    fn into_element(self) -> Self::Element {
        self.0.into_element()
    }
}

impl<'q, DB: Database> sqlx::Encode<'q, DB> for DBString
where
    String: sqlx::Encode<'q, DB>,
{
    fn encode_by_ref(
        &self,
        out: &mut <DB as Database>::ArgumentBuffer,
    ) -> Result<IsNull, BoxDynError> {
        let string = self.0.to_string();
        <String>::encode_by_ref(&string, out)
    }
}

impl<'r, DB: Database> Decode<'r, DB> for DBString
where
    String: Decode<'r, DB>,
{
    fn decode(value: <DB as Database>::ValueRef<'r>) -> Result<Self, BoxDynError> {
        let data = String::decode(value)?;
        Ok(Self::from(data))
    }
}

impl Type<Sqlite> for DBString {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <String as Type<Sqlite>>::type_info()
    }
}

/// `Album::date_precision` value: only the year is known.
pub const DATE_PRECISION_YEAR: i32 = 0;
/// `Album::date_precision` value: a full `YYYY-MM-DD` date.
pub const DATE_PRECISION_FULL_DATE: i32 = 1;
/// `Album::date_precision` value: year and month (`YYYY-MM`).
pub const DATE_PRECISION_YEAR_MONTH: i32 = 2;

#[derive(sqlx::FromRow, Clone)]
pub struct Album {
    pub id: i64,
    pub title: DBString,
    /// Raw album artist tag, shown in place of the linked artists' names.
    pub artist_display_override: Option<DBString>,
    #[sqlx(default)]
    pub release_date: Option<DBString>,
    #[sqlx(default)]
    /// Date precision: 0 = year only, 1 = full date, 2 = year + month. None if no date info.
    pub date_precision: Option<i32>,
    #[sqlx(default)]
    pub label: Option<DBString>,
    #[sqlx(default)]
    pub catalog_number: Option<DBString>,
    #[sqlx(default)]
    pub isrc: Option<DBString>,
    #[sqlx(default)]
    /// Whether this album uses vinyl-style track numbering (A1, A2, B1, B2, etc.)
    /// When true, disc numbers should be displayed as "SIDE A", "SIDE B", etc.
    pub vinyl_numbering: bool,
}

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct Track {
    pub id: i64,
    pub title: DBString,
    #[sqlx(default)]
    pub album_id: Option<i64>,
    #[sqlx(default)]
    pub track_number: Option<i32>,
    #[sqlx(default)]
    pub disc_number: Option<i32>,
    pub duration: i64,
    #[sqlx(try_from = "String")]
    pub location: PathBuf,
    pub artist_names: Option<DBString>,
    #[sqlx(default)]
    pub disc_subtitle: Option<DBString>,
}

#[derive(sqlx::Type, Clone, Copy, Debug, PartialEq)]
#[repr(i32)]
pub enum PlaylistType {
    User = 0,
    System = 1,
}

#[derive(sqlx::FromRow, Clone, Debug, PartialEq)]
pub struct Playlist {
    pub id: i64,
    pub name: DBString,
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "type")]
    pub playlist_type: PlaylistType,
    pub position: i64,
    pub track_count: i64,
    pub total_duration: i64,
}

impl Playlist {
    /// The Liked Songs playlist is a singleton system playlist stored under
    /// this fixed name; the UI renders it through the `LIKED_SONGS` translation.
    pub fn is_liked_songs(&self) -> bool {
        self.playlist_type == PlaylistType::System && self.name.0.as_str() == "Liked Songs"
    }
}

#[derive(sqlx::FromRow, Clone, Debug, PartialEq)]
pub struct PlaylistItem {
    pub id: i64,
    pub playlist_id: i64,
    pub track_id: i64,
    pub created_at: DateTime<Utc>,
    pub position: i64,
}

#[derive(sqlx::FromRow, Clone)]
pub struct ArtistWithCounts {
    pub id: i64,
    pub name: Option<DBString>,
    pub album_count: i64,
    pub track_count: i64,
}
