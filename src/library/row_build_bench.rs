//! Timing probe for the per-row blocking cost behind the `LibraryAccess for
//! App` debt (db.rs:961-971): every row built while scrolling a table parks
//! the UI thread on `RUNTIME.block_on` for one of these queries, so the numbers
//! here are what a single row build costs the render thread (warm cache).
//!
//! Excluded from the normal test run — timings are environment-dependent and
//! only meaningful as a human-read report:
//!
//! ```text
//! cargo test --release row_build_bench -- --ignored --nocapture
//! ```

use std::{sync::Arc, time::Instant};

use sqlx::SqlitePool;

use crate::{library::db, test_support::create_test_pool};

const ALBUMS: usize = 200;
const TRACKS_PER_ALBUM: usize = 50;
const ARTISTS: usize = ALBUMS;
const WARMUP: usize = 50;
const ITERATIONS: usize = 400;

async fn seed(pool: &SqlitePool) {
    let mut tx = pool.begin().await.unwrap();
    for artist in 0..ARTISTS {
        sqlx::query("INSERT INTO artist (id, name, name_sortable) VALUES ($1, $2, $3)")
            .bind(artist as i64)
            .bind(format!("artist-{artist:04}"))
            .bind(format!("artist-{artist:04}"))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for album in 0..ALBUMS {
        sqlx::query("INSERT INTO album (id, title, title_sortable) VALUES ($1, $2, $3)")
            .bind(album as i64)
            .bind(format!("album-{album:04}"))
            .bind(format!("album-{album:04}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO album_artist (album_id, artist_id) VALUES ($1, $2)")
            .bind(album as i64)
            .bind(album as i64)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let mut id = 0i64;
    for album in 0..ALBUMS {
        for track in 0..TRACKS_PER_ALBUM {
            sqlx::query(
                "INSERT INTO track (id, title, title_sortable, album_id, track_number, duration, location) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(id)
            .bind(format!("track-{track:03}"))
            .bind(format!("track-{track:03}"))
            .bind(album as i64)
            .bind(track as i64)
            .bind(180)
            .bind(format!(r"C:\bench\album-{album:04}\track-{track:03}.flac"))
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query("INSERT INTO track_artist (track_id, artist_id) VALUES ($1, $2)")
                .bind(id)
                .bind(album as i64)
                .execute(&mut *tx)
                .await
                .unwrap();
            id += 1;
        }
    }
    tx.commit().await.unwrap();
}

/// Time `f` `ITERATIONS` times after `WARMUP` untimed runs, and print a
/// min/p50/p90/p99/max/mean summary in microseconds.
fn summarize(name: &str, mut f: impl FnMut()) {
    for _ in 0..WARMUP {
        f();
    }
    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        f();
        samples.push(start.elapsed().as_nanos() as u64 / 1_000);
    }
    samples.sort_unstable();
    let mean = samples.iter().sum::<u64>() / samples.len() as u64;
    println!(
        "{name:<28} min={:>6}µs  p50={:>6}µs  p90={:>6}µs  p99={:>6}µs  max={:>6}µs  mean={:>6}µs",
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() * 9 / 10],
        samples[samples.len() * 99 / 100],
        samples[samples.len() - 1],
        mean,
    );
}

#[test]
#[ignore = "timing report, run with --ignored --nocapture"]
fn report_row_build_query_costs() {
    // Seeding runs inside the runtime; the measurements run on this plain
    // thread with one `RUNTIME.block_on` per query — exactly what the UI
    // thread pays per row build via `blocking_query`.
    let (_dir, pool) = crate::RUNTIME.block_on(async {
        let (_dir, pool) = create_test_pool("row-build-bench").await;
        seed(&pool).await;
        (_dir, pool)
    });

    let track_count = (ALBUMS * TRACKS_PER_ALBUM) as i64;
    let mut track_ids: Vec<i64> = (0..track_count).collect();
    // Deterministic shuffle so consecutive probes hit different pages.
    for i in (1..track_ids.len()).rev() {
        track_ids.swap(i, (i * 7 + 3) % (i + 1));
    }
    let album_ids: Vec<i64> = (0..ALBUMS as i64).collect();
    let artist_ids: Vec<i64> = (0..ARTISTS as i64).collect();

    println!(
        "library: {ALBUMS} albums, {} tracks, {ARTISTS} artists",
        ALBUMS * TRACKS_PER_ALBUM
    );

    let mut idx = 0usize;
    summarize("get_track_by_id", || {
        let id = track_ids[idx % track_ids.len()];
        idx += 1;
        let _ = crate::RUNTIME.block_on(db::get_track_by_id(&pool, id)).unwrap();
    });
    let mut idx = 0usize;
    summarize("get_album_by_id", || {
        let id = album_ids[idx % album_ids.len()];
        idx += 1;
        let _ = crate::RUNTIME.block_on(db::get_album_by_id(&pool, id)).unwrap();
    });
    let mut idx = 0usize;
    summarize("get_artist_with_counts", || {
        let id = artist_ids[idx % artist_ids.len()];
        idx += 1;
        let _ = crate::RUNTIME
            .block_on(db::get_artist_with_counts(&pool, id))
            .unwrap();
    });
    let mut idx = 0usize;
    summarize("get_all_tracks_by_artist", || {
        let id = artist_ids[idx % artist_ids.len()];
        idx += 1;
        let _: Arc<Vec<_>> =
            crate::RUNTIME.block_on(db::get_all_tracks_by_artist(&pool, id)).unwrap();
    });
}
