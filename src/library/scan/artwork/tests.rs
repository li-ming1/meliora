use std::sync::Arc;

use rustc_hash::FxHashSet;
use tokio::task::spawn_blocking;
use xxhash_rust::xxh3::xxh3_64;

use super::*;
use crate::library::scan::{
    artist_match::ArtistMatcher,
    database::{
        WriteCaches, flush_album_artists, flush_track_artists, sweep_orphan_artists,
        update_metadata,
    },
    decode::{ArtSource, FileArt, ProcessedArt, RawArt, ScannedArt, process_owned_album_art},
    discover::{FolderArtCandidate, FolderArtObservationMap, read_scan_directory},
};
use crate::media::metadata::Metadata;
use crate::test_support::{TestDir, count_rows, create_test_pool, seed_library, track_metadata};

/// 合法的小 PNG。颜色不同则字节不同。
fn png(r: u8, g: u8, b: u8) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(16, 16, image::Rgb([r, g, b]));
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn red() -> Vec<u8> {
    png(255, 0, 0)
}

fn green() -> Vec<u8> {
    png(0, 255, 0)
}

fn blue() -> Vec<u8> {
    png(0, 0, 255)
}

fn scanned(bytes: Vec<u8>, source: ArtSource) -> ScannedArt {
    let hash = xxh3_64(&bytes);
    let (image, thumb) = process_owned_album_art(RawArt::Owned(bytes.into_boxed_slice())).unwrap();
    ScannedArt {
        hash,
        raw: None,
        processed: Some(Arc::new(ProcessedArt { image, thumb })),
        source,
    }
}

fn raw_embedded(bytes: Vec<u8>) -> FileArt {
    FileArt {
        embedded: Some(ScannedArt {
            hash: xxh3_64(&bytes),
            raw: Some(RawArt::Owned(bytes.into_boxed_slice())),
            processed: None,
            source: ArtSource::Embedded,
        }),
        folder: None,
        representative: false,
    }
}

fn embedded_art(bytes: Vec<u8>) -> FileArt {
    FileArt {
        embedded: Some(scanned(bytes, ArtSource::Embedded)),
        folder: None,
        representative: false,
    }
}

fn with_folder_art(embedded: Vec<u8>, cover: Vec<u8>) -> FileArt {
    let mut art = embedded_art(embedded);
    art.folder = Some(scanned(cover, ArtSource::Folder(1)));
    art
}

#[test]
fn folder_candidates_keep_the_best_rank_then_lowest_hash() {
    let mut candidates = FolderArtCandidates::default();

    consider_folder_art(&mut candidates, 1, 30, 2);
    consider_folder_art(&mut candidates, 1, 20, 3);
    consider_folder_art(&mut candidates, 1, 40, 1);
    consider_folder_art(&mut candidates, 1, 10, 1);

    assert_eq!(candidates.get(&1), Some(&(10, 1)));
}

#[tokio::test]
async fn artwork_processor_reuses_each_hash_until_it_is_resolved() {
    let processor = ArtworkProcessor::new([]);
    let bytes = red();
    let mut first = raw_embedded(bytes.clone());
    let mut duplicate = raw_embedded(bytes);

    processor.process_file_art(&mut first).await;
    processor.process_file_art(&mut duplicate).await;

    let first_processed = first.embedded.as_ref().unwrap().processed.as_ref().unwrap();
    let duplicate_processed = duplicate
        .embedded
        .as_ref()
        .unwrap()
        .processed
        .as_ref()
        .unwrap();
    assert!(Arc::ptr_eq(first_processed, duplicate_processed));

    let mut ids = ArtIdCache::default();
    ids.insert(first.embedded.as_ref().unwrap().hash, Some(1));
    processor.mark_resolved(&first, &ids);

    let mut after_resolve = raw_embedded(red());
    processor.process_file_art(&mut after_resolve).await;
    let after_resolve = after_resolve.embedded.unwrap();
    assert!(after_resolve.raw.is_none());
    assert!(after_resolve.processed.is_none());
}

#[tokio::test]
async fn artwork_processor_skips_preexisting_hashes() {
    let bytes = red();
    let hash = xxh3_64(&bytes);
    let processor = ArtworkProcessor::new([hash]);
    let mut existing = raw_embedded(bytes);

    processor.process_file_art(&mut existing).await;

    let existing = existing.embedded.unwrap();
    assert!(existing.raw.is_none());
    assert!(existing.processed.is_none());
}

