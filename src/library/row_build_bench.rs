//! `LibraryAccess for App` 那笔债背后逐行阻塞成本的耗时探针（即 db.rs 里
//! 「UI 线程数据库阻塞测量」那一段）：滚动表格时每构建一行都会让 UI 线程在某一条这样的
//! 查询上 `RUNTIME.block_on` 停住，所以这里的数字就是一次单行构建向渲染线程收取的代价
//! （缓存热的情况下）。
//!
//! 已从常规测试运行中排除——耗时依赖环境，只作为给人读的报告才有意义：
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

/// 循环取用 `ids`，这样相邻的探测会命中不同的行（以及不同的页）。
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
    // 播种在运行时内部跑；测量则跑在本普通线程上，每条查询一次 `RUNTIME.block_on`
    // ——这正是 UI 线程经 `blocking_query` 为每行构建所付的代价。
    let (_dir, pool) = crate::RUNTIME.block_on(async {
        let (_dir, pool) = create_test_pool("row-build-bench").await;
        seed_library(&pool, ALBUMS, TRACKS_PER_ALBUM, ARTISTS).await;
        (_dir, pool)
    });

    let mut track_ids: Vec<i64> = (0..(ALBUMS * TRACKS_PER_ALBUM) as i64).collect();
    // 确定性洗牌，让相邻探测命中不同的页。
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
