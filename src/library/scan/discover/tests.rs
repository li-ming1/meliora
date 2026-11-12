use std::time::{SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap;

use super::*;
use crate::library::scan::{
    decode::folder_art_rank,
    fs_case::{fold_path, is_case_insensitive},
};
use crate::test_support::TestDir;

fn folded_index(records: &FxHashMap<Utf8PathBuf, SystemTime>) -> FoldedIndex {
    let mut index = FoldedIndex::default();
    for (path, timestamp) in records {
        index
            .entry(fold_path(path))
            .or_default()
            .push((path.clone(), *timestamp));
    }
    index
}

fn expect_relocate(
    result: (DiscoverAction, Vec<(Utf8PathBuf, SystemTime)>),
) -> (Vec<(Utf8PathBuf, SystemTime)>, SystemTime) {
    match result {
        (DiscoverAction::Relocate { candidates, ts }, _) => (candidates, ts),
        _ => panic!("expected relocation"),
    }
}

#[test]
fn sidecar_lyrics_path_falls_back_to_lrc_next_to_track() {
    let dir = TestDir::new("lrc-sidecar-preference-test");
    std::fs::write(dir.join("song.lrc"), b"[00:00.00]").unwrap();
    assert_eq!(
        sidecar_lyrics_path(&dir.utf8_join("song.flac")),
        Some(dir.utf8_join("song.lrc"))
    );
}

#[test]
fn sidecar_lyrics_path_prefers_krc_next_to_track() {
    let dir = TestDir::new("krc-sidecar-preference-test");
    std::fs::write(dir.join("song.krc"), b"[0,0]lyric").unwrap();
    std::fs::write(dir.join("song.lrc"), b"[00:00.00]").unwrap();
    assert_eq!(
        sidecar_lyrics_path(&dir.utf8_join("song.flac")),
        Some(dir.utf8_join("song.krc"))
    );
}

#[test]
fn directory_snapshot_marks_krc_sidecar_present() {
    let dir = TestDir::new("krc-sidecar-discovery-test");
    std::fs::write(dir.join("track.mp3"), b"").unwrap();
    std::fs::write(dir.join("track.krc"), b"[0,0]lyric").unwrap();

    let snapshot = read_scan_directory(&dir.utf8_path()).unwrap();
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.path == dir.utf8_join("track.mp3"))
        .expect("track entry");
    assert_eq!(entry.sidecar_lyrics, SidecarLyrics::Present);
}

#[test]
fn sidecar_lyrics_path_returns_none_without_stem() {
    let path = Utf8PathBuf::from("/");
    assert_eq!(sidecar_lyrics_path(&path), None);
}

#[test]
#[cfg(windows)]
fn slow_directory_policy_matches_verbatim_paths_on_windows() {
    // canonicalize 产出 \\?\ 前缀的 verbatim 形态，而 sysinfo 的挂载点是 C:\
    // 普通形态——裸 starts_with 恒不匹配会让所有读串到通道 0（2026-10-25
    // 修复的回归钉：正确命中各自通道、且与普通形态路径同判）。
    let mounts = vec![Utf8PathBuf::from("C:\\"), Utf8PathBuf::from("D:\\")];
    let mount_to_channel = [
        (Utf8PathBuf::from("C:\\"), 0),
        (Utf8PathBuf::from("D:\\"), 1),
    ]
    .into_iter()
    .collect();
    let policy = DirectoryReadPolicy::slow(mounts, mount_to_channel, 2);

    assert_eq!(policy.channel_for(Utf8Path::new(r"\\?\C:\music")), 0);
    assert_eq!(policy.channel_for(Utf8Path::new(r"\\?\D:\music")), 1);
    assert_eq!(policy.channel_for(Utf8Path::new(r"C:\music")), 0);
}

