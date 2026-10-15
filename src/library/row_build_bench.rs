//! Timing probe for the per-row blocking cost behind the `LibraryAccess for
//! App` debt (the "UI-thread database blocking measurement" block in db.rs):
//! every row built while scrolling a table parks the UI thread on
//! `RUNTIME.block_on` for one of these queries, so the numbers here are what a
//! single row build costs the render thread (warm cache).
//!
//! Excluded from the normal test run — timings are environment-dependent and
//! only meaningful as a human-read report:
//!
//! ```text
//! cargo test --release row_build_bench -- --ignored --nocapture
//! ```

use std::sync::Arc;

use crate::{
    library::db,
    test_support::{bench_summarize, create_test_pool, seed_library},
};

const ALBUMS: usize = 200;
const TRACKS_PER_ALBUM: usize = 50;
const ARTISTS: usize = ALBUMS;

/// Cycles through `ids` so consecutive probes hit different rows (and pages).
fn cycler(ids: &[i64]) -> impl FnMut() -> i64 + '_ {
    let mut idx = 0;
    move || {
        let id = ids[idx % ids.len()];
        idx += 1;
        id
    }
}

#[test]
#[ignore = "timing report, run with --ignored --nocapture"]
fn report_row_build_query_costs() {
    // Seeding runs inside the runtime; the measurements run on this plain
    // thread with one `RUNTIME.block_on` per query — exactly what the UI
    // thread pays per row build via `blocking_query`.
    let (_dir, pool) = crate::RUNTIME.block_on(async {
        let (_dir, pool) = create_test_pool("row-build-bench").await;
        seed_library(&pool, ALBUMS, TRACKS_PER_ALBUM, ARTISTS).await;
        (_dir, pool)
    });

    let mut track_ids: Vec<i64> = (0..(ALBUMS * TRACKS_PER_ALBUM) as i64).collect();
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

    let mut next_track_id = cycler(&track_ids);
    let mut next_album_id = cycler(&album_ids);
    let mut next_artist_id = cycler(&artist_ids);
    bench_summarize("get_track_by_id", || {
        let _ = crate::RUNTIME
            .block_on(db::get_track_by_id(&pool, next_track_id()))
            .unwrap();
    });
    bench_summarize("get_album_by_id", || {
        let _ = crate::RUNTIME
            .block_on(db::get_album_by_id(&pool, next_album_id()))
            .unwrap();
    });
    bench_summarize("get_artist_with_counts", || {
        let _ = crate::RUNTIME
            .block_on(db::get_artist_with_counts(&pool, next_artist_id()))
            .unwrap();
    });
    bench_summarize("list_all_tracks_by_artist", || {
        let _: Arc<Vec<_>> = crate::RUNTIME
            .block_on(db::list_all_tracks_by_artist(&pool, next_artist_id()))
            .unwrap();
    });
}
