use rustc_hash::{FxHashMap, FxHashSet};
use sqlx::SqliteConnection;

// 规范化函数本体在 media 层（media/lofty.rs 同用它做 tag 归一），此处再导
// 出维持 scan 内部与 database/artists.rs 的既有引用路径不变。
pub use crate::media::text::token_key;

struct ArtistEntry {
    id: i64,
    name: String,
    key: String,
}

pub struct ArtistMatcher {
    artists: Option<FxHashMap<i64, ArtistEntry>>,
    by_key: FxHashMap<String, i64>,
}

impl ArtistMatcher {
    pub fn new() -> Self {
        Self {
            artists: None,
            by_key: FxHashMap::default(),
        }
    }

    pub fn clear(&mut self) {
        self.artists = None;
        self.by_key.clear();
    }

    /// 批量逐出最后一根链接消失的歌手：对整个受害集只把两张 map 各扫一遍。
    /// `sync_artist_links` 能在一次提交里孤立大量歌手（批量重打 tag 会孤立全部
    /// 歌手）；逐 id 的旧实现每个受害者都要扫一遍 `by_key`，相对表规模是 O(n²)
    /// （bench F028：10k 项的表上逐出 10k 条 = 顺序 77 ms vs 本实现 0.5 ms）。
    /// 终态与循环调用旧的逐 id evict 完全一致——由 bench 的
    /// `assert_batch_matches_loop` 断言。旧的逐 id 形态仍留在 bench 里，作修复前
    /// 的对比变体。
    pub fn evict_many(&mut self, artist_ids: &[i64]) {
        if artist_ids.is_empty() {
            return;
        }
        let victims: FxHashSet<i64> = artist_ids.iter().copied().collect();
        if let Some(artists) = &mut self.artists {
            artists.retain(|id, _| !victims.contains(id));
        }
        self.by_key.retain(|_, id| !victims.contains(id));
    }

    async fn load(&mut self, conn: &mut SqliteConnection) -> anyhow::Result<()> {
        if self.artists.is_some() {
            return Ok(());
        }

        let rows: Vec<(i64, String, String)> =
            sqlx::query_as(include_str!("../../../queries/scan/list_artists.sql"))
                .fetch_all(&mut *conn)
                .await?;

        let mut artists = FxHashMap::default();
        // SQL 按 id 排序——重复 key 由最小 id 取胜
        let mut by_key = FxHashMap::default();
        for (id, name, sort) in rows {
            let entry = ArtistEntry {
                id,
                name,
                key: token_key(&sort),
            };
            by_key.entry(entry.key.clone()).or_insert(entry.id);
            artists.insert(id, entry);
        }
        self.by_key = by_key;
        self.artists = Some(artists);
        Ok(())
    }

    pub async fn resolve(
        &mut self,
        conn: &mut SqliteConnection,
        name: &str,
        sort_name: Option<&str>,
    ) -> anyhow::Result<i64> {
        self.load(&mut *conn).await?;

        let sort = sort_name.filter(|s| !s.trim().is_empty()).unwrap_or(name);
        let key = token_key(sort);
        let Some(&artist_id) = self.by_key.get(&key) else {
            return self.create(conn, name, sort, key).await;
        };

        // 优先采用匹配 sort key 的名字——平局时保留现有行，所以顺序无关紧要
        let entry = self
            .artists
            .as_mut()
            .unwrap()
            .get_mut(&artist_id)
            .expect("artist matcher key should point to a loaded artist");
        // `by_key` 命中即蕴含 `entry.key == key`（`load`/`create` 维持该不变量），
        // 故两处规范化检查都直接与 `key` 比较。当 sort 回退成名字时 `key` 就等于
        // `token_key(name)`，入参名的检查平凡为真——无需重算。bench F029 实测旧形态
        // 每次命中调 3 次 `token_key`（单次命中 551 ns，其中 111% 花在 `token_key`
        // 上）；命中路径现在最多付 1 次（entry 名检查），在存储名已是规范形态的
        // 稳态下付 0 次。
        let entry_name_matches_key = token_key(&entry.name) == key;
        let name_matches_key = sort == name || token_key(name) == key;
        if name_matches_key && !entry_name_matches_key {
            let updated = sqlx::query(include_str!("../../../queries/scan/update_artist_name.sql"))
                .bind(name)
                .bind(entry.id)
                .execute(&mut *conn)
                .await?;
            // 这个名字已被别的歌手占用——忽略这次改名
            if updated.rows_affected() > 0 {
                entry.name = name.to_string();
            }
        }

        Ok(artist_id)
    }