#[test]
fn directory_snapshot_selects_the_best_supported_art_name() {
    let dir = TestDir::new("folder-art-discovery-test");
    std::fs::write(dir.join("front.jpeg"), b"front").unwrap();
    std::fs::write(dir.join("folder.jpg"), b"folder").unwrap();
    std::fs::write(dir.join("cover.PNG"), b"cover").unwrap();
    std::fs::write(dir.join("cover.gif"), b"unsupported").unwrap();

    let snapshot = read_scan_directory(&dir.utf8_path()).unwrap();
    let candidate = snapshot.folder_art.unwrap();

    assert_eq!(candidate.path, dir.utf8_join("cover.PNG"));
    assert_eq!(candidate.rank, folder_art_rank("cover").unwrap());
}

#[test]
fn slow_directory_policy_uses_one_permit_per_disk() {
    let mounts = vec![Utf8PathBuf::from("/mnt/one"), Utf8PathBuf::from("/mnt/two")];
    let mount_to_channel = [
        (Utf8PathBuf::from("/mnt/one"), 0),
        (Utf8PathBuf::from("/mnt/two"), 1),
    ]
    .into_iter()
    .collect();
    let policy = DirectoryReadPolicy::slow(mounts, mount_to_channel, 2);

    assert_eq!(policy.channel_for(Utf8Path::new("/mnt/one/music")), 0);
    assert_eq!(policy.channel_for(Utf8Path::new("/mnt/two/music")), 1);
    assert_eq!(policy.semaphores.len(), 2);
    assert!(
        policy
            .semaphores
            .iter()
            .all(|semaphore| semaphore.available_permits() == 1)
    );
}

#[test]
fn missing_paths_groups_existing_and_missing_siblings() {
    let dir = TestDir::new("missing-paths-test");
    let present = dir.utf8_join("present.flac");
    let missing_a = dir.utf8_join("missing-a.flac");
    let missing_b = dir.utf8_join("missing-b.flac");
    std::fs::write(&present, b"").unwrap();

    let missing = missing_paths([present.clone(), missing_a.clone(), missing_b.clone()]);

    assert!(!missing.contains(&present));
    assert!(missing.contains(&missing_a));
    assert!(missing.contains(&missing_b));
}

#[test]
fn missing_paths_confirms_all_children_when_the_parent_is_gone() {
    let dir = TestDir::new("missing-paths-test");
    let missing_parent = dir.utf8_join("gone");
    let paths = [
        missing_parent.join("one.flac"),
        missing_parent.join("two.flac"),
    ];

    assert_eq!(missing_paths(paths.clone()), paths.into_iter().collect());
}

#[test]
fn classify_scans_case_variant_when_index_is_empty() {
    let dir = TestDir::new("classify-test");
    let stale = dir.utf8_join("TRACK.FLAC");
    let path = dir.utf8_join("track.flac");
    let timestamp = SystemTime::now();
    let records = FxHashMap::from_iter([(stale, timestamp)]);
    let index = FoldedIndex::default();

    assert!(matches!(
        classify(&path, timestamp, &records, &index),
        (DiscoverAction::Scan(_), _)
    ));
}

#[test]
fn classify_returns_folded_hit_as_relocate_candidate() {
    let dir = TestDir::new("classify-test");
    if !is_case_insensitive(&dir.utf8_path()) {
        return;
    }
    let timestamp = SystemTime::now();
    let stale = dir.utf8_join("TRACK.FLAC");
    let path = dir.utf8_join("track.flac");
    let records = FxHashMap::from_iter([(stale.clone(), timestamp)]);
    let index = folded_index(&records);

    let (candidates, got_timestamp) = expect_relocate(classify(&path, timestamp, &records, &index));
    assert_eq!(candidates, vec![(stale, timestamp)]);
    assert_eq!(got_timestamp, timestamp);
}

#[test]
fn classify_relocate_candidate_carries_the_recorded_timestamp() {
    let dir = TestDir::new("classify-test");
    if !is_case_insensitive(&dir.utf8_path()) {
        return;
    }
    let timestamp = SystemTime::now();
    let stale = dir.utf8_join("TRACK.FLAC");
    let path = dir.utf8_join("track.flac");
    let records = FxHashMap::from_iter([(stale.clone(), UNIX_EPOCH)]);
    let index = folded_index(&records);

    let (candidates, got_timestamp) = expect_relocate(classify(&path, timestamp, &records, &index));
    assert_eq!(candidates, vec![(stale, UNIX_EPOCH)]);
    assert_eq!(got_timestamp, timestamp);
}

