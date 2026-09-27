use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::App;
use serde::{Deserialize, Serialize};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use tracing::debug;

use crate::library::{
    Pool,
    types::{ArtistWithCounts, Playlist, PlaylistItem},
};

use super::types::{Album, Artist, Track};

pub async fn create_pool(path: impl AsRef<Path>) -> sqlx::Result<SqlitePool> {
    debug!("Creating database pool at {:?}", path.as_ref());
    let options = SqliteConnectOptions::new()
        .filename(path)
        .optimize_on_close(true, None)
        .synchronous(SqliteSynchronous::Normal)
        .journal_mode(SqliteJournalMode::Wal)
        .create_if_missing(true);
    // 桌面应用读写并发有限：限制连接数（默认 10 个连接 × 各自 page cache 徒增常驻内存）
    let pool = SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(options)
        .await?;

    let migrations = sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(&pool)
        .await;

    if let Err(e) = migrations {
        // old Windows databases can hit line-ending caused hash mismatches on these versions,
        // rewrite their checksums and retry, any other migration failure is fatal
        let recoverable = match &e {
            sqlx::migrate::MigrateError::VersionMismatch(v) => {
                cfg!(target_os = "windows")
                    && matches!(
                        v,
                        20240730163128
                            | 20240730163151
                            | 20240730163200
                            | 20240817201809
                            | 20240817201912
                            | 20240917084650
                            | 20250424090924
                            | 20250512214434
                            | 20250512231103
                            | 20250825224757
                            | 20250825225240
                            | 20250825234341
                            | 20251022214837
                    )
            }
            _ => false,
        };
        if !recoverable {
            return Err(e.into());
        }

        let fix_query = include_str!("../../queries/windows_fix_checksums.sql");
        sqlx::query(fix_query).execute(&pool).await?;

        sqlx::migrate!("./migrations")
            .set_ignore_missing(true)
            .run(&pool)
            .await?;
    }

    Ok(pool)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlbumSortMethod {
    TitleAsc,
    TitleDesc,
    ArtistAsc,
    ArtistDesc,
    ReleaseAsc,
    ReleaseDesc,
    LabelAsc,
    LabelDesc,
    CatalogAsc,
    CatalogDesc,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackSortMethod {
    TitleAsc,
    TitleDesc,
    ArtistAsc,
    ArtistDesc,
    AlbumAsc,
    AlbumDesc,
    DurationAsc,
    DurationDesc,
    TrackNumberAsc,
    TrackNumberDesc,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArtistSortMethod {
    NameAsc,
    NameDesc,
    AlbumsAsc,
    AlbumsDesc,
    TracksAsc,
    TracksDesc,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LikedTrackSortMethod {
    TitleAsc,
    TitleDesc,
    ReleaseOrder,
    ReleaseOrderDesc,
    RecentlyAdded,
    RecentlyAddedAsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PlaylistTrackSortMethod {
    Custom,
    TitleAsc,
    TitleDesc,
    ArtistAsc,
    ArtistDesc,
    AlbumAsc,
    AlbumDesc,
    DurationAsc,
    DurationDesc,
    RecentlyAdded,
    RecentlyAddedAsc,
}

pub async fn list_albums(
    pool: &SqlitePool,
    sort_method: AlbumSortMethod,
) -> sqlx::Result<Vec<(u32, String)>> {
    let query = match sort_method {
        AlbumSortMethod::TitleAsc => {
            include_str!("../../queries/library/find_albums_title_asc.sql")
        }
        AlbumSortMethod::TitleDesc => {
            include_str!("../../queries/library/find_albums_title_desc.sql")
        }
        AlbumSortMethod::ArtistAsc => {
            include_str!("../../queries/library/find_albums_artist_asc.sql")
        }
        AlbumSortMethod::ArtistDesc => {
            include_str!("../../queries/library/find_albums_artist_desc.sql")
        }
        AlbumSortMethod::ReleaseAsc => {
            include_str!("../../queries/library/find_albums_release_asc.sql")
        }
        AlbumSortMethod::ReleaseDesc => {
            include_str!("../../queries/library/find_albums_release_desc.sql")
        }
        AlbumSortMethod::LabelAsc => {
            include_str!("../../queries/library/find_albums_label_asc.sql")
        }
        AlbumSortMethod::LabelDesc => {
            include_str!("../../queries/library/find_albums_label_desc.sql")
        }
        AlbumSortMethod::CatalogAsc => {
            include_str!("../../queries/library/find_albums_catnum_asc.sql")
        }
        AlbumSortMethod::CatalogDesc => {
            include_str!("../../queries/library/find_albums_catnum_desc.sql")
        }
    };

    let albums = sqlx::query_as::<_, (u32, String)>(query)
        .fetch_all(pool)
        .await?;

    Ok(albums)
}

pub async fn list_tracks(
    pool: &SqlitePool,
    sort_method: TrackSortMethod,
) -> sqlx::Result<Vec<(i64, String, Option<i64>, String)>> {
    let query = match sort_method {
        TrackSortMethod::TitleAsc => {
            include_str!("../../queries/library/find_tracks_title_asc.sql")
        }
        TrackSortMethod::TitleDesc => {
            include_str!("../../queries/library/find_tracks_title_desc.sql")
        }
        TrackSortMethod::ArtistAsc => {
            include_str!("../../queries/library/find_tracks_artist_asc.sql")
        }
        TrackSortMethod::ArtistDesc => {
            include_str!("../../queries/library/find_tracks_artist_desc.sql")
        }
        TrackSortMethod::AlbumAsc => {
            include_str!("../../queries/library/find_tracks_album_asc.sql")
        }
        TrackSortMethod::AlbumDesc => {
            include_str!("../../queries/library/find_tracks_album_desc.sql")
        }
        TrackSortMethod::DurationAsc => {
            include_str!("../../queries/library/find_tracks_length_asc.sql")
        }
        TrackSortMethod::DurationDesc => {
            include_str!("../../queries/library/find_tracks_length_desc.sql")
        }
        TrackSortMethod::TrackNumberAsc => {
            include_str!("../../queries/library/find_tracks_number_asc.sql")
        }
        TrackSortMethod::TrackNumberDesc => {
            include_str!("../../queries/library/find_tracks_number_desc.sql")
        }
    };

    let tracks = sqlx::query_as::<_, (i64, String, Option<i64>, String)>(query)
        .fetch_all(pool)
        .await?;

    Ok(tracks)
}

pub async fn list_tracks_in_album(
    pool: &SqlitePool,
    album_id: i64,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = include_str!("../../queries/library/find_tracks_in_album.sql");

    let albums = Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(album_id)
            .fetch_all(pool)
            .await?,
    );

    Ok(albums)
}

pub async fn get_album_by_id(pool: &SqlitePool, album_id: i64) -> sqlx::Result<Arc<Album>> {
    let query = include_str!("../../queries/library/find_album_metadata_by_id.sql");

    let album: Arc<Album> = Arc::new(sqlx::query_as(query).bind(album_id).fetch_one(pool).await?);

    Ok(album)
}

pub async fn get_artist_by_id(pool: &SqlitePool, artist_id: i64) -> sqlx::Result<Arc<Artist>> {
    let query = include_str!("../../queries/library/find_artist_by_id.sql");

    let artist: Arc<Artist> = Arc::new(
        sqlx::query_as(query)
            .bind(artist_id)
            .fetch_one(pool)
            .await?,
    );

    Ok(artist)
}

pub async fn list_artists(
    pool: &SqlitePool,
    sort_method: ArtistSortMethod,
) -> sqlx::Result<Vec<i64>> {
    let query = match sort_method {
        ArtistSortMethod::NameAsc => {
            include_str!("../../queries/library/find_artists_name_asc.sql")
        }
        ArtistSortMethod::NameDesc => {
            include_str!("../../queries/library/find_artists_name_desc.sql")
        }
        ArtistSortMethod::AlbumsAsc => {
            include_str!("../../queries/library/find_artists_albums_asc.sql")
        }
        ArtistSortMethod::AlbumsDesc => {
            include_str!("../../queries/library/find_artists_albums_desc.sql")
        }
        ArtistSortMethod::TracksAsc => {
            include_str!("../../queries/library/find_artists_tracks_asc.sql")
        }
        ArtistSortMethod::TracksDesc => {
            include_str!("../../queries/library/find_artists_tracks_desc.sql")
        }
    };

    let artists: Vec<(i64,)> = sqlx::query_as(query).fetch_all(pool).await?;

    Ok(artists.into_iter().map(|r| r.0).collect())
}

pub async fn list_albums_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Vec<(u32, String)>> {
    let query = include_str!("../../queries/library/find_albums_by_artist.sql");

    let albums = sqlx::query_as::<_, (u32, String)>(query)
        .bind(artist_id)
        .fetch_all(pool)
        .await?;

    Ok(albums)
}

pub async fn get_artist_with_counts(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Arc<ArtistWithCounts>> {
    let query = include_str!("../../queries/library/find_artist_with_counts_by_id.sql");

    let artist: ArtistWithCounts = sqlx::query_as(query)
        .bind(artist_id)
        .fetch_one(pool)
        .await?;

    Ok(Arc::new(artist))
}

pub async fn get_liked_tracks_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
    sort_method: LikedTrackSortMethod,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = match sort_method {
        LikedTrackSortMethod::TitleAsc => {
            include_str!("../../queries/library/find_liked_tracks_by_artist_title_asc.sql")
        }
        LikedTrackSortMethod::TitleDesc => {
            include_str!("../../queries/library/find_liked_tracks_by_artist_title_desc.sql")
        }
        LikedTrackSortMethod::ReleaseOrder => {
            include_str!("../../queries/library/find_liked_tracks_by_artist_release_asc.sql")
        }
        LikedTrackSortMethod::ReleaseOrderDesc => {
            include_str!("../../queries/library/find_liked_tracks_by_artist_release_desc.sql")
        }
        LikedTrackSortMethod::RecentlyAdded => {
            include_str!("../../queries/library/find_liked_tracks_by_artist_recent_desc.sql")
        }
        LikedTrackSortMethod::RecentlyAddedAsc => {
            include_str!("../../queries/library/find_liked_tracks_by_artist_recent_asc.sql")
        }
    };

    let tracks = Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(artist_id)
            .fetch_all(pool)
            .await?,
    );

    Ok(tracks)
}

pub async fn get_all_tracks_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = include_str!("../../queries/library/find_all_tracks_by_artist.sql");

    let tracks = Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(artist_id)
            .fetch_all(pool)
            .await?,
    );

    Ok(tracks)
}

pub async fn get_standalone_tracks_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
    sort_method: LikedTrackSortMethod,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = match sort_method {
        LikedTrackSortMethod::TitleAsc => {
            include_str!("../../queries/library/find_standalone_tracks_by_artist_title_asc.sql")
        }
        LikedTrackSortMethod::TitleDesc => {
            include_str!("../../queries/library/find_standalone_tracks_by_artist_title_desc.sql")
        }
        LikedTrackSortMethod::ReleaseOrder => {
            include_str!("../../queries/library/find_standalone_tracks_by_artist_release_asc.sql")
        }
        LikedTrackSortMethod::ReleaseOrderDesc => {
            include_str!("../../queries/library/find_standalone_tracks_by_artist_release_desc.sql")
        }
        LikedTrackSortMethod::RecentlyAdded => {
            include_str!("../../queries/library/find_standalone_tracks_by_artist_recent_desc.sql")
        }
        LikedTrackSortMethod::RecentlyAddedAsc => {
            include_str!("../../queries/library/find_standalone_tracks_by_artist_recent_asc.sql")
        }
    };

    let tracks = Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(artist_id)
            .fetch_all(pool)
            .await?,
    );

    Ok(tracks)
}