    async fn create(
        &mut self,
        conn: &mut SqliteConnection,
        name: &str,
        sort_name: &str,
        key: String,
    ) -> anyhow::Result<i64> {
        let result: Result<(i64,), sqlx::Error> =
            sqlx::query_as(include_str!("../../../queries/scan/create_artist.sql"))
                .bind(name)
                .bind(sort_name)
                .fetch_one(&mut *conn)
                .await;

        let (id, name, key) = match result {
            Ok((id,)) => (id, name.to_string(), key),
            // 名字被 key 不同的另一个歌手占着——改用那一行
            Err(sqlx::Error::RowNotFound) => {
                let (id, name, sort): (i64, String, String) =
                    sqlx::query_as(include_str!("../../../queries/scan/get_artist_id.sql"))
                        .bind(name)
                        .fetch_one(&mut *conn)
                        .await?;
                let existing_key = token_key(&sort);
                // 若 sort 回退成了名字，且入参 sort 尚未被占用就采纳它，好让别名合并
                let upgrade =
                    sort == name && key != existing_key && !self.by_key.contains_key(&key);
                if upgrade {
                    sqlx::query(include_str!("../../../queries/scan/update_artist_sort.sql"))
                        .bind(sort_name)
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                    (id, name, key)
                } else {
                    (id, name, existing_key)
                }
            }
            Err(e) => return Err(e.into()),
        };

        if let Some(entry) = self.artists.as_mut().unwrap().get_mut(&id) {
            let old_key = std::mem::replace(&mut entry.key, key.clone());
            entry.name = name;
            if old_key != key {
                self.by_key.remove(&old_key);
                self.by_key.insert(key, id);
            }
            return Ok(id);
        }
        self.by_key.insert(key.clone(), id);
        self.artists
            .as_mut()
            .unwrap()
            .insert(id, ArtistEntry { id, name, key });
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_key_sorts_and_normalizes() {
        assert_eq!(token_key("Rundgren, Todd"), "rundgren todd");
        assert_eq!(token_key("Todd Rundgren"), "rundgren todd");
        assert_eq!(token_key("TR-i"), "i tr");
    }

    #[tokio::test]
    async fn load_resolves_duplicate_sort_keys_to_lowest_id() {
        let (_dir, pool) = crate::test_support::create_test_pool("matcher-dup-test").await;
        let mut conn = pool.acquire().await.unwrap();

        // 旧数据库可能有两个歌手共用一个 token key
        sqlx::query("INSERT INTO artist (name, name_sortable) VALUES ('Alpha', 'Shared Sort')")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO artist (name, name_sortable) VALUES ('Beta', 'Shared Sort')")
            .execute(&mut *conn)
            .await
            .unwrap();
        let (lowest,): (i64,) = sqlx::query_as("SELECT MIN(id) FROM artist")
            .fetch_one(&mut *conn)
            .await
            .unwrap();

        let mut matcher = ArtistMatcher::new();
        let id = matcher
            .resolve(&mut conn, "Beta", Some("Shared Sort"))
            .await
            .unwrap();
        assert_eq!(id, lowest);
    }

    #[tokio::test]
    async fn create_resolve_and_evict_keep_artist_cache_consistent() {
        let (_dir, pool) = crate::test_support::create_test_pool("matcher-cache-test").await;
        let mut conn = pool.acquire().await.unwrap();
        let mut matcher = ArtistMatcher::new();

        let first = matcher
            .resolve(&mut conn, "First Artist", None)
            .await
            .unwrap();
        let second = matcher
            .resolve(&mut conn, "Second Artist", None)
            .await
            .unwrap();
        assert_ne!(first, second);

        matcher.evict_many(&[first]);
        assert_eq!(
            matcher
                .resolve(&mut conn, "Second Artist", None)
                .await
                .unwrap(),
            second
        );
        assert_eq!(
            matcher
                .resolve(&mut conn, "First Artist", None)
                .await
                .unwrap(),
            first
        );
    }
}

/// 审计发现 F028 与 F029 的耗时探针：
///
/// ```text
/// cargo test --release artist_match::bench -- --ignored --nocapture
/// ```
///
/// F028 - `ArtistMatcher::evict` 每逐出一个歌手就要用一次 `retain` 扫整张
/// `by_key` 表，而 `sync_artist_links`（database/artist_links.rs）每删一根链接就
/// 调它一次。审计的最坏情形是批量重打 tag 把全部歌手孤立：10k 条逐出打在 10k
/// 项的表上。**2026-10-15 已修复**：生产代码现在收集成批后调 `evict_many`（下面
/// 的 A' 形态）。三个变体都在按 `load` 完全相同方式构造的合成 matcher 上计时
/// （不涉及数据库——本子模块能访问私有字段）：
///
/// - A：修复前的 `evict(id)` 在循环里逐个歌手调用（旧调用点形态，由
///   `evict_one_sequential` 复刻；真实循环还额外为每根链接付一次 SQL DELETE，
///   本 bench 刻意把它隔离掉）
/// - A'：单遍批量变体——对整个受害集在两张表上各做一次 `retain`，即批量清理 API
///   会采取的形态；计时前先断言其终态与循环 `evict` 相同
/// - A''：任何 `evict(id)` API 配上逆向索引（id -> keys）所能达到的每次调用下限：
///   O(1) 哈希移除，完全不扫表
///
/// 另有场景 B：常规增量重扫（同一个 10k matcher 里只逐出 50 条），每次普通扫描
/// 付的都是这个量级。
///
/// F029 - `resolve` 的命中路径会重算 `token_key`：`sort_name` 缺失时
/// `key = token_key(name)`，随后对入参名的规范化检查又算一次 `token_key(name)`——
/// 命中时这一句永远是冗余的，因为 `by_key` 命中蕴含 `entry.key == key`；而当入参名
/// 等于存储名（本 bench 播种出的稳态）时，短路仍会对同一字符串再算一次存储名的
/// 规范化检查。于是命中形态变成每次命中 3 次 `token_key`，其中 2 次可以靠复用
/// `key` 省掉。**2026-10-15 已修复**：命中路径现在复用 `key`（sort 回退成名字时
/// 干脆跳过入参名那次调用），最多付 1 次 `token_key`。bench 先单独计时
/// `token_key`（100k 次调用，报每次 ns），再对播种了 10k 歌手的数据库计完整
/// `resolve` 命中（命中路径不跑 SQL，因此占比计算不被 I/O 稀释），并报告修复后的
/// 命中成本与审计的全库外推。修复前后的数字记在 GPUI HARDCORE PERFORMANCE §44
/// 台账（2026-10-15 轮次）。
///
/// 实测数字只在一次真实运行之后才写进这份文档注释（GPUI HARDCORE §1：没有数字
/// 就没有结论——绝不预填）。
#[cfg(test)]
mod bench {
    use std::{hint::black_box, time::Instant};

    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{ArtistEntry, ArtistMatcher, token_key};
    use crate::test_support::create_test_pool;

