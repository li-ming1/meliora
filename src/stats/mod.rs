pub mod queries;
mod recorder;

use std::sync::{Arc, Mutex};

use gpui::{AsyncApp, Entity, Global};

#[cfg(feature = "online_sources")]
use crate::playback::queue::OnlineIdentity;
use crate::ui::models::Queue;

pub use recorder::ListenRow;
use recorder::StatsRecorder;

/// Unix seconds of local midnight on 2026-08-01, the first day listening
/// stats are tracked. Everything before it is ignored by every query and by
/// the heat-map window (the feature shipped 2026-08).
pub fn epoch_ts() -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .earliest()
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

/// Display metadata attached to listen rows: captured from the online
/// provider registry at song-change time, or resolved from the library at
/// write time for local tracks.
#[derive(Clone, Debug, Default)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// Shared recorder handle: the playback event loop feeds it on the UI thread,
/// the app-quit hook drains it.
pub struct StatsHandle(pub Arc<Mutex<StatsRecorder>>);

impl Global for StatsHandle {}

impl StatsHandle {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(StatsRecorder::new())))
    }
}

/// Persists finished rows off the UI thread; a no-op for an empty batch.
pub fn flush_rows_async(pool: sqlx::SqlitePool, rows: Vec<ListenRow>) {
    if rows.is_empty() {
        return;
    }
    crate::RUNTIME.spawn(async move {
        queries::write_rows(&pool, rows).await;
    });
}

/// Identity + display metadata for the song that just started, keyed for
/// `listen_event.track_key`. Local files key on their path; online streams key
/// on their provider identity with a URL-hash fallback.
pub fn song_info(
    path: &std::path::Path,
    queue: &Entity<Queue>,
    cx: &mut AsyncApp,
) -> (String, Option<TrackMeta>) {
    if !crate::media::is_http_path(path) {
        return (format!("local:{}", path.to_string_lossy()), None);
    }

    #[cfg(feature = "online_sources")]
    {
        let item = queue.update(cx, |q, _| {
            q.data.read().ok().and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get_path() == path)
                    .cloned()
            })
        });

        let identity = item.as_ref().and_then(|item| item.online_identity());
        let key = match identity {
            Some(OnlineIdentity::Kugou { mix_song_id, .. }) => format!("kugou:{mix_song_id}"),
            Some(OnlineIdentity::Netease { id }) => format!("netease:{id}"),
            None => return (format!("hash:{}", url_hash(path)), None),
        };

        let mut meta = None;
        #[cfg(feature = "kugou")]
        if matches!(identity, Some(OnlineIdentity::Kugou { .. }))
            && let Some(track) = crate::ui::online::online_track_matching_path(path)
        {
            meta = Some(TrackMeta {
                title: track.title.to_string(),
                artist: track.artist.to_string(),
                album: track.album.to_string(),
            });
        }
        #[cfg(feature = "netease")]
        if matches!(identity, Some(OnlineIdentity::Netease { .. }))
            && let Some(track) = crate::ui::online::netease_online_track_matching_path(path)
        {
            meta = Some(TrackMeta {
                title: track.title.to_string(),
                artist: track.artist.to_string(),
                album: track.album.to_string(),
            });
        }
        if meta.is_none()
            && let Some((name, artist, _, _)) =
                item.as_ref().and_then(|item| item.persisted_display())
        {
            meta = Some(TrackMeta {
                title: name.unwrap_or_default(),
                artist: artist.unwrap_or_default(),
                album: String::new(),
            });
        }
        (key, meta)
    }

    #[cfg(not(feature = "online_sources"))]
    {
        let _ = (queue, cx);
        (format!("hash:{}", url_hash(path)), None)
    }
}

fn url_hash(path: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
