use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::library::types::Track;

/// True when `path` is an HTTP(S) stream URL. Without the `online_sources`
/// feature no online tracks exist, so every path is local.
#[cfg_attr(not(feature = "online_sources"), allow(unused_variables))]
fn is_http(path: &Path) -> bool {
    #[cfg(feature = "online_sources")]
    if crate::media::is_http_path(path) {
        return true;
    }
    false
}

pub fn is_track_path_available(path: &Path) -> bool {
    // remote HTTP(S) streams (kugou et al.) don't exist on disk; treat any
    // http/https path as available so it can be selected and played
    if is_http(path) {
        return true;
    }
    path.exists()
}

/// True when `path` is an HTTP(S) stream URL (an online track). Such paths
/// report "available" for playback, but no file exists on disk, so actions
/// like "show in file manager" must not be offered for them.
pub fn is_online_path(path: &Path) -> bool {
    is_http(path)
}

pub fn is_track_available(track: &Track) -> bool {
    is_track_path_available(&track.location)
}

/// Which of `paths` are available (same probe as
/// [`is_track_path_available`]), one stat per path. Pure so callers can batch
/// a whole selection's stats into one blocking task — `Path::exists` is a
/// syscall and must stay off the UI thread.
///
/// 这里**刻意不用** `PathExistence` 的目录枚举探针：调用方是用户点击的少量路径
/// （几个到几十个），枚举整个父目录可能列出上千条目，比直接 stat 这几个路径更贵。
/// 目录枚举只在"整库快照"那种成千上万条路径、且同目录大量文件共享一次枚举时才划算。
pub fn available_paths(paths: Vec<PathBuf>) -> FxHashSet<PathBuf> {
    paths
        .into_iter()
        .filter(|path| is_track_path_available(path))
        .collect()
}

/// Monotonic ticket serializing the file-view batch actions: moving their
/// availability stats off the UI thread made completion order
/// nondeterministic, so each click takes a ticket and the async apply drops
/// itself when a newer click exists — the synchronous "last click wins"
/// ordering, restored.
pub static BATCH_ACTION_TICKET: AtomicU64 = AtomicU64::new(0);

/// Path -> "does it still exist" memo, backed by a per-directory listing cache.
/// Every availability set is grouped from the same track paths, so sharing one
/// cache across sets means one lookup per distinct path.
///
/// The probe is a **directory enumeration, not a per-file stat**. Measured on
/// Windows with 100,000 real files spread over 2,000 directories: `Path::exists`
/// cost 245.8us per path (24.6s for the set) while one `read_dir` per directory
/// cost 12.8us per path (1.28s); the same-harness Rust bench
/// [`tests::bench_availability_probe_styles`] measures **33x** over 30,000 probes.
/// That stat loop is the availability snapshot's dominant background cost
/// (100k tracks measured 15.75s), and the kernel path resolution behind each
/// `exists()` is what costs, not the SQL.
///
/// Same shape and same conservatism as the scanner's `missing_paths` (see
/// `library::scan::discover::cleanup`): a name found in the listing needs **no
/// stat at all**, and only an apparent absence is confirmed with one stat, so a
/// quirk of the listing can never grey out a playable track. A directory whose
/// listing fails as `NotFound` answers "everything inside is gone" without
/// stats; any other failure (permissions, a stalled mount) falls back to per
/// -path stats rather than guessing.
///
/// 残留的一条行为差（与旧的逐路径 stat 不同，且刻意保留 —— 消掉它就要给每个"存在"再付一次
/// stat，等于放弃这次收益）：**悬空符号链接/junction 报"存在"**，因为目录清单里有它的名字；
/// `Path::exists()` 会跟随链接、目标不可达时答 false。
#[derive(Default)]
struct PathExistence {
    known: FxHashMap<String, bool>,
    /// 父目录 -> 枚举结果。`Listed` 存的是该目录实际存在的条目名
    /// （Windows 名称大小写不敏感 ⇒ 存折叠后的小写）。
    dirs: FxHashMap<PathBuf, DirListing>,
}