#[test]
fn classify_prefers_timestamp_match_among_folded_candidates() {
    let dir = TestDir::new("classify-test");
    if !is_case_insensitive(&dir.utf8_path()) {
        return;
    }
    let timestamp = SystemTime::now();
    let upper = dir.utf8_join("TRACK.FLAC");
    let mixed = dir.utf8_join("Track.flac");
    let records = FxHashMap::from_iter([(upper.clone(), UNIX_EPOCH), (mixed.clone(), timestamp)]);
    let index = folded_index(&records);
    let path = dir.utf8_join("track.flac");

    let (candidates, _) = expect_relocate(classify(&path, timestamp, &records, &index));
    assert_eq!(candidates, vec![(mixed, timestamp), (upper, UNIX_EPOCH)]);
}

#[test]
fn classify_reports_stale_spellings_on_exact_hit() {
    let dir = TestDir::new("classify-test");
    if !is_case_insensitive(&dir.utf8_path()) {
        return;
    }
    let timestamp = SystemTime::now();
    let path = dir.utf8_join("track.flac");
    let duplicate = dir.utf8_join("TRACK.FLAC");
    let records = FxHashMap::from_iter([(path.clone(), timestamp), (duplicate.clone(), timestamp)]);
    let index = folded_index(&records);

    match classify(&path, timestamp, &records, &index) {
        (DiscoverAction::Skip, stale) => assert_eq!(stale, vec![(duplicate, timestamp)]),
        _ => panic!("expected skip with a stale spelling"),
    }
}

#[test]
fn classify_from_folded_matches_classify_across_states() {
    let dir = TestDir::new("classify-from-folded-test");
    if !is_case_insensitive(&dir.utf8_path()) {
        return;
    }

    let ts = SystemTime::now();
    let exact = dir.utf8_join("exact.flac");
    let stale_upper = dir.utf8_join("DUP.FLAC");
    let stale_lower = dir.utf8_join("dup.flac");
    let changed = dir.utf8_join("changed.flac");
    let relocate_old = dir.utf8_join("RENAME.FLAC");
    let relocate_new = dir.utf8_join("rename.flac");
    let fresh = dir.utf8_join("fresh.flac");

    let records = FxHashMap::from_iter([
        (exact.clone(), ts),
        (stale_lower.clone(), ts),
        (stale_upper.clone(), ts),
        (changed.clone(), UNIX_EPOCH),
        (relocate_old.clone(), ts),
    ]);
    let index = folded_index(&records);

    // 精确命中且无碰撞：Skip + 空 stale
    assert!(matches!(
        classify_from_folded(&exact, ts, &index),
        (DiscoverAction::Skip, stale) if stale.is_empty()
    ));

    // 精确命中且同折叠键下存在旧拼写：Skip + 返回旧拼写
    match classify_from_folded(&stale_lower, ts, &index) {
        (DiscoverAction::Skip, stale) => assert_eq!(stale, vec![(stale_upper, ts)]),
        _ => panic!("expected skip with stale spelling"),
    }

    // 时间戳变化：Scan(ts)
    assert!(matches!(
        classify_from_folded(&changed, ts, &index),
        (DiscoverAction::Scan(got), stale) if got == ts && stale.is_empty()
    ));

    // 仅大小写变体命中：Relocate
    let (candidates, got_ts) = expect_relocate(classify_from_folded(&relocate_new, ts, &index));
    assert_eq!(candidates, vec![(relocate_old, ts)]);
    assert_eq!(got_ts, ts);

    // 完全未记录：Scan(ts)
    assert!(matches!(
        classify_from_folded(&fresh, ts, &index),
        (DiscoverAction::Scan(got), stale) if got == ts && stale.is_empty()
    ));

    // 时间戳变化且同桶内有旧拼写：必须返回 (Scan(ts), stale)，与 classify 完全一致
    let mut mixed_records = records.clone();
    let stale_changed_upper = dir.utf8_join("MIX.FLAC");
    let stale_changed_lower = dir.utf8_join("mix.flac");
    mixed_records.insert(stale_changed_lower.clone(), UNIX_EPOCH);
    mixed_records.insert(stale_changed_upper.clone(), UNIX_EPOCH);
    let mixed_index = folded_index(&mixed_records);
    match (
        classify(&stale_changed_lower, ts, &mixed_records, &mixed_index),
        classify_from_folded(&stale_changed_lower, ts, &mixed_index),
    ) {
        ((DiscoverAction::Scan(a_ts), a_stale), (DiscoverAction::Scan(b_ts), b_stale)) => {
            assert_eq!(a_ts, ts);
            assert_eq!(b_ts, ts);
            assert_eq!(a_stale, vec![(stale_changed_upper.clone(), UNIX_EPOCH)]);
            assert_eq!(b_stale, a_stale);
        }
        _ => panic!("expected matching Scan with stale spellings"),
    }

    // 多个搬迁候选中，优先把时间戳一致的排在最前
    let multi_upper = dir.utf8_join("MULTI.FLAC");
    let multi_mixed = dir.utf8_join("Multi.flac");
    let multi_target = dir.utf8_join("multi.flac");
    let multi_records =
        FxHashMap::from_iter([(multi_upper.clone(), UNIX_EPOCH), (multi_mixed.clone(), ts)]);
    let multi_index = folded_index(&multi_records);
    let (got_candidates, _) =
        expect_relocate(classify_from_folded(&multi_target, ts, &multi_index));
    assert_eq!(
        got_candidates,
        vec![(multi_mixed, ts), (multi_upper, UNIX_EPOCH)]
    );
}