pub async fn get_track_by_id(pool: &SqlitePool, track_id: i64) -> sqlx::Result<Arc<Track>> {
    let query = include_str!("../../queries/library/find_track_by_id.sql");

    let track: Arc<Track> = Arc::new(sqlx::query_as(query).bind(track_id).fetch_one(pool).await?);

    Ok(track)
}

pub async fn get_track_by_path(pool: &SqlitePool, path: &Path) -> sqlx::Result<Option<Arc<Track>>> {
    let query = include_str!("../../queries/library/find_track_by_path.sql");

    let track = sqlx::query_as(query)
        .bind(path.to_string_lossy().as_ref())
        .fetch_optional(pool)
        .await?
        .map(Arc::new);

    Ok(track)
}

/// Lists all albums for searching. Returns (id, title, artist display override, artist names).
#[allow(clippy::type_complexity)]
pub async fn list_albums_search(
    pool: &SqlitePool,
) -> sqlx::Result<Vec<(i64, String, Option<String>, String)>> {
    let query = include_str!("../../queries/library/find_albums_search.sql");

    let albums = sqlx::query_as::<_, (i64, String, Option<String>, String)>(query)
        .fetch_all(pool)
        .await?;

    Ok(albums)
}

/// Lists all tracks for searching. Returns (id, title, artist_names, album_id).
pub async fn list_tracks_search(
    pool: &SqlitePool,
) -> sqlx::Result<Vec<(i64, String, String, Option<i64>)>> {
    let query = include_str!("../../queries/library/find_tracks_search.sql");

    let tracks = sqlx::query_as::<_, (i64, String, String, Option<i64>)>(query)
        .fetch_all(pool)
        .await?;

    Ok(tracks)
}

