use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use camino::{Utf8Path, Utf8PathBuf};
use futures::{StreamExt, stream::FuturesUnordered};
use rustc_hash::{FxHashMap, FxHashSet};
use sqlx::SqlitePool;
use tokio::{
    sync::{Mutex, mpsc::Sender},
    task::spawn_blocking,
};
use tracing::error;

use crate::library::scan::{
    discover::{
        DirectoryReadPolicy, DiscoverAction, DiscoveredPath, FoldedIndex, FolderArtCandidate,
        FolderArtObservations, PendingDirectoryRead, Relocation, SidecarLyrics, apply_relocation,
        canonicalize_or_keep, classify, delete_tracks, fold_excluded_roots, is_missing,
        is_under_excluded, missing_paths, schedule_directory_read,
    },
    fs_case::{fold_key, fold_path, folded_starts_with, same_file},
    record::ScanRecord,
};

/// 目标文件夹本身（`folder` 或 `location` 相等）里已记录的曲目。
const RECONCILE_FOLDER_OR_LOCATION_SQL: &str =
    include_str!("../../../../queries/scan/list_tracks_in_folder_or_location.sql");
/// 目标路径前缀之下任意位置已记录的曲目。
const RECONCILE_UNDER_PREFIX_SQL: &str =
    include_str!("../../../../queries/scan/list_tracks_under_prefix.sql");

struct RescanState {
    scan_record: Arc<Mutex<ScanRecord>>,
    /// 在遍历开始前建好的索引：目标文件夹之下已记录的路径。
    folded_index: FoldedIndex,
}

impl RescanState {
    /// 遍历开始前持锁构建一次折叠索引，emit 阶段锁内只剩 classify 的哈希查询。
    async fn new(scan_record: Arc<Mutex<ScanRecord>>, targets: &FxHashSet<Utf8PathBuf>) -> Self {
        let folded_targets: FxHashSet<Utf8PathBuf> =
            targets.iter().map(|target| fold_path(target)).collect();
        // 锁只覆盖索引构建：guard 若活到函数末尾，其 Drop 对 `scan_record`
        // 的借用会与 Self 捕获该 Arc 的移动冲突（E0505）
        let folded_index = {
            let records = scan_record.lock().await;
            index_records_under(&records.records, &folded_targets)
        };
        Self {
            scan_record,
            folded_index,
        }
    }
}

/// 为折叠后的目标索引已记录的路径，先按前缀过滤，以避免 Unix 上的 stat。
fn index_records_under(
    records: &FxHashMap<Utf8PathBuf, SystemTime>,
    folded_targets: &FxHashSet<Utf8PathBuf>,
) -> FoldedIndex {
    let mut index = FoldedIndex::default();
    'records: for (key, ts) in records {
        // 每个键只折叠一次，而不是每个目标折一次
        let folded_key = fold_key(key);
        for target in folded_targets {
            if folded_starts_with(&folded_key, target.as_str()) {
                index
                    .entry(fold_path(key))
                    .or_default()
                    .push((key.clone(), *ts));
                continue 'records;
            }
        }
    }
    index
}

