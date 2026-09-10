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

/// Ids of albums that still have at least one track on disk, from a
/// `(album_id, track location)` row set. Replaces the per-album
/// `list_tracks_in_album` N+1 with one query plus one `exists()` stat per
/// distinct path. Pure so the search index loader can run it on a blocking
/// thread; `Path::exists` is a syscall and must stay off the UI thread.
pub fn compute_available_albums(rows: Vec<(i64, String)>) -> FxHashSet<i64> {
    let mut by_album: FxHashMap<i64, Vec<String>> = FxHashMap::default();
    for (album_id, location) in rows {
        by_album.entry(album_id).or_default().push(location);
    }

    // One stat per distinct path, shared across albums (multi-artist
    // compilations put the same file under several albums).
    let mut visited: FxHashSet<String> = FxHashSet::default();
    let mut available: FxHashSet<i64> = FxHashSet::default();
    for (album_id, locations) in by_album {
        for location in locations {
            if !visited.insert(location.clone()) {
                continue;
            }
            if Path::new(&location).exists() {
                available.insert(album_id);
                break;
            }
        }
    }

    available
}

/// Ids of artists that still have at least one credited track on disk, from an
/// `(artist id, track location)` row set. Replaces the per-artist
/// `get_all_tracks_by_artist` N+1 (a DB round trip per rendered artist row)
/// with one query plus one `exists()` stat per distinct path. Pure so the
/// loader can run it on a blocking thread; `Path::exists` is a syscall and
/// must stay off the UI thread.
pub fn compute_available_artists(rows: Vec<(i64, String)>) -> FxHashSet<i64> {
    let mut by_artist: FxHashMap<i64, Vec<String>> = FxHashMap::default();
    for (artist_id, location) in rows {
        by_artist.entry(artist_id).or_default().push(location);
    }

    // One stat per distinct path, shared across artists (a track with several
    // credited artists puts the same file under each of them).
    let mut known: FxHashMap<String, bool> = FxHashMap::default();
    let mut available: FxHashSet<i64> = FxHashSet::default();
    for (artist_id, locations) in by_artist {
        for location in locations {
            let exists = *known
                .entry(location.clone())
                .or_insert_with(|| Path::new(&location).exists());
            if exists {
                available.insert(artist_id);
                break;
            }
        }
    }

    available
}

#[cfg(test)]
mod tests {
    use super::{compute_available_albums, compute_available_artists};
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

        let available = compute_available_artists(rows);

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
        let available = compute_available_artists(rows);

        assert!(
            available.contains(&artist_id),
            "an album-level credit must keep the artist available"
        );
    }
}
