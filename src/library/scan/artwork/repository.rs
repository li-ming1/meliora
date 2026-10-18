use rustc_hash::FxHashMap;
use sqlx::{SqliteConnection, SqlitePool};
use tracing::warn;

use super::ArtIdCache;
use crate::library::scan::decode::ProcessedArt;

/// 把一次失败的数据库读取按 `context` 记日志，并映射成 `None`。
fn logged<T>(result: sqlx::Result<T>, context: &str) -> Option<T> {
    result.map_err(|e| warn!("{context}: {:?}", e)).ok()
}

pub async fn load_art_ids(pool: &SqlitePool) -> ArtIdCache {
    match sqlx::query_as::<_, (i64, i64)>(include_str!(
        "../../../../queries/scan/list_artwork_ids.sql"
    ))
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(hash, id)| (hash as u64, Some(id)))
            .collect(),
        Err(e) => {
            warn!("Failed to preload artwork hashes: {:?}", e);
            ArtIdCache::default()
        }
    }
}

pub enum ArtworkData<'a> {
    Processed(&'a ProcessedArt),
}

/// 为 `hash` 找到或创建一行 artwork，可能时复用旧数据库里没有哈希的行。
pub(crate) async fn get_or_create_artwork(
    conn: &mut SqliteConnection,
    hash: i64,
    data: Option<ArtworkData<'_>>,
) -> Option<i64> {
    let existing: Option<(i64,)> = logged(
        sqlx::query_as(include_str!(
            "../../../../queries/scan/get_artwork_by_hash.sql"
        ))
        .bind(hash)
        .fetch_optional(&mut *conn)
        .await,
        "Failed to look up artwork",
    )
    .flatten();

    if let Some((id,)) = existing {
        return Some(id);
    }

    let data = data?;
    let (image, thumb) = match data {
        ArtworkData::Processed(processed) => (processed.image.as_ref(), processed.thumb.as_slice()),
    };

    let adopt: Option<(i64,)> = logged(
        sqlx::query_as(include_str!(
            "../../../../queries/scan/adopt_migrated_artwork.sql"
        ))
        .bind(image)
        .fetch_optional(&mut *conn)
        .await,
        "Failed to look up migrated artwork",
    )
    .flatten();

    if let Some((id,)) = adopt {
        if let Err(e) = sqlx::query(include_str!(
            "../../../../queries/scan/update_artwork_hash.sql"
        ))
        .bind(hash)
        .bind(id)
        .execute(&mut *conn)
        .await
        {
            warn!("Failed to adopt migrated artwork: {:?}", e);
        }
        return Some(id);
    }

    logged(
        sqlx::query_as::<_, (i64,)>(include_str!("../../../../queries/scan/insert_artwork.sql"))
            .bind(hash)
            .bind(image)
            .bind(thumb)
            .fetch_one(&mut *conn)
            .await,
        "Failed to insert artwork",
    )
    .map(|(id,)| id)
}

/// 每个哈希对应的 artwork 行 id（创建不出来时为 `None`），
/// 在一趟 finalize 的多个专辑之间做记忆化。
pub(super) type FinalizationArtCache = FxHashMap<i64, Option<i64>>;
