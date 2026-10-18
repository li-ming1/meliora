use std::{collections::HashSet, path::Path, sync::Arc};

use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use tracing::debug;

use super::types::{Album, Artist, ArtistWithCounts, Playlist, PlaylistItem, Track};

/// Migration versions whose recorded checksum can mismatch purely because an
/// old Windows database wrote them with different line endings; the open path
/// rewrites those checksums and retries instead of failing.
fn windows_line_ending_recovery_versions() -> &'static [i64] {
    &[
        20240730163128,
        20240730163151,
        20240730163200,
        20240817201809,
        20240817201912,
        20240917084650,
        20250424090924,
        20250512214434,
        20250512231103,
        20250825224757,
        20250825225240,
        20250825234341,
        20251022214837,
    ]
}

pub async fn create_pool(path: impl AsRef<Path>) -> sqlx::Result<SqlitePool> {
    debug!("Creating database pool at {:?}", path.as_ref());
    let options = SqliteConnectOptions::new()
        .filename(path)
        .optimize_on_close(true, None)
        .synchronous(SqliteSynchronous::Normal)
        .journal_mode(SqliteJournalMode::Wal)
        // SQLite 的默认页缓存是 2 MB/连接（`cache_size=-2000`），而这个库把封面
        // blob 存在表里（实测 artwork 占库体积 42%），所以几十张专辑就能越过 2 MB。
        // 配对实测（真 migrations 建的 schema、20k 项歌单、OS 文件缓存已热、min-of-7）：
        // 有序列表查询 79.4ms(-2000) → 21.3ms(-8000) = **3.7 倍**，再大到 -32000/-131072
        // 都不再改善（22.5/23.1ms，已到拐点）；全表扫描 19.3 → 12.4ms。
        // 同一批数字也推翻了"给 playlist_item 加 (playlist_id,track_id,position)
        // 覆盖索引"的设想：20k 档的悬崖是页缓存抖动，不是计划形状。
        // 预算：这是**每连接**的额度，3 连接最坏 24 MB，与下面把连接数压到 3
        // 的取舍同源，故取拐点左侧的保守值。
        .pragma("cache_size", "-8000")
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
                cfg!(target_os = "windows") && windows_line_ending_recovery_versions().contains(v)
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

    sqlx::query_as::<_, (u32, String)>(query)
        .fetch_all(pool)
        .await
}

pub async fn list_tracks(
    pool: &SqlitePool,
    sort_method: TrackSortMethod,
) -> sqlx::Result<Vec<(i64, Option<i64>, String)>> {
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

    sqlx::query_as::<_, (i64, Option<i64>, String)>(query)
        .fetch_all(pool)
        .await
}

pub async fn list_tracks_in_album(
    pool: &SqlitePool,
    album_id: i64,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = include_str!("../../queries/library/find_tracks_in_album.sql");

    Ok(Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(album_id)
            .fetch_all(pool)
            .await?,
    ))
}

pub async fn get_album_by_id(pool: &SqlitePool, album_id: i64) -> sqlx::Result<Arc<Album>> {
    let query = include_str!("../../queries/library/find_album_metadata_by_id.sql");

    Ok(Arc::new(
        sqlx::query_as(query).bind(album_id).fetch_one(pool).await?,
    ))
}

pub async fn get_artist_by_id(pool: &SqlitePool, artist_id: i64) -> sqlx::Result<Arc<Artist>> {
    let query = include_str!("../../queries/library/find_artist_by_id.sql");

    Ok(Arc::new(
        sqlx::query_as(query)
            .bind(artist_id)
            .fetch_one(pool)
            .await?,
    ))
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

    sqlx::query_scalar::<_, i64>(query).fetch_all(pool).await
}

pub async fn list_albums_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Vec<(u32, String)>> {
    let query = include_str!("../../queries/library/find_albums_by_artist.sql");

    sqlx::query_as::<_, (u32, String)>(query)
        .bind(artist_id)
        .fetch_all(pool)
        .await
}