/// 在给定路径之下跑 discovery：classify 与纯大小写改名搬迁和全量同款（合并语义）。
/// `respect_record=false`（record-blind，右键重扫/watcher 封面批次）只保留一个差异
/// ——时间戳未变也强制重读；若 record-blind 干脆跳过 classify，大小写改名会在库里
/// 插出新拼写行而旧拼写行保留，同一文件变成双行。
#[allow(clippy::too_many_arguments)]
pub async fn rescan_discover(
    paths: Vec<Utf8PathBuf>,
    scan_record: Arc<Mutex<ScanRecord>>,
    respect_record: bool,
    recursive: bool,
    path_tx: Sender<DiscoveredPath>,
    relocate_tx: Sender<Relocation>,
    cancel_flag: Arc<AtomicBool>,
    read_policy: DirectoryReadPolicy,
    folder_art: FolderArtObservations,
) -> u64 {
    let mut targets = FxHashSet::default();
    for entry in paths {
        if cancel_flag.load(Ordering::Relaxed) {
            return 0;
        }

        match entry.canonicalize_utf8() {
            Ok(canonical) => {
                targets.insert(canonical);
            }
            Err(e) => error!("Failed to canonicalize rescan path {:?}: {:?}", entry, e),
        }
    }

    let mut state = RescanState::new(scan_record, &targets).await;

    let mut visited = FxHashSet::default();
    let mut discovered_total: u64 = 0;
    let mut directories = VecDeque::new();
    let mut direct_files = Vec::new();

    for canonical in &targets {
        if cancel_flag.load(Ordering::Relaxed) {
            return discovered_total;
        }

        let Ok(inspection) = read_policy.inspect(canonical.clone()).await else {
            continue;
        };

        if inspection.metadata.is_dir() {
            directories.push_back(canonical.clone());
        } else if inspection.metadata.is_file()
            && let Some(timestamp) = inspection.scan_timestamp
        {
            direct_files.push((canonical.clone(), timestamp));
        }
    }

    let mut pending: FuturesUnordered<PendingDirectoryRead> = FuturesUnordered::new();
    while !directories.is_empty() || !pending.is_empty() {
        if cancel_flag.load(Ordering::Relaxed) {
            return discovered_total;
        }

        while pending.len() < read_policy.max_pending()
            && let Some(directory) = directories.pop_front()
        {
            if visited.insert(directory.clone()) {
                schedule_directory_read(&mut pending, read_policy.clone(), directory);
            }
        }

        let Some((directory, result)) = pending.next().await else {
            continue;
        };
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(e) => {
                error!("Failed to read directory {:?}: {:?}", directory, e);
                continue;
            }
        };
        let directory_art = snapshot.folder_art.clone();
        folder_art.record(directory, directory_art.clone());

        for entry in snapshot.entries {
            if cancel_flag.load(Ordering::Relaxed) {
                return discovered_total;
            }

            if entry.metadata.is_dir() {
                if recursive {
                    directories.push_back(entry.path);
                }
                continue;
            }
            if !entry.metadata.is_file() || !visited.insert(entry.path.clone()) {
                continue;
            }

            if emit_rescan_path(
                &entry.path,
                entry.scan_timestamp,
                directory_art.clone(),
                &mut state,
                respect_record,
                &path_tx,
                &relocate_tx,
                &cancel_flag,
            )
            .await
            .is_some()
            {
                discovered_total += 1;
            } else if cancel_flag.load(Ordering::Relaxed) {
                return discovered_total;
            }
        }
    }

    for (path, timestamp) in direct_files {
        if !visited.insert(path.clone()) {
            continue;
        }

        let folder_candidate = match path.parent() {
            Some(parent) => folder_art_for_parent(parent, &read_policy, &folder_art).await,
            None => None,
        };

        if emit_rescan_path(
            &path,
            Some(timestamp),
            folder_candidate,
            &mut state,
            respect_record,
            &path_tx,
            &relocate_tx,
            &cancel_flag,
        )
        .await
        .is_some()
        {
            discovered_total += 1;
        } else if cancel_flag.load(Ordering::Relaxed) {
            return discovered_total;
        }
    }

    discovered_total
}

#[allow(clippy::too_many_arguments)]
async fn emit_rescan_path(
    path: &Utf8Path,
    timestamp: Option<SystemTime>,
    folder_art: Option<FolderArtCandidate>,
    state: &mut RescanState,
    respect_record: bool,
    path_tx: &Sender<DiscoveredPath>,
    relocate_tx: &Sender<Relocation>,
    cancel_flag: &Arc<AtomicBool>,
) -> Option<SystemTime> {
    let timestamp = timestamp?;

    let (action, stale) = {
        let RescanState {
            scan_record,
            folded_index,
        } = state;
        let records = scan_record.lock().await;
        classify(path, timestamp, &records.records, folded_index)
    };

    // 与全量同款（full.rs）：丢掉指向同一个文件的其它已记录拼写。大小写不敏感
    // 卷上不合并的话，重扫入库就是同文件双行。搬迁发送在记录锁外进行。
    for (old, old_ts) in stale {
        if same_file(&old, path)
            && relocate_tx
                .send((old, path.to_path_buf(), old_ts))
                .await
                .is_err()
        {
            return None;
        }
    }

    let rescan_ts = match action {
        // record-blind 的目的就是强制重读：Skip 只在尊重记录时生效
        DiscoverAction::Skip => {
            if respect_record {
                return None;
            }
            Some(timestamp)
        }
        DiscoverAction::Scan(timestamp) => Some(timestamp),
        DiscoverAction::Relocate { candidates, ts } => {
            let relocated = apply_relocation(path, ts, candidates, relocate_tx)
                .await
                .ok()?;
            // 时间戳相等的纯改名在尊重模式下只搬迁不重读；record-blind 仍要重读
            if respect_record { relocated } else { Some(ts) }
        }
    };

    let timestamp = rescan_ts?;

    if cancel_flag.load(Ordering::Relaxed) {
        return None;
    }

    if path_tx
        .send(DiscoveredPath {
            path: path.to_path_buf(),
            timestamp,
            folder_art,
            // 定向扫描没有目录级的 lrc 列举——保留解码侧的那次探测
            sidecar_lyrics: SidecarLyrics::Unknown,
        })
        .await
        .is_err()
    {
        return None;
    }

    Some(timestamp)
}