    /// 审计的批量重打 tag 场景所用的 matcher 规模 / 逐出条数，
    /// 也是 resolve 命中计时所播种的数据库规模
    const ARTISTS: usize = 10_000;
    /// 场景 B：常规增量重扫期间的逐出条数
    const EVICT_SMALL: usize = 50;
    /// 每个计时场景的实测轮数；报告口径取 min
    const MEASURED_RUNS: usize = 3;
    /// 单独计时的 `token_key` 调用次数（F029 第 1 部分）
    const TOKEN_KEY_CALLS: usize = 100_000;
    /// resolve 命中计时对播种数据集的完整遍历轮数
    const RESOLVE_PASSES: usize = 5;
    /// 审计的全库外推：曲目数 x 每曲歌手数
    const FULL_SCAN_RESOLVES: usize = 150_000;

    /// 按 `load` 从 `list_artists` 行构造时的完全相同方式，合成 `load` 之后的状态：
    /// 每个 id 一个 `ArtistEntry`，`by_key` 把每个 token key 指向其歌手。名字唯一
    /// 就得到 1:1 的 key->id 映射，正是审计算术所假设的标称情形。
    fn make_matcher(artists: usize) -> ArtistMatcher {
        let mut entries = FxHashMap::default();
        let mut by_key = FxHashMap::default();
        for id in 0..artists as i64 {
            let name = format!("artist-{id:04}");
            let key = token_key(&name);
            by_key.entry(key.clone()).or_insert(id);
            entries.insert(id, ArtistEntry { id, name, key });
        }
        ArtistMatcher {
            artists: Some(entries),
            by_key,
        }
    }

