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
        sqlx::query("INSERT INTO playlist_item (playlist_id, track_id, position) VALUES (?, ?, ?)")
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
        include_str!("../../../queries/library/find_tracks_title_asc.sql")
    );
    const EQP_DESC: &str = concat!(
        "EXPLAIN QUERY PLAN ",
        include_str!("../../../queries/library/find_tracks_title_desc.sql")
    );

    async fn plan(pool: &SqlitePool, sql: &'static str) -> String {
        let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(sql).fetch_all(pool).await.unwrap();
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
    const UPSERT: &str = include_str!("../../../queries/scan/create_track.sql");
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

/// 时长 / 曲序两枚候选索引（migrations/20261020120000）到底值多少——配对设计
/// 与 [`report_title_sort_index_benefit`] 完全相同：两份字节同源的 6 万行库、
/// 只在 B 上保留候选索引（迁移已把它装进两份库 ⇒ 对照组在 A 上显式拆掉）、
/// 每遍交换 A/B 先后、min-of-4，同时打印两侧 EXPLAIN QUERY PLAN——"计划有没
/// 有从临时树变成索引扫描"才是机制证据，计时只是它的价格。
///
/// 与标题探针的差异：时长与曲序的排序键都**不是**全序（真实库里大量并列），
/// 而并列行在"排序器输出"与"索引顺序"下的相对次序没有保证，故播种时把两个
/// 键都取成全序（duration = 行号、track_number = 行号），`assert_eq!` 两侧
/// 序列相同才站得住。写侧成本未在此复量：形状与 20261017120000 相同
/// （同一张表再多两枚索引的维护），那次的写放大数字可直接类比。
///
/// `#[ignore]`：要灌 6 万行。运行：
/// `cargo test --release --features kugou report_track_sort_index_benefit -- --ignored --nocapture`
#[tokio::test]
#[ignore = "cost probe: duration/number index benefit, seeds 60k rows, explicit run only"]
async fn report_track_sort_index_benefit() {
    // sqlx 的 `SqliteConnectOptions` 默认开着外键，所以 `track.album_id` 必须指向真实专辑行。
    const ALBUMS: usize = 4_000;
    const ROWS: usize = 60_000;
    // `EXPLAIN` 必须用 `&'static` 串（sqlx 0.9 禁运行期拼接），故走 `concat!`。
    const EQP_LENGTH_ASC: &str = concat!(
        "EXPLAIN QUERY PLAN ",
        include_str!("../../../queries/library/find_tracks_length_asc.sql")
    );
    const EQP_LENGTH_DESC: &str = concat!(
        "EXPLAIN QUERY PLAN ",
        include_str!("../../../queries/library/find_tracks_length_desc.sql")
    );
    const EQP_NUMBER_ASC: &str = concat!(
        "EXPLAIN QUERY PLAN ",
        include_str!("../../../queries/library/find_tracks_number_asc.sql")
    );
    const EQP_NUMBER_DESC: &str = concat!(
        "EXPLAIN QUERY PLAN ",
        include_str!("../../../queries/library/find_tracks_number_desc.sql")
    );

    async fn plan(pool: &SqlitePool, sql: &'static str) -> String {
        let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(sql).fetch_all(pool).await.unwrap();
        rows.iter()
            .map(|r| r.3.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    let (dir, seed_pool) = crate::test_support::create_test_pool("meliora-tracksort").await;
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
                "INSERT INTO track (title, title_sortable, album_id, duration, location, folder, \
                     disc_number, track_number) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("T{i:06}"))
            .bind(format!("ts{:05}", i % 20_000))
            .bind((i % ALBUMS) as i64 + 1)
            // 排序键取"行号的伪随机置换"：仍是全序（两侧序列可逐项对比），但
            // 索引序与 rowid 序**无关**——真实库里索引扫描要付的随机回表代价
            // 只有这样才量得出来。旧版把两个键都写成 `i`，索引扫描恰好退化成
            // 顺序访问，等于只测了最有利的情形（2026-10-21 修正形状）。
            .bind((i as i64 * 7919) % ROWS as i64)
            .bind(format!("lib\\d{:03}\\f{i:06}.flac", i % 300))
            .bind(format!("d{:03}", i % 300))
            .bind((i % 5) as i64 + 1)
            .bind((i as i64 * 104_729) % ROWS as i64)
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
    // 20261021090000 已把这两枚索引从迁移里撤掉（配对实测为回退，见该迁移
    // 注释），所以对照组 A 天然无索引；B 必须**显式**建回来。探针因此自足，
    // 不再依赖"迁移里刚好有这两枚索引"这个副作用。
    for ddl in [
        "CREATE INDEX idx_track_duration ON track (duration)",
        "CREATE INDEX idx_track_disc_track_number ON track (disc_number, track_number)",
    ] {
        sqlx::query(ddl).execute(&b).await.unwrap();
    }
    for pool in [&a, &b] {
        sqlx::query("ANALYZE").execute(pool).await.unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_optional(pool)
            .await
            .unwrap();
    }

    println!("rows = {ROWS} / albums = {ALBUMS}（两份库内容同源，只差那两枚索引）");
    for (label, sql, sort) in [
        ("length asc", EQP_LENGTH_ASC, TrackSortMethod::DurationAsc),
        (
            "length desc",
            EQP_LENGTH_DESC,
            TrackSortMethod::DurationDesc,
        ),
        (
            "number asc",
            EQP_NUMBER_ASC,
            TrackSortMethod::TrackNumberAsc,
        ),
        (
            "number desc",
            EQP_NUMBER_DESC,
            TrackSortMethod::TrackNumberDesc,
        ),
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
        sqlx::query("INSERT INTO playlist_item (playlist_id, track_id, position) VALUES (?, ?, ?)")
            .bind(playlist_id)
            .bind(n)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
    }

    // 一次性批量删除 #2 与 #5；其余必须按序压缩成 1..4
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
        sqlx::query("INSERT INTO playlist_item (playlist_id, track_id, position) VALUES (?, ?, ?)")
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