#[test]
#[ignore = "microbench: measures single-table classify_from_folded vs cloned records + classify"]
fn report_classify_from_folded_vs_legacy_two_table_cost() {
    let dir = TestDir::new("discover-bench-dir");
    let _ = is_case_insensitive(&dir.utf8_path());
    let ts = SystemTime::now();
    let count = 10_000usize;
    let paths: Vec<Utf8PathBuf> = (0..count)
        .map(|i| dir.utf8_join(&format!("Track-{i:05}.flac")))
        .collect();
    let records: FxHashMap<Utf8PathBuf, SystemTime> =
        paths.iter().map(|p| (p.clone(), ts)).collect();

    let ((legacy_us, legacy_hits), legacy_allocs) =
        crate::test_support::alloc_guard::count_allocations(|| {
            let t_legacy = std::time::Instant::now();
            let legacy_snapshot = records.clone();
            let mut legacy_index: FxHashMap<Utf8PathBuf, Vec<(Utf8PathBuf, SystemTime)>> =
                FxHashMap::default();
            for (k, v) in &records {
                legacy_index
                    .entry(fold_path(k))
                    .or_default()
                    .push((k.clone(), *v));
            }
            let mut hits = 0usize;
            for p in &paths {
                let exact = legacy_snapshot.get(p).copied();
                let stale_lookup = legacy_index.get(&fold_path(p));
                if exact == Some(ts) && stale_lookup.is_some_and(|v| v.len() == 1) {
                    hits += 1;
                }
            }
            (t_legacy.elapsed().as_micros(), hits)
        });

    let ((new_us, new_hits), new_allocs) =
        crate::test_support::alloc_guard::count_allocations(|| {
            let t_new = std::time::Instant::now();
            let mut new_index =
                FoldedIndex::with_capacity_and_hasher(records.len(), Default::default());
            for (k, v) in &records {
                new_index
                    .entry(fold_path(k))
                    .or_default()
                    .push((k.clone(), *v));
            }
            let mut hits = 0usize;
            for p in &paths {
                if matches!(
                    classify_from_folded(p, ts, &new_index),
                    (DiscoverAction::Skip, s) if s.is_empty()
                ) {
                    hits += 1;
                }
            }
            (t_new.elapsed().as_micros(), hits)
        });

    assert_eq!(legacy_hits, count);
    assert_eq!(new_hits, count);
    assert!(new_allocs < legacy_allocs);
    println!(
        "discover 10k paths (setup+classify): legacy_two_table={legacy_us}us ({legacy_allocs} allocs) vs single_folded_smallvec={new_us}us ({new_allocs} allocs)"
    );
}
