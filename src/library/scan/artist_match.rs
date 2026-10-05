use rustc_hash::FxHashMap;
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

    pub fn evict(&mut self, artist_id: i64) {
        if let Some(artists) = &mut self.artists {
            artists.remove(&artist_id);
        }
        self.by_key.retain(|_, id| *id != artist_id);
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
        // query orders by id - lowest id wins a duplicate key
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

        // prefer names matching the sort key - ties keep the existing row so order doesn't matter
        let entry = self
            .artists
            .as_mut()
            .unwrap()
            .get_mut(&artist_id)
            .expect("artist matcher key should point to a loaded artist");
        let is_canonical = |name: &str| token_key(name) == entry.key;
        if is_canonical(name) && !is_canonical(&entry.name) {
            let updated = sqlx::query(include_str!("../../../queries/scan/update_artist_name.sql"))
                .bind(name)
                .bind(entry.id)
                .execute(&mut *conn)
                .await?;
            // another artist already has this name - ignore the rename
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
            // name is taken by an artist with a different key - use that row
            Err(sqlx::Error::RowNotFound) => {
                let (id, name, sort): (i64, String, String) =
                    sqlx::query_as(include_str!("../../../queries/scan/get_artist_id.sql"))
                        .bind(name)
                        .fetch_one(&mut *conn)
                        .await?;
                let existing_key = token_key(&sort);
                // if sort defaulted to the name, take the incoming sort when free so aliases merge
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

        // an old database can have two artists sharing one token key
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

        matcher.evict(first);
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

/// Timing probes for audit findings F028 and F029:
///
/// ```text
/// cargo test --release artist_match::bench -- --ignored --nocapture
/// ```
///
/// F028 - `ArtistMatcher::evict` sweeps the whole `by_key` map with one
/// `retain` per evicted artist, and `sync_artist_links`
/// (database/artist_links.rs:62) calls it once per deleted link. The audited
/// worst case is a mass re-tag that orphans every artist: 10k evictions
/// against a 10k-entry map. Three variants are timed on synthetic matchers
/// built exactly the way `load` builds them (no database involved - this
/// child module can see the private fields):
///
/// - A: the current `evict(id)` called per artist in a loop (call-site
///   shape; the real loop additionally pays one SQL DELETE per link, which
///   this bench deliberately isolates out)
/// - A': a batched one-pass variant - one `retain` over both maps against
///   the whole victim set, the shape a batched cleanup API would take; its
///   end state is asserted equal to looping `evict` before anything is timed
/// - A'': the per-call floor any `evict(id)` API could reach with a reverse
///   index (id -> keys): O(1) hash removals, no sweep
///
/// plus scenario B, a regular incremental rescan (50 evictions out of the
/// same 10k matcher), which is what every ordinary scan pays.
///
/// F029 - the `resolve` hit path recomputes `token_key`: with `sort_name`
/// absent (:74) `key = token_key(name)` at :75, then `is_canonical(name)` at
/// :88 recomputes `token_key(name)` - always redundant on a hit, because a
/// `by_key` hit implies `entry.key == key` - and when the incoming name
/// equals the stored name (the steady state this bench seeds) the
/// short-circuit still evaluates `is_canonical(&entry.name)` on the same
/// string. The hit shape is therefore 3 `token_key` calls per hit, 2 of them
/// removable by reusing `key`. The bench times `token_key` standalone (100k
/// calls, per-call ns), then full `resolve` hits against a seeded
/// 10k-artist database (the hit path runs no SQL, so the share calculation
/// is not I/O-diluted), and reports the hit share plus the audit's
/// full-library extrapolation (100k tracks x 1.5 artists = 150k resolves).
///
/// Measured numbers are only ever written into this doc comment after an
/// actual run (GPUI HARDCORE §1: no numbers, no conclusion - never pre-fill).
#[cfg(test)]
mod bench {
    use std::{hint::black_box, time::Instant};

    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{ArtistEntry, ArtistMatcher, token_key};
    use crate::test_support::create_test_pool;

    /// matcher size / eviction count for the audited mass re-tag scenario,
    /// and the seeded database size for the resolve hits
    const ARTISTS: usize = 10_000;
    /// scenario B: evictions during a regular incremental rescan
    const EVICT_SMALL: usize = 50;
    /// measured runs per timed scenario; headline numbers use min
    const MEASURED_RUNS: usize = 3;
    /// standalone `token_key` calls (F029 part 1)
    const TOKEN_KEY_CALLS: usize = 100_000;
    /// full passes over the seeded dataset for the resolve hit timing
    const RESOLVE_PASSES: usize = 5;
    /// audit's full-library extrapolation: tracks x artists per track
    const FULL_SCAN_RESOLVES: usize = 150_000;

    /// Synthesizes the post-`load` state exactly the way `load` builds it
    /// from the `list_artists` rows: one `ArtistEntry` per id, `by_key`
    /// pointing each token key at its artist. Unique names give a 1:1
    /// key->id map, the nominal case the audit's arithmetic assumes.
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

    /// Comparison variant for the mass-eviction scenario: partition both
    /// maps once against the whole victim set instead of one full
    /// `by_key.retain` sweep per victim. Same end state as looping `evict`
    /// (asserted by `assert_batch_matches_loop`).
    fn evict_all_one_pass(matcher: &mut ArtistMatcher, victims: &[i64]) {
        let victims: FxHashSet<i64> = victims.iter().copied().collect();
        if let Some(artists) = &mut matcher.artists {
            artists.retain(|id, _| !victims.contains(id));
        }
        matcher.by_key.retain(|_, id| !victims.contains(id));
    }

    /// Per-call floor any `evict(id)` API could reach with a reverse index
    /// (id -> keys): with the key known, both removals are O(1) hash ops and
    /// no map sweep happens at all. Keys are handed in precomputed - that is
    /// the point of the variant.
    fn evict_with_known_keys(matcher: &mut ArtistMatcher, victims: &[(i64, String)]) {
        for (id, key) in victims {
            matcher.by_key.remove(key);
            if let Some(artists) = &mut matcher.artists {
                artists.remove(id);
            }
        }
    }

    /// The batch variant must leave exactly the state looping `evict`
    /// leaves. Verified over a set where only every other artist is evicted,
    /// so both maps keep survivors to compare.
    fn assert_batch_matches_loop() {
        const N: i64 = 1_000;
        let victims: Vec<i64> = (0..N).filter(|id| id % 2 == 0).collect();
        let mut looped = make_matcher(N as usize);
        for id in &victims {
            looped.evict(*id);
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

    /// Builds the matcher untimed, times only the eviction work, min/mean of
    /// MEASURED_RUNS in ms.
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
            let mut matcher = build(); // untimed
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
             the sync_artist_links (artist_links.rs:62) shape"
        );

        // scenario A: the audited mass re-tag - every artist evicted, one
        // full by_key sweep per call
        let (a_min, _) = time_evictions(
            "A   evict 10000/10000 sequential (current)",
            || make_matcher(ARTISTS),
            |m| {
                for id in &all_ids {
                    m.evict(*id);
                }
            },
        );

        // comparison: single-pass partition over the whole victim set
        let (b_min, _) = time_evictions(
            "A'  evict 10000/10000 batch one-pass",
            || make_matcher(ARTISTS),
            |m| evict_all_one_pass(m, &all_ids),
        );

        // floor: per-call O(1) removals with pre-known keys (reverse index)
        let (f_min, _) = time_evictions(
            "A'' evict 10000/10000 per-call floor (key known)",
            || make_matcher(ARTISTS),
            |m| evict_with_known_keys(m, &known_keys),
        );

        // scenario B: regular incremental rescan
        let (s_min, _) = time_evictions(
            "B   evict 50/10000 sequential (current)",
            || make_matcher(ARTISTS),
            |m| {
                for id in &small_ids {
                    m.evict(*id);
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
            "bench_artist_evict: 10k×10k 总 {a_min:.0} ms（现实现）vs {b_min:.0} ms\
             （单遍批量）vs {f_min:.0} ms（逆向索引下限），加速比 {:.0}；\
             50 条增量驱逐 {:.0} µs",
            a_min / b_min,
            s_min * 1_000.0,
        );
    }

    /// F029 part 1 times `token_key` standalone; part 2 times the full
    /// `resolve` hit path against a seeded database and reports how much of
    /// a hit the (partially redundant) `token_key` calls make up. See the
    /// module doc for the 3-calls-2-removable shape.
    #[tokio::test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    async fn report_resolve_hit_path_costs() {
        // --- part 1: standalone token_key over realistic 21-char names ---
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

        // --- part 2: resolve hits against a seeded 10k-artist database ---
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

        // first resolve triggers load(); the warm pass costs nothing else -
        // the hit path is stateless
        for name in &names[..ARTISTS] {
            black_box(matcher.resolve(&mut conn, name, None).await.unwrap());
        }

        let mut pass_min = f64::INFINITY;
        let mut pass_total = 0.0;
        for _ in 0..RESOLVE_PASSES {
            let start = Instant::now();
            for name in &names[..ARTISTS] {
                black_box(matcher.resolve(&mut conn, black_box(name), None).await.unwrap());
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

        // share: seeded shape = 3 token_key calls per hit, 2 removable by
        // reusing `key` (module doc); the hit path runs no SQL, so the share
        // is not I/O-diluted
        let total_share = 3.0 * tk_ns / hit_ns * 100.0;
        let removable_share = 2.0 * tk_ns / hit_ns * 100.0;
        println!(
            "token_key share of one resolve hit: total (3 calls) {total_share:.1}%  \
             removable (2 calls) {removable_share:.1}%"
        );

        // audit arithmetic: 100k tracks x 1.5 artists = 150k resolves per
        // full-library scan, priced at the measured per-call cost
        let task_ms = FULL_SCAN_RESOLVES as f64 * tk_ns / 1e6;
        let measured_ms = FULL_SCAN_RESOLVES as f64 * 2.0 * tk_ns / 1e6;
        println!(
            "full-scan redundancy: 150k calls = {task_ms:.1}ms (audit's 1/hit); \
             measured shape (2/hit) = {measured_ms:.1}ms"
        );
        println!(
            "bench_resolve_hit: 单次命中 {hit_ns:.0} ns，token_key 占比 {total_share:.1}%\
             （其中可省 {removable_share:.1}%）；全库 150k 次冗余 ≈ {measured_ms:.1} ms"
        );
    }
}
