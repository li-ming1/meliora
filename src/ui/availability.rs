use std::path::Path;

use gpui::App;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::library::{db::LibraryAccess, types::Track};

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

pub fn album_has_available_tracks(cx: &mut App, album_id: i64) -> bool {
    cx.list_tracks_in_album(album_id)
        .map(|tracks| tracks.iter().any(is_track_available))
        .unwrap_or_default()
}

/// Ids of albums that still have at least one track on disk. Replaces the
/// per-album `list_tracks_in_album` N+1 with one query plus one `exists()`
/// stat per distinct path, so loading a full-library search index stops
/// issuing one query per album.
pub fn available_album_ids(cx: &mut App) -> FxHashSet<i64> {
    let Ok(rows) = cx.list_album_availability() else {
        return FxHashSet::default();
    };

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

pub fn artist_has_available_tracks(cx: &mut App, artist_id: i64) -> bool {
    cx.get_all_tracks_by_artist(artist_id)
        .map(|tracks| tracks.iter().any(is_track_available))
        .unwrap_or_default()
}
