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

use crate::{
    library::scan::{
        discover::{
            DirectoryReadPolicy, DiscoverAction, DiscoveredPath, FoldedIndex,
            FolderArtObservations, PendingDirectoryRead, Relocation, apply_relocation,
            canonicalize_or_keep, classify, delete_tracks, fold_excluded_roots, is_under_excluded,
            missing_paths, schedule_directory_read,
        },
        fs_case::{fold_path, same_file},
        record::ScanRecord,
    },
    settings::scan::ScanSettings,
};

fn build_folded_index(records: &FxHashMap<Utf8PathBuf, SystemTime>) -> FoldedIndex {
    let mut index = FoldedIndex::default();
    for (key, ts) in records {
        index
            .entry(fold_path(key))
            .or_default()
            .push((key.clone(), *ts));
    }
    index
}

/// 遍历库根目录，把需要扫描的文件发往 `path_tx`。
pub async fn discover(
    settings: ScanSettings,
    scan_record: Arc<Mutex<ScanRecord>>,
    path_tx: Sender<DiscoveredPath>,
    relocate_tx: Sender<Relocation>,
    cancel_flag: Arc<AtomicBool>,
    read_policy: DirectoryReadPolicy,
    folder_art: FolderArtObservations,
) -> u64 {
    let mut visited: FxHashSet<Utf8PathBuf> = FxHashSet::default();
    // 做规范化，好让大小写与 \\?\ 变体在 visited 里收敛到一处
    let mut directories: VecDeque<Utf8PathBuf> = settings
        .paths
        .iter()
        .map(|p| canonicalize_or_keep(p))
        .collect();
    let folded_index = {
        let sr = scan_record.lock().await;
        build_folded_index(&sr.records)
    };
    let mut discovered_total: u64 = 0;
    let mut pending: FuturesUnordered<PendingDirectoryRead> = FuturesUnordered::new();

    while !directories.is_empty() || !pending.is_empty() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
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
        let directory_art = snapshot.folder_art;
        folder_art.record(directory, directory_art.clone());

        for entry in snapshot.entries {
            if cancel_flag.load(Ordering::Relaxed) {
                return discovered_total;
            }

            let path = entry.path;
            let metadata = entry.metadata;

            if metadata.is_dir() {
                directories.push_back(path);
                continue;
            }

            if !metadata.is_file() {
                continue;
            }

            let mut rescan_ts = None;
            if let Some(timestamp) = entry.scan_timestamp {
                let (action, stale) = {
                    let sr = scan_record.lock().await;
                    classify(&path, timestamp, &sr.records, &folded_index)
                };
                // 丢掉指向同一个文件的其他已记录路径
                for (old, old_ts) in stale {
                    if same_file(&old, &path)
                        && relocate_tx.send((old, path.clone(), old_ts)).await.is_err()
                    {
                        return discovered_total;
                    }
                }
                match action {
                    DiscoverAction::Scan(ts) => rescan_ts = Some(ts),
                    DiscoverAction::Relocate { candidates, ts } => {
                        match apply_relocation(&path, ts, candidates, &relocate_tx).await {
                            Ok(next) => rescan_ts = next,
                            Err(()) => return discovered_total,
                        }
                    }
                    DiscoverAction::Skip => {}
                }
            }

            let Some(ts) = rescan_ts else {
                continue;
            };

            discovered_total += 1;

            if cancel_flag.load(Ordering::Relaxed) {
                return discovered_total;
            }

            if path_tx
                .send(DiscoveredPath {
                    path,
                    timestamp: ts,
                    folder_art: directory_art.clone(),
                    sidecar_lyrics: entry.sidecar_lyrics,
                })
                .await
                .is_err()
            {
                return discovered_total;
            }
        }
    }

    discovered_total
}

const CLEANUP_PAGE_SIZE: i64 = 1000;

/// `path` 位于某个已不再配置的目录之下、且自身不在任何当前扫描根之下时为 true。
/// 后半句不可省：库根从父目录改成其子目录时，旧父根仍是「已移除」，但子根下的
/// 曲目仍在扫描范围内，按前缀直接删行会丢掉歌单条目。
fn in_removed_directory(
    path: &Utf8Path,
    removed_dirs: &[Utf8PathBuf],
    current_dirs: &FxHashSet<Utf8PathBuf>,
) -> bool {
    !removed_dirs.is_empty() && {
        let folded = fold_path(path);
        !current_dirs.iter().any(|dir| folded.starts_with(dir))
            && removed_dirs.iter().any(|dir| folded.starts_with(dir))
    }
}

