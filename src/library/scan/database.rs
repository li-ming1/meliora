mod albums;
mod artist_links;
mod artists;
mod relocate;
mod tracks;

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use sqlx::SqliteConnection;
use tracing::debug;

use crate::{
    library::scan::{
        artwork::{
            ArtIdCache, ArtworkData, FolderArtCandidates, consider_folder_art,
            get_or_create_artwork,
        },
        decode::{ArtSource, FileArt, ScannedArt},
    },
    media::metadata::Metadata,
};

pub use albums::AlbumCacheKey;
use albums::insert_album;
pub(crate) use artist_links::recompute_album_artists;
pub use artist_links::{flush_album_artists, flush_track_artists, sweep_orphan_artists};
pub use relocate::relocate_track;
pub use tracks::{AlbumPathCacheKey, TrackWriteOutcome};
use tracks::{delete_lyrics, insert_track, upsert_lyrics};

#[cfg(test)]
use albums::bind_release_date;
#[cfg(test)]
use artists::{decode_artist_list, encode_artist_list, sort_mentions_artist};
#[cfg(test)]
use sqlx::SqlitePool;

#[cfg(test)]
use crate::library::{
    scan::artist_match::ArtistMatcher,
    types::{DATE_PRECISION_FULL_DATE, DATE_PRECISION_YEAR, DATE_PRECISION_YEAR_MONTH},
};

#[derive(Default)]
pub(crate) struct WriteCaches {
    pub(crate) force_encountered: FxHashSet<i64>,
    pub(crate) albums: FxHashMap<AlbumCacheKey, i64>,
    pub(crate) paths: FxHashMap<AlbumPathCacheKey, Utf8PathBuf>,
    pub(crate) pending_albums: FxHashSet<i64>,
    pub(crate) pending_tracks: FxHashSet<i64>,
    pub(crate) folder_art_candidates: FolderArtCandidates,
    pub(crate) art_ids: ArtIdCache,
    /// 本次扫描检查过文件夹封面的专辑。
    pub(crate) examined_albums: FxHashSet<i64>,
}

/// 解析候选项的 artwork 行 id；第一次见到其哈希时先把新行暂存起来。
async fn stage_artwork(
    conn: &mut SqliteConnection,
    caches: &mut WriteCaches,
    candidate: &ScannedArt,
) -> Option<i64> {
    if let Some(&cached) = caches.art_ids.get(&candidate.hash) {
        return cached;
    }
    let data = candidate.processed.as_deref().map(ArtworkData::Processed);
    let id = get_or_create_artwork(conn, candidate.hash as i64, data).await;
    caches.art_ids.insert(candidate.hash, id);
    id
}

pub async fn update_metadata(
    conn: &mut SqliteConnection,
    metadata: &Metadata,
    path: &Utf8Path,
    length: u64,
    art: &FileArt,
    is_force: bool,
    caches: &mut WriteCaches,
) -> anyhow::Result<TrackWriteOutcome> {
    debug!(
        "Adding/updating record for {:?} - {:?}",
        metadata.artist, metadata.name
    );

    let album_artist = metadata
        .album_artist
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let artist = metadata.artist.as_deref().filter(|s| !s.trim().is_empty());
    let display_override = album_artist.or(artist);

    let album_id = insert_album(
        conn,
        metadata,
        display_override,
        is_force,
        &mut caches.force_encountered,
        &mut caches.albums,
    )
    .await?;

    // 重打 tag 可能把曲目挪到另一个专辑——旧专辑同样需要重建
    let previous_album: Option<(Option<i64>,)> = sqlx::query_as(include_str!(
        "../../../queries/scan/get_album_id_at_location.sql"
    ))
    .bind(path.as_str())
    .fetch_optional(&mut *conn)
    .await?;

    let art_hash = art.embedded.as_ref().map(|embedded| embedded.hash as i64);
    let Some(track_id) = insert_track(
        conn,
        metadata,
        album_id,
        art_hash,
        path,
        length,
        &mut caches.paths,
    )
    .await?
    else {
        return Ok(TrackWriteOutcome::SkippedDuplicateFolder);
    };

    if let Some(lyrics) = &metadata.lyrics {
        upsert_lyrics(conn, track_id, lyrics).await?;
    } else {
        delete_lyrics(conn, track_id).await?;
    }

    // 现在就处理图片——扫描结束时的挑选只需要那些已暂存的行
    if let Some(album_id) = album_id
        // 首曲目那次读取会把文件夹标成已检查，即使没找到任何封面
        && art.representative
    {
        caches.examined_albums.insert(album_id);
    }

    let mut embedded_id: Option<i64> = None;
    let mut folder_id: Option<(i64, i64)> = None;
    for candidate in [&art.embedded, &art.folder].into_iter().flatten() {
        let artwork_id = stage_artwork(conn, caches, candidate).await;
        let source = candidate.source.db_value();
        if let Some(album_id) = album_id
            && matches!(candidate.source, ArtSource::Folder(_))
        {
            consider_folder_art(
                &mut caches.folder_art_candidates,
                album_id,
                candidate.hash,
                source,
            );
        }
        match candidate.source {
            ArtSource::Embedded => embedded_id = artwork_id,
            ArtSource::Folder(_) => folder_id = artwork_id.map(|id| (id, source)),
        }
    }

    // 专辑内曲目用内嵌封面，没有专辑的曲目在可用时用文件夹封面
    let track_art = match album_id {
        Some(_) => embedded_id,
        None => folder_id.map(|(id, _)| id).or(embedded_id),
    };
    if let Some(artwork_id) = track_art {
        sqlx::query(include_str!("../../../queries/scan/update_track_art.sql"))
            .bind(artwork_id)
            .bind(track_id)
            .execute(&mut *conn)
            .await?;
    }

    // 专辑还没有封面时的临时挑选（有文件夹封面就用它，否则用内嵌）
    if let Some(album_id) = album_id
        && let Some((artwork_id, source)) = folder_id.or(embedded_id.map(|id| (id, 0)))
    {
        sqlx::query(include_str!("../../../queries/scan/set_album_art.sql"))
            .bind(artwork_id)
            .bind(source)
            .bind(album_id)
            .execute(&mut *conn)
            .await?;
    }

    // 每批次里每个专辑只算一次专辑歌手，没有专辑的曲目各自建链接
    if let Some(album_id) = album_id {
        caches.pending_albums.insert(album_id);
    } else {
        caches.pending_tracks.insert(track_id);
    }
    if let Some((Some(old_id),)) = previous_album
        && Some(old_id) != album_id
    {
        caches.pending_albums.insert(old_id);
    }

    Ok(TrackWriteOutcome::Written)
}

#[cfg(test)]
#[path = "database/tests.rs"]
mod tests;