/// Lists all artists for searching. Returns (id, name).
pub async fn list_artists_search(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artists_search.sql");

    let artists = sqlx::query_as::<_, (i64, String)>(query)
        .fetch_all(pool)
        .await?;

    Ok(artists)
}

/// Every (album id, track location) pair, used to compute album availability
/// (any track still on disk) in a single query instead of one full
/// `list_tracks_in_album` fetch per album.
pub async fn list_album_availability(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_album_availability.sql");

    let rows = sqlx::query_as::<_, (i64, String)>(query)
        .fetch_all(pool)
        .await?;

    Ok(rows)
}

/// Every (artist id, track location) pair, used to compute artist availability
/// (any credited track still on disk) in a single query instead of one
/// full `get_all_tracks_by_artist` fetch per artist row.
///
/// The query must mirror `find_all_tracks_by_artist`: an artist is credited
/// through BOTH `album_artist` (album-level) and `track_artist` (track-level).
/// Querying only one of the two greys out every artist credited the other way,
/// which is how most libraries credit theirs.
pub async fn list_artist_availability(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artist_availability.sql");

    let rows = sqlx::query_as::<_, (i64, String)>(query)
        .fetch_all(pool)
        .await?;

    Ok(rows)
}

pub async fn add_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    track_id: i64,
) -> sqlx::Result<i64> {
    let query = include_str!("../../queries/playlist/add_track.sql");

    let id = sqlx::query(query)
        .bind(playlist_id)
        .bind(track_id)
        .execute(pool)
        .await?
        .last_insert_rowid();

    Ok(id)
}