/// 把夹具专辑（"Album"/"Artist"）的第 `track_no` 首曲目写成 `t{track_no}.flac`。
async fn write_album_track(dir: &TestDir, pool: &SqlitePool, track_no: u64, art: &FileArt) {
    write_track_cached(
        dir,
        pool,
        &track_metadata("Album", "Artist", &format!("Track {track_no}"), track_no),
        &format!("t{track_no}.flac"),
        art,
        false,
        &mut WriteCaches::default(),
    )
    .await;
}

async fn write_album(dir: &TestDir, pool: &SqlitePool, arts: &[&FileArt]) {
    let mut caches = WriteCaches::default();
    write_album_cached(dir, pool, arts, false, &mut caches).await;
}

async fn write_album_cached(
    dir: &TestDir,
    pool: &SqlitePool,
    arts: &[&FileArt],
    is_force: bool,
    caches: &mut WriteCaches,
) {
    for (i, art) in arts.iter().enumerate() {
        write_track_cached(
            dir,
            pool,
            &track_metadata("Album", "Artist", &format!("Track {}", i + 1), i as u64 + 1),
            &format!("t{}.flac", i + 1),
            art,
            is_force,
            caches,
        )
        .await;
    }
}

async fn finalize(pool: &SqlitePool, is_force: bool) {
    let mut candidates = FolderArtCandidates::default();
    finalize_with(pool, is_force, &FxHashSet::default(), &mut candidates).await;
}

async fn finalize_with(
    pool: &SqlitePool,
    is_force: bool,
    examined: &FxHashSet<i64>,
    candidates: &mut FolderArtCandidates,
) {
    let touched: FxHashSet<i64> = sqlx::query_scalar("SELECT id FROM album")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    finalize_scan_art(pool, is_force, &touched, examined, candidates, true)
        .await
        .unwrap();
}

/// 对经 `caches` 写入的那些专辑跑一次扫描结束时的挑选，但不把封面落盘。
async fn finalize_from_caches(pool: &SqlitePool, is_force: bool, caches: &mut WriteCaches) {
    let touched: FxHashSet<i64> = caches.albums.values().copied().collect();
    finalize_scan_art(
        pool,
        is_force,
        &touched,
        &caches.examined_albums,
        &mut caches.folder_art_candidates,
        false,
    )
    .await
    .unwrap();
}

/// 跑一趟文件夹封面 examine，返回被暂存的内容。
async fn examine_observations(
    pool: &SqlitePool,
    observations: &FolderArtObservationMap,
) -> (FxHashSet<i64>, FolderArtCandidates, ArtIdCache) {
    let processor = ArtworkProcessor::new([]);
    let loader = FolderArtLoader::new(processor.concurrency());
    let mut examined = FxHashSet::default();
    let mut candidates = FolderArtCandidates::default();
    let mut art_ids = ArtIdCache::default();
    examine_folder_art(
        pool,
        observations,
        &loader,
        &processor,
        &mut examined,
        &mut candidates,
        &mut art_ids,
    )
    .await
    .unwrap();
    (examined, candidates, art_ids)
}