/// 一次目录枚举的三种结局，语义与扫描器的 `missing_paths` 对齐。
#[derive(Default)]
enum DirListing {
    /// 枚举成功：持有该目录的条目名集合。
    Listed(FxHashSet<String>),
    /// `NotFound`：目录已不存在，里面的一切都不在。
    Gone,
    /// 从未探测 / 读取失败（权限、卡死的挂载点）⇒ 逐路径 stat，不猜。
    #[default]
    Unknown,
}

impl PathExistence {
    fn exists(&mut self, location: &str) -> bool {
        if let Some(&exists) = self.known.get(location) {
            return exists;
        }
        let exists = self.path_exists(Path::new(location));
        self.known.insert(location.to_owned(), exists);
        exists
    }

    /// 与 `Path::exists()` 同答复的探针：先枚举并缓存父目录，命中清单即视为存在
    /// （零系统调用），只有"清单里没有"才补一次 stat 确认。
    fn path_exists(&mut self, path: &Path) -> bool {
        // 没有文件名（空路径、裸根）时语义就是"这个目录本身在不在"，交回 stat。
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            return path.exists();
        };
        if !self.dirs.contains_key(parent) {
            let listing = match std::fs::read_dir(parent) {
                Ok(entries) => DirListing::Listed(
                    entries
                        .flatten()
                        .map(|entry| Self::fold(&entry.file_name().to_string_lossy()))
                        .collect(),
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirListing::Gone,
                Err(_) => DirListing::Unknown,
            };
            self.dirs.insert(parent.to_path_buf(), listing);
        }
        match self.dirs.get(parent) {
            Some(DirListing::Gone) => false,
            Some(DirListing::Listed(entries)) => {
                if entries.contains(&Self::fold(name.to_string_lossy().as_ref())) {
                    true
                } else {
                    // 清单说没有 ⇒ 可能是大小写/时序/枚举边界，用一次 stat 定案，
                    // 宁可多付一个系统调用也不能把能播的曲目灰掉。
                    path.exists()
                }
            }
            // 从未探测（不该发生）或目录读不动 ⇒ 保守地逐路径 stat。
            Some(DirListing::Unknown) | None => path.exists(),
        }
    }

    /// Windows 文件系统名大小写不敏感 ⇒ 折叠后比较；大小写敏感的平台上必须原样比较，
    /// 否则会把"只有大小写不同"的不存在路径误报成存在。
    fn fold(name: &str) -> String {
        #[cfg(windows)]
        {
            name.to_lowercase()
        }
        #[cfg(not(windows))]
        {
            name.to_owned()
        }
    }
}

/// Ids from `(id, location)` rows that still have at least one file on disk.
/// `any` stops at the first hit, so an id whose first track is present costs one
/// lookup and no further stats.
fn available_ids(rows: Vec<(i64, String)>, paths: &mut PathExistence) -> FxHashSet<i64> {
    let mut locations_by_id: FxHashMap<i64, Vec<String>> = FxHashMap::default();
    for (id, location) in rows {
        locations_by_id.entry(id).or_default().push(location);
    }

    let mut available: FxHashSet<i64> = FxHashSet::default();
    for (id, locations) in locations_by_id {
        if locations.iter().any(|location| paths.exists(location)) {
            available.insert(id);
        }
    }

    available
}