pub async fn create_playlist(pool: &SqlitePool, name: &str) -> sqlx::Result<i64> {
    let query = include_str!("../../queries/playlist/create_playlist.sql");

    let playlist_id = sqlx::query(query)
        .bind(name)
        .execute(pool)
        .await?
        .last_insert_rowid();

    Ok(playlist_id)
}

pub async fn delete_playlist(pool: &SqlitePool, playlist_id: i64) -> sqlx::Result<()> {
    let query = include_str!("../../queries/playlist/delete_playlist.sql");

    sqlx::query(query).bind(playlist_id).execute(pool).await?;

    Ok(())
}

pub async fn rename_playlist(pool: &SqlitePool, playlist_id: i64, name: &str) -> sqlx::Result<()> {
    let query = include_str!("../../queries/playlist/rename_playlist.sql");

    sqlx::query(query)
        .bind(name)
        .bind(playlist_id)
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn get_all_playlists(pool: &SqlitePool) -> sqlx::Result<Arc<Vec<Playlist>>> {
    let query = include_str!("../../queries/playlist/get_all_playlists.sql");

    let playlists: Vec<Playlist> = sqlx::query_as(query).fetch_all(pool).await?;

    Ok(Arc::new(playlists))
}

pub async fn get_playlist(pool: &SqlitePool, playlist_id: i64) -> sqlx::Result<Arc<Playlist>> {
    let query = include_str!("../../queries/playlist/get_playlist.sql");

    let playlist: Playlist = sqlx::query_as(query)
        .bind(playlist_id)
        .fetch_one(pool)
        .await?;

    Ok(Arc::new(playlist))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlaylistTrackRow {
    #[sqlx(rename = "id")]
    pub playlist_item_id: i64,
    pub track_id: i64,
    pub album_id: i64,
    pub location: String,
}

pub async fn get_playlist_tracks(
    pool: &SqlitePool,
    playlist_id: i64,
) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
    let query = include_str!("../../queries/playlist/get_track_listing.sql");

    let tracks: Vec<PlaylistTrackRow> = sqlx::query_as(query)
        .bind(playlist_id)
        .fetch_all(pool)
        .await?;

    Ok(Arc::new(tracks))
}

pub async fn get_playlist_tracks_sorted(
    pool: &SqlitePool,
    playlist_id: i64,
    sort_method: PlaylistTrackSortMethod,
) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
    let query = match sort_method {
        PlaylistTrackSortMethod::Custom => {
            return get_playlist_tracks(pool, playlist_id).await;
        }
        PlaylistTrackSortMethod::TitleAsc => {
            include_str!("../../queries/playlist/get_track_listing_title_asc.sql")
        }
        PlaylistTrackSortMethod::TitleDesc => {
            include_str!("../../queries/playlist/get_track_listing_title_desc.sql")
        }
        PlaylistTrackSortMethod::ArtistAsc => {
            include_str!("../../queries/playlist/get_track_listing_artist_asc.sql")
        }
        PlaylistTrackSortMethod::ArtistDesc => {
            include_str!("../../queries/playlist/get_track_listing_artist_desc.sql")
        }
        PlaylistTrackSortMethod::AlbumAsc => {
            include_str!("../../queries/playlist/get_track_listing_album_asc.sql")
        }
        PlaylistTrackSortMethod::AlbumDesc => {
            include_str!("../../queries/playlist/get_track_listing_album_desc.sql")
        }
        PlaylistTrackSortMethod::DurationAsc => {
            include_str!("../../queries/playlist/get_track_listing_length_asc.sql")
        }
        PlaylistTrackSortMethod::DurationDesc => {
            include_str!("../../queries/playlist/get_track_listing_length_desc.sql")
        }
        PlaylistTrackSortMethod::RecentlyAdded => {
            include_str!("../../queries/playlist/get_track_listing_recent_desc.sql")
        }
        PlaylistTrackSortMethod::RecentlyAddedAsc => {
            include_str!("../../queries/playlist/get_track_listing_recent_asc.sql")
        }
    };

    let tracks: Vec<PlaylistTrackRow> = sqlx::query_as(query)
        .bind(playlist_id)
        .fetch_all(pool)
        .await?;

    Ok(Arc::new(tracks))
}