async fn album_row(pool: &SqlitePool) -> (i64, Option<i64>, i64) {
    sqlx::query_as("SELECT id, artwork_id, artwork_source FROM album")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn assert_album_art(pool: &SqlitePool, id: Option<i64>, source: i64) {
    let (_, album_art, src) = album_row(pool).await;
    assert_eq!(album_art, id);
    assert_eq!(src, source);
}

async fn track_art(pool: &SqlitePool) -> Vec<(Option<i64>, Option<i64>)> {
    sqlx::query_as("SELECT track_number, artwork_id FROM track ORDER BY track_number")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn artwork_id_for_hash(pool: &SqlitePool, bytes: &[u8]) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM artwork WHERE hash = $1")
        .bind(xxh3_64(bytes) as i64)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn update_metadata_processes_embedded_art_immediately() {
    let (dir, pool) = create_test_pool("artwork-stage-test").await;
    let a = red();

    write_album_track(&dir, &pool, 1, &embedded_art(a.clone())).await;

    // finalize 之前 track 就已经指向该封面行
    let (art_hash, track_art): (i64, Option<i64>) =
        sqlx::query_as("SELECT art_hash, artwork_id FROM track")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(art_hash, xxh3_64(&a) as i64);
    assert_eq!(track_art, artwork_id_for_hash(&pool, &a).await);
    assert!(track_art.is_some());
}

#[tokio::test]
async fn album_gets_provisional_art_at_write_time() {
    let (dir, pool) = create_test_pool("artwork-provisional-test").await;
    let a = red();
    let b = green();

    write_album_track(&dir, &pool, 1, &embedded_art(a.clone())).await;

    // finalize 之前专辑就已指向第一首曲目的封面
    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();
    assert_album_art(&pool, Some(a_id), 0).await;

    // 扫描途中后面曲目的封面不得顶掉已经选定的那张
    write_album_track(&dir, &pool, 2, &embedded_art(b.clone())).await;
    assert_album_art(&pool, Some(a_id), 0).await;

    write_album_track(&dir, &pool, 3, &embedded_art(b.clone())).await;

    finalize(&pool, false).await;
    let b_id = artwork_id_for_hash(&pool, &b).await.unwrap();
    assert_album_art(&pool, Some(b_id), 0).await;
}

#[tokio::test]
async fn folder_art_is_the_provisional_pick() {
    let (dir, pool) = create_test_pool("artwork-provisional-folder-test").await;
    let a = red();
    let cover = blue();

    let representative = with_folder_art(a, cover.clone());
    write_album_track(&dir, &pool, 1, &representative).await;

    let cover_id = artwork_id_for_hash(&pool, &cover).await.unwrap();
    assert_album_art(&pool, Some(cover_id), 1).await;
}

#[tokio::test]
async fn folder_art_wins_and_differing_tracks_get_own_rows() {
    let (dir, pool) = create_test_pool("artwork-folder-test").await;
    let a = red();
    let b = green();
    let cover = blue();

    let representative = with_folder_art(a.clone(), cover.clone());
    write_album_track(&dir, &pool, 1, &representative).await;
    write_album_track(&dir, &pool, 2, &embedded_art(a.clone())).await;
    write_album_track(&dir, &pool, 3, &embedded_art(b.clone())).await;

    finalize(&pool, false).await;

    let cover_id = artwork_id_for_hash(&pool, &cover).await.unwrap();
    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();
    let b_id = artwork_id_for_hash(&pool, &b).await.unwrap();

    assert_album_art(&pool, Some(cover_id), 1).await;

    // 内嵌封面与文件夹胜者不同的曲目保留自己的行
    assert_eq!(
        track_art(&pool).await,
        vec![
            (Some(1), Some(a_id)),
            (Some(2), Some(a_id)),
            (Some(3), Some(b_id)),
        ]
    );
    assert_eq!(count_rows(&pool, "artwork").await, 3);
}

#[tokio::test]
async fn majority_embedded_wins_and_conforming_tracks_share_the_album_row() {
    let (dir, pool) = create_test_pool("artwork-majority-test").await;
    let a = red();
    let b = green();

    let a_art = embedded_art(a.clone());
    let b_art = embedded_art(b.clone());
    write_album(&dir, &pool, &[&a_art, &a_art, &b_art]).await;

    finalize(&pool, false).await;

    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();
    let b_id = artwork_id_for_hash(&pool, &b).await.unwrap();

    assert_album_art(&pool, Some(a_id), 0).await;
    assert_eq!(
        track_art(&pool).await,
        vec![
            (Some(1), Some(a_id)),
            (Some(2), Some(a_id)),
            (Some(3), Some(b_id)),
        ]
    );
    assert_eq!(count_rows(&pool, "artwork").await, 2);
}

#[tokio::test]
async fn rescan_with_no_changes_keeps_artwork_rows_stable() {
    let (dir, pool) = create_test_pool("artwork-stable-test").await;
    let a = red();
    let b = green();

    let a_art = embedded_art(a.clone());
    let b_art = embedded_art(b.clone());
    let album = [&a_art, &a_art, &b_art];
    write_album(&dir, &pool, &album).await;
    finalize(&pool, false).await;

    let before = (album_row(&pool).await, track_art(&pool).await);

    write_album(&dir, &pool, &album).await;
    finalize(&pool, false).await;

    assert_eq!(before, (album_row(&pool).await, track_art(&pool).await));
    assert_eq!(count_rows(&pool, "artwork").await, 2);
}

#[tokio::test]
async fn partial_rescan_keeps_folder_incumbent_until_force_scan() {
    let (dir, pool) = create_test_pool("artwork-incumbent-test").await;
    let a = red();
    let b = green();
    let cover = blue();

    let representative = with_folder_art(a.clone(), cover.clone());
    write_album_track(&dir, &pool, 1, &representative).await;
    write_album_track(&dir, &pool, 2, &embedded_art(a.clone())).await;
    finalize(&pool, false).await;

    let cover_id = artwork_id_for_hash(&pool, &cover).await.unwrap();
    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();

    // 对非首曲目的定向重扫不会暂存文件夹封面——已有的文件夹封面保持不变
    write_album_track(&dir, &pool, 3, &embedded_art(b.clone())).await;
    finalize(&pool, false).await;
    assert_album_art(&pool, Some(cover_id), 1).await;

    let b_id = artwork_id_for_hash(&pool, &b).await.unwrap();
    assert_eq!(
        track_art(&pool).await,
        vec![
            (Some(1), Some(a_id)),
            (Some(2), Some(a_id)),
            (Some(3), Some(b_id)),
        ]
    );

    // 强制重扫且没有文件夹封面——内嵌多数派顶替已有的文件夹封面
    write_album_track(&dir, &pool, 3, &embedded_art(b.clone())).await;
    finalize(&pool, true).await;
    assert_album_art(&pool, Some(a_id), 0).await;
    assert_eq!(
        track_art(&pool).await,
        vec![
            (Some(1), Some(a_id)),
            (Some(2), Some(a_id)),
            (Some(3), Some(b_id)),
        ]
    );
}

#[tokio::test]
async fn rescan_without_art_clears_artwork() {
    for (is_force, prefix) in [
        (false, "artwork-clear-test"),
        (true, "artwork-force-clear-test"),
    ] {
        let (dir, pool) = create_test_pool(prefix).await;
        let a = red();
        let mut caches = WriteCaches::default();
        let a_art = embedded_art(a);
        write_album_cached(&dir, &pool, &[&a_art, &a_art], false, &mut caches).await;
        finalize_from_caches(&pool, false, &mut caches).await;
        assert_eq!(count_rows(&pool, "artwork").await, 1);

        // 文件失去了内嵌封面并被重扫——已无文件夹候选
        let no_art = FileArt::default();
        write_album_cached(&dir, &pool, &[&no_art, &no_art], is_force, &mut caches).await;
        finalize_from_caches(&pool, is_force, &mut caches).await;

        assert_album_art(&pool, None, 0).await;
        assert_eq!(
            track_art(&pool).await,
            vec![(Some(1), None), (Some(2), None)]
        );
        assert_eq!(count_rows(&pool, "artwork").await, 0);
    }
}

#[tokio::test]
async fn orphan_artwork_is_swept_after_track_deletion() {
    let (dir, pool) = create_test_pool("artwork-orphan-test").await;
    let a = red();
    let b = green();

    write_album_track(&dir, &pool, 1, &embedded_art(a)).await;
    write_album_track(&dir, &pool, 2, &embedded_art(b)).await;
    finalize(&pool, false).await;
    assert_eq!(count_rows(&pool, "artwork").await, 2);

    sqlx::query("DELETE FROM track")
        .execute(&pool)
        .await
        .unwrap();
    // 专辑已被删除——未使用封面清理会把这张封面一并删掉
    finalize(&pool, false).await;
    assert_eq!(count_rows(&pool, "artwork").await, 0);
}

async fn write_track_cached(
    dir: &TestDir,
    pool: &SqlitePool,
    meta: &Metadata,
    name: &str,
    art: &FileArt,
    is_force: bool,
    caches: &mut WriteCaches,
) {
    let mut conn = pool.acquire().await.unwrap();
    update_metadata(
        &mut conn,
        meta,
        &dir.utf8_join(name),
        100,
        art,
        is_force,
        caches,
    )
    .await
    .unwrap();
    flush_album_artists(
        &mut conn,
        &mut ArtistMatcher::new(),
        &mut caches.pending_albums,
    )
    .await
    .unwrap();
    flush_track_artists(
        &mut conn,
        &mut ArtistMatcher::new(),
        &mut caches.pending_tracks,
    )
    .await
    .unwrap();
}

async fn examine_and_finalize(dir: &TestDir, pool: &SqlitePool) {
    // read_scan_directory 会经由 media provider 表解码曲目元数据，
    // 而那张表只在 provider 注册之后才存在。
    crate::test_support::register_test_media_providers();
    let directory = dir.utf8_path();
    let read_directory = directory.clone();
    let snapshot = spawn_blocking(move || read_scan_directory(&read_directory))
        .await
        .unwrap()
        .unwrap();
    let observations = [(directory, snapshot.folder_art)].into_iter().collect();
    let (examined, mut candidates, _) = examine_observations(pool, &observations).await;
    finalize_with(pool, false, &examined, &mut candidates).await;
}

#[tokio::test]
async fn examine_folder_art_does_not_store_art_without_an_album_claim() {
    let (dir, pool) = create_test_pool("artwork-unclaimed-folder-test").await;
    let cover_path = dir.utf8_join("cover.png");
    std::fs::write(&cover_path, blue()).unwrap();
    let observations = [(
        dir.utf8_path(),
        Some(FolderArtCandidate {
            path: cover_path,
            rank: 0,
        }),
    )]
    .into_iter()
    .collect();
    let (examined, candidates, art_ids) = examine_observations(&pool, &observations).await;

    assert_eq!(count_rows(&pool, "artwork").await, 0);
    assert!(examined.is_empty());
    assert!(candidates.is_empty());
    assert!(art_ids.is_empty());
}

#[tokio::test]
async fn examine_folder_art_picks_up_cover_added_without_track_changes() {
    let (dir, pool) = create_test_pool("artwork-examine-add-test").await;
    let a = red();
    let cover = blue();

    let a_art = embedded_art(a.clone());
    write_album(&dir, &pool, &[&a_art, &a_art]).await;
    finalize(&pool, false).await;
    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();
    assert_album_art(&pool, Some(a_id), 0).await;

    // 新封面被暂存，且没有重读音频
    std::fs::write(dir.join("cover.jpg"), &cover).unwrap();
    examine_and_finalize(&dir, &pool).await;

    let cover_id = artwork_id_for_hash(&pool, &cover).await.unwrap();
    assert_album_art(&pool, Some(cover_id), 1).await;
    // 各曲目的内嵌封面与文件夹胜者不同——它们保留自己的行
    assert_eq!(
        track_art(&pool).await,
        vec![(Some(1), Some(a_id)), (Some(2), Some(a_id))]
    );
}

#[tokio::test]
async fn examine_folder_art_dethrones_incumbent_after_cover_deleted() {
    let (dir, pool) = create_test_pool("artwork-examine-delete-test").await;
    let a = red();
    let cover = blue();

    std::fs::write(dir.join("cover.jpg"), &cover).unwrap();
    let representative = with_folder_art(a.clone(), cover.clone());
    write_album_track(&dir, &pool, 1, &representative).await;
    write_album_track(&dir, &pool, 2, &embedded_art(a.clone())).await;
    finalize(&pool, false).await;

    let cover_id = artwork_id_for_hash(&pool, &cover).await.unwrap();
    assert_album_art(&pool, Some(cover_id), 1).await;

    // 封面被删之后，内嵌多数派无需强制扫描就顶替文件夹封面
    std::fs::remove_file(dir.join("cover.jpg")).unwrap();
    examine_and_finalize(&dir, &pool).await;

    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();
    assert_album_art(&pool, Some(a_id), 0).await;
    assert_eq!(
        track_art(&pool).await,
        vec![(Some(1), Some(a_id)), (Some(2), Some(a_id))]
    );
}

#[tokio::test]
async fn examine_folder_art_with_unchanged_cover_keeps_incumbent_stable() {
    let (dir, pool) = create_test_pool("artwork-examine-stable-test").await;
    let a = red();
    let cover = blue();

    std::fs::write(dir.join("cover.jpg"), &cover).unwrap();
    let representative = with_folder_art(a.clone(), cover.clone());
    write_album_track(&dir, &pool, 1, &representative).await;
    write_album_track(&dir, &pool, 2, &embedded_art(a.clone())).await;
    finalize(&pool, false).await;

    let before = (album_row(&pool).await, track_art(&pool).await);

    // 同一张封面重新暂存——胜者不变，没有任何行被移动
    examine_and_finalize(&dir, &pool).await;

    assert_eq!(before, (album_row(&pool).await, track_art(&pool).await));
    assert_eq!(count_rows(&pool, "artwork").await, 2);
}

/// 一次扫描在其目录已带内嵌封面的专辑里找不到封面文件时，绝不能把它们标成 examined：
/// finalize 只能把同一个多数派重新投一遍，而这遍重投在每次扫描上都要为每个专辑付至多
/// 五条语句。文件夹在位者那一例恰好相反——那里「封面不见了」才是新信息（见
/// `examine_folder_art_dethrones_incumbent_after_cover_deleted`）。
#[tokio::test]
async fn examine_without_cover_and_embedded_incumbent_marks_no_albums() {
    let (dir, pool) = create_test_pool("artwork-examine-embedded-noop-test").await;
    let a = red();
    let a_art = embedded_art(a.clone());

    write_album(&dir, &pool, &[&a_art, &a_art]).await;
    finalize(&pool, false).await;
    let a_id = artwork_id_for_hash(&pool, &a).await.unwrap();
    assert_album_art(&pool, Some(a_id), 0).await;

    let observations: FolderArtObservationMap = [(dir.utf8_path(), None)].into_iter().collect();
    let (examined, candidates, _) = examine_observations(&pool, &observations).await;

    assert!(
        examined.is_empty(),
        "an embedded incumbent in a cover-less directory has nothing to learn, got {examined:?}"
    );
    assert!(candidates.is_empty());
}

#[tokio::test]
async fn folder_art_loader_retries_after_read_failure() {
    let loader = FolderArtLoader::new(1);
    let dir = TestDir::new("folder-art-retry");
    let path = dir.utf8_join("cover.jpg");
    let candidate = FolderArtCandidate {
        path: path.clone(),
        rank: 1,
    };

    // 文件还不存在：加载会失败，这条死条目必须被丢掉，
    // 不能留成一个永远 join 到 None future 的 Loading 行
    assert!(loader.load(candidate.clone()).await.is_none());

    std::fs::write(&path, red()).unwrap();
    let loaded = loader
        .load(candidate)
        .await
        .expect("retry after failure should read the now-existing file");
    assert_eq!(loaded.hash, xxh3_64(&red()));
}

/// finalize 那一趟可能写下的全部内容，收进一个值：专辑封面指针与来源、曲目封面指针、
/// artwork 表规模。两次指纹相同就意味着其间那一趟一行都没动。
async fn art_fingerprint(pool: &SqlitePool) -> (i64, i64, i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM album WHERE artwork_id IS NULL), \
                (SELECT COALESCE(SUM(artwork_id), 0) FROM album), \
                (SELECT COALESCE(SUM(artwork_source), 0) FROM album), \
                (SELECT COUNT(*) FROM track WHERE artwork_id IS NULL), \
                (SELECT COALESCE(SUM(artwork_id), 0) FROM track), \
                (SELECT COUNT(*) FROM artwork)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// **每次**扫描完成都要跑的那趟封面工作的规模成本，包括什么都没改的扫描。
///
/// 形态取自生产：`execution.rs::finish_completed` 无条件调 `finalize_artwork`，
/// 而 `examine_folder_art` 会把目录里放着封面图的每个专辑标成 `examined`。正是在
/// `examined` 里这件事关掉了 finalize 的「保留现有」提前返回，于是每个专辑都要把多数派
/// 哈希重新投一遍、专辑封面重新发布、曲目封面重新指派——每个专辑 3 SELECT + 2 UPDATE——
/// 哪怕这次扫描一行都没写。
///
/// 两股风暴都调生产的 `finalize_scan_art`，所以对照组就是旧代码本身，且每一组都被断言
/// 零行移动——这条断言正是重点：一趟不可能改变答案的工作就是重算。
/// - `folder`：每个专辑目录都放着一张封面，因此 examine 为每个专辑暂存一个候选，
///   并仍然把它标成 examined——每个专辑 4 条语句（状态 SELECT、带守卫的专辑 UPDATE、
///   带守卫的曲目 UPDATE、离群项 SELECT）
/// - `embedded`：任何地方都没有封面文件，内嵌封面已经收敛（`artwork_source = 0`），
///   examine 仍把每个专辑标成 examined——每个专辑 5 条语句，多出的正是那次多数派重投
/// - `gated`：没有任何 examined、没有任何改动——即 `albums.is_empty()` 的提前返回，
///   也就是一趟「跳过自己改不动的东西」的工作所能达到的上限
///
/// 这里试过第三种形态——`examined` 全满且**没有**候选——它不是稳态：意思相当于「所有封面
/// 文件都消失了」，而 finalize 会正确地把文件夹在位者换成品内嵌多数派（见
/// `examine_folder_art_dethrones_incumbent_after_cover_deleted`）。它会移动行，所以是
/// 排除掉，而不是当作浪费来测量。
///
/// 本处未测量：`examine_folder_art` 每次扫描还会对所有文件夹封面文件做一次 `fs::read`
/// （加载器的弱缓存活不过一次扫描），这部分要用真实图片才能量化——未经证实。
///
/// 播种 4k 专辑 / 60k 曲目，因此不参与默认测试运行：
/// `cargo test --release --features kugou artwork_finalize -- --ignored --nocapture`
#[tokio::test]
#[ignore = "scale bench: seeds 4k albums / 60k tracks"]
async fn report_zero_change_artwork_finalize_storm() {
    const ALBUMS: usize = 4_000;
    const PER_ALBUM: usize = 15;
    const REPS: usize = 3;

    let (_dir, pool) = create_test_pool("artwork-storm-scale").await;
    seed_library(&pool, ALBUMS, PER_ALBUM, 1_000).await;

    // 每个专辑一行 artwork（artwork id == album id）并发布为文件夹封面，另加一条
    // album_path 认领：这是一座填满内容的库面对一次「什么都没改」的扫描时所呈现的状态。
    // 全部用集合式写入，因此播种不会漏进被测的那段。
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO artwork (id, hash, image, thumb) \
         SELECT id, 5000000 + id, x'8950', x'8950' FROM album",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE album SET artwork_id = id, artwork_source = 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE track SET art_hash = 5000000 + album_id, artwork_id = album_id")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO album_path (album_id, path, disc_num) \
         SELECT id, printf('C:\\bench\\album-%04d', id), 1 FROM album",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();

    let albums: Vec<i64> = sqlx::query_scalar("SELECT id FROM album ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(albums.len(), ALBUMS);
    let (tracks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tracks as usize, ALBUMS * PER_ALBUM);

    let examined: FxHashSet<i64> = albums.iter().copied().collect();
    let none: FxHashSet<i64> = FxHashSet::default();
    // examine_folder_art 对「每个专辑目录都放着一张封面」的库会暂存什么：每个专辑
    // 一个候选，哈希与在位者相同。
    let folder_candidates: FolderArtCandidates = albums
        .iter()
        .map(|&id| (id, ((5_000_000 + id) as u64, 1_i64)))
        .collect();

    let mut folder = Vec::new();
    let mut embedded = Vec::new();
    let mut gated = Vec::new();

    // `finalize_scan_art` 会清空交给它的候选表，所以每一轮都得从全新的副本开始。
    for rep in 0..REPS {
        let stamp = art_fingerprint(&pool).await;
        let mut candidates = folder_candidates.clone();
        let started = std::time::Instant::now();
        finalize_scan_art(&pool, false, &none, &examined, &mut candidates, false)
            .await
            .unwrap();
        folder.push(started.elapsed());
        assert_eq!(
            stamp,
            art_fingerprint(&pool).await,
            "folder-shape storm moved rows (rep {rep}) - it is not recomputation"
        );

        let started = std::time::Instant::now();
        finalize_scan_art(
            &pool,
            false,
            &none,
            &none,
            &mut FolderArtCandidates::default(),
            false,
        )
        .await
        .unwrap();
        gated.push(started.elapsed());
    }

    // 「没有封面文件」那种形态——本机库的真实情况（普查发现封面文件为零，
    // 每一行 artwork 都来自内嵌封面）。
    sqlx::query("UPDATE album SET artwork_source = 0")
        .execute(&pool)
        .await
        .unwrap();
    for rep in 0..REPS {
        let stamp = art_fingerprint(&pool).await;
        let started = std::time::Instant::now();
        finalize_scan_art(
            &pool,
            false,
            &none,
            &examined,
            &mut FolderArtCandidates::default(),
            false,
        )
        .await
        .unwrap();
        embedded.push(started.elapsed());
        assert_eq!(
            stamp,
            art_fingerprint(&pool).await,
            "embedded-shape storm moved rows (rep {rep}) - it is not recomputation"
        );
    }

    // 扫描完成每次运行都要付的其余开销，无论有无变更，单独隔离出来。
    let mut claims = Vec::new();
    let mut claims_plain = Vec::new();
    let mut art_ids = Vec::new();
    let mut sweep = Vec::new();
    let mut orphan_artists = Vec::new();
    let mut orphan = Vec::new();
    for _ in 0..REPS {
        // 已发布形态（它 join album 取 `artwork_source`，正是这一点让 examine
        // 可以跳过标记）与它所替代的形态对比，好把那次 join 的代价记录在案。
        let started = std::time::Instant::now();
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(include_str!(
            "../../../../queries/scan/list_album_path_claims.sql"
        ))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), ALBUMS);
        claims.push(started.elapsed());

        let started = std::time::Instant::now();
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT album_id, path FROM album_path WHERE disc_num IN (-1, 0, 1)")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), ALBUMS);
        claims_plain.push(started.elapsed());

        let started = std::time::Instant::now();
        let ids: Vec<(i64, i64)> = sqlx::query_as(include_str!(
            "../../../../queries/scan/list_artwork_ids.sql"
        ))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ids.len(), ALBUMS);
        art_ids.push(started.elapsed());

        // 清理用的存活扫描会分页走完每一条曲目行（页大小是
        // discover::full::CLEANUP_PAGE_SIZE，对该模块私有）。
        let started = std::time::Instant::now();
        let mut last = 0i64;
        let mut seen = 0usize;
        loop {
            let page: Vec<(i64, String)> = sqlx::query_as(include_str!(
                "../../../../queries/scan/list_track_locations_paged.sql"
            ))
            .bind(last)
            .bind(1000i64)
            .fetch_all(&pool)
            .await
            .unwrap();
            if page.is_empty() {
                break;
            }
            seen += page.len();
            last = page.last().unwrap().0;
        }
        assert_eq!(seen, ALBUMS * PER_ALBUM);
        sweep.push(started.elapsed());

        let started = std::time::Instant::now();
        sweep_orphan_artists(&pool).await;
        orphan_artists.push(started.elapsed());

        let mut tx = pool.begin().await.unwrap();
        let started = std::time::Instant::now();
        sqlx::query(include_str!(
            "../../../../queries/scan/delete_orphan_artwork.sql"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        orphan.push(started.elapsed());
        tx.rollback().await.unwrap();
    }

    println!(
        "scale: {ALBUMS} albums / {tracks} tracks / 1000 artists, one artwork row + one album_path claim per album"
    );
    let report = |label: &str, times: &[std::time::Duration], stmts: Option<u32>| {
        let min = *times.iter().min().unwrap();
        let max = *times.iter().max().unwrap();
        match stmts {
            Some(stmts) => println!(
                "  {label:<26} min={min:.3?} max={max:.3?}  per-album={:.2}us  {stmts} stmts/album",
                min.as_micros() as f64 / ALBUMS as f64,
            ),
            None => println!("  {label:<26} min={min:.3?} max={max:.3?}"),
        }
    };
    report("finalize folder-shape", &folder, Some(4));
    report("finalize embedded-shape", &embedded, Some(5));
    report("finalize gated (ceiling)", &gated, None);
    report("list_album_path_claims", &claims, None);
    report("  (old 2-column shape)", &claims_plain, None);
    report("list_artwork_ids", &art_ids, None);
    report("track liveness sweep", &sweep, None);
    report("sweep_orphan_artists", &orphan_artists, None);
    report("delete_orphan_artwork", &orphan, None);

    let fixed = *embedded.iter().min().unwrap()
        + *claims.iter().min().unwrap()
        + *art_ids.iter().min().unwrap()
        + *sweep.iter().min().unwrap()
        + *orphan_artists.iter().min().unwrap()
        + *orphan.iter().min().unwrap();
    println!(
        "  => zero-change scan, fixed DB cost at this scale: {fixed:.3?} \
         (gated ceiling removes the embedded storm only)"
    );
}
