//! UI-thread database access: the [`LibraryAccess`] trait implemented for
//! [`gpui::App`], plus the UI-thread blocking measurement around it.
//!
//! Lives in the UI layer (moved out of `library::db`) because the trait hands
//! query results to view code synchronously on the UI thread — keeping it here
//! leaves the database layer free of gpui dependencies.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::App;
use rustc_hash::FxHashSet;

use crate::library::{
    Pool,
    db::{self, LikedTrackSortMethod, PlaylistTrackRow, PlaylistTrackSortMethod},
    types::{Album, Artist, ArtistWithCounts, Playlist, PlaylistItem, Track},
};

pub trait LibraryAccess {
    fn list_tracks_in_album(&self, album_id: i64) -> sqlx::Result<Arc<Vec<Track>>>;
    fn get_album_by_id(&self, album_id: i64) -> sqlx::Result<Arc<Album>>;
    fn get_artist_by_id(&self, artist_id: i64) -> sqlx::Result<Arc<Artist>>;
    fn get_track_by_id(&self, track_id: i64) -> sqlx::Result<Arc<Track>>;
    fn get_track_by_path(&self, path: &Path) -> sqlx::Result<Option<Arc<Track>>>;
    fn create_playlist(&self, name: &str) -> sqlx::Result<i64>;
    fn delete_playlist(&self, playlist_id: i64) -> sqlx::Result<()>;
    fn rename_playlist(&self, playlist_id: i64, name: &str) -> sqlx::Result<()>;
    fn list_all_playlists(&self) -> sqlx::Result<Arc<Vec<Playlist>>>;
    fn list_playlist_tracks(&self, playlist_id: i64) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>>;
    fn list_playlist_tracks_sorted(
        &self,
        playlist_id: i64,
        sort_method: PlaylistTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>>;
    fn move_playlist_item(&self, item_id: i64, new_position: i64) -> sqlx::Result<()>;
    fn reorder_playlist(&self, playlist_id: i64, new_position: i64) -> sqlx::Result<()>;
    fn get_playlist_item(&self, item_id: i64) -> sqlx::Result<PlaylistItem>;
    fn playlist_has_track(&self, playlist_id: i64, track_id: i64) -> sqlx::Result<Option<i64>>;
    fn playlist_ids_for_track(&self, track_id: i64) -> sqlx::Result<FxHashSet<i64>>;
    fn playlist_contains_all_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
    ) -> sqlx::Result<bool>;
    fn list_albums_by_artist(&self, artist_id: i64) -> sqlx::Result<Vec<(u32, String)>>;
    fn get_artist_with_counts(&self, artist_id: i64) -> sqlx::Result<Arc<ArtistWithCounts>>;
    fn list_liked_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>>;
    fn list_standalone_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>>;
    fn list_all_tracks_by_artist(&self, artist_id: i64) -> sqlx::Result<Arc<Vec<Track>>>;
    fn artist_ids_for_album(&self, album_id: i64) -> sqlx::Result<Vec<(i64, String)>>;
    fn artist_ids_for_track(&self, track_id: i64) -> sqlx::Result<Vec<(i64, String)>>;
    fn list_album_paths(&self, album_id: i64) -> sqlx::Result<Vec<String>>;
}

// ---------------------------------------------------------------------------
// UI-thread database blocking measurement
//
// Every `LibraryAccess for App` method parks the UI thread on
// `RUNTIME.block_on`. Doctrine §2.3 forbids blocking the main thread and §14
// forbids database access on the UI hot path, so those `block_on`s are debt —
// but which of them is worth converting to a background load is a question for
// data, not intuition (§1/§36). The methods routed through `blocking_query`
// below are the ones the row- and view-construction paths call, which is where
// the call volume is; the remaining methods can be added the same way.
// ---------------------------------------------------------------------------

/// A single query slower than this is logged with its method name.
const UI_QUERY_SLOW_MICROS: u64 = 2_000;
/// Two log lines per accumulated quarter-second of UI-thread blocking: silent
/// while the total stays small, a growing curve when it does not.
const UI_QUERY_REPORT_STEP_MICROS: u64 = 250_000;

static UI_QUERY_CALLS: AtomicU64 = AtomicU64::new(0);
static UI_QUERY_TOTAL_MICROS: AtomicU64 = AtomicU64::new(0);
static UI_QUERY_PEAK_MICROS: AtomicU64 = AtomicU64::new(0);

/// Runs a UI-thread database query, recording how long it parked the UI thread.
fn blocking_query<T>(method: &'static str, future: impl std::future::Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    let result = crate::RUNTIME.block_on(future);
    let micros = started.elapsed().as_micros() as u64;

    let calls = UI_QUERY_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let total = UI_QUERY_TOTAL_MICROS.fetch_add(micros, Ordering::Relaxed) + micros;
    let peak = UI_QUERY_PEAK_MICROS
        .fetch_max(micros, Ordering::Relaxed)
        .max(micros);

    if micros >= UI_QUERY_SLOW_MICROS {
        tracing::warn!(method, micros, "[db] slow ui-thread query");
    }

    if total / UI_QUERY_REPORT_STEP_MICROS != (total - micros) / UI_QUERY_REPORT_STEP_MICROS {
        tracing::info!(
            calls,
            total_ms = total / 1000,
            peak_ms = peak / 1000,
            "[db] ui-thread query total"
        );
    }

    result
}