    /// 修复前的逐 id evict 形态，留作场景 A 的对比：每个受害者扫一遍 `by_key`。
    /// `evict_many` 落地时（2026-10-15）已从生产代码移除；此处保留是为了让修复
    /// 前后的差值仍可测量（§37 回归防护）。
    fn evict_one_sequential(matcher: &mut ArtistMatcher, artist_id: i64) {
        if let Some(artists) = &mut matcher.artists {
            artists.remove(&artist_id);
        }
        matcher.by_key.retain(|_, id| *id != artist_id);
    }

    /// 批量逐出场景的对比变体：对整个受害集把两张表各切分一次，而不是每个受害者
    /// 扫一整遍 `by_key.retain`。终态与循环 `evict_one_sequential` 相同（由
    /// `assert_batch_matches_loop` 断言）。
    fn evict_all_one_pass(matcher: &mut ArtistMatcher, victims: &[i64]) {
        let victims: FxHashSet<i64> = victims.iter().copied().collect();
        if let Some(artists) = &mut matcher.artists {
            artists.retain(|id, _| !victims.contains(id));
        }
        matcher.by_key.retain(|_, id| !victims.contains(id));
    }

    /// 配上逆向索引（id -> keys）后任何 `evict(id)` API 能达到的每次调用下限：
    /// key 已知时两处移除都是 O(1) 哈希操作，完全不扫表。key 以预先算好的形式传入
    /// ——这正是该变体的意义。
    fn evict_with_known_keys(matcher: &mut ArtistMatcher, victims: &[(i64, String)]) {
        for (id, key) in victims {
            matcher.by_key.remove(key);
            if let Some(artists) = &mut matcher.artists {
                artists.remove(id);
            }
        }
    }

    /// 批量变体必须留下与循环 `evict` 完全一致的状态。验证用的集合只逐出奇数序号
    /// 歌手，这样两张表都留有存活项可比。
    fn assert_batch_matches_loop() {
        const N: i64 = 1_000;
        let victims: Vec<i64> = (0..N).filter(|id| id % 2 == 0).collect();
        let mut looped = make_matcher(N as usize);
        for id in &victims {
            evict_one_sequential(&mut looped, *id);
        }
        let mut batched = make_matcher(N as usize);
        evict_all_one_pass(&mut batched, &victims);

        assert_eq!(looped.by_key, batched.by_key);
        let looped_artists = looped.artists.as_ref().unwrap();
        let batched_artists = batched.artists.as_ref().unwrap();
        assert_eq!(looped_artists.len(), batched_artists.len());
        for (id, entry) in looped_artists {
            let other = batched_artists
                .get(id)
                .unwrap_or_else(|| panic!("survivor {id} missing after batch evict"));
            assert_eq!(entry.name, other.name);
            assert_eq!(entry.key, other.key);
        }
    }

