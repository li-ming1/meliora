use std::path::Path;

use gpui::App;

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

pub fn artist_has_available_tracks(cx: &mut App, artist_id: i64) -> bool {
    cx.get_all_tracks_by_artist(artist_id)
        .map(|tracks| tracks.iter().any(is_track_available))
        .unwrap_or_default()
}
