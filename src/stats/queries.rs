use sqlx::{Row, SqlitePool};

use super::{ListenRow, TrackMeta};

/// Persists finished listening rows. Online tracks carry their display
/// metadata with them; local tracks are resolved from the library here (write
/// time, off the UI thread), falling back to the file stem.
pub async fn write_rows(pool: &SqlitePool, rows: Vec<ListenRow>) {
    for row in rows {
        let ListenRow {
            ts,
            track_key,
            seconds,
            meta,
        } = row;
        let meta = match meta {
            Some(meta) => Some(meta),
            None => resolve_local_meta(pool, &track_key).await,
        };
        if let Some(meta) = &meta
            && !meta.title.is_empty()
            && let Err(e) = upsert_meta(pool, &track_key, meta).await
        {
            tracing::warn!("listen stats: meta upsert failed for {track_key}: {e}");
        }
        if let Err(e) = insert_event(pool, ts, &track_key, seconds).await {
            tracing::warn!("listen stats: insert failed for {track_key}: {e}");
        }
    }
}

async fn insert_event(
    pool: &SqlitePool,
    ts: i64,
    track_key: &str,
    seconds: i64,
) -> sqlx::Result<()> {
    let sql = include_str!("../../queries/stats/insert_event.sql");
    sqlx::query(sql)
        .bind(ts)
        .bind(track_key)
        .bind(seconds)
        .execute(pool)
        .await?;
    Ok(())
}

async fn upsert_meta(pool: &SqlitePool, track_key: &str, meta: &TrackMeta) -> sqlx::Result<()> {
    let sql = include_str!("../../queries/stats/upsert_meta.sql");
    sqlx::query(sql)
        .bind(track_key)
        .bind(&meta.title)
        .bind(&meta.artist)
        .bind(&meta.album)
        .execute(pool)
        .await?;
    Ok(())
}

async fn resolve_local_meta(pool: &SqlitePool, track_key: &str) -> Option<TrackMeta> {
    let path = track_key.strip_prefix("local:")?;
    let sql = include_str!("../../queries/stats/resolve_local_meta.sql");
    let row = match sqlx::query(sql).bind(path).fetch_optional(pool).await {
        Ok(row) => row?,
        Err(e) => {
            tracing::warn!("listen stats: meta resolve failed for {path}: {e}");
            return None;
        }
    };
    let title: String = row.try_get("title").unwrap_or_default();
    let artist: String = row.try_get("artist").unwrap_or_default();
    let album: String = row.try_get("album").unwrap_or_default();
    let title = if title.is_empty() {
        std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        title
    };
    Some(TrackMeta { title, artist, album })
}

pub async fn daily_sums(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, i64)>> {
    let sql = include_str!("../../queries/stats/daily_sums.sql");
    let rows = sqlx::query(sql).bind(since).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("day"), r.get::<i64, _>("total")))
        .collect())
}

pub async fn hour_histogram(pool: &SqlitePool, since: i64) -> sqlx::Result<[i64; 24]> {
    let sql = include_str!("../../queries/stats/hour_histogram.sql");
    let rows = sqlx::query(sql).bind(since).fetch_all(pool).await?;
    let mut hours = [0i64; 24];
    for r in rows {
        let hour: i64 = r.get("hour");
        if (0..24).contains(&hour) {
            hours[hour as usize] = r.get("total");
        }
    }
    Ok(hours)
}

/// (title, artist, seconds), best first.
pub async fn top_tracks(
    pool: &SqlitePool,
    since: i64,
) -> sqlx::Result<Vec<(String, String, i64)>> {
    let sql = include_str!("../../queries/stats/top_tracks.sql");
    let rows = sqlx::query(sql).bind(since).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("title"),
                r.get::<String, _>("artist"),
                r.get::<i64, _>("total"),
            )
        })
        .collect())
}

/// (artist, seconds), best first.
pub async fn top_artists(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, i64)>> {
    let sql = include_str!("../../queries/stats/top_artists.sql");
    let rows = sqlx::query(sql).bind(since).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("artist"), r.get::<i64, _>("total")))
        .collect())
}

/// (album, seconds), best first.
pub async fn top_albums(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, i64)>> {
    let sql = include_str!("../../queries/stats/top_albums.sql");
    let rows = sqlx::query(sql).bind(since).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("album"), r.get::<i64, _>("total")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn write_and_aggregate() {
        let pool = test_pool().await;
        write_rows(
            &pool,
            vec![
                ListenRow {
                    ts: 1_700_000_000,
                    track_key: "local:C:\\a.mp3".into(),
                    seconds: 120,
                    meta: Some(TrackMeta {
                        title: "Song A".into(),
                        artist: "X".into(),
                        album: "L".into(),
                    }),
                },
                ListenRow {
                    ts: 1_700_086_400,
                    track_key: "local:C:\\b.mp3".into(),
                    seconds: 60,
                    meta: None,
                },
                ListenRow {
                    ts: 1_700_086_400 + 3600,
                    track_key: "local:C:\\a.mp3".into(),
                    seconds: 30,
                    meta: Some(TrackMeta {
                        title: "Song A".into(),
                        artist: "X".into(),
                        album: "L".into(),
                    }),
                },
            ],
        )
        .await;

        let daily = daily_sums(&pool, 0).await.unwrap();
        assert_eq!(daily.len(), 2); // 1_700_000_000 and +86400 fall on distinct local days

        let hours = hour_histogram(&pool, 0).await.unwrap();
        assert_eq!(hours.iter().sum::<i64>(), 210);

        let tracks = top_tracks(&pool, 0).await.unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].0, "Song A"); // 150 s
        assert_eq!(tracks[0].2, 150);

        let artists = top_artists(&pool, 0).await.unwrap();
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].0, "X");
        assert_eq!(artists[0].1, 150);

        let albums = top_albums(&pool, 0).await.unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].1, 150);

        // Sliding window excludes the oldest row; b.mp3 stays in range with
        // no resolvable meta (empty title, no meta row written).
        let recent = top_tracks(&pool, 1_700_000_001).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].0, "");
        assert_eq!(recent[0].2, 60);
        assert_eq!(recent[1].0, "Song A");
        assert_eq!(recent[1].2, 30);
    }
}