pub async fn reorder_playlist(
    pool: &SqlitePool,
    playlist_id: i64,
    new_position: i64,
) -> sqlx::Result<()> {
    let original_position: i64 = sqlx::query_scalar(include_str!(
        "../../queries/playlist/get_playlist_position.sql"
    ))
    .bind(playlist_id)
    .fetch_one(pool)
    .await?;

    let move_query = if original_position < new_position {
        include_str!("../../queries/playlist/move_playlist_down.sql")
    } else if original_position > new_position {
        include_str!("../../queries/playlist/move_playlist_up.sql")
    } else {
        return Ok(());
    };

    // The two UPDATEs in the move script must apply together or not at all:
    // committing only the shift leaves every playlist after it off by one.
    let mut tx = pool.begin().await?;

    sqlx::query(move_query)
        .bind(new_position)
        .bind(original_position)
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn move_playlist_item(
    pool: &SqlitePool,
    item_id: i64,
    new_position: i64,
) -> sqlx::Result<()> {
    // retrieve the current item's position
    let original_item = get_playlist_item(pool, item_id).await?;

    let move_query = if original_item.position < new_position {
        include_str!("../../queries/playlist/move_track_down.sql")
    } else if original_item.position > new_position {
        include_str!("../../queries/playlist/move_track_up.sql")
    } else {
        return Ok(());
    };

    // the shift + reposition statements must apply together or not at all
    let mut tx = pool.begin().await?;

    sqlx::query(move_query)
        .bind(new_position)
        .bind(original_item.position)
        .bind(item_id)
        .bind(original_item.playlist_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn remove_playlist_item(pool: &SqlitePool, item_id: i64) -> sqlx::Result<()> {
    let query = include_str!("../../queries/playlist/remove_track.sql");
    let item = get_playlist_item(pool, item_id).await?;

    // the position compaction and the delete must apply together or not at all
    let mut tx = pool.begin().await?;

    sqlx::query(query)
        .bind(item.playlist_id)
        .bind(item.position)
        .bind(item_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn get_playlist_item(pool: &SqlitePool, item_id: i64) -> sqlx::Result<PlaylistItem> {
    let query = include_str!("../../queries/playlist/select_playlist_item.sql");

    let item: PlaylistItem = sqlx::query_as(query).bind(item_id).fetch_one(pool).await?;

    Ok(item)
}

pub async fn playlist_has_track(
    pool: &SqlitePool,
    playlist_id: i64,
    track_id: i64,
) -> sqlx::Result<Option<i64>> {
    let query = include_str!("../../queries/playlist/playlist_has_track.sql");

    let has_track: Option<i64> = sqlx::query_scalar(query)
        .bind(playlist_id)
        .bind(track_id)
        .fetch_optional(pool)
        .await?;

    Ok(has_track)
}

/// IN-clause chunk size for the playlist batch queries: stays under SQLite's
/// bind-variable cap (999 on legacy builds, 32766 modern) so no selection size
/// can fail the whole batch with "too many SQL variables".
const PLAYLIST_IN_CHUNK: usize = 900;

pub async fn playlist_contains_all_tracks(
    pool: &SqlitePool,
    playlist_id: i64,
    track_ids: &[i64],
) -> sqlx::Result<bool> {
    if track_ids.is_empty() {
        return Ok(true);
    }

    // chunked so the parameter count never hits SQLite's variable cap; each
    // chunk counting its own full size is equivalent to one COUNT over the
    // whole batch (the chunks are disjoint)
    for chunk in track_ids.chunks(PLAYLIST_IN_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT COUNT(DISTINCT track_id) FROM playlist_item \
             WHERE playlist_id = ? AND track_id IN ({placeholders})"
        );

        let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql)).bind(playlist_id);
        for &id in chunk {
            query = query.bind(id);
        }

        let count: i64 = query.fetch_one(pool).await?;
        if count as usize != chunk.len() {
            return Ok(false);
        }
    }

    Ok(true)
}

pub async fn add_tracks_to_playlist_if_missing(
    pool: &SqlitePool,
    playlist_id: i64,
    track_ids: &[i64],
) -> sqlx::Result<()> {
    // one whole-playlist SELECT replaces the per-track check-then-insert pair
    // (3000 round trips to like a 1000-track album); the unique
    // (playlist_id, track_id) index stays as the final guard, and each insert
    // runs the same add_track.sql position query so position semantics are
    // unchanged. the whole batch shares a single transaction instead of one
    // autocommit round trip per track (same shape as import_playlist)
    let query = include_str!("../../queries/playlist/playlist_track_ids.sql");
    let existing: HashSet<i64> = sqlx::query_scalar(query)
        .bind(playlist_id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();

    let insert_query = include_str!("../../queries/playlist/add_track.sql");
    let mut tx = pool.begin().await?;

    for &track_id in track_ids {
        if !existing.contains(&track_id) {
            sqlx::query(insert_query)
                .bind(playlist_id)
                .bind(track_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

pub async fn remove_tracks_from_playlist(
    pool: &SqlitePool,
    playlist_id: i64,
    track_ids: &[i64],
) -> sqlx::Result<()> {
    if track_ids.is_empty() {
        return Ok(());
    }

    // one DELETE for the whole batch replaces the per-track
    // lookup-select-delete-renumber chain (remove_track.sql rewrites every
    // later row per removal: 100 removals from a 3000-item playlist used to
    // cost ~100 O(n) updates plus 200 lookups). the unique
    // (playlist_id, track_id) index serves the delete; positions are
    // re-compacted once in the same transaction. the DELETE is chunked so the
    // parameter count never hits SQLite's variable cap; the chunks share the
    // transaction, so the batch stays all-or-nothing.
    let renumber_query = include_str!("../../queries/playlist/renumber_playlist_positions.sql");

    let mut tx = pool.begin().await?;

    for chunk in track_ids.chunks(PLAYLIST_IN_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "DELETE FROM playlist_item WHERE playlist_id = ? AND track_id IN ({placeholders})"
        );

        let mut delete_query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(playlist_id);
        for &id in chunk {
            delete_query = delete_query.bind(id);
        }

        delete_query.execute(&mut *tx).await?;
    }

    sqlx::query(renumber_query)
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// Lists all (id, name) artist pairs linked to an album.
pub async fn artist_ids_for_album(
    pool: &SqlitePool,
    album_id: i64,
) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artist_ids_for_album.sql");

    let artists = sqlx::query_as::<_, (i64, String)>(query)
        .bind(album_id)
        .fetch_all(pool)
        .await?;

    Ok(artists)
}

/// Lists all (id, name) artist pairs linked to a track, through its album or standalone links.
pub async fn artist_ids_for_track(
    pool: &SqlitePool,
    track_id: i64,
) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artist_ids_for_track.sql");

    let artists = sqlx::query_as::<_, (i64, String)>(query)
        .bind(track_id)
        .fetch_all(pool)
        .await?;

    Ok(artists)
}

pub async fn get_all_tracks(pool: &SqlitePool) -> sqlx::Result<Vec<(String, i64, i64)>> {
    let query = include_str!("../../queries/library/get_all_tracks.sql");

    let tracks: Vec<(String, i64, i64)> = sqlx::query_as(query).fetch_all(pool).await?;

    Ok(tracks)
}

pub async fn list_album_paths(pool: &SqlitePool, album_id: i64) -> sqlx::Result<Vec<String>> {
    let query = include_str!("../../queries/scan/list_album_paths.sql");

    let rows: Vec<(String,)> = sqlx::query_as(query).bind(album_id).fetch_all(pool).await?;

    Ok(rows.into_iter().map(|(path,)| path).collect())
}

pub async fn lyrics_for_track(pool: &SqlitePool, track_id: i64) -> sqlx::Result<Option<String>> {
    let query = include_str!("../../queries/library/get_lyrics_by_track_id.sql");

    let row: Option<(String,)> = sqlx::query_as(query)
        .bind(track_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|(content,)| content))
}

pub trait LibraryAccess {
    fn list_tracks_in_album(&self, album_id: i64) -> sqlx::Result<Arc<Vec<Track>>>;
    fn get_album_by_id(&self, album_id: i64) -> sqlx::Result<Arc<Album>>;
    fn get_artist_by_id(&self, artist_id: i64) -> sqlx::Result<Arc<Artist>>;
    fn get_track_by_id(&self, track_id: i64) -> sqlx::Result<Arc<Track>>;
    fn get_track_by_path(&self, path: &Path) -> sqlx::Result<Option<Arc<Track>>>;
    fn create_playlist(&self, name: &str) -> sqlx::Result<i64>;
    fn delete_playlist(&self, playlist_id: i64) -> sqlx::Result<()>;
    fn rename_playlist(&self, playlist_id: i64, name: &str) -> sqlx::Result<()>;
    fn get_all_playlists(&self) -> sqlx::Result<Arc<Vec<Playlist>>>;
    fn get_playlist_tracks(&self, playlist_id: i64) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>>;
    fn get_playlist_tracks_sorted(
        &self,
        playlist_id: i64,
        sort_method: PlaylistTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>>;
    fn move_playlist_item(&self, item_id: i64, new_position: i64) -> sqlx::Result<()>;
    fn reorder_playlist(&self, playlist_id: i64, new_position: i64) -> sqlx::Result<()>;
    fn get_playlist_item(&self, item_id: i64) -> sqlx::Result<PlaylistItem>;
    fn playlist_has_track(&self, playlist_id: i64, track_id: i64) -> sqlx::Result<Option<i64>>;
    fn playlist_contains_all_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
    ) -> sqlx::Result<bool>;
    fn list_albums_by_artist(&self, artist_id: i64) -> sqlx::Result<Vec<(u32, String)>>;
    fn get_artist_with_counts(&self, artist_id: i64) -> sqlx::Result<Arc<ArtistWithCounts>>;
    fn get_liked_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>>;
    fn get_standalone_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>>;
    fn get_all_tracks_by_artist(&self, artist_id: i64) -> sqlx::Result<Arc<Vec<Track>>>;
    fn artist_ids_for_album(&self, album_id: i64) -> sqlx::Result<Vec<(i64, String)>>;
    fn artist_ids_for_track(&self, track_id: i64) -> sqlx::Result<Vec<(i64, String)>>;
    fn list_album_paths(&self, album_id: i64) -> sqlx::Result<Vec<String>>;
}

// ---------------------------------------------------------------------------
// UI-thread database blocking measurement
//
// Every `LibraryAccess for App` method parks the UI thread on
// `RUNTIME.block_on`. Doctrine §2.3 forbids blocking the main thread and §14
// forbids database access on the UI hot path, so those `block_on`s are debt —
// but which of them is worth converting to a background load is a question for
// data, not intuition (§1/§36). The methods routed through `blocking_query`
// below are the ones the row- and view-construction paths call, which is where
// the call volume is; the remaining methods can be added the same way.
// ---------------------------------------------------------------------------

/// A single query slower than this is logged with its method name.
const UI_QUERY_SLOW_MICROS: u64 = 2_000;
/// Two log lines per accumulated quarter-second of UI-thread blocking: silent
/// while the total stays small, a growing curve when it does not.
const UI_QUERY_REPORT_STEP_MICROS: u64 = 250_000;

static UI_QUERY_CALLS: AtomicU64 = AtomicU64::new(0);
static UI_QUERY_TOTAL_MICROS: AtomicU64 = AtomicU64::new(0);
static UI_QUERY_PEAK_MICROS: AtomicU64 = AtomicU64::new(0);

/// Runs a UI-thread database query, recording how long it parked the UI thread.
fn blocking_query<T>(method: &'static str, future: impl std::future::Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    let result = crate::RUNTIME.block_on(future);
    let micros = started.elapsed().as_micros() as u64;

    let calls = UI_QUERY_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let total = UI_QUERY_TOTAL_MICROS.fetch_add(micros, Ordering::Relaxed) + micros;
    let peak = UI_QUERY_PEAK_MICROS
        .fetch_max(micros, Ordering::Relaxed)
        .max(micros);

    if micros >= UI_QUERY_SLOW_MICROS {
        tracing::warn!(method, micros, "[db] slow ui-thread query");
    }

    if total / UI_QUERY_REPORT_STEP_MICROS != (total - micros) / UI_QUERY_REPORT_STEP_MICROS {
        tracing::info!(
            calls,
            total_ms = total / 1000,
            peak_ms = peak / 1000,
            "[db] ui-thread query total"
        );
    }

    result
}

impl LibraryAccess for App {
    fn list_tracks_in_album(&self, album_id: i64) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_tracks_in_album",
            list_tracks_in_album(&pool.0, album_id),
        )
    }

    fn get_album_by_id(&self, album_id: i64) -> sqlx::Result<Arc<Album>> {
        let pool: &Pool = self.global();
        blocking_query("get_album_by_id", get_album_by_id(&pool.0, album_id))
    }

    fn get_artist_by_id(&self, artist_id: i64) -> sqlx::Result<Arc<Artist>> {
        let pool: &Pool = self.global();
        blocking_query("get_artist_by_id", get_artist_by_id(&pool.0, artist_id))
    }

    fn get_track_by_id(&self, track_id: i64) -> sqlx::Result<Arc<Track>> {
        let pool: &Pool = self.global();
        blocking_query("get_track_by_id", get_track_by_id(&pool.0, track_id))
    }

    fn get_track_by_path(&self, path: &Path) -> sqlx::Result<Option<Arc<Track>>> {
        let pool: &Pool = self.global();
        blocking_query("get_track_by_path", get_track_by_path(&pool.0, path))
    }

    fn create_playlist(&self, name: &str) -> sqlx::Result<i64> {
        let pool: &Pool = self.global();
        blocking_query("create_playlist", create_playlist(&pool.0, name))
    }

    fn delete_playlist(&self, playlist_id: i64) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query("delete_playlist", delete_playlist(&pool.0, playlist_id))
    }

    fn rename_playlist(&self, playlist_id: i64, name: &str) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query(
            "rename_playlist",
            rename_playlist(&pool.0, playlist_id, name),
        )
    }

    fn get_all_playlists(&self) -> sqlx::Result<Arc<Vec<Playlist>>> {
        let pool: &Pool = self.global();
        blocking_query("get_all_playlists", get_all_playlists(&pool.0))
    }

    fn get_playlist_tracks(&self, playlist_id: i64) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_playlist_tracks",
            get_playlist_tracks(&pool.0, playlist_id),
        )
    }

    fn get_playlist_tracks_sorted(
        &self,
        playlist_id: i64,
        sort_method: PlaylistTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_playlist_tracks_sorted",
            get_playlist_tracks_sorted(&pool.0, playlist_id, sort_method),
        )
    }

    fn move_playlist_item(&self, item_id: i64, new_position: i64) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query(
            "move_playlist_item",
            move_playlist_item(&pool.0, item_id, new_position),
        )
    }

    fn reorder_playlist(&self, playlist_id: i64, new_position: i64) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query(
            "reorder_playlist",
            reorder_playlist(&pool.0, playlist_id, new_position),
        )
    }

    fn get_playlist_item(&self, item_id: i64) -> sqlx::Result<PlaylistItem> {
        let pool: &Pool = self.global();
        blocking_query("get_playlist_item", get_playlist_item(&pool.0, item_id))
    }

    fn playlist_has_track(&self, playlist_id: i64, track_id: i64) -> sqlx::Result<Option<i64>> {
        let pool: &Pool = self.global();
        blocking_query(
            "playlist_has_track",
            playlist_has_track(&pool.0, playlist_id, track_id),
        )
    }

    fn playlist_contains_all_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
    ) -> sqlx::Result<bool> {
        let pool: &Pool = self.global();
        blocking_query(
            "playlist_contains_all_tracks",
            playlist_contains_all_tracks(&pool.0, playlist_id, track_ids),
        )
    }

    fn list_albums_by_artist(&self, artist_id: i64) -> sqlx::Result<Vec<(u32, String)>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_albums_by_artist",
            list_albums_by_artist(&pool.0, artist_id),
        )
    }

    fn get_artist_with_counts(&self, artist_id: i64) -> sqlx::Result<Arc<ArtistWithCounts>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_artist_with_counts",
            get_artist_with_counts(&pool.0, artist_id),
        )
    }

    fn get_liked_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_liked_tracks_by_artist",
            get_liked_tracks_by_artist(&pool.0, artist_id, sort_method),
        )
    }

    fn get_standalone_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_standalone_tracks_by_artist",
            get_standalone_tracks_by_artist(&pool.0, artist_id, sort_method),
        )
    }

    fn get_all_tracks_by_artist(&self, artist_id: i64) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_all_tracks_by_artist",
            get_all_tracks_by_artist(&pool.0, artist_id),
        )
    }

    fn artist_ids_for_album(&self, album_id: i64) -> sqlx::Result<Vec<(i64, String)>> {
        let pool: &Pool = self.global();
        blocking_query(
            "artist_ids_for_album",
            artist_ids_for_album(&pool.0, album_id),
        )
    }

    fn artist_ids_for_track(&self, track_id: i64) -> sqlx::Result<Vec<(i64, String)>> {
        let pool: &Pool = self.global();
        blocking_query(
            "artist_ids_for_track",
            artist_ids_for_track(&pool.0, track_id),
        )
    }

    fn list_album_paths(&self, album_id: i64) -> sqlx::Result<Vec<String>> {
        let pool: &Pool = self.global();
        blocking_query("list_album_paths", list_album_paths(&pool.0, album_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remove_tracks_renumbers_positions_contiguously() {
        let (_dir, pool) = crate::test_support::create_test_pool("playlist-remove-test").await;

        let playlist_id = create_playlist(&pool, "test").await.unwrap();
        for n in 1..=6 {
            sqlx::query(
                "INSERT INTO track (title, title_sortable, duration, location) \
                 VALUES ('t', 't', 0, ?)",
            )
            .bind(format!("t{n}.flac"))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO playlist_item (playlist_id, track_id, position) VALUES (?, ?, ?)",
            )
            .bind(playlist_id)
            .bind(n)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
        }

        // remove #2 and #5 in one batch; the rest must compact to 1..4 in order
        remove_tracks_from_playlist(&pool, playlist_id, &[2, 5])
            .await
            .unwrap();

        let positions: Vec<i64> = sqlx::query_scalar(
            "SELECT position FROM playlist_item WHERE playlist_id = ? ORDER BY track_id",
        )
        .bind(playlist_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(positions, vec![1, 2, 3, 4]);
    }
}