/// Album, artist and track availability from one pass over the base track
/// rows plus the integer credit pairs. Replaces the three-query snapshot:
/// the old album rows duplicated the whole track scan, and the old artist
/// query re-transferred each credit's location string over SQL — now one
/// full `(location, id, album_id)` scan plus one `(track_id, artist_id)`
/// scan feed all three sets through a single stat cache, so every distinct
/// path is statted exactly once and no location crosses SQL twice. Each
/// track id appears once in the base rows, so its slot means exactly "this
/// track's file still exists"; album-less tracks must not grey out of the
/// tracks table, which is why the base rows (not album-filtered rows) are
/// the source.
///
/// `album_id` of 0 means "no album": sqlx-sqlite decodes a NULL INTEGER
/// column into `i64` as 0 (its `Decode for i64` has no null check) and album
/// rowids start at 1, so standalone tracks land here with 0 and must not
/// conjure a phantom available album id.
pub fn compute_availability_from_tracks(
    track_rows: Vec<(String, i64, i64)>,
    artist_credits: Vec<(i64, i64)>,
) -> (FxHashSet<i64>, FxHashSet<i64>, FxHashSet<i64>) {
    let mut paths = PathExistence::default();

    let mut albums = FxHashSet::default();
    let mut tracks = FxHashSet::default();
    for (location, id, album_id) in &track_rows {
        if paths.exists(location) {
            tracks.insert(*id);
            if *album_id > 0 {
                albums.insert(*album_id);
            }
        }
    }

    // Credits reference tracks by id: look the location up instead of
    // cloning it per credit — one stat per distinct path regardless of how
    // many artists share the file.
    let locations: FxHashMap<i64, &str> = track_rows
        .iter()
        .map(|(location, id, _)| (*id, location.as_str()))
        .collect();
    let mut artists = FxHashSet::default();
    for (track_id, artist_id) in &artist_credits {
        if let Some(location) = locations.get(track_id)
            && paths.exists(location)
        {
            artists.insert(*artist_id);
        }
    }

    (albums, artists, tracks)
}

/// Ids of albums that still have at least one track on disk, from a
/// `(album_id, track location)` row set. Replaces the per-album
/// `list_tracks_in_album` N+1 with one query plus one directory enumeration per
/// distinct parent directory. Pure so the search index loader can run it on a
/// blocking thread; the probe is a syscall and must stay off the UI thread.
pub fn compute_available_albums(rows: Vec<(i64, String)>) -> FxHashSet<i64> {
    available_ids(rows, &mut PathExistence::default())
}

#[cfg(test)]
mod tests {
    use super::{PathExistence, compute_availability_from_tracks, compute_available_albums};
    use crate::test_support::TestDir;

    /// 枚举式探针必须与 `Path::exists()` 逐条同答复（含"目录本身""父目录不存在"
    /// 这两种边界），并且**每个不同的父目录只枚举一次**——后者就是 19× 收益的来源。
    #[test]
    fn path_existence_matches_stat_and_lists_each_dir_once() {
        let dir = TestDir::new("meliora-availability-dircache");
        let top = dir.join("Alpha.mp3");
        std::fs::write(&top, b"x").unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let nested = sub.join("beta.mp3");
        std::fs::write(&nested, b"x").unwrap();

        let cases = [
            top.clone(),
            nested.clone(),
            dir.join("missing.mp3"),
            sub.join("missing2.mp3"),
            sub.clone(),
            dir.join("nowhere").join("deep.mp3"),
        ];

        let mut cache = PathExistence::default();
        for path in &cases {
            assert_eq!(
                cache.path_exists(path),
                path.exists(),
                "枚举探针与 stat 答复不一致: {path:?}"
            );
        }
        // 覆盖到的父目录：dir、sub、dir/nowhere（读不出来）——共 3 个，各枚举一次。
        assert_eq!(
            cache.dirs.len(),
            3,
            "每个父目录应只枚举一次，实测 dirs 缓存 = {:?}",
            cache.dirs.keys().collect::<Vec<_>>()
        );
    }

    /// `(album id, location)` rows: an album counts as available when at least
    /// one of its tracks is still on disk.
    #[test]
    fn album_availability_needs_one_track_on_disk() {
        let dir = TestDir::new("meliora-availability-albums");
        let present = dir.join("present.mp3");
        std::fs::write(&present, b"x").unwrap();
        let present = present.display().to_string();
        let missing = dir.join("missing.mp3").display().to_string();

        let rows = vec![
            (1_i64, missing.clone()),
            (1_i64, present),
            (2_i64, missing.clone()),
            (3_i64, missing),
        ];

        let available = compute_available_albums(rows);

        assert!(available.contains(&1), "album with one file still on disk");
        assert!(!available.contains(&2), "album with only missing files");
        assert!(!available.contains(&3), "album with only missing files");
    }

