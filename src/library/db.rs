use std::{collections::HashSet, path::Path, sync::Arc};

use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
    },
};
use tracing::debug;

use super::types::{Album, Artist, ArtistWithCounts, Playlist, PlaylistItem, Track};

/// 迁移版本中记录的校验和可能只因旧版 Windows 数据库以不同行尾写入才对不上的那些；
/// 打开路径会重写这些校验和并重试，而不是直接失败。
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
        // 留一枚常驻连接。默认 `min_connections = 0` 且 `idle_timeout = 10min`：
        // 桌面应用的库访问是稀疏的（点一下才查），闲置十分钟后连接全关，下一次
        // UI 线程查询要付整次重开——`sqlite3_open` + `journal_mode=WAL` +
        // `cache_size` 建页缓存。同库实测（同一份 66 曲库、OS 文件缓存已热）：
        // 暖连接里 100 次专辑主键查询 125ms（≈0.13ms/次，含 prepare），
        // 而 100 次「关闭 + 重开 + 设 PRAGMA + 查询」1301ms → **每次重开 ≈11.8ms**。
        // 这正是日志里 `[db] slow ui-thread query` 唯一还在出现的两种形态
        // （`get_album_by_id` 4759µs / 2824µs、启动 `list_all_playlists` 2318µs）
        // 的量级来源：慢的不是 SQL，是连接生命周期。
        // 常驻一枚的代价上界：一份 8MB 页缓存额度（按需真实占用），
        // 与上面把连接数压到 3 的取舍同源；突发仍会借到 3 枚。
        .min_connections(1)
        .connect_with(options)
        .await?;

    let migrations = sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(&pool)
        .await;

    if let Err(e) = migrations {
        // 旧版 Windows 数据库在这些版本上会因行尾差异撞上哈希不匹配，
        // 重写它们的校验和并重试；其它任何迁移失败都是致命的
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

    fetch_rows(pool, query).await
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

    fetch_rows(pool, query).await
}

/// 单参数查询的共用收尾：结果全进程共享、从不原地改写，所以直接包成 `Arc`
/// （同一份结果只存一次，而不是每个调用点各拷一份 vector）。`T` 由调用点的
/// 返回类型推出；`query` 仍由各站点 `include_str!` 传入，SQL 留在看得见的位置。
async fn fetch_shared_vec<T>(
    pool: &SqlitePool,
    query: &'static str,
    id: i64,
) -> sqlx::Result<Arc<Vec<T>>>
where
    T: for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
{
    Ok(Arc::new(
        sqlx::query_as::<_, T>(query)
            .bind(id)
            .fetch_all(pool)
            .await?,
    ))
}

/// 同上，主键单行版。
async fn fetch_shared_one<T>(
    pool: &SqlitePool,
    query: &'static str,
    id: i64,
) -> sqlx::Result<Arc<T>>
where
    T: for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
{
    Ok(Arc::new(
        sqlx::query_as::<_, T>(query)
            .bind(id)
            .fetch_one(pool)
            .await?,
    ))
}

