use sqlx::{Row, SqlitePool};

use super::{ListenRow, TrackMeta};

/// 持久化已完成的收听行。在线曲目自带展示元数据；本地曲目在这里（写入时、UI 线程
/// 之外）从音乐库解析，取不到时退回文件名主体。
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
    Some(TrackMeta {
        title,
        artist,
        album,
    })
}

/// `(文本键, 秒数)` 二元聚合行的共用读取：按日 / 按艺人 / 按专辑三个查询只在
/// SQL 与文本列的键名上不同，`total` 是三者共同的列契约。
async fn pair_totals(
    pool: &SqlitePool,
    since: i64,
    sql: &'static str,
    key: &str,
) -> sqlx::Result<Vec<(String, i64)>> {
    let rows = sqlx::query(sql).bind(since).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>(key), r.get::<i64, _>("total")))
        .collect())
}

pub async fn daily_sums(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, i64)>> {
    pair_totals(
        pool,
        since,
        include_str!("../../queries/stats/daily_sums.sql"),
        "day",
    )
    .await
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

/// `(title, artist, seconds)`，按秒数从高到低。
pub async fn top_tracks(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, String, i64)>> {
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

/// `(artist, seconds)`，按秒数从高到低。
pub async fn top_artists(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, i64)>> {
    pair_totals(
        pool,
        since,
        include_str!("../../queries/stats/top_artists.sql"),
        "artist",
    )
    .await
}

/// `(album, seconds)`，按秒数从高到低。
pub async fn top_albums(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<(String, i64)>> {
    pair_totals(
        pool,
        since,
        include_str!("../../queries/stats/top_albums.sql"),
        "album",
    )
    .await
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
        assert_eq!(daily.len(), 2); // 1_700_000_000 与 +86400 落在不同的本地日

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

        // 滑动窗口排除了最旧的一行；b.mp3 仍在窗口内且解析不出元数据
        // （标题为空，不写 meta 行）。
        let recent = top_tracks(&pool, 1_700_000_001).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].0, "");
        assert_eq!(recent[0].2, 60);
        assert_eq!(recent[1].0, "Song A");
        assert_eq!(recent[1].2, 30);
    }

    fn meta(title: &str, artist: &str, album: &str) -> TrackMeta {
        TrackMeta {
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
        }
    }

    /// 按歌曲而不是按 track_key 分组的意义：同一首歌从磁盘播和从酷狗播是一首歌，
    /// 不是两首。
    #[tokio::test]
    async fn top_tracks_folds_every_source_of_one_song() {
        let pool = test_pool().await;
        write_rows(
            &pool,
            vec![
                ListenRow {
                    ts: 100,
                    track_key: "local:/music/song.mp3".into(),
                    seconds: 120,
                    meta: Some(meta("Song A", "X", "L")),
                },
                ListenRow {
                    ts: 200,
                    track_key: "kugou:12345".into(),
                    seconds: 60,
                    meta: Some(meta("Song A", "X", "L")),
                },
                ListenRow {
                    ts: 300,
                    track_key: "kugou:999".into(),
                    seconds: 30,
                    // 大小写与空白都不同：仍是同一首歌。
                    meta: Some(meta(" song a ", "x", "L")),
                },
            ],
        )
        .await;

        let tracks = top_tracks(&pool, 0).await.unwrap();
        assert_eq!(tracks.len(), 1, "all three sources should be one entry");
        assert_eq!(tracks[0].2, 210, "120 + 60 + 30");
    }

    /// 文件被移动或改名后仍保有其历史记录，因为本地键不再是这些行分组的依据。
    #[tokio::test]
    async fn top_tracks_survives_a_file_move() {
        let pool = test_pool().await;
        write_rows(
            &pool,
            vec![
                ListenRow {
                    ts: 100,
                    track_key: "local:/old/song.mp3".into(),
                    seconds: 90,
                    meta: Some(meta("Song A", "X", "L")),
                },
                ListenRow {
                    ts: 200,
                    track_key: "local:/new/song.mp3".into(),
                    seconds: 45,
                    meta: Some(meta("Song A", "X", "L")),
                },
            ],
        )
        .await;

        let tracks = top_tracks(&pool, 0).await.unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].2, 135);
    }

    /// 折叠不得过度合并：不同的歌曲要保持分开，没有元数据的行要保留自己的条目，
    /// 而不是塌成一个空白项。
    #[tokio::test]
    async fn top_tracks_keeps_distinct_songs_and_unresolved_rows_apart() {
        let pool = test_pool().await;
        write_rows(
            &pool,
            vec![
                ListenRow {
                    ts: 100,
                    track_key: "local:/a.mp3".into(),
                    seconds: 60,
                    meta: Some(meta("Song A", "X", "L")),
                },
                ListenRow {
                    ts: 200,
                    track_key: "local:/b.mp3".into(),
                    seconds: 50,
                    meta: Some(meta("Song B", "X", "L")),
                },
                // 同标题、不同艺术家：是翻唱版本，不是同一首曲目。
                ListenRow {
                    ts: 300,
                    track_key: "local:/c.mp3".into(),
                    seconds: 40,
                    meta: Some(meta("Song A", "Y", "L")),
                },
                // 解析不出元数据：不得与那些空白行合并。
                ListenRow {
                    ts: 400,
                    track_key: "local:/d.mp3".into(),
                    seconds: 30,
                    meta: None,
                },
                ListenRow {
                    ts: 500,
                    track_key: "local:/e.mp3".into(),
                    seconds: 20,
                    meta: None,
                },
            ],
        )
        .await;

        let tracks = top_tracks(&pool, 0).await.unwrap();
        assert_eq!(
            tracks.len(),
            5,
            "each distinct song, plus each unresolved row"
        );
        assert_eq!(tracks[0].0, "Song A");
        assert_eq!(tracks[0].2, 60);
        assert_eq!(tracks[1].0, "Song B");
        assert_eq!(tracks[2].2, 40);
        // 两条未解析出的行保持分开，而不是加总成 50。
        assert_eq!(tracks[3].2, 30);
        assert_eq!(tracks[4].2, 20);
    }
}
