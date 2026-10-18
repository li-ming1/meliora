use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::library::types::Track;

/// `path` 是否是 HTTP(S) 流 URL。没有 `online_sources` feature 时不存在在线曲目，
/// 所以每个路径都是本地的。
#[cfg_attr(not(feature = "online_sources"), allow(unused_variables))]
fn is_http(path: &Path) -> bool {
    #[cfg(feature = "online_sources")]
    if crate::media::is_http_path(path) {
        return true;
    }
    false
}

pub fn is_track_path_available(path: &Path) -> bool {
    // 远程 HTTP(S) 流（酷狗等）在盘上并不存在；把任何 http/https 路径都当作
    // 可用，这样它能被选中并播放
    if is_http(path) {
        return true;
    }
    path.exists()
}

/// `path` 是否是 HTTP(S) 流 URL（在线曲目）。这类路径在播放时报告「可用」，
/// 但盘上并不存在文件，所以「在文件管理器中显示」这类动作绝不能为它们提供。
pub fn is_online_path(path: &Path) -> bool {
    is_http(path)
}

pub fn is_track_available(track: &Track) -> bool {
    is_track_path_available(&track.location)
}

/// `paths` 中哪些可用（与 [`is_track_path_available`] 同一枚探针），每路径一次
/// stat。写成纯函数，好让调用方把整个选区的 stat 合并进一次阻塞任务——
/// `Path::exists` 是系统调用，必须留在 UI 线程之外。
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

/// 用一个单调 ticket 给文件视图的批量动作串行化：把它们的可用性 stat 挪出 UI
/// 线程之后完成顺序就不确定了，所以每次点击取一张 ticket，异步应用发现自己
/// 后面有更新的点击时就自我丢弃——同步时代的「最后一次点击为准」顺序就此恢复。
pub static BATCH_ACTION_TICKET: AtomicU64 = AtomicU64::new(0);

/// 路径 → 「它还存在吗」的记忆表，底下是一本按目录的列表缓存。
/// 每一组可用性集合都从同一批曲目路径分组而来，所以跨集合共享一本缓存就意味着
/// 每个不同路径只查一次。
///
/// 这枚探针是**目录枚举，不是逐文件 stat**。在 Windows 上实测：100,000 个真实
/// 文件散布在 2,000 个目录里，`Path::exists` 每路径 245.8us（整组 24.6s），
/// 而每目录一次 `read_dir` 只花每路径 12.8us（1.28s）；同一 harness 的 Rust
/// 基准 [`tests::bench_availability_probe_styles`] 在 30,000 次探测上测得
/// **33 倍**。那圈 stat 正是可用性快照的主要后台开销（100k 曲目实测 15.75s），
/// 花钱的是每次 `exists()` 背后的内核路径解析，不是 SQL。
///
/// 形状与保守程度都同扫描器的 `missing_paths`（见
/// `library::scan::discover::cleanup`）：在列表里找到名字**一次 stat 都不需要**，
/// 只有「看起来不存在」才用一次 stat 确认，所以列表的任何怪异都绝不可能把一首
/// 可播放的曲目置灰。列表枚举以 `NotFound` 失败的目录直接回答「里面全没了」，
/// 不花 stat；其他任何失败（权限、卡住的挂载）回退到逐路径 stat，而不是猜。
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

/// 从 `(id, location)` 行里取出「至少还有一个文件在盘上」的 id。
/// `any` 命中第一个就停，所以第一首曲目仍在的那个 id 只花一次查找、不再多花 stat。
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

/// 专辑、艺人与曲目的可用性由一趟基础曲目行加整数署名对的扫描得出。它取代原来的
/// 三查询快照：旧的专辑行把整趟曲目扫描重复了一遍，旧的艺人查询又让每条署名的
/// location 字符串第二次穿过 SQL——现在一次全量 `(location, id, album_id)` 扫描
/// 加一次 `(track_id, artist_id)` 扫描，通过同一份 stat 缓存喂出三个集合，于是
/// 每个不同路径恰好 stat 一次、没有任何 location 二次穿过 SQL。每首曲目在基础行
/// 里只出现一次，所以它的槽位含义正是「这首曲目的文件还在」；没有专辑的曲目
/// 绝不能从曲目表里置灰，这就是以基础行（而不是按专辑过滤后的行）为来源的原因。
///
/// `album_id` 为 0 表示「没有专辑」：sqlx-sqlite 把 NULL INTEGER 列解码成 `i64`
/// 时给出 0（它的 `Decode for i64` 没有 null 检查），而专辑 rowid 从 1 开始，
/// 所以独立曲目到这里是 0，绝不能凭空造出一个可用的专辑 id。
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

    // 署名按 id 引用曲目：把 location 查出来而不是每条署名克隆一次——
    // 无论多少个艺人共用这个文件，每个不同路径都只 stat 一次。
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

/// 从 `(album_id, track location)` 行集里取出「至少还有一首曲目在盘上」的专辑 id。
/// 它取代按专辑逐个 `list_tracks_in_album` 的 N+1，改为一次查询外加每个不同父目录
/// 一次目录枚举。写成纯函数，好让搜索索引加载器在阻塞线程上跑它——探针是系统
/// 调用，必须留在 UI 线程之外。
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

    /// `(专辑 id, location)` 行：一张专辑只要至少一首曲目还在盘上就算可用。
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

    /// 三个集合都由一趟基础 `(location, id, album_id)` 行加整数署名对推出：
    /// 专辑只要任一曲目在盘上就算可用，曲目看它自己的文件，艺人看任一署名曲目的
    /// 文件——包括与另一个艺人共用的那个文件，它必须让两个人都标记为可用。
    /// `album_id` 0（sqlx 对 NULL 列的静默解码）绝不能凭空造出一个可用的专辑。
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

    /// 只在专辑层被署名（有 `album_artist` 但没有 `track_artist` 行）的艺人也必须
    /// 算作可用。可用性过去只从 `track_artist` 算，那会把所有按专辑方式署名的艺人
    /// 置灰——并且因为不可用的行会失去点击处理器——让他们点不动，而绝大多数库
    /// 正是按专辑方式署名的。
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

        // 钉住「仅专辑署名」这种情形：无论 `update_metadata` 记了什么署名，
        // 都让这个艺人只保留 `album_artist` 这一条归属路径。
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