impl LibraryAccess for App {
    fn list_tracks_in_album(&self, album_id: i64) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_tracks_in_album",
            db::list_tracks_in_album(&pool.0, album_id),
        )
    }

    fn get_album_by_id(&self, album_id: i64) -> sqlx::Result<Arc<Album>> {
        let pool: &Pool = self.global();
        blocking_query("get_album_by_id", db::get_album_by_id(&pool.0, album_id))
    }

    fn get_artist_by_id(&self, artist_id: i64) -> sqlx::Result<Arc<Artist>> {
        let pool: &Pool = self.global();
        blocking_query("get_artist_by_id", db::get_artist_by_id(&pool.0, artist_id))
    }

    fn get_track_by_id(&self, track_id: i64) -> sqlx::Result<Arc<Track>> {
        let pool: &Pool = self.global();
        blocking_query("get_track_by_id", db::get_track_by_id(&pool.0, track_id))
    }

    fn get_track_by_path(&self, path: &Path) -> sqlx::Result<Option<Arc<Track>>> {
        let pool: &Pool = self.global();
        blocking_query("get_track_by_path", db::get_track_by_path(&pool.0, path))
    }

    fn create_playlist(&self, name: &str) -> sqlx::Result<i64> {
        let pool: &Pool = self.global();
        blocking_query("create_playlist", db::create_playlist(&pool.0, name))
    }

    fn delete_playlist(&self, playlist_id: i64) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query("delete_playlist", db::delete_playlist(&pool.0, playlist_id))
    }

    fn rename_playlist(&self, playlist_id: i64, name: &str) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query(
            "rename_playlist",
            db::rename_playlist(&pool.0, playlist_id, name),
        )
    }

    fn list_all_playlists(&self) -> sqlx::Result<Arc<Vec<Playlist>>> {
        let pool: &Pool = self.global();
        blocking_query("list_all_playlists", db::list_all_playlists(&pool.0))
    }

    fn list_playlist_tracks(&self, playlist_id: i64) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_playlist_tracks",
            db::list_playlist_tracks(&pool.0, playlist_id),
        )
    }

    fn list_playlist_tracks_sorted(
        &self,
        playlist_id: i64,
        sort_method: PlaylistTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<PlaylistTrackRow>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_playlist_tracks_sorted",
            db::list_playlist_tracks_sorted(&pool.0, playlist_id, sort_method),
        )
    }

    fn move_playlist_item(&self, item_id: i64, new_position: i64) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query(
            "move_playlist_item",
            db::move_playlist_item(&pool.0, item_id, new_position),
        )
    }

    fn reorder_playlist(&self, playlist_id: i64, new_position: i64) -> sqlx::Result<()> {
        let pool: &Pool = self.global();
        blocking_query(
            "reorder_playlist",
            db::reorder_playlist(&pool.0, playlist_id, new_position),
        )
    }

    fn get_playlist_item(&self, item_id: i64) -> sqlx::Result<PlaylistItem> {
        let pool: &Pool = self.global();
        blocking_query("get_playlist_item", db::get_playlist_item(&pool.0, item_id))
    }

    fn playlist_has_track(&self, playlist_id: i64, track_id: i64) -> sqlx::Result<Option<i64>> {
        let pool: &Pool = self.global();
        blocking_query(
            "playlist_has_track",
            db::playlist_has_track(&pool.0, playlist_id, track_id),
        )
    }

    fn playlist_ids_for_track(&self, track_id: i64) -> sqlx::Result<FxHashSet<i64>> {
        let pool: &Pool = self.global();
        blocking_query(
            "playlist_ids_for_track",
            db::playlist_ids_for_track(&pool.0, track_id),
        )
    }

    fn playlist_contains_all_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
    ) -> sqlx::Result<bool> {
        let pool: &Pool = self.global();
        blocking_query(
            "playlist_contains_all_tracks",
            db::playlist_contains_all_tracks(&pool.0, playlist_id, track_ids),
        )
    }

    fn list_albums_by_artist(&self, artist_id: i64) -> sqlx::Result<Vec<(u32, String)>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_albums_by_artist",
            db::list_albums_by_artist(&pool.0, artist_id),
        )
    }

    fn get_artist_with_counts(&self, artist_id: i64) -> sqlx::Result<Arc<ArtistWithCounts>> {
        let pool: &Pool = self.global();
        blocking_query(
            "get_artist_with_counts",
            db::get_artist_with_counts(&pool.0, artist_id),
        )
    }

    fn list_liked_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_liked_tracks_by_artist",
            db::list_liked_tracks_by_artist(&pool.0, artist_id, sort_method),
        )
    }

    fn list_standalone_tracks_by_artist(
        &self,
        artist_id: i64,
        sort_method: LikedTrackSortMethod,
    ) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_standalone_tracks_by_artist",
            db::list_standalone_tracks_by_artist(&pool.0, artist_id, sort_method),
        )
    }

    fn list_all_tracks_by_artist(&self, artist_id: i64) -> sqlx::Result<Arc<Vec<Track>>> {
        let pool: &Pool = self.global();
        blocking_query(
            "list_all_tracks_by_artist",
            db::list_all_tracks_by_artist(&pool.0, artist_id),
        )
    }

    fn artist_ids_for_album(&self, album_id: i64) -> sqlx::Result<Vec<(i64, String)>> {
        let pool: &Pool = self.global();
        blocking_query(
            "artist_ids_for_album",
            db::artist_ids_for_album(&pool.0, album_id),
        )
    }

    fn artist_ids_for_track(&self, track_id: i64) -> sqlx::Result<Vec<(i64, String)>> {
        let pool: &Pool = self.global();
        blocking_query(
            "artist_ids_for_track",
            db::artist_ids_for_track(&pool.0, track_id),
        )
    }

    fn list_album_paths(&self, album_id: i64) -> sqlx::Result<Vec<String>> {
        let pool: &Pool = self.global();
        blocking_query("list_album_paths", db::list_album_paths(&pool.0, album_id))
    }
}