pub async fn get_artist_with_counts(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Arc<ArtistWithCounts>> {
    let query = include_str!("../../queries/library/find_artist_with_counts_by_id.sql");

    Ok(Arc::new(
        sqlx::query_as(query)
            .bind(artist_id)
            .fetch_one(pool)
            .await?,
    ))
}

pub async fn list_liked_tracks_by_artist(
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

    Ok(Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(artist_id)
            .fetch_all(pool)
            .await?,
    ))
}

pub async fn list_all_tracks_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = include_str!("../../queries/library/find_all_tracks_by_artist.sql");

    Ok(Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(artist_id)
            .fetch_all(pool)
            .await?,
    ))
}

pub async fn list_standalone_tracks_by_artist(
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

    Ok(Arc::new(
        sqlx::query_as::<_, Track>(query)
            .bind(artist_id)
            .fetch_all(pool)
            .await?,
    ))
}

pub async fn get_track_by_id(pool: &SqlitePool, track_id: i64) -> sqlx::Result<Arc<Track>> {
    let query = include_str!("../../queries/library/find_track_by_id.sql");

    Ok(Arc::new(
        sqlx::query_as(query).bind(track_id).fetch_one(pool).await?,
    ))
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

    sqlx::query_as::<_, (i64, String, Option<String>, String)>(query)
        .fetch_all(pool)
        .await
}

/// Lists all tracks for searching. Returns (id, title, artist_names, album_id).
pub async fn list_tracks_search(
    pool: &SqlitePool,
) -> sqlx::Result<Vec<(i64, String, String, Option<i64>)>> {
    let query = include_str!("../../queries/library/find_tracks_search.sql");

    sqlx::query_as::<_, (i64, String, String, Option<i64>)>(query)
        .fetch_all(pool)
        .await
}

/// Lists all artists for searching. Returns (id, name).
pub async fn list_artists_search(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artists_search.sql");

    sqlx::query_as::<_, (i64, String)>(query)
        .fetch_all(pool)
        .await
}

/// Every (album id, track location) pair, used to compute album availability
/// (any track still on disk) in a single query instead of one full
/// `list_tracks_in_album` fetch per album.
pub async fn list_album_availability(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_album_availability.sql");

    sqlx::query_as::<_, (i64, String)>(query)
        .fetch_all(pool)
        .await
}

/// Every `(track_id, artist_id)` credit pair, used to compute artist
/// availability (any credited track still on disk) in one query instead of
/// one full `list_all_tracks_by_artist` fetch per artist row. Yields integer
/// pairs against the track table's own ids — the caller joins them with the
/// base `(location, id, album_id)` rows in Rust, so no credit re-transfers
/// its location string over SQL.
///
/// The query must mirror `find_all_tracks_by_artist`: an artist is credited
/// through BOTH `album_artist` (album-level) and `track_artist` (track-level).
/// Querying only one of the two greys out every artist credited the other way,
/// which is how most libraries credit theirs.
pub async fn list_track_artist_credits(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, i64)>> {
    let query = include_str!("../../queries/library/find_track_artist_credits.sql");

    sqlx::query_as::<_, (i64, i64)>(query).fetch_all(pool).await
}

pub async fn add_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    track_id: i64,
) -> sqlx::Result<i64> {
    let query = include_str!("../../queries/playlist/add_track.sql");

    Ok(sqlx::query(query)
        .bind(playlist_id)
        .bind(track_id)
        .execute(pool)
        .await?
        .last_insert_rowid())
}

