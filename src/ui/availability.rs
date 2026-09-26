use std::path::Path;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::library::types::Track;

pub fn is_track_path_available(path: &Path) -> bool {
    // remote HTTP(S) streams (kugou et al.) don't exist on disk; treat any
    // http/https path as available so it can be selected and played
    #[cfg(feature = "online_sources")]
    if crate::media::is_http_path(path) {
        return true;
    }
    path.exists()
}

/// True when `path` is an HTTP(S) stream URL (an online track). Such paths
/// report "available" for playback, but no file exists on disk, so actions
/// like "show in file manager" must not be offered for them.
#[cfg_attr(not(feature = "online_sources"), allow(unused_variables))]
pub fn is_online_path(path: &Path) -> bool {
    #[cfg(feature = "online_sources")]
    if crate::media::is_http_path(path) {
        return true;
    }
    false
}

pub fn is_track_available(track: &Track) -> bool {
    is_track_path_available(&track.location)
}

/// Path -> "does it still exist" memo. Every availability set is grouped from
/// the same track paths, so sharing one cache across sets means one `exists()`
/// syscall per distinct path instead of one per set.
#[derive(Default)]
struct PathExistence {
    known: FxHashMap<String, bool>,
}

impl PathExistence {
    fn exists(&mut self, location: &str) -> bool {
        if let Some(&exists) = self.known.get(location) {
            return exists;
        }
        let exists = Path::new(location).exists();
        self.known.insert(location.to_owned(), exists);
        exists
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

/// Ids of albums that still have at least one track on disk, from a
/// `(album_id, track location)` row set. Replaces the per-album
/// `list_tracks_in_album` N+1 with one query plus one `exists()` stat per
/// distinct path. Pure so the search index loader can run it on a blocking
/// thread; `Path::exists` is a syscall and must stay off the UI thread.
pub fn compute_available_albums(rows: Vec<(i64, String)>) -> FxHashSet<i64> {
    available_ids(rows, &mut PathExistence::default())
}

/// Album, artist and track availability from the same pass, sharing a single
/// stat cache. Album rows and artist rows describe the same track paths, so
/// computing the sets separately statted every path twice — and on a large
/// library that pass is the expensive part of startup. The track rows come
/// from `get_all_tracks` (every track, album-less ones included — the
/// album-availability rows filter those out); each track id appears once, so
/// its slot in the result means exactly "this track's file still exists".
pub fn compute_availability(
    album_rows: Vec<(i64, String)>,
    artist_rows: Vec<(i64, String)>,
    track_rows: Vec<(i64, String)>,
) -> (FxHashSet<i64>, FxHashSet<i64>, FxHashSet<i64>) {
    let mut paths = PathExistence::default();
    let albums = available_ids(album_rows, &mut paths);
    let artists = available_ids(artist_rows, &mut paths);
    let tracks = available_ids(track_rows, &mut paths);
    (albums, artists, tracks)
}

#[cfg(test)]
mod tests {
    use super::{compute_availability, compute_available_albums};
    use crate::test_support::TestDir;

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

    /// `(artist id, location)` rows: an artist is available when any credited
    /// track is on disk — including a file shared with another artist, which
    /// must still mark both of them available.
    #[test]
    fn artist_availability_is_per_credited_artist() {
        let dir = TestDir::new("meliora-availability-artists");
        let present = dir.join("present.mp3");
        std::fs::write(&present, b"x").unwrap();
        let present = present.display().to_string();
        let missing = dir.join("missing.mp3").display().to_string();

        let rows = vec![
            (10_i64, present.clone()),
            (10_i64, missing.clone()),
            (11_i64, missing.clone()),
            (12_i64, present),
        ];

        let (_, available, _) = compute_availability(Vec::new(), rows, Vec::new());

        assert!(available.contains(&10), "artist with a file that exists");
        assert!(!available.contains(&11), "artist with only missing files");
        assert!(available.contains(&12), "artist sharing the present file");
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

        let rows = db::list_artist_availability(&pool).await.unwrap();
        let (_, available, _) = compute_availability(Vec::new(), rows, Vec::new());

        assert!(
            available.contains(&artist_id),
            "an album-level credit must keep the artist available"
        );
    }
}
