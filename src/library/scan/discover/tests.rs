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