/// 批量追加曲目进歌单并可选搬到目标位，返回新条目的 item_id。逐条仍跑同一
/// add_track.sql，位置语义与循环调用 add_playlist_item 完全一致；区别在于
/// 整批共享一次连接租借与一次 commit，取代每首歌一次 autocommit 往返
/// （同 add_tracks_to_playlist_if_missing 的批量理由）。target_position 命中
/// 时在同一事务内一次区间平移让位 + 逐条定位块内条目，全部生效或全部回滚。
pub async fn add_playlist_items(
    pool: &SqlitePool,
    playlist_id: i64,
    track_ids: &[i64],
    target_position: Option<i64>,
) -> sqlx::Result<Vec<i64>> {
    let insert_query = include_str!("../../queries/playlist/add_track.sql");

    let mut tx = pool.begin().await?;

    // 追加块的起始 position：add_track.sql 恒取 MAX+1，块内逐条 +1 连续排布
    let first_position: Option<i64> = match target_position {
        Some(_) => {
            let max: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(position), 0) FROM playlist_item WHERE playlist_id = ?",
            )
            .bind(playlist_id)
            .fetch_one(&mut *tx)
            .await?;
            Some(max + 1)
        }
        None => None,
    };

    let mut item_ids = Vec::with_capacity(track_ids.len());
    for &track_id in track_ids {
        let item_id = sqlx::query(insert_query)
            .bind(playlist_id)
            .bind(track_id)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        item_ids.push(item_id);
    }

    // 目标位取自既有条目的 position（或其 +1），不会大于追加块首（MAX+1），
    // 故只需上移分支。一次平移（position >= 目标位整体 +块长）让既有行与刚
    // 追加的块同时让位，再逐条定位到目标位 + offset：O(受让行数+块长) 行更新，
    // 避免逐条「区间上移+定位」的 O(块长×区间) 写放大（15 曲专辑拖到 3000 曲
    // 歌单顶由 ~4.5 万行降为 ~3 千行）。(playlist_id, position) 仅普通索引
    // （idx_playlist_item_playlist_position），平移中间态无唯一约束冲突。
    // 等于块顶即已在目标位，跳过（对齐 move_playlist_item 的同位早退）。
    if let (Some(new_position), Some(first_position)) = (target_position, first_position)
        && new_position < first_position
    {
        sqlx::query(
            "UPDATE playlist_item SET position = position + ? \
                 WHERE playlist_id = ? AND position >= ?",
        )
        .bind(item_ids.len() as i64)
        .bind(playlist_id)
        .bind(new_position)
        .execute(&mut *tx)
        .await?;
        for (offset, &item_id) in item_ids.iter().enumerate() {
            sqlx::query("UPDATE playlist_item SET position = ? WHERE id = ?")
                .bind(new_position + offset as i64)
                .bind(item_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(item_ids)
}

pub async fn create_playlist(pool: &SqlitePool, name: &str) -> sqlx::Result<i64> {
    let query = include_str!("../../queries/playlist/create_playlist.sql");

    Ok(sqlx::query(query)
        .bind(name)
        .execute(pool)
        .await?
        .last_insert_rowid())
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

pub async fn list_all_playlists(pool: &SqlitePool) -> sqlx::Result<Arc<Vec<Playlist>>> {
    let query = include_str!("../../queries/playlist/get_all_playlists.sql");

    Ok(Arc::new(sqlx::query_as(query).fetch_all(pool).await?))
}

pub async fn get_playlist(pool: &SqlitePool, playlist_id: i64) -> sqlx::Result<Arc<Playlist>> {
    let query = include_str!("../../queries/playlist/get_playlist.sql");

    Ok(Arc::new(
        sqlx::query_as(query)
            .bind(playlist_id)
            .fetch_one(pool)
            .await?,
    ))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlaylistTrackRow {
    #[sqlx(rename = "id")]
    pub playlist_item_id: i64,
    pub track_id: i64,
    pub album_id: i64,
    pub location: String,
}

pub async fn list_playlist_tracks(
    pool: &SqlitePool,
    playlist_id: i64,
) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
    let query = include_str!("../../queries/playlist/get_track_listing.sql");

    // 顺序留在 SQL 的 `ORDER BY pl.position`。试过搬到 Rust（SELECT 带 position，
    // 取完 sort_by_key），计划证明买不到东西：SELECT 一旦需要 position，
    // `(playlist_id, track_id)` 就不再覆盖，planner 仍走
    // `idx_playlist_item_playlist_position` + rowid 回查，访问顺序与随机回查都没变，
    // 只是多排一次序。sqlx 口径同轮实测 5000 行 23.9ms / 10000 行 39.0ms，
    // 与改动前分不出来（§44 第十八轮·自我证伪，改动已回滚）。
    // 真要治 2 万行那一档，得让索引同时覆盖 track_id 与 position
    // ——`(playlist_id, track_id, position)` 是迁移成本，本机歌单规模远未触发。
    // 契约测试 `list_playlist_tracks_returns_position_ascending` 与形态无关，保留。
    Ok(Arc::new(
        sqlx::query_as(query)
            .bind(playlist_id)
            .fetch_all(pool)
            .await?,
    ))
}

pub async fn list_playlist_tracks_sorted(
    pool: &SqlitePool,
    playlist_id: i64,
    sort_method: PlaylistTrackSortMethod,
) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
    let query = match sort_method {
        PlaylistTrackSortMethod::Custom => {
            return list_playlist_tracks(pool, playlist_id).await;
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

    Ok(Arc::new(
        sqlx::query_as(query)
            .bind(playlist_id)
            .fetch_all(pool)
            .await?,
    ))
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

    sqlx::query_as(query).bind(item_id).fetch_one(pool).await
}

pub async fn playlist_has_track(
    pool: &SqlitePool,
    playlist_id: i64,
    track_id: i64,
) -> sqlx::Result<Option<i64>> {
    let query = include_str!("../../queries/playlist/playlist_has_track.sql");

    sqlx::query_scalar(query)
        .bind(playlist_id)
        .bind(track_id)
        .fetch_optional(pool)
        .await
}

/// 一个曲目已在的全部歌单 id 集合。add-to-playlist 弹窗的逐歌单
/// playlist_has_track（N 个歌单 = N 次 UI 线程 block_on）收敛为这一次查询。
pub async fn playlist_ids_for_track(
    pool: &SqlitePool,
    track_id: i64,
) -> sqlx::Result<FxHashSet<i64>> {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT playlist_id FROM playlist_item WHERE track_id = ?")
            .bind(track_id)
            .fetch_all(pool)
            .await?;

    Ok(ids.into_iter().collect())
}

/// IN-clause chunk size for the playlist batch queries: stays under SQLite's
/// bind-variable cap (999 on legacy builds, 32766 modern) so no selection size
/// can fail the whole batch with "too many SQL variables".
const PLAYLIST_IN_CHUNK: usize = 900;

/// `?,?,?` — the bind-slot list for an IN clause of `n` parameters.
/// Shared with `library/types/table.rs`'s chunked row prefetch.
pub(crate) fn in_placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

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
        let sql = format!(
            "SELECT COUNT(DISTINCT track_id) FROM playlist_item \
             WHERE playlist_id = ? AND track_id IN ({})",
            in_placeholders(chunk.len())
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
        let sql = format!(
            "DELETE FROM playlist_item WHERE playlist_id = ? AND track_id IN ({})",
            in_placeholders(chunk.len())
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

    sqlx::query_as::<_, (i64, String)>(query)
        .bind(album_id)
        .fetch_all(pool)
        .await
}

/// Lists all (id, name) artist pairs linked to a track, through its album or standalone links.
pub async fn artist_ids_for_track(
    pool: &SqlitePool,
    track_id: i64,
) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artist_ids_for_track.sql");

    sqlx::query_as::<_, (i64, String)>(query)
        .bind(track_id)
        .fetch_all(pool)
        .await
}

pub async fn list_all_tracks(pool: &SqlitePool) -> sqlx::Result<Vec<(String, i64, i64)>> {
    let query = include_str!("../../queries/library/get_all_tracks.sql");

    sqlx::query_as(query).fetch_all(pool).await
}

pub async fn list_album_paths(pool: &SqlitePool, album_id: i64) -> sqlx::Result<Vec<String>> {
    let query = include_str!("../../queries/scan/list_album_paths.sql");

    sqlx::query_scalar::<_, String>(query)
        .bind(album_id)
        .fetch_all(pool)
        .await
}

pub async fn lyrics_for_track(pool: &SqlitePool, track_id: i64) -> sqlx::Result<Option<String>> {
    let query = include_str!("../../queries/library/get_lyrics_by_track_id.sql");

    let row: Option<(String,)> = sqlx::query_as(query)
        .bind(track_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|(content,)| content))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认排序的语义契约：返回的行必须严格按 `playlist_item.position` 升序。
    /// 排序由 SQL 的 `ORDER BY pl.position` 保证（搬去 Rust 的尝试已回滚），
    /// 这条断言把契约钉住——position 与 track_id 刻意反序，任何"忘了排序"
    /// 或"按 track_id 返回"都会红。
    #[tokio::test]
    async fn list_playlist_tracks_returns_position_ascending() {
        let (_dir, pool) = crate::test_support::create_test_pool("playlist-order-test").await;
        let playlist_id = create_playlist(&pool, "order").await.unwrap();

        let mut track_ids = Vec::new();
        for n in 1..=3 {
            sqlx::query(
                "INSERT INTO track (title, title_sortable, duration, location) \
                 VALUES ('t', 't', 0, ?)",
            )
            .bind(format!("order-{n}.flac"))
            .execute(&pool)
            .await
            .unwrap();
            let (track_id,): (i64,) = sqlx::query_as("SELECT id FROM track WHERE location = ?")
                .bind(format!("order-{n}.flac"))
                .fetch_one(&pool)
                .await
                .unwrap();
            track_ids.push(track_id);
        }

        // track 1 排最后、track 3 排最前 —— 与 track_id 顺序完全相反
        for (rank, track_id) in [track_ids[2], track_ids[0], track_ids[1]]
            .iter()
            .enumerate()
        {
            sqlx::query(
                "INSERT INTO playlist_item (playlist_id, track_id, position) VALUES (?, ?, ?)",
            )
            .bind(playlist_id)
            .bind(*track_id)
            .bind((rank as i64 + 1) * 10)
            .execute(&pool)
            .await
            .unwrap();
        }

        let rows = list_playlist_tracks(&pool, playlist_id).await.unwrap();
        let got: Vec<i64> = rows.iter().map(|row| row.track_id).collect();
        assert_eq!(
            got,
            vec![track_ids[2], track_ids[0], track_ids[1]],
            "list_playlist_tracks 必须按 position 升序返回"
        );
    }

    /// 标题档的语义契约：降序必须是升序的**完全镜像**，且不得与升序同序列。
    /// 起因是 `find_tracks_title_desc.sql` 曾把 `DESC` 只写在最后一个键上
    /// （SQLite 语法 `ORDER BY a, b, c DESC` = a ASC, b ASC, c DESC），而前两键在本机
    /// 数据上从不并列 ⇒ 那个 DESC 永不生效，"标题降序"返回的就是升序列表。
    /// 数据里刻意放了 `(title_sortable, album_id)` 的两处并列，好让第二键的方向也被检查到。
    #[tokio::test]
    async fn list_tracks_title_desc_is_mirror_of_asc() {
        let (_dir, pool) = crate::test_support::create_test_pool("title-sort-mirror").await;
        // 测试池开着外键约束，track.album_id 必须指向真实专辑
        for id in [1i64, 2] {
            sqlx::query("INSERT INTO album (id, title, title_sortable) VALUES (?, ?, ?)")
                .bind(id)
                .bind(format!("al{id}"))
                .bind(format!("al{id}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        for (title_sortable, album_id, location) in [
            ("a", 1, "x1.flac"),
            ("b", 2, "x2.flac"),
            ("b", 1, "x5.flac"),
            ("c", 1, "x3.flac"),
            ("c", 2, "x4.flac"),
        ] {
            sqlx::query(
                "INSERT INTO track (title, title_sortable, duration, location, album_id) \
                 VALUES (?, ?, 0, ?, ?)",
            )
            .bind(title_sortable)
            .bind(title_sortable)
            .bind(location)
            .bind(album_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        let asc = list_tracks(&pool, TrackSortMethod::TitleAsc).await.unwrap();
        let desc = list_tracks(&pool, TrackSortMethod::TitleDesc)
            .await
            .unwrap();
        let mut mirrored = asc.clone();
        mirrored.reverse();
        assert_ne!(
            asc, desc,
            "降序档位返回了与升序相同的序列 ⇒ DESC 又挂错键了"
        );
        assert_eq!(desc, mirrored, "降序必须是升序的完全镜像（三键整体反转）");
    }

    /// 标题族那枚候选索引到底值多少——**在 sqlx 的真实链路上量**，
    /// 因为落地决策不能靠 python 侧的跨 harness 比值（既有纪律：改动属于哪条链路，对照就在哪条链路做）。
    ///
    /// 配对设计（本仓已记下"同一份库上加索引前后不算配对"，所以这里用两份库）：
    /// 灌 6 万行 → `wal_checkpoint(TRUNCATE)` → 关池 → **按字节复制成两份同内容库** →
    /// 只在 B 上建候选索引 → 两边都 ANALYZE + checkpoint → 交替测量（**每遍交换 A/B 先后**），
    /// min-of-4。同时打印两侧的 `EXPLAIN QUERY PLAN`，因为"计划有没有从临时树变成索引扫描"
    /// 才是机制证据，计时只是它的价格。
    ///
    /// `#[ignore]`：要灌 6 万行。运行：
    /// `cargo test --release --features kugou report_title_sort_index_benefit -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "cost probe: title-family index benefit, seeds 60k rows, explicit run only"]
    async fn report_title_sort_index_benefit() {
        // sqlx 的 `SqliteConnectOptions` 默认开着外键，所以 `track.album_id` 必须指向真实专辑行。
        const ALBUMS: usize = 4_000;
        const ROWS: usize = 60_000;
        // `EXPLAIN` 必须用 `&'static` 串（sqlx 0.9 禁运行期拼接），故走 `concat!`。
        const EQP_ASC: &str = concat!(
            "EXPLAIN QUERY PLAN ",
            include_str!("../../queries/library/find_tracks_title_asc.sql")
        );
        const EQP_DESC: &str = concat!(
            "EXPLAIN QUERY PLAN ",
            include_str!("../../queries/library/find_tracks_title_desc.sql")
        );

        async fn plan(pool: &SqlitePool, sql: &'static str) -> String {
            let rows: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sql).fetch_all(pool).await.unwrap();
            rows.iter()
                .map(|r| r.3.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        }

        let (dir, seed_pool) = crate::test_support::create_test_pool("meliora-titlesort").await;
        let seed_path = dir.join("library.db");
        {
            let mut tx = seed_pool.begin().await.unwrap();
            for i in 0..ALBUMS {
                sqlx::query("INSERT INTO album (id, title, title_sortable) VALUES (?, ?, ?)")
                    .bind(i as i64 + 1)
                    .bind(format!("AL{i:05}"))
                    .bind(format!("al{i:05}"))
                    .fetch_optional(&mut *tx)
                    .await
                    .unwrap();
            }
            for i in 0..ROWS {
                sqlx::query(
                    "INSERT INTO track (title, title_sortable, album_id, duration, location, folder) \
                     VALUES (?, ?, ?, 180, ?, ?)",
                )
                .bind(format!("T{i:06}"))
                .bind(format!("ts{:05}", i % 20_000))
                .bind((i % ALBUMS) as i64 + 1)
                .bind(format!("lib\\d{:03}\\f{i:06}.flac", i % 300))
                .bind(format!("d{:03}", i % 300))
                .fetch_optional(&mut *tx)
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
        }
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_optional(&seed_pool)
            .await
            .unwrap();
        sqlx::query("ANALYZE").execute(&seed_pool).await.unwrap();
        seed_pool.close().await;

        // 按字节复制成两份同内容库（-wal 已 TRUNCATE、池已关闭 ⇒ 无半截事务）
        let a_path = dir.join("noidx.db");
        let b_path = dir.join("withidx.db");
        std::fs::copy(&seed_path, &a_path).unwrap();
        std::fs::copy(&seed_path, &b_path).unwrap();
        let a = create_pool(&a_path).await.unwrap();
        let b = create_pool(&b_path).await.unwrap();
        // `create_pool` 会跑迁移，而 20261017120000 已经把同名索引装进两份库 ⇒
        // 不显式拆掉对照组，这里测的就是"有索引 vs 有索引"。判"对照是否真的来自旧形态"的规矩。
        sqlx::query("DROP INDEX IF EXISTS idx_track_title_sort")
            .execute(&a)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_track_title_sort ON track(title_sortable, album_id, location COLLATE NOCASE)")
            .execute(&b)
            .await
            .unwrap();
        for pool in [&a, &b] {
            sqlx::query("ANALYZE").execute(pool).await.unwrap();
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_optional(pool)
                .await
                .unwrap();
        }

        println!("rows = {ROWS} / albums = {ALBUMS}（两份库内容同源，只差那一枚索引）");
        for (label, sql, sort) in [
            ("asc", EQP_ASC, TrackSortMethod::TitleAsc),
            ("desc", EQP_DESC, TrackSortMethod::TitleDesc),
        ] {
            println!("  [{label}] A 无索引计划: {}", plan(&a, sql).await);
            println!("  [{label}] B 有索引计划: {}", plan(&b, sql).await);

            let mut ta: Vec<u128> = Vec::new();
            let mut tb: Vec<u128> = Vec::new();
            for rep in 0..4 {
                // 每遍交换 A/B 先后：谁先跑谁可能吃到页缓存/文件系统预读的便宜
                let (ra, rb) = if rep % 2 == 0 {
                    let t0 = std::time::Instant::now();
                    let ra = list_tracks(&a, sort).await.unwrap();
                    ta.push(t0.elapsed().as_micros());
                    let t1 = std::time::Instant::now();
                    let rb = list_tracks(&b, sort).await.unwrap();
                    tb.push(t1.elapsed().as_micros());
                    (ra, rb)
                } else {
                    let t1 = std::time::Instant::now();
                    let rb = list_tracks(&b, sort).await.unwrap();
                    tb.push(t1.elapsed().as_micros());
                    let t0 = std::time::Instant::now();
                    let ra = list_tracks(&a, sort).await.unwrap();
                    ta.push(t0.elapsed().as_micros());
                    (ra, rb)
                };
                assert_eq!(ra, rb, "{label}: 两侧序列必须相同");
            }
            let ma = ta.iter().min().unwrap();
            let mb = tb.iter().min().unwrap();
            println!(
                "  [{label}] A 无索引 min={:.1}ms {ta:?} | B 有索引 min={:.1}ms {tb:?} | 收益 {:.2}x",
                *ma as f64 / 1000.0,
                *mb as f64 / 1000.0,
                *ma as f64 / (*mb).max(1) as f64
            );
        }
        // —— 写侧成本：这枚索引是本轮刚落地的，代价必须在同一条链路（sqlx + 生产 UPSERT 语句）上有数 ——
        // 两种形状：`fresh` 新位置（走 INSERT 分支）；`reupsert` 重写已有位置（≈定向重扫命中未变行，
        // 这才是扫描期最常见的写形态，也是索引维护最贵的一种：值没变也要删旧插新索引项）。
        const UPSERT: &str = include_str!("../../queries/scan/create_track.sql");
        const W_N: i64 = 3_000;
        async fn write_phase(pool: &SqlitePool, base: i64, reupsert: bool) -> (u128, i64) {
            let before = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM track")
                .fetch_one(pool)
                .await
                .unwrap();
            let t0 = std::time::Instant::now();
            let mut tx = pool.begin().await.unwrap();
            for i in 0..W_N {
                // 必须与 fresh 分支用同一套 id 派生，否则"重写已有行"会变成"再插一批新行"，
                // 测不到定向重扫的真实形状（下面的"行数增量应为 0"就是这条的自检）。
                let id = if reupsert {
                    base + 1_000_000 + (i % W_N)
                } else {
                    base + 1_000_000 + i
                };
                sqlx::query_as::<_, (i64,)>(UPSERT)
                    .bind(format!("T{id}"))
                    .bind(format!("ts{:05}", id % 20_000))
                    .bind(id % 4_000 + 1)
                    .bind(None::<i64>)
                    .bind(None::<i64>)
                    .bind(180i64)
                    .bind(format!("lib\\wr\\f{id:07}.flac"))
                    .bind(None::<String>)
                    .bind(Some(format!("Ar{id}")))
                    .bind("wr")
                    .bind(None::<f64>)
                    .bind(None::<f64>)
                    .bind(None::<f64>)
                    .bind(None::<f64>)
                    .bind(None::<String>)
                    .bind(Some(format!("Ar{id}")))
                    .bind(None::<String>)
                    .bind(None::<String>)
                    .bind(None::<i64>)
                    .bind(None::<String>)
                    .bind(None::<i64>)
                    .fetch_optional(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
            let elapsed = t0.elapsed().as_micros();
            let after = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM track")
                .fetch_one(pool)
                .await
                .unwrap();
            (elapsed, after - before)
        }

        for (shape, reupsert) in [("fresh 新行", false), ("reupsert 重写已有行", true)] {
            let mut wa: Vec<u128> = Vec::new();
            let mut wb: Vec<u128> = Vec::new();
            let mut delta = (-1i64, -1i64);
            for rep in 0..3 {
                let base = 10_000 * (rep as i64 + 1) * 1_000_000;
                if rep % 2 == 0 {
                    let (a, da) = write_phase(&a, base, reupsert).await;
                    let (b, db_) = write_phase(&b, base, reupsert).await;
                    wa.push(a);
                    wb.push(b);
                    delta = (da, db_);
                } else {
                    let (b, db_) = write_phase(&b, base, reupsert).await;
                    let (a, da) = write_phase(&a, base, reupsert).await;
                    wa.push(a);
                    wb.push(b);
                    delta = (da, db_);
                }
            }
            let (ma, mb) = (wa.iter().min().unwrap(), wb.iter().min().unwrap());
            println!(
                "  [写:{shape}] A 无索引 min={:.1}ms {wa:?} | B 有索引 min={:.1}ms {wb:?} | 写放大 {:+.1}% | 行数增量 A={} B={}（reupsert 应为 0）",
                *ma as f64 / 1000.0,
                *mb as f64 / 1000.0,
                (*mb as f64 / *ma as f64 - 1.0) * 100.0,
                delta.0,
                delta.1
            );
        }
        a.close().await;
        b.close().await;
    }

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

    #[tokio::test]
    async fn add_playlist_items_moves_block_contiguously() {
        let (_dir, pool) = crate::test_support::create_test_pool("playlist-add-move-test").await;

        let playlist_id = create_playlist(&pool, "test").await.unwrap();
        for n in 0..=4i64 {
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
            .bind(n + 1)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
        }
        // 追加块用的 3 首新曲目（track id 6..=8），尚未入列
        for n in 5..=7i64 {
            sqlx::query(
                "INSERT INTO track (title, title_sortable, duration, location) \
                 VALUES ('t', 't', 0, ?)",
            )
            .bind(format!("t{n}.flac"))
            .execute(&pool)
            .await
            .unwrap();
        }

        // 追加 track 6..=8 搬到目标位 2：既有位 2..=4（track 3..=5）让位到
        // 5..=7，块按追加顺序占 2..=4；位置全集 0..=7 无重复无缺口
        add_playlist_items(&pool, playlist_id, &[6, 7, 8], Some(2))
            .await
            .unwrap();

        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT position, track_id FROM playlist_item \
             WHERE playlist_id = ? ORDER BY position",
        )
        .bind(playlist_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|(p, _)| *p).collect::<Vec<i64>>(),
            vec![0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            rows.iter().map(|(_, t)| *t).collect::<Vec<i64>>(),
            vec![1, 2, 6, 7, 8, 3, 4, 5]
        );
    }
}