/// 移除缺失或已不再配置的曲目并返回受影响的歌单。
pub async fn cleanup_stale_tracks(
    pool: &SqlitePool,
    scan_record: &mut ScanRecord,
    current_directories: &[Utf8PathBuf],
    excluded_roots: &[Utf8PathBuf],
) -> FxHashSet<i64> {
    cleanup_stale_tracks_paged(
        pool,
        scan_record,
        current_directories,
        excluded_roots,
        CLEANUP_PAGE_SIZE,
    )
    .await
}

// 单独成函数，好让测试可以调小页大小
async fn cleanup_stale_tracks_paged(
    pool: &SqlitePool,
    scan_record: &mut ScanRecord,
    current_directories: &[Utf8PathBuf],
    excluded_roots: &[Utf8PathBuf],
    page_size: i64,
) -> FxHashSet<i64> {
    // 折叠后的前缀不受大小写或 \\?\ 差异影响，仍能匹配
    let current_set: FxHashSet<Utf8PathBuf> = current_directories
        .iter()
        .map(|p| fold_path(&canonicalize_or_keep(p)))
        .collect();
    let removed_dirs: Vec<Utf8PathBuf> = scan_record
        .directories
        .iter()
        .map(|p| fold_path(&canonicalize_or_keep(p)))
        // 被当前任一扫描根覆盖的旧目录不算「已移除」：库根从子目录改成其父目录时，
        // 旧子目录仍在扫描范围内，单纯按前缀判定会把仍存在的曲目删行（playlist_item
        // 会随外键级联清空）。
        .filter(|p| !current_set.iter().any(|current| p.starts_with(current)))
        .collect();

    let excluded = fold_excluded_roots(excluded_roots);

    // 记录键用借用引用跟踪，而不是把整张记录表克隆进一个临时集合；幽灵项在下面
    // 按拥有权收集，这样借用能在 `delete_tracks` 可变取用记录表之前结束
    let mut seen: FxHashSet<&Utf8PathBuf> = FxHashSet::default();
    let mut to_delete: Vec<Utf8PathBuf> = Vec::new();

    let mut last_id: i64 = 0;

    loop {
        let page = sqlx::query_as::<_, (i64, String)>(include_str!(
            "../../../../queries/scan/list_track_locations_paged.sql"
        ))
        .bind(last_id)
        .bind(page_size)
        .fetch_all(pool)
        .await;

        let page = match page {
            Ok(page) => page,
            Err(e) => {
                error!("Track cleanup paging failed, aborting sweep: {:?}", e);
                return FxHashSet::default();
            }
        };

        if page.is_empty() {
            break;
        }

        let mut existence_candidates: Vec<Utf8PathBuf> = Vec::with_capacity(page.len());
        for (id, location) in &page {
            last_id = *id;
            let path = Utf8PathBuf::from(location);
            if let Some((recorded, _)) = scan_record.records.get_key_value(&path) {
                seen.insert(recorded);
            }
            if is_under_excluded(&path, &excluded) {
                continue;
            }
            if in_removed_directory(&path, &removed_dirs, &current_set) {
                to_delete.push(path);
            } else {
                existence_candidates.push(path);
            }
        }
        // exists 探测是阻塞的文件系统工作：把它们挪出扫描工作线程，且探测失败时
        // 保留本页的行（下一次扫描会重试）
        let missing = spawn_blocking(move || missing_paths(existence_candidates))
            .await
            .unwrap_or_default();
        to_delete.extend(missing);

        if page.len() < page_size as usize {
            break;
        }
    }

    // 没有任何 DB 位置能对上的记录表幽灵项（通常极少）
    let ghosts: Vec<Utf8PathBuf> = scan_record
        .records
        .keys()
        .filter(|recorded| !seen.contains(recorded))
        .cloned()
        .collect();

    let mut existence_candidates: Vec<Utf8PathBuf> = Vec::with_capacity(ghosts.len());
    for path in ghosts {
        if is_under_excluded(&path, &excluded) {
            continue;
        }
        if in_removed_directory(&path, &removed_dirs, &current_set) {
            to_delete.push(path);
        } else {
            existence_candidates.push(path);
        }
    }

    let missing = spawn_blocking(move || missing_paths(existence_candidates))
        .await
        .unwrap_or_default();
    to_delete.extend(missing);
    delete_tracks(pool, scan_record, &to_delete).await
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use camino::{Utf8Path, Utf8PathBuf};
    use rustc_hash::FxHashMap;

    use super::*;
    use crate::library::scan::{
        discover::{file_scan_timestamp, helpers::*},
        fs_case::is_case_insensitive,
    };

    #[test]
    fn discover_emits_supported_files_recursively() {
        register_test_media_providers();
        let dir = TestDir::new("discover-test");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("track1.flac"), b"").unwrap();
        std::fs::write(dir.join("readme.txt"), b"").unwrap();
        std::fs::write(sub.join("track2.mp3"), b"").unwrap();

        let (count, path_rx, _relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), ScanRecord::new_current());

        let paths = collect_paths(path_rx);
        assert_eq!(count, 2);
        assert_eq!(paths.len(), 2);
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string())
            .collect();
        assert!(names.contains(&"track1.flac".to_string()));
        assert!(names.contains(&"track2.mp3".to_string()));
    }

    #[test]
    fn discover_attaches_the_directory_art_candidate_to_tracks() {
        register_test_media_providers();
        let dir = TestDir::new("discover-folder-art-test");
        let track = write_track(&dir, "track.flac");
        std::fs::write(dir.join("folder.jpg"), b"folder").unwrap();
        std::fs::write(dir.join("cover.png"), b"cover").unwrap();

        let (count, mut path_rx, _relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), ScanRecord::new_current());
        let discovered = path_rx.blocking_recv().unwrap();

        assert_eq!(count, 1);
        assert_eq!(discovered.path, track);
        // 扫描器会对每个根做规范化，所以文件夹封面候选是以规范化形态
        // （Windows 上带 `\\?\` 前缀）上报的，与上面的曲目路径完全一致
        assert_eq!(
            discovered.folder_art.unwrap().path,
            dir.utf8_join("cover.png").canonicalize_utf8().unwrap()
        );
    }

    #[test]
    fn discover_skips_unchanged_recorded_files() {
        register_test_media_providers();
        let dir = TestDir::new("discover-test");
        let path = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&path).unwrap();

        let mut record = ScanRecord::new_current();
        record.records.insert(path, ts);

        let (count, mut path_rx, _relocate_rx) = run_discover(
            scan_settings(dir.utf8_path().canonicalize_utf8().unwrap()),
            record,
        );
        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
    }

    #[test]
    fn discover_emits_file_when_timestamp_differs() {
        register_test_media_providers();
        let dir = TestDir::new("discover-test");
        let path = write_track(&dir, "track.flac");

        let mut record = ScanRecord::new_current();
        record
            .records
            .insert(path.clone(), UNIX_EPOCH + Duration::from_secs(1));

        let (count, mut path_rx, _relocate_rx) = run_discover(
            scan_settings(dir.utf8_path().canonicalize_utf8().unwrap()),
            record,
        );
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
    }

    #[test]
    fn discover_emits_file_when_sidecar_lyrics_changes() {
        register_test_media_providers();
        let dir = TestDir::new("discover-test");
        let path = write_track(&dir, "track.flac");
        let old_ts = file_scan_timestamp(&path).unwrap();

        std::fs::write(dir.join("track.lrc"), "[00:00.00] lyrics").unwrap();

        let mut record = ScanRecord::new_current();
        record.records.insert(path.clone(), old_ts);

        let (count, mut path_rx, _relocate_rx) = run_discover(
            scan_settings(dir.utf8_path().canonicalize_utf8().unwrap()),
            record,
        );
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, path);
    }

    #[test]
    fn discover_walks_case_variant_roots_once() {
        register_test_media_providers();
        let dir = TestDir::new("discover-case-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        std::fs::write(dir.join("track.flac"), b"").unwrap();

        let root = dir.utf8_path().canonicalize_utf8().unwrap();
        let variant = Utf8PathBuf::from(root.as_str().to_uppercase());

        let settings = ScanSettings {
            paths: vec![root, variant],
            ..Default::default()
        };
        let (count, mut path_rx, _relocate_rx) = run_discover(settings, ScanRecord::new_current());
        assert_eq!(count, 1);
        assert!(path_rx.blocking_recv().is_some());
        assert!(path_rx.blocking_recv().is_none());
    }

    #[test]
    fn discover_sends_relocation_without_rescan_for_stale_cased_record_key() {
        register_test_media_providers();
        let dir = TestDir::new("discover-relocate-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let on_disk = write_track(&dir, "Track.flac");
        // 记录里仍是改名之前的旧大小写
        let stale = on_disk.parent().unwrap().join("TRACK.FLAC");
        let ts = file_scan_timestamp(&on_disk).unwrap();

        let mut record = ScanRecord::new_current();
        record.records.insert(stale.clone(), ts);

        let (count, mut path_rx, mut relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), record);

        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
        let (old, new, relocated_ts) = relocate_rx.blocking_recv().expect("expected a relocation");
        assert_eq!(old, stale);
        assert_eq!(new, on_disk);
        assert_eq!(relocated_ts, ts);
        assert!(relocate_rx.blocking_recv().is_none());
    }

    #[test]
    fn discover_relocates_the_unmodified_spelling_among_candidates() {
        register_test_media_providers();
        let dir = TestDir::new("discover-relocate-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let on_disk = write_track(&dir, "track.flac");
        let ts = file_scan_timestamp(&on_disk).unwrap();

        let stale = on_disk.parent().unwrap().join("TRACK.FLAC");
        let current = on_disk.parent().unwrap().join("Track.flac");
        let mut record = ScanRecord::new_current();
        record.records.insert(stale, UNIX_EPOCH);
        record.records.insert(current.clone(), ts);

        let (count, mut path_rx, mut relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), record);

        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
        assert_eq!(relocate_rx.blocking_recv(), Some((current, on_disk, ts)));
        assert!(relocate_rx.blocking_recv().is_none());
    }

    #[test]
    fn discover_relocates_modified_case_only_rename_with_recorded_timestamp() {
        register_test_media_providers();
        let dir = TestDir::new("discover-relocate-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let on_disk = write_track(&dir, "track.flac");
        // 更旧时间戳上的旧大小写——既改名又修改过
        let stale = on_disk.parent().unwrap().join("TRACK.FLAC");

        let mut record = ScanRecord::new_current();
        record.records.insert(stale.clone(), UNIX_EPOCH);

        let (count, mut path_rx, mut relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), record);

        // 保留旧时间戳，这样被打断的重扫仍会重读它
        assert_eq!(count, 1);
        assert_eq!(path_rx.blocking_recv().unwrap().path, on_disk);
        assert_eq!(
            relocate_rx.blocking_recv(),
            Some((stale, on_disk, UNIX_EPOCH))
        );
        assert!(relocate_rx.blocking_recv().is_none());
    }

    #[test]
    fn discover_merges_duplicate_record_spellings_of_the_same_file() {
        register_test_media_providers();
        let dir = TestDir::new("discover-merge-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let on_disk = write_track(&dir, "track.flac");
        // 记录里有两种拼写，都指向同一个文件
        let dupe = on_disk.parent().unwrap().join("TRACK.FLAC");
        let ts = file_scan_timestamp(&on_disk).unwrap();

        let mut record = ScanRecord::new_current();
        record.records.insert(on_disk.clone(), ts);
        record.records.insert(dupe.clone(), ts);

        let (count, mut path_rx, mut relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), record);

        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
        let (old, new, _) = relocate_rx
            .blocking_recv()
            .expect("expected a merge relocation");
        assert_eq!(old, dupe);
        assert_eq!(new, on_disk);
        assert!(relocate_rx.blocking_recv().is_none());
    }

    // 以点结尾的目录需要 \\?\ 前缀——折叠路径只用于比较
    #[test]
    fn discover_relocates_paths_that_require_the_verbatim_prefix() {
        if !cfg!(windows) {
            return;
        }
        register_test_media_providers();
        let dir = TestDir::new("discover-verbatim-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }

        let deep = std::path::PathBuf::from(format!(r"\\?\{}\dot-dir.", dir.path().display()));
        std::fs::create_dir_all(&deep).unwrap();
        let file_verbatim = deep.join("track.flac");
        std::fs::write(&file_verbatim, b"").unwrap();
        let on_disk = Utf8PathBuf::from_path_buf(file_verbatim.canonicalize().unwrap()).unwrap();

        // 剥掉前缀的那种拼写必须解析不到，否则这个测试什么都证明不了
        let stripped = on_disk.as_str().strip_prefix(r"\\?\").unwrap();
        assert!(!matches!(Utf8Path::new(stripped).try_exists(), Ok(true)));

        // 记录里仍是改名之前的旧大小写
        let stale = on_disk.parent().unwrap().join("TRACK.FLAC");
        let ts = file_scan_timestamp(&on_disk).unwrap();

        let mut record = ScanRecord::new_current();
        record.records.insert(stale.clone(), ts);

        let (count, mut path_rx, mut relocate_rx) =
            run_discover(scan_settings(dir.utf8_path()), record);

        assert_eq!(count, 0);
        assert!(path_rx.blocking_recv().is_none());
        let (old, new, relocated_ts) = relocate_rx.blocking_recv().expect("expected a relocation");
        assert_eq!(old, stale);
        assert_eq!(new, on_disk);
        assert_eq!(relocated_ts, ts);
        // 搬迁后的路径保留 \\?\ 前缀，因此仍然可以用于 I/O
        assert!(new.as_str().starts_with(r"\\?\"));
        assert!(matches!(new.try_exists(), Ok(true)));
        assert!(relocate_rx.blocking_recv().is_none());

        // 对普通路径调用 remove_dir_all 够不到那个以点结尾的目录
        std::fs::remove_file(&file_verbatim).unwrap();
        std::fs::remove_dir(&deep).unwrap();
    }

    #[test]
    fn classify_skips_exact_hit_with_equal_timestamp() {
        let dir = TestDir::new("classify-test");
        let path = dir.utf8_join("track.flac");
        let ts = SystemTime::now();
        let records = FxHashMap::from_iter([(path.clone(), ts)]);
        let index = build_folded_index(&records);
        assert!(matches!(
            classify(&path, ts, &records, &index),
            (DiscoverAction::Skip, _)
        ));
    }

    #[test]
    fn classify_scans_exact_hit_with_different_timestamp() {
        let dir = TestDir::new("classify-test");
        let path = dir.utf8_join("track.flac");
        let ts = SystemTime::now();
        let records = FxHashMap::from_iter([(path.clone(), UNIX_EPOCH)]);
        let index = build_folded_index(&records);
        assert!(matches!(
            classify(&path, ts, &records, &index),
            (DiscoverAction::Scan(got), _) if got == ts
        ));
    }

    #[test]
    fn classify_scans_unrecorded_file() {
        let dir = TestDir::new("classify-test");
        let path = dir.utf8_join("track.flac");
        let ts = SystemTime::now();
        let records = FxHashMap::default();
        let index = FoldedIndex::default();
        assert!(matches!(
            classify(&path, ts, &records, &index),
            (DiscoverAction::Scan(got), _) if got == ts
        ));
    }

    #[test]
    fn classify_never_relocates_on_case_sensitive_volumes() {
        let dir = TestDir::new("classify-test");
        if is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        // 在大小写敏感的卷上，大小写不同就是新文件
        let ts = SystemTime::now();
        let stale = dir.utf8_join("TRACK.FLAC");
        let records = FxHashMap::from_iter([(stale, ts)]);
        let index = build_folded_index(&records);
        let path = dir.utf8_join("track.flac");
        assert!(matches!(
            classify(&path, ts, &records, &index),
            (DiscoverAction::Scan(_), _)
        ));
    }

    #[test]
    fn classify_relocates_rename_away_from_lowercase_key() {
        let dir = TestDir::new("classify-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let ts = SystemTime::now();
        // 小写键折叠到自身，但纯大小写改名之后仍必须匹配
        let stale = dir.utf8_join("track.flac");
        let path = dir.utf8_join("Track.flac");
        let records = FxHashMap::from_iter([(stale.clone(), ts)]);
        let index = build_folded_index(&records);

        let candidates = match classify(&path, ts, &records, &index) {
            (DiscoverAction::Relocate { candidates, .. }, _) => candidates,
            _ => panic!("expected relocation"),
        };
        assert_eq!(candidates, vec![(stale, ts)]);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_removes_missing_tracks() {
        let (dir, pool) = create_test_pool("cleanup-test").await;
        let path = dir.utf8_join("track.flac");
        std::fs::write(dir.join("track.flac"), b"").unwrap();
        insert_track_row(&pool, &path, track_metadata("Album", "Artist", "Track", 1)).await;

        std::fs::remove_file(dir.join("track.flac")).unwrap();

        let mut scan_record = record_of(&[&path]);
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.is_empty());
        assert!(!scan_record.records.contains_key(&path));
        assert_eq!(count_tracks_at(&pool, &path).await, 0);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_keeps_missing_tracks_under_excluded_roots() {
        let (dir, pool) = create_test_pool("cleanup-test").await;
        let path = write_track(&dir, "track.flac");
        insert_track_row(&pool, &path, track_metadata("Album", "Artist", "Track", 1)).await;

        std::fs::remove_file(dir.join("track.flac")).unwrap();

        let mut scan_record = record_of(&[&path]);
        let root = dir.utf8_path().canonicalize_utf8().unwrap();
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[root]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&path));
        assert_eq!(count_tracks_at(&pool, &path).await, 1);
    }

    #[tokio::test]
    async fn cleanup_removed_directories_removes_tracks_under_removed_dir() {
        let (dir, pool) = create_test_pool("cleanup-removed-test").await;

        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let path1 = insert_track_file(&pool, &dir_path, "track1.flac", 1).await;
        let path2 = insert_track_file(&pool, &dir_path, "track2.flac", 2).await;

        let mut scan_record = record_of(&[&path1, &path2]);
        scan_record.directories = vec![dir_path];

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.is_empty());
        assert!(!scan_record.records.contains_key(&path1));
        assert!(!scan_record.records.contains_key(&path2));
        assert_eq!(count_rows(&pool, "track").await, 0);
    }

    #[tokio::test]
    async fn cleanup_removed_directories_preserves_tracks_in_remaining_dirs() {
        let (dir, pool) = create_test_pool("cleanup-removed-test").await;

        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let path_a = insert_track_file_with_meta(
            &pool,
            &dir_path,
            "track_a.flac",
            track_metadata("Album A", "Artist", "Track A", 1),
        )
        .await;
        // 被移除的根必须是独立目录：若它落在保留根之下，保留根的递归扫描下一轮
        // 仍会发现它，删掉旧行只会让曲目以新 id 回来并丢掉歌单条目（父根覆盖
        // 旧子根的场景见 keeps_tracks_covered_by_a_parent_root）。
        let removed_dir = TestDir::new("cleanup-removed-other");
        let removed_path = removed_dir.utf8_path().canonicalize_utf8().unwrap();
        let path_b = insert_track_file_with_meta(
            &pool,
            &removed_path,
            "track_b.flac",
            track_metadata("Album B", "Artist", "Track B", 1),
        )
        .await;

        // 上次扫描时两个独立根都在配置里，如今只剩 dir_path
        let mut scan_record = record_of(&[&path_a, &path_b]);
        scan_record.directories = vec![dir_path.clone(), removed_path];

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[dir_path], &[]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&path_a));
        assert!(!scan_record.records.contains_key(&path_b));
        assert_eq!(count_tracks_at(&pool, &path_a).await, 1);
        assert_eq!(count_tracks_at(&pool, &path_b).await, 0);
    }

    /// 库根从子目录改成其父目录：旧子目录仍在当前根下，曲目不得被当成
    /// 「已移除目录」删行（playlist_item 会随外键级联清空）。
    #[tokio::test]
    async fn cleanup_removed_directories_keeps_tracks_covered_by_a_parent_root() {
        let (dir, pool) = create_test_pool("cleanup-removed-test").await;

        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let sub = dir_path.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let path = insert_track_file(&pool, &sub, "track.flac", 1).await;

        let mut scan_record = record_of(&[&path]);
        scan_record.directories = vec![sub];

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[dir_path], &[]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&path));
        assert_eq!(count_tracks_at(&pool, &path).await, 1);
    }

    /// 反向：库根从父目录改成其子目录。旧父根仍是「已移除」，父根下其它曲目
    /// 照删，但当前子根里的曲目不能被旧父根的前缀判定带走。
    #[tokio::test]
    async fn cleanup_removed_parent_root_keeps_tracks_in_current_child_root() {
        let (dir, pool) = create_test_pool("cleanup-removed-test").await;

        let parent = dir.utf8_path().canonicalize_utf8().unwrap();
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let inside = insert_track_file(&pool, &child, "inside.flac", 1).await;
        let outside = insert_track_file(&pool, &parent, "outside.flac", 2).await;

        let mut scan_record = record_of(&[&inside, &outside]);
        scan_record.directories = vec![parent];

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[child], &[]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&inside));
        assert!(!scan_record.records.contains_key(&outside));
        assert_eq!(count_tracks_at(&pool, &inside).await, 1);
        assert_eq!(count_tracks_at(&pool, &outside).await, 0);
    }

    #[tokio::test]
    async fn cleanup_removed_directories_returns_affected_playlist_ids() {
        let (dir, pool) = create_test_pool("cleanup-removed-test").await;
        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        let path = insert_track_file(&pool, &dir_path, "track.flac", 1).await;

        let playlist_id = add_track_to_playlist(&pool, &path, "Test Playlist").await;

        let mut scan_record = record_of(&[&path]);
        scan_record.directories = vec![dir_path];

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.contains(&playlist_id));
        assert_eq!(count_rows(&pool, "playlist_item").await, 0);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_removes_multiple_missing_files() {
        let (dir, pool) = create_test_pool("cleanup-test").await;
        let path1 = insert_track_file(&pool, &dir.utf8_path(), "track1.flac", 1).await;
        let path2 = insert_track_file(&pool, &dir.utf8_path(), "track2.flac", 1).await;
        let path3 = insert_track_file(&pool, &dir.utf8_path(), "track3.flac", 1).await;

        std::fs::remove_file(dir.join("track1.flac")).unwrap();
        std::fs::remove_file(dir.join("track2.flac")).unwrap();

        let mut scan_record = record_of(&[&path1, &path2, &path3]);
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.is_empty());
        assert!(!scan_record.records.contains_key(&path1));
        assert!(!scan_record.records.contains_key(&path2));
        assert!(scan_record.records.contains_key(&path3));
        assert_eq!(count_rows(&pool, "track").await, 1);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_removes_lyrics_for_deleted_tracks() {
        let (dir, pool) = create_test_pool("cleanup-test").await;
        let path = dir.utf8_join("track.flac");
        std::fs::write(dir.join("track.flac"), b"").unwrap();

        let mut meta = track_metadata("Album", "Artist", "Track", 1);
        meta.lyrics = Some("test lyrics".to_string());
        insert_track_row(&pool, &path, meta).await;

        assert_eq!(count_rows(&pool, "lyrics").await, 1);

        std::fs::remove_file(dir.join("track.flac")).unwrap();

        let mut scan_record = record_of(&[&path]);
        cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert_eq!(count_rows(&pool, "lyrics").await, 0);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_cascades_album_and_artist_deletion() {
        let (dir, pool) = create_test_pool("cleanup-test").await;
        let path = insert_track_file(&pool, &dir.utf8_path(), "track.flac", 1).await;

        assert_eq!(count_rows(&pool, "album").await, 1);
        assert_eq!(count_rows(&pool, "artist").await, 1);

        std::fs::remove_file(dir.join("track.flac")).unwrap();

        let mut scan_record = record_of(&[&path]);
        cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;

        assert_eq!(count_rows(&pool, "album").await, 0);
        assert_eq!(count_rows(&pool, "artist").await, 0);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_keeps_album_when_other_tracks_remain() {
        let (dir, pool) = create_test_pool("cleanup-test").await;
        let path1 = insert_track_file(&pool, &dir.utf8_path(), "track1.flac", 1).await;
        let path2 = insert_track_file(&pool, &dir.utf8_path(), "track2.flac", 2).await;

        assert_eq!(count_rows(&pool, "album").await, 1);
        assert_eq!(count_rows(&pool, "artist").await, 1);

        std::fs::remove_file(dir.join("track1.flac")).unwrap();

        let mut scan_record = record_of(&[&path1, &path2]);
        cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;

        assert_eq!(count_rows(&pool, "album").await, 1);
        assert_eq!(count_rows(&pool, "artist").await, 1);
    }

    #[tokio::test]
    async fn cleanup_with_exclusions_handles_moved_file() {
        let (dir, pool) = create_test_pool("cleanup-move-test").await;

        let old_path = dir.utf8_join("track.flac");
        std::fs::write(dir.join("track.flac"), b"").unwrap();
        let mut old_meta = track_metadata("Album", "Artist", "Old Track", 1);
        old_meta.lyrics = Some("old lyrics".to_string());
        insert_track_row(&pool, &old_path, old_meta).await;

        let playlist_id = add_track_to_playlist(&pool, &old_path, "Test Playlist").await;

        std::fs::remove_file(dir.join("track.flac")).unwrap();

        let new_path = dir.utf8_join("moved.flac");
        std::fs::write(dir.join("moved.flac"), b"").unwrap();
        let mut new_meta = track_metadata("Album", "Artist", "Moved Track", 1);
        new_meta.lyrics = Some("moved lyrics".to_string());
        insert_track_row(&pool, &new_path, new_meta).await;

        let mut scan_record = record_of(&[&old_path, &new_path]);
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;

        assert!(!scan_record.records.contains_key(&old_path));
        assert_eq!(count_tracks_at(&pool, &old_path).await, 0);
        assert!(scan_record.records.contains_key(&new_path));
        assert_eq!(count_tracks_at(&pool, &new_path).await, 1);
        assert_eq!(count_rows(&pool, "lyrics").await, 1);
        assert_eq!(count_rows(&pool, "playlist_item").await, 0);
        assert!(updated.contains(&playlist_id));
    }

    #[tokio::test]
    async fn cleanup_removes_db_row_with_no_record_entry() {
        let (dir, pool) = create_test_pool("cleanup-ghost-test").await;
        let path = insert_track_file(&pool, &dir.utf8_path(), "ghost.flac", 1).await;

        std::fs::remove_file(dir.join("ghost.flac")).unwrap();

        let mut scan_record = ScanRecord::new_current();
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.is_empty());
        assert_eq!(count_tracks_at(&pool, &path).await, 0);
    }

    #[tokio::test]
    async fn cleanup_removes_db_ghost_under_removed_directory() {
        let (dir, pool) = create_test_pool("cleanup-ghost-removed-test").await;

        let dir_path = dir.utf8_path().canonicalize_utf8().unwrap();
        // 被移除的根必须是独立目录：落在当前根之下时它仍被递归扫描覆盖，删行
        // 只会让曲目以新 id 回来并丢掉歌单条目（父根覆盖旧子根的场景见
        // cleanup_removed_directories_keeps_tracks_covered_by_a_parent_root）。
        let removed_dir = TestDir::new("cleanup-ghost-removed-other");
        let removed = removed_dir.utf8_path().canonicalize_utf8().unwrap();
        let path = insert_track_file(&pool, &removed, "track.flac", 1).await;

        // `removed` 上次扫描时还在配置里，现在不在了
        let mut scan_record = ScanRecord::new_current();
        scan_record.directories = vec![removed];

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[dir_path], &[]).await;
        assert!(updated.is_empty());
        assert_eq!(count_tracks_at(&pool, &path).await, 0);
    }

    #[tokio::test]
    async fn cleanup_prunes_record_only_entry_for_missing_file() {
        let (_dir, pool) = create_test_pool("cleanup-record-only-test").await;

        let mut scan_record = ScanRecord::new_current();
        let missing = Utf8PathBuf::from("/nonexistent/decode-fail.flac");
        scan_record.records.insert(missing.clone(), UNIX_EPOCH);

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.is_empty());
        assert!(!scan_record.records.contains_key(&missing));
    }

    #[tokio::test]
    async fn cleanup_keeps_db_row_under_excluded_root() {
        let (dir, pool) = create_test_pool("cleanup-excluded-test").await;
        let path = write_track(&dir, "track.flac");
        insert_track_row(&pool, &path, track_metadata("Album", "Artist", "Track", 1)).await;

        // 文件不见了，但其根目录已被排除
        std::fs::remove_file(dir.join("track.flac")).unwrap();

        let mut scan_record = ScanRecord::new_current();
        let root = dir.utf8_path().canonicalize_utf8().unwrap();
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[root]).await;
        assert!(updated.is_empty());
        assert_eq!(count_rows(&pool, "track").await, 1);
    }

    #[tokio::test]
    async fn cleanup_keeps_row_with_stale_casing_on_insensitive_volume() {
        let (dir, pool) = create_test_pool("cleanup-case-test").await;
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let canonical = write_track(&dir, "Track.flac");
        // 数据库与记录里都还是改名之前的旧大小写
        let stale = canonical.parent().unwrap().join("TRACK.FLAC");
        insert_track_row(&pool, &stale, track_metadata("Album", "Artist", "Track", 1)).await;

        let mut scan_record = ScanRecord::new_current();
        scan_record.records.insert(stale.clone(), UNIX_EPOCH);

        // 在大小写不敏感的卷上旧拼写仍能解析，因此清理会保留这一行
        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&stale));
        assert_eq!(count_rows(&pool, "track").await, 1);
    }

    #[tokio::test]
    async fn cleanup_keeps_row_with_current_casing() {
        let (dir, pool) = create_test_pool("cleanup-case-test").await;
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let canonical = write_track(&dir, "Track.flac");
        insert_track_row(
            &pool,
            &canonical,
            track_metadata("Album", "Artist", "Track", 1),
        )
        .await;

        let mut scan_record = ScanRecord::new_current();
        scan_record.records.insert(canonical.clone(), UNIX_EPOCH);

        cleanup_stale_tracks(&pool, &mut scan_record, &[], &[]).await;
        assert!(scan_record.records.contains_key(&canonical));
        assert_eq!(count_rows(&pool, "track").await, 1);
    }

    #[tokio::test]
    async fn cleanup_keeps_tracks_under_excluded_root_with_different_casing() {
        let (dir, pool) = create_test_pool("cleanup-excluded-case-test").await;
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let sub = dir.join("Removable");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("track.flac");
        std::fs::write(&file, b"").unwrap();
        let path = Utf8PathBuf::from_path_buf(file)
            .unwrap()
            .canonicalize_utf8()
            .unwrap();
        insert_track_row(&pool, &path, track_metadata("Album", "Artist", "Track", 1)).await;

        // 整个文件夹都没了，而其配置根的大小写与扫描记录里的不一致
        std::fs::remove_dir_all(dir.join("Removable")).unwrap();
        let root = dir.utf8_join("REMOVABLE");

        let mut scan_record = ScanRecord::new_current();
        scan_record.records.insert(path.clone(), UNIX_EPOCH);

        let updated = cleanup_stale_tracks(&pool, &mut scan_record, &[], &[root]).await;
        assert!(updated.is_empty());
        assert!(scan_record.records.contains_key(&path));
        assert_eq!(count_rows(&pool, "track").await, 1);
    }

    #[tokio::test]
    async fn cleanup_paginates_across_page_boundaries() {
        let (dir, pool) = create_test_pool("cleanup-paging-test").await;

        for i in 0..5 {
            let name = format!("track{i}.flac");
            let path = insert_track_file(&pool, &dir.utf8_path(), &name, i + 1).await;
            std::fs::remove_file(&path).unwrap();
        }

        let mut scan_record = ScanRecord::new_current();
        let updated = cleanup_stale_tracks_paged(&pool, &mut scan_record, &[], &[], 2).await;
        assert!(updated.is_empty());
        assert_eq!(count_rows(&pool, "track").await, 0);
    }
}