/// 直接传入的文件的父目录的文件夹封面：复用遍历期间记录下的观测，若该目录从未被
/// 访问过就现在读一次。
async fn folder_art_for_parent(
    parent: &Utf8Path,
    read_policy: &DirectoryReadPolicy,
    folder_art: &FolderArtObservations,
) -> Option<FolderArtCandidate> {
    if let Some(observed) = folder_art.get(parent) {
        return observed;
    }
    match read_policy.read(parent.to_path_buf()).await {
        Ok(snapshot) => {
            let candidate = snapshot.folder_art;
            folder_art.record(parent.to_path_buf(), candidate.clone());
            candidate
        }
        Err(e) => {
            error!("Failed to read directory {:?}: {:?}", parent, e);
            None
        }
    }
}

/// 为 `target` 跑一次对账查询，返回它已记录的位置字符串；失败时记日志并返回 `None`。
/// `lookup` 用来补全日志消息（例如 `"query"` / `"prefix query"`）。
async fn fetch_reconciliation_locations(
    pool: &SqlitePool,
    query: &'static str,
    target: &Utf8Path,
    lookup: &str,
) -> Option<Vec<String>> {
    match sqlx::query_scalar::<_, String>(query)
        .bind(target.as_str())
        .fetch_all(pool)
        .await
    {
        Ok(rows) => Some(rows),
        Err(e) => {
            error!(
                "Rescan reconciliation {lookup} failed for {:?}: {:?}",
                target, e
            );
            None
        }
    }
}