/// 无绑定参数的整表查询收尾（列表 / 搜索 / 可用性一类）：结果按所有权交回调用点，
/// 不需要 `Arc` 包装。`T` 由调用点的返回类型推出——此前七个站点都在签名与
/// `query_as::<_, T>` 里把同一个元组类型写两遍。
async fn fetch_rows<T>(pool: &SqlitePool, query: &'static str) -> sqlx::Result<Vec<T>>
where
    T: for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
{
    sqlx::query_as::<_, T>(query).fetch_all(pool).await
}

/// 同上，带一个绑定 id 的版本（`fetch_shared_vec` 的不包 `Arc` 姊妹）。
async fn fetch_bound_rows<T>(
    pool: &SqlitePool,
    query: &'static str,
    id: i64,
) -> sqlx::Result<Vec<T>>
where
    T: for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
{
    sqlx::query_as::<_, T>(query).bind(id).fetch_all(pool).await
}

pub async fn list_tracks_in_album(
    pool: &SqlitePool,
    album_id: i64,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = include_str!("../../queries/library/find_tracks_in_album.sql");

    fetch_shared_vec(pool, query, album_id).await
}

pub async fn get_album_by_id(pool: &SqlitePool, album_id: i64) -> sqlx::Result<Arc<Album>> {
    let query = include_str!("../../queries/library/find_album_metadata_by_id.sql");

    fetch_shared_one(pool, query, album_id).await
}

pub async fn get_artist_by_id(pool: &SqlitePool, artist_id: i64) -> sqlx::Result<Arc<Artist>> {
    let query = include_str!("../../queries/library/find_artist_by_id.sql");

    fetch_shared_one(pool, query, artist_id).await
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

    fetch_bound_rows(pool, query, artist_id).await
}

pub async fn get_artist_with_counts(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Arc<ArtistWithCounts>> {
    let query = include_str!("../../queries/library/find_artist_with_counts_by_id.sql");

    fetch_shared_one(pool, query, artist_id).await
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

    fetch_shared_vec(pool, query, artist_id).await
}

pub async fn list_all_tracks_by_artist(
    pool: &SqlitePool,
    artist_id: i64,
) -> sqlx::Result<Arc<Vec<Track>>> {
    let query = include_str!("../../queries/library/find_all_tracks_by_artist.sql");

    fetch_shared_vec(pool, query, artist_id).await
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

    fetch_shared_vec(pool, query, artist_id).await
}

pub async fn get_track_by_id(pool: &SqlitePool, track_id: i64) -> sqlx::Result<Arc<Track>> {
    let query = include_str!("../../queries/library/find_track_by_id.sql");

    fetch_shared_one(pool, query, track_id).await
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

/// 列出全部专辑用于搜索。返回 (id, title, artist display override, artist names)。
#[allow(clippy::type_complexity)]
pub async fn list_albums_search(
    pool: &SqlitePool,
) -> sqlx::Result<Vec<(i64, String, Option<String>, String)>> {
    let query = include_str!("../../queries/library/find_albums_search.sql");

    fetch_rows(pool, query).await
}

/// 列出全部曲目用于搜索。返回 (id, title, artist_names, album_id)。
pub async fn list_tracks_search(
    pool: &SqlitePool,
) -> sqlx::Result<Vec<(i64, String, String, Option<i64>)>> {
    let query = include_str!("../../queries/library/find_tracks_search.sql");

    fetch_rows(pool, query).await
}

/// 列出全部歌手用于搜索。返回 (id, name)。
pub async fn list_artists_search(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artists_search.sql");

    fetch_rows(pool, query).await
}

/// 全部 (album id, track location) 配对：用一条查询就算出专辑可用性（还有曲目留在
/// 盘上），取代每个专辑一次完整的 `list_tracks_in_album` 取数。
pub async fn list_album_availability(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_album_availability.sql");

    fetch_rows(pool, query).await
}

/// 全部 `(track_id, artist_id)` 署名配对：用一条查询就算出歌手可用性（署名的曲目还有
/// 留在盘上的），取代每行歌手一次完整的 `list_all_tracks_by_artist` 取数。产出的是相对
/// track 表自身 id 的整数配对——调用方在 Rust 里把它们与基础的
/// `(location, id, album_id)` 行做 join，因此没有哪条署名需要在 SQL 上重新搬运自己的
/// location 字符串。
///
/// 这条查询必须与 `find_all_tracks_by_artist` 对齐：歌手同时通过 `album_artist`
/// （专辑级）与 `track_artist`（曲目级）两处署名。只查其中一处，就会把另一处署名的
/// 所有歌手置灰，而大多数库正是那样署名的。
pub async fn list_track_artist_credits(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, i64)>> {
    let query = include_str!("../../queries/library/find_track_artist_credits.sql");

    fetch_rows(pool, query).await
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

    fetch_shared_one(pool, query, playlist_id).await
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
    fetch_shared_vec(pool, query, playlist_id).await
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

    fetch_shared_vec(pool, query, playlist_id).await
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

    // move 脚本里的两条 UPDATE 必须同时生效或全不生效：只提交位移那一条，
    // 会让其后的每个歌单都错一位。
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
    // 取当前项的位置
    let original_item = get_playlist_item(pool, item_id).await?;

    let move_query = if original_item.position < new_position {
        include_str!("../../queries/playlist/move_track_down.sql")
    } else if original_item.position > new_position {
        include_str!("../../queries/playlist/move_track_up.sql")
    } else {
        return Ok(());
    };

    // 位移 + 重排这两条语句必须同时生效或全不生效
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

    // 位置压缩与删除必须同时生效或全不生效
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

/// 歌单批量查询里 IN 子句的分块大小：始终留在 SQLite 绑定变量上限之下
/// （旧版构建 999，现代 32766），因此任何选择规模都不会让整批以
/// "too many SQL variables" 失败。
const PLAYLIST_IN_CHUNK: usize = 900;

/// `?,?,?`——IN 子句 `n` 个参数的绑定槽列表。
/// 与 `library/types/table.rs` 的分块行预取共用。
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

    // 分块是为了参数个数永远碰不到 SQLite 的变量上限；每块按自己的完整大小计数，
    // 等价于对整批做一次 COUNT（各块互不相交）
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
    // 一次整歌单 SELECT 取代逐曲目的「先查再插」那一对（对上千家曲的专辑就是 3000 次
    // 往返）；唯一 (playlist_id, track_id) 索引仍作最后防线，且每次插入都跑同一条
    // add_track.sql 位置查询，位置语义因此不变。整批共用单个事务，而不是每曲目一次
    // autocommit 往返（形态与 import_playlist 相同）
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

    // 整批一条 DELETE 取代逐曲目的「查-选-删-重排」链条（remove_track.sql 每删一条
    // 都要重写其后所有行：从 3000 项歌单删 100 条过去要付约 100 次 O(n) 更新加 200 次
    // 查询）。删除由唯一 (playlist_id, track_id) 索引承接；位置在同一事务里一次性
    // 重新压缩。DELETE 做了分块以使参数个数碰不到 SQLite 的变量上限；各块共用该事务，
    // 整批仍是全成或全废。
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

/// 列出与某专辑相连的全部 (id, name) 歌手配对。
pub async fn artist_ids_for_album(
    pool: &SqlitePool,
    album_id: i64,
) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artist_ids_for_album.sql");

    fetch_bound_rows(pool, query, album_id).await
}

/// 列出与某曲目相连的全部 (id, name) 歌手配对（经由其专辑或独立链接）。
pub async fn artist_ids_for_track(
    pool: &SqlitePool,
    track_id: i64,
) -> sqlx::Result<Vec<(i64, String)>> {
    let query = include_str!("../../queries/library/find_artist_ids_for_track.sql");

    fetch_bound_rows(pool, query, track_id).await
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
mod tests;