    /// matcher 的构造不计入耗时，只计逐出本身，报 MEASURED_RUNS 轮的 min/mean（ms）。
    fn time_evictions(
        name: &str,
        build: impl Fn() -> ArtistMatcher,
        evict: impl Fn(&mut ArtistMatcher),
    ) -> (f64, f64) {
        let mut warm = build();
        evict(&mut warm);
        black_box((
            warm.by_key.len(),
            warm.artists.as_ref().map_or(0, |a| a.len()),
        ));

        let mut samples = Vec::with_capacity(MEASURED_RUNS);
        for _ in 0..MEASURED_RUNS {
            let mut matcher = build(); // 不计入耗时
            let start = Instant::now();
            evict(&mut matcher);
            samples.push(start.elapsed().as_secs_f64() * 1_000.0);
            black_box((
                matcher.by_key.len(),
                matcher.artists.as_ref().map_or(0, |a| a.len()),
            ));
        }
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let mean = samples.iter().sum::<f64>() / MEASURED_RUNS as f64;
        println!("{name:<50} min={min:>9.2}ms  mean={mean:>9.2}ms");
        (min, mean)
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn report_artist_evict_costs() {
        assert_batch_matches_loop();

        let all_ids: Vec<i64> = (0..ARTISTS as i64).collect();
        let known_keys: Vec<(i64, String)> = all_ids
            .iter()
            .map(|&id| (id, token_key(&format!("artist-{id:04}"))))
            .collect();
        let small_ids: Vec<i64> = (0..EVICT_SMALL as i64).collect();

        println!(
            "dataset: {ARTISTS} artists in the matcher; evict called once per artist, \
             the pre-fix sync_artist_links call-site shape"
        );

        // 场景 A：审计的批量重打 tag——逐出全部歌手，每次调用扫一整遍 by_key
        // （修复前形态；生产代码现在成批处理）
        let (a_min, _) = time_evictions(
            "A   evict 10000/10000 sequential (pre-fix shape)",
            || make_matcher(ARTISTS),
            |m| {
                for id in &all_ids {
                    evict_one_sequential(m, *id);
                }
            },
        );

        // 对比：对整个受害集做一次单遍切分
        let (b_min, _) = time_evictions(
            "A'  evict 10000/10000 batch one-pass",
            || make_matcher(ARTISTS),
            |m| evict_all_one_pass(m, &all_ids),
        );

        // 下限：key 预先已知（逆向索引）时每次调用的 O(1) 移除
        let (f_min, _) = time_evictions(
            "A'' evict 10000/10000 per-call floor (key known)",
            || make_matcher(ARTISTS),
            |m| evict_with_known_keys(m, &known_keys),
        );

        // 场景 B：常规增量重扫
        let (s_min, _) = time_evictions(
            "B   evict 50/10000 sequential (current)",
            || make_matcher(ARTISTS),
            |m| {
                for id in &small_ids {
                    evict_one_sequential(m, *id);
                }
            },
        );

        println!(
            "per-evict average, scenario A (min run / {ARTISTS}): current {:.2}µs \
             vs batch {:.2}µs vs floor {:.2}µs",
            a_min * 1_000.0 / ARTISTS as f64,
            b_min * 1_000.0 / ARTISTS as f64,
            f_min * 1_000.0 / ARTISTS as f64,
        );
        println!(
            "bench_artist_evict: 10k×10k 总 {a_min:.0} ms（修复前：逐条 evict）vs {b_min:.0} ms\
             （修复后：evict_many 单遍批量）vs {f_min:.0} ms（逆向索引下限），加速比 {:.0}；\
             50 条增量逐出 {:.0} µs",
            a_min / b_min,
            s_min * 1_000.0,
        );
    }

    /// F029 第 1 部分单独计时 `token_key`；第 2 部分对播种好的数据库计完整
    /// `resolve` 命中路径，并报告这些（部分冗余的）`token_key` 调用占单次命中的
    /// 比例。「3 次调用、其中 2 次可省」的形态见模块文档注释。
    #[tokio::test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    async fn report_resolve_hit_path_costs() {
        // --- 第 1 部分：对真实的 21 字符名字单独计时 token_key ---
        let names: Vec<String> = (0..TOKEN_KEY_CALLS)
            .map(|i| format!("Some Artist Name {i:04}"))
            .collect();
        for name in &names[..1_000] {
            black_box(token_key(black_box(name)));
        }
        let mut tk_min = f64::INFINITY;
        let mut tk_total = 0.0;
        for _ in 0..MEASURED_RUNS {
            let start = Instant::now();
            for name in &names {
                black_box(token_key(black_box(name)));
            }
            let ms = start.elapsed().as_secs_f64() * 1_000.0;
            tk_min = tk_min.min(ms);
            tk_total += ms;
        }
        let tk_mean = tk_total / MEASURED_RUNS as f64;
        let tk_ns = tk_min * 1e6 / TOKEN_KEY_CALLS as f64;
        println!(
            "token_key standalone: {TOKEN_KEY_CALLS} calls, \"Some Artist Name %04d\": \
             min={tk_min:.1}ms  mean={tk_mean:.1}ms  ({tk_ns:.0} ns/call)"
        );

        // --- 第 2 部分：对播种了 10k 歌手的数据库计 resolve 命中 ---
        let (_dir, pool) = create_test_pool("artist-match-bench").await;
        {
            let mut tx = pool.begin().await.unwrap();
            for i in 0..ARTISTS {
                let name = format!("Some Artist Name {i:04}");
                sqlx::query("INSERT INTO artist (name, name_sortable) VALUES ($1, $2)")
                    .bind(&name)
                    .bind(&name)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }

        let mut conn = pool.acquire().await.unwrap();
        let mut matcher = ArtistMatcher::new();

        // 首次 resolve 触发 load()；预热轮不付别的成本——命中路径是无状态的
        for name in &names[..ARTISTS] {
            black_box(matcher.resolve(&mut conn, name, None).await.unwrap());
        }

        let mut pass_min = f64::INFINITY;
        let mut pass_total = 0.0;
        for _ in 0..RESOLVE_PASSES {
            let start = Instant::now();
            for name in &names[..ARTISTS] {
                black_box(
                    matcher
                        .resolve(&mut conn, black_box(name), None)
                        .await
                        .unwrap(),
                );
            }
            let ms = start.elapsed().as_secs_f64() * 1_000.0;
            pass_min = pass_min.min(ms);
            pass_total += ms;
        }
        let pass_mean = pass_total / RESOLVE_PASSES as f64;
        let hit_ns = pass_min * 1e6 / ARTISTS as f64;
        println!(
            "resolve hit path: {ARTISTS} hits x {RESOLVE_PASSES} passes: \
             min pass={pass_min:.2}ms  mean={pass_mean:.2}ms  ({hit_ns:.0} ns/hit)"
        );

        // 占比：修复后的命中形态是 1 次 token_key 调用（entry 名检查；`sort == name`
        // 稳态干脆跳过入参名那次调用——见 `resolve`）。命中路径不跑 SQL，所以占比
        // 不被 I/O 稀释。修复前的命中成本与占比记在 GPUI HARDCORE PERFORMANCE §44
        // 台账（2026-10-15 轮次）。
        let post_fix_share = tk_ns / hit_ns * 100.0;
        println!("token_key share of one resolve hit (post-fix, 1 call): {post_fix_share:.1}%");

        // 审计算术：100k 曲目 x 1.5 歌手 = 全库扫描 150k 次 resolve；修复前形态
        // 每次命中多付 2 次冗余调用，按实测每次调用成本计价
        let pre_fix_ms = FULL_SCAN_RESOLVES as f64 * 2.0 * tk_ns / 1e6;
        println!("full-scan redundancy eliminated (pre-fix 2 calls/hit): {pre_fix_ms:.1}ms");
        println!(
            "bench_resolve_hit: 单次命中 {hit_ns:.0} ns，token_key 占比 {post_fix_share:.1}%\
             ；全库 150k 次冗余 ≈ {pre_fix_ms:.1} ms（已消除）"
        );
    }
}