/// 删除目标之下缺失的曲目与记录项。被排除的根目录不去动它们。
pub async fn reconcile_rescan_paths(
    pool: &SqlitePool,
    scan_record: &mut ScanRecord,
    paths: &[Utf8PathBuf],
    excluded_roots: &[Utf8PathBuf],
) -> FxHashSet<i64> {
    // 折叠后的前缀不受大小写或 \\?\ 差异影响，仍能匹配
    let excluded = fold_excluded_roots(excluded_roots);

    let targets: FxHashSet<Utf8PathBuf> = paths.iter().map(|p| canonicalize_or_keep(p)).collect();

    let mut candidates: FxHashSet<Utf8PathBuf> = FxHashSet::default();
    for target in &targets {
        let Some(mut rows) =
            fetch_reconciliation_locations(pool, RECONCILE_FOLDER_OR_LOCATION_SQL, target, "query")
                .await
        else {
            continue;
        };

        if is_missing(target)
            && let Some(descendants) = fetch_reconciliation_locations(
                pool,
                RECONCILE_UNDER_PREFIX_SQL,
                target,
                "prefix query",
            )
            .await
        {
            rows.extend(descendants);
        }

        candidates.extend(
            rows.into_iter()
                .map(Utf8PathBuf::from)
                // 无可排除项——跳过折叠（Unix 上那是每行一次 stat）
                .filter(|location| !is_under_excluded(location, &excluded)),
        );
    }

    // exists 探测是阻塞的文件系统工作：把它们挪出扫描工作线程，且探测失败时保留这些行
    // （下一次扫描会重试）
    let missing = spawn_blocking(move || missing_paths(candidates))
        .await
        .unwrap_or_default();
    let to_delete: Vec<Utf8PathBuf> = missing.into_iter().collect();
    delete_tracks(pool, scan_record, &to_delete).await
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::library::scan::{
        discover::{file_scan_timestamp, helpers::*},
        fs_case::is_case_insensitive,
    };

    #[test]
    fn rescan_discover_deduplicates_paths() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");
        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();

        let (count, mut path_rx, _relocate_rx) = run_rescan(
            vec![path.clone(), dir_path],
            shared_empty_record(),
            false,
            false,
        );
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
        assert!(path_rx.blocking_recv().is_none());
    }

    #[test]
    fn rescan_discover_expands_directories_one_level_only() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        std::fs::write(dir.join("top.flac"), b"").unwrap();
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.flac"), b"").unwrap();

        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let (count, mut path_rx, _relocate_rx) =
            run_rescan(vec![dir_path], shared_empty_record(), false, false);
        assert_eq!(count, 1);
        assert_eq!(
            path_rx.blocking_recv().unwrap().path.file_name().unwrap(),
            "top.flac"
        );
    }

    #[test]
    fn rescan_discover_recursive_walks_nested_directories() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        std::fs::write(dir.join("top.flac"), b"").unwrap();
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.flac"), b"").unwrap();

        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let (count, path_rx, _relocate_rx) =
            run_rescan(vec![dir_path], shared_empty_record(), false, true);
        assert_eq!(count, 2);

        let mut names: Vec<String> = collect_paths(path_rx)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["deep.flac", "top.flac"]);
    }

    /// record-blind 的核心契约：记录时间戳与文件一致（尊重记录时会 Skip）时，
    /// 强制重扫仍必须重读。
    #[test]
    fn rescan_discover_forces_reread_when_record_blind() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&path).unwrap();

        let record = shared_record_with(&path, ts);
        let (count, mut path_rx, _relocate_rx) =
            run_rescan(vec![path.clone()], record, false, false);
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
        assert!(path_rx.blocking_recv().is_none());
    }

    #[test]
    fn rescan_discover_respects_scan_record_state() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&path).unwrap();

        let record = shared_record_with(&path, ts);
        let (count, mut path_rx, _relocate_rx) =
            run_rescan(vec![path.clone()], Arc::clone(&record), true, true);
        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());

        // 陈旧条目意味着文件已变化——再发一次
        record
            .blocking_lock()
            .records
            .insert(path.clone(), ts - Duration::from_secs(1));

        let (count, mut path_rx, _relocate_rx) = run_rescan(vec![path.clone()], record, true, true);
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
    }

    #[test]
    fn rescan_discover_relocates_case_only_renames() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&path).unwrap();

        // 记录在同一路径的旧大小写之下——纯大小写改名
        let old = dir.utf8_join("TRACK.FLAC");
        let record = shared_record_with(&old, ts);

        let (count, mut path_rx, mut relocate_rx) =
            run_rescan(vec![path.clone()], record, true, true);

        if !is_case_insensitive(&dir.utf8_path()) {
            // 在大小写敏感的卷上它们是两个不同的文件——按新文件处理
            assert_eq!(count, 1);
            assert_eq!(path_rx.blocking_recv().unwrap().path, path);
            return;
        }

        // 改名没有改变时间戳——只搬迁，不重读
        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
        assert_eq!(relocate_rx.blocking_recv(), Some((old, path, ts)));
        assert!(relocate_rx.blocking_recv().is_none());
    }

    /// 大小写改名 + record-blind（右键重扫）：搬迁照发（合并旧拼写行），且仍强制
    /// 重读。修复前 record-blind 完全不跑 classify——新拼写直接解码入库，旧拼写
    /// 行保留，同一文件双行。
    #[test]
    fn rescan_discover_record_blind_relocates_case_only_renames() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&path).unwrap();

        // 记录在改名前的旧大小写之下——时间戳未变的纯大小写改名
        let old = dir.utf8_join("TRACK.FLAC");
        let record = shared_record_with(&old, ts);

        let (count, mut path_rx, mut relocate_rx) =
            run_rescan(vec![path.clone()], record, false, false);

        if !is_case_insensitive(&dir.utf8_path()) {
            // 大小写敏感的卷上它们是两个不同的文件——按新文件处理，无搬迁
            assert_eq!(count, 1);
            assert_eq!(path_rx.blocking_recv().unwrap().path, path);
            assert!(relocate_rx.blocking_recv().is_none());
            return;
        }

        // 与尊重记录模式（只搬迁不重读）相对：搬迁 + 强制重读
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
        assert!(path_rx.blocking_recv().is_none());
        assert_eq!(relocate_rx.blocking_recv(), Some((old, path, ts)));
        assert!(relocate_rx.blocking_recv().is_none());
    }

    /// 记录里同一文件有两种拼写（历史双行）：发现其一时即使时间戳未变（尊重记录
    /// 模式不重读），另一种拼写也必须搬迁合并——与全量同款语义。
    #[test]
    fn rescan_discover_merges_duplicate_record_spellings() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&path).unwrap();

        let dupe = dir.utf8_join("TRACK.FLAC");
        let mut record = ScanRecord::new_current();
        record.records.insert(path.clone(), ts);
        record.records.insert(dupe.clone(), ts);
        let record = Arc::new(Mutex::new(record));

        let (count, mut path_rx, mut relocate_rx) =
            run_rescan(vec![path.clone()], record, true, false);

        if !is_case_insensitive(&dir.utf8_path()) {
            // 大小写敏感的卷上没有折叠命中：时间戳相等即跳过，无搬迁
            assert_eq!(count, 0);
            assert!(path_rx.blocking_recv().is_none());
            assert!(relocate_rx.blocking_recv().is_none());
            return;
        }

        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
        assert_eq!(relocate_rx.blocking_recv(), Some((dupe, path, ts)));
        assert!(relocate_rx.blocking_recv().is_none());
    }

    #[test]
    fn rescan_discover_relocates_modified_case_only_renames() {
        register_test_media_providers();
        let dir = TestDir::new("rescan-test");
        let path = write_track(&dir, "track.flac");

        let old = dir.utf8_join("TRACK.FLAC");
        let record = shared_record_with(&old, UNIX_EPOCH);

        let (count, mut path_rx, mut relocate_rx) =
            run_rescan(vec![path.clone()], record, true, true);

        if !is_case_insensitive(&dir.utf8_path()) {
            assert_eq!(count, 1);
            assert_eq!(path_rx.blocking_recv().unwrap().path, path);
            return;
        }

        // 既改名又修改过——带着旧时间戳搬迁，然后再重读
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
        assert_eq!(relocate_rx.blocking_recv(), Some((old, path, UNIX_EPOCH)));
        assert!(relocate_rx.blocking_recv().is_none());
    }

    #[tokio::test]
    async fn reconcile_removes_deleted_file_in_directory() {
        let (dir, pool) = create_test_pool("reconcile-test").await;
        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let path1 = insert_track_file(&pool, &dir_path, "track1.flac", 1).await;
        let path2 = insert_track_file(&pool, &dir_path, "track2.flac", 2).await;

        std::fs::remove_file(&path1).unwrap();
        let mut scan_record = record_of(&[&path1, &path2]);

        let updated = reconcile_rescan_paths(&pool, &mut scan_record, &[dir_path], &[]).await;
        assert!(updated.is_empty());

        assert!(!scan_record.records.contains_key(&path1));
        assert!(scan_record.records.contains_key(&path2));
        assert_eq!(count_tracks_at(&pool, &path1).await, 0);
        assert_eq!(count_tracks_at(&pool, &path2).await, 1);
    }

    #[tokio::test]
    async fn reconcile_removes_deleted_single_file() {
        let (dir, pool) = create_test_pool("reconcile-test").await;
        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let path = insert_track_file(&pool, &dir_path, "track.flac", 1).await;

        std::fs::remove_file(&path).unwrap();
        let mut scan_record = record_of(&[&path]);

        // 只重扫这一个文件（按 location 匹配）
        let updated =
            reconcile_rescan_paths(&pool, &mut scan_record, std::slice::from_ref(&path), &[]).await;
        assert!(updated.is_empty());
        assert!(!scan_record.records.contains_key(&path));
        assert_eq!(count_rows(&pool, "track").await, 0);
    }

    #[tokio::test]
    async fn reconcile_handles_deleted_directory() {
        let (dir, pool) = create_test_pool("reconcile-test").await;
        let album_dir = dir.utf8_path().canonicalize_utf8().unwrap().join("album");
        std::fs::create_dir_all(&album_dir).unwrap();
        let path1 = insert_track_file(&pool, &album_dir, "track1.flac", 1).await;
        let path2 = insert_track_file(&pool, &album_dir, "track2.flac", 2).await;

        std::fs::remove_dir_all(&album_dir).unwrap();
        let mut scan_record = record_of(&[&path1, &path2]);

        let updated = reconcile_rescan_paths(&pool, &mut scan_record, &[album_dir], &[]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.is_empty());
        assert_eq!(count_rows(&pool, "track").await, 0);
    }

    #[tokio::test]
    async fn reconcile_removes_deleted_directory_tree() {
        let (dir, pool) = create_test_pool("reconcile-test").await;
        let artist_dir = dir.utf8_path().canonicalize_utf8().unwrap().join("artist");
        let album_dir = artist_dir.join("album");
        std::fs::create_dir_all(&album_dir).unwrap();
        let path1 = insert_track_file_with_meta(
            &pool,
            &artist_dir,
            "track1.flac",
            track_metadata("Album One", "Artist", "t1", 1),
        )
        .await;
        let path2 = insert_track_file_with_meta(
            &pool,
            &album_dir,
            "track2.flac",
            track_metadata("Album Two", "Artist", "t2", 1),
        )
        .await;

        // 整棵树都没了——匹配必须覆盖嵌套路径，而不只是直接子项
        std::fs::remove_dir_all(&artist_dir).unwrap();
        let mut scan_record = record_of(&[&path1, &path2]);

        let updated = reconcile_rescan_paths(&pool, &mut scan_record, &[artist_dir], &[]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.is_empty());
        assert_eq!(count_rows(&pool, "track").await, 0);
    }

    #[tokio::test]
    async fn reconcile_removes_deleted_windows_style_tree() {
        // Windows 的位置用反斜杠。生产存的是规范化过的路径（带 `\\?\` 前缀），
        // 因为扫描器会对每个根做规范化，而调用方可能传的是普通拼写：
        // canonicalize_or_keep 会向上找到确实存在的 `C:\` 祖先，并在其 verbatim
        // 前缀下重新拼回尾部，所以这两种形态在文件夹查询与前缀查询里仍必须匹配。
        let (_dir, pool) = create_test_pool("reconcile-test").await;
        let _stored_root = Utf8PathBuf::from(r"\\?\C:\Music\artist");
        let path1 = Utf8PathBuf::from(r"\\?\C:\Music\artist\track1.flac");
        let path2 = Utf8PathBuf::from(r"\\?\C:\Music\artist\album\track2.flac");
        insert_track_row(
            &pool,
            &path1,
            track_metadata("Album One", "Artist", "t1", 1),
        )
        .await;
        insert_track_row(
            &pool,
            &path2,
            track_metadata("Album Two", "Artist", "t2", 1),
        )
        .await;

        let mut scan_record = record_of(&[&path1, &path2]);

        let updated = reconcile_rescan_paths(
            &pool,
            &mut scan_record,
            &[Utf8PathBuf::from(r"C:\Music\artist")],
            &[],
        )
        .await;
        assert!(updated.is_empty());
        assert!(scan_record.records.is_empty());
        assert_eq!(count_rows(&pool, "track").await, 0);
    }

    #[tokio::test]
    async fn reconcile_keeps_tracks_under_excluded_roots() {
        let (dir, pool) = create_test_pool("reconcile-test").await;
        let root = dir.utf8_path().canonicalize_utf8().unwrap().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let path = insert_track_file(&pool, &root, "track.flac", 1).await;

        std::fs::remove_dir_all(&root).unwrap();
        let mut scan_record = record_of(&[&path]);

        let updated = reconcile_rescan_paths(
            &pool,
            &mut scan_record,
            std::slice::from_ref(&root),
            std::slice::from_ref(&root),
        )
        .await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&path));
        assert_eq!(count_rows(&pool, "track").await, 1);
    }
}