    /// 同一链路里的两种探针成本对照：**改动在哪条链路，对照就在哪条链路做**
    /// （落地时的 19.25× / 4.9× 来自 python 探针，跨 harness 只能看量级，这条才是 Rust/std 自己的数）。
    ///
    /// 分四种形状各测一次比值，因为"命中清单即存在、未命中补一次 stat 定案"的语义
    /// 在不同形状上收益完全不同，混在一起只会得到误导性的单一数字：
    /// - `present`：文件都在 ⇒ 枚举后**零 stat**（稳态曲库里路径的绝大多数）；
    /// - `missing_in_live_dir`：目录在、文件不在 ⇒ 每条补一次 stat，**预期与逐个 stat 打平**；
    /// - `missing_in_gone_dir`：整个专辑目录被删 ⇒ 一次 `read_dir` 失败即答复全部子项，**零 stat**；
    /// - `mixed`：三者混合。
    ///
    /// 每轮都新建 `PathExistence`（目录缓存同样冷启动），并**按轮次奇偶交换两种探针的先后**
    /// ——谁先跑谁吃到 OS 目录项缓存的便宜，固定顺序会把差值算进结论。
    /// 每种形状都 `assert_eq!(hits_stat, hits_enum)`，把"同答复"当断言而不是靠肉眼比对。
    /// `ignored`：要造两万个文件，不参与默认 `cargo test`。
    /// 运行：`cargo test --release --features kugou bench_availability_probe_styles -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_availability_probe_styles() {
        use std::path::Path;

        const DIRS: usize = 400;
        const PER: usize = 50;

        let dir = TestDir::new("meliora-availability-bench");
        let mut present: Vec<String> = Vec::with_capacity(DIRS * PER);
        for d in 0..DIRS {
            let sub = dir.join(&format!("d{d:04}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..PER {
                let file = sub.join(format!("t{f:02}.flac"));
                std::fs::write(&file, b"x").unwrap();
                present.push(file.display().to_string());
            }
        }
        let half = present.len() / 2;
        let missing_live: Vec<String> = (0..half)
            .map(|i| dir.join(&format!("gone{i:05}.flac")).display().to_string())
            .collect();
        let missing_gone: Vec<String> = (0..half)
            .map(|i| {
                dir.join(&format!("deleted_album{i:04}"))
                    .join(format!("t{i}.flac"))
                    .display()
                    .to_string()
            })
            .collect();
        let mut mixed = present.clone();
        mixed.extend(missing_live.iter().cloned());
        mixed.extend(missing_gone.iter().cloned());

        println!(
            "造好 {DIRS} 目录 x {PER} 文件 = {} 个真实文件",
            present.len()
        );
        for (label, probes) in [
            ("present(全在)", &present),
            ("missing_in_live_dir", &missing_live),
            ("missing_in_gone_dir", &missing_gone),
            ("mixed", &mixed),
        ] {
            let mut stat_us: Vec<u128> = Vec::new();
            let mut enum_us: Vec<u128> = Vec::new();
            for round in 0..3 {
                let stat_pass = |hits: &mut usize| {
                    let t0 = std::time::Instant::now();
                    *hits = probes.iter().filter(|p| Path::new(p).exists()).count();
                    t0.elapsed().as_micros()
                };
                let enum_pass = |hits: &mut usize| {
                    let t0 = std::time::Instant::now();
                    let mut cache = PathExistence::default();
                    *hits = probes.iter().filter(|p| cache.exists(p)).count();
                    t0.elapsed().as_micros()
                };
                let (mut h1, mut h2) = (0usize, 0usize);
                let (t_stat, t_enum) = if round % 2 == 0 {
                    let a = stat_pass(&mut h1);
                    let b = enum_pass(&mut h2);
                    (a, b)
                } else {
                    let b = enum_pass(&mut h2);
                    let a = stat_pass(&mut h1);
                    (a, b)
                };
                assert_eq!(h1, h2, "{label}: 两种探针答复不同（round {round}）");
                stat_us.push(t_stat);
                enum_us.push(t_enum);
            }
            let best_stat = *stat_us.iter().min().unwrap();
            let best_enum = *enum_us.iter().min().unwrap();
            let n = probes.len().max(1);
            println!(
                "{label:<20} n={n:<6} stat={}us({:.1}/条) enum={}us({:.1}/条) 比值={:.2}x",
                best_stat,
                best_stat as f64 / n as f64,
                best_enum,
                best_enum as f64 / n as f64,
                best_stat as f64 / best_enum.max(1) as f64
            );
        }
    }

