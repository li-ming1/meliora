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
/// `list_tracks_in_album` N+1 with one query plus one `exists()` stat per
/// distinct path. Pure so the search index loader can run it on a blocking
/// thread; `Path::exists` is a syscall and must stay off the UI thread.
pub fn compute_available_albums(rows: Vec<(i64, String)>) -> FxHashSet<i64> {
    available_ids(rows, &mut PathExistence::default())
}

#[cfg(test)]
mod tests {
    use super::{compute_availability_from_tracks, compute_available_albums};
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