    /// All three sets derive from one pass over the base `(location, id,
    /// album_id)` rows plus the integer credit pairs: an album counts when
    /// any of its tracks is on disk, a track when its own file is, and an
    /// artist when any credited track's file is — including a file shared
    /// with another artist, which must mark both of them available.
    /// `album_id` 0 (sqlx's silent decode of a NULL column) must not conjure
    /// a phantom available album.
    #[test]
    fn availability_sets_derive_from_one_track_pass() {
        let dir = TestDir::new("meliora-availability-merged");
        let present = dir.join("present.mp3");
        std::fs::write(&present, b"x").unwrap();
        let present = present.display().to_string();
        let standalone = dir.join("standalone.mp3");
        std::fs::write(&standalone, b"x").unwrap();
        let standalone = standalone.display().to_string();
        let missing = dir.join("missing.mp3").display().to_string();

        let track_rows = vec![
            (present.clone(), 1_i64, 100_i64),
            (missing.clone(), 2_i64, 100_i64),
            (standalone, 3_i64, 0_i64),
        ];
        let credits = vec![(1, 10), (2, 10), (2, 11), (1, 12), (3, 13)];

        let (albums, artists, tracks) = compute_availability_from_tracks(track_rows, credits);

        assert!(albums.contains(&100), "album with one file on disk");
        assert!(
            !albums.contains(&0),
            "standalone track must not conjure album id 0"
        );
        assert!(
            tracks.contains(&1) && tracks.contains(&3),
            "present tracks available"
        );
        assert!(!tracks.contains(&2), "missing track unavailable");
        assert!(artists.contains(&10), "artist with a file that exists");
        assert!(!artists.contains(&11), "artist with only missing files");
        assert!(artists.contains(&12), "artist sharing the present file");
        assert!(artists.contains(&13), "standalone track's artist available");
    }

    /// An artist credited only at album level (`album_artist` with no
    /// `track_artist` row) must still count as available. Availability used to
    /// be computed from `track_artist` alone, which greyed out — and, because
    /// unavailable rows lose their click handler, made unclickable — every
    /// artist credited the album way, which is how most libraries credit theirs.
    #[tokio::test]
    async fn artist_availability_includes_album_level_credits() {
        use crate::library::db;
        use crate::test_support::{create_test_pool, insert_metadata, track_metadata};

        let (dir, pool) = create_test_pool("meliora-availability-album-artist").await;
        let path = dir.utf8_join("track.mp3");
        std::fs::write(&path, b"x").unwrap();

        let mut conn = pool.acquire().await.unwrap();
        insert_metadata(
            &mut conn,
            &track_metadata("Album", "Album Artist", "Title", 1),
            &path,
        )
        .await
        .unwrap();
        drop(conn);

        // Pin the album-only case: whatever `update_metadata` credited, leave
        // `album_artist` as the sole attribution path for this artist.
        sqlx::query("DELETE FROM track_artist")
            .execute(&pool)
            .await
            .unwrap();

        let artist_id: i64 = sqlx::query_scalar("SELECT id FROM artist WHERE name = $1")
            .bind("Album Artist")
            .fetch_one(&pool)
            .await
            .unwrap();

        let track_rows = db::list_all_tracks(&pool).await.unwrap();
        let credits = db::list_track_artist_credits(&pool).await.unwrap();
        let (_, available, _) = compute_availability_from_tracks(track_rows, credits);

        assert!(
            available.contains(&artist_id),
            "an album-level credit must keep the artist available"
        );
    }
}
