//! NetEase shared online-source layer: the stream URL → track registry
//! ("stream map"), play-URL fetching and the restored-URL refresh. A-1
//! step-① sink from `crate::ui::netease`, so the playback thread and stats
//! resolve streams without touching the UI; the UI keeps calling these
//! through the re-exports in `crate::ui::netease`. Gated behind the
//! `netease` cargo feature. Mirrors `super::kugou` (but keeps its own
//! Mutex-based map — deliberately not unified, to avoid behavior changes in
//! this step).

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use gpui::SharedString;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::stream_registry::StreamMapPersistence;
use crate::netease;

/// One online track as shown in search results and playlists. `id` is the
/// NetEase song id (the play-URL / lyrics / like key).
#[derive(Clone, Debug, PartialEq)]
pub struct NeteaseTrackInfo {
    pub title: SharedString,
    pub artist: SharedString,
    pub album: SharedString,
    /// Duration in seconds (0 when unknown).
    pub duration: i64,
    pub id: i64,
    pub album_id: i64,
    /// `fee` field: 0 free, 1 VIP-only, 4 album purchase, 8 low quality free.
    pub fee: i64,
    /// Absolute album-art URL from the API. Empty when there is none.
    pub cover_url: SharedString,
}

// ---------------------------------------------------------------------------
// Stream URL → track registry
//
// NetEase play URLs are opaque signed CDN links that embed no song id, so the
// queue (which only carries the URL) cannot resolve lyrics / the like state
// on its own. Every resolved play URL is recorded here (in memory AND on
// disk, so a restored queue still matches after a restart).
// ---------------------------------------------------------------------------

/// On-disk mirror of a registry entry (plain strings, JSON friendly).
///
/// `pub(crate)`: the UI-side `play_track` scans the map directly through the
/// re-export in `crate::ui::netease` (queue de-dup pass).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct StreamMapEntry {
    pub(crate) url: String,
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) artist: String,
    #[serde(default)]
    pub(crate) album: String,
    #[serde(default)]
    pub(crate) album_id: i64,
    #[serde(default)]
    pub(crate) fee: i64,
    #[serde(default)]
    pub(crate) duration: i64,
    #[serde(default)]
    pub(crate) cover_url: String,
}

const STREAM_MAP_CAP: usize = 200;

fn stream_map_path() -> PathBuf {
    crate::paths::data_dir().join("netease_stream_map.json")
}

/// The in-memory stream map, mirrored on disk. `pub(crate)`: the UI-side
/// `play_track` scans it directly through the re-export in
/// `crate::ui::netease`.
pub(crate) fn stream_map() -> &'static Mutex<Vec<StreamMapEntry>> {
    static STREAM_MAP: OnceLock<Mutex<Vec<StreamMapEntry>>> = OnceLock::new();
    STREAM_MAP.get_or_init(|| {
        let entries = std::fs::read_to_string(stream_map_path())
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();
        Mutex::new(entries)
    })
}

fn entry_to_track(entry: &StreamMapEntry) -> NeteaseTrackInfo {
    NeteaseTrackInfo {
        title: SharedString::from(entry.title.clone()),
        artist: SharedString::from(entry.artist.clone()),
        album: SharedString::from(entry.album.clone()),
        duration: entry.duration,
        id: entry.id,
        album_id: entry.album_id,
        fee: entry.fee,
        cover_url: SharedString::from(entry.cover_url.clone()),
    }
}

/// 流注册表持久化设施（去重基线 + 写闸门）。本 provider 专属 static 实例：
/// 基线绝不能与 kugou 共享，否则去重跳写与失败回滚会互相误伤。
static STREAM_MAP_PERSISTENCE: StreamMapPersistence = StreamMapPersistence::new();

/// Records that `url` (a NetEase stream) belongs to `track` and persists the
/// registry (LRU-capped).
pub fn remember_online_track(url: String, track: NeteaseTrackInfo) {
    let entry = StreamMapEntry {
        url,
        id: track.id,
        title: track.title.to_string(),
        artist: track.artist.to_string(),
        album: track.album.to_string(),
        album_id: track.album_id,
        fee: track.fee,
        duration: track.duration,
        cover_url: track.cover_url.to_string(),
    };
    // Only mutate + snapshot under the lock; serialization and the disk write
    // both happen outside of it: the map is read on both the UI thread and
    // the playback thread, so holding the mutex across either lets a slow FS
    // stall song changes.
    let snapshot = {
        let mut guard = stream_map().lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|existing| existing.url != entry.url);
        guard.insert(0, entry);
        guard.truncate(STREAM_MAP_CAP);
        guard.clone()
    };
    let Some(json) = serde_json::to_vec(&snapshot).ok() else {
        return;
    };
    // Off-thread and outside the lock; the gate serialization and the
    // failure-rollback rationale live on the shared persist helpers.
    if let Some(json) = STREAM_MAP_PERSISTENCE.persist_if_changed(json) {
        STREAM_MAP_PERSISTENCE.spawn_persist(stream_map_path(), json, |err| {
            tracing::warn!(%err, "failed to persist netease stream map");
        });
    }
}

/// If `path` is a stream reached through this app, returns the remembered
/// online track for it.
pub fn online_track_matching_path(path: &Path) -> Option<NeteaseTrackInfo> {
    let path_str = path.to_string_lossy();
    let guard = stream_map().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .find(|entry| entry.url == path_str.as_ref())
        .map(entry_to_track)
}

/// Extracts the first playable URL out of a `song_url` response
/// (`data[0].url`), together with whether it is only a trial clip
/// (`freeTrialInfo` present).
pub fn extract_song_url(body: &Value) -> Option<(String, bool)> {
    body.pointer("/data/0").and_then(|entry| {
        entry
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(|url| {
                let trial = entry
                    .get("freeTrialInfo")
                    .is_some_and(|info| !info.is_null());
                (url.to_string(), trial)
            })
    })
}

/// Playable URL for one NetEase song at the requested level, falling back to
/// standard when the higher tier is unavailable. Trial clips are accepted for
/// playback (that is what a non-VIP account gets for VIP songs, same as the
/// web player); downloads reject them. Shared by live playback and the
/// session-restore URL refresh.
pub async fn fetch_stream_url(
    client: &netease::NeteaseClient,
    id: i64,
    quality: &str,
) -> Option<String> {
    // "standard" already is the lowest tier, so it gets no fallback level.
    let fallback = if quality == "standard" {
        None
    } else {
        Some("standard")
    };
    for level in [Some(quality), fallback].into_iter().flatten() {
        if let Ok(resp) = client.song_url(id, level).await
            && let Some((url, _trial)) = extract_song_url(&resp.body)
        {
            return Some(url);
        }
    }
    None
}

/// Re-fetches a fresh play URL for a restored NetEase queue item and
/// re-records the new URL in the stream registry, so lyrics / like resolution
/// for the running stream keeps working after the signed URL expired.
pub async fn refresh_restored_url(
    id: i64,
    quality: &str,
    name: Option<String>,
    artist: Option<String>,
    duration: Option<i64>,
    cover_url: Option<String>,
) -> Option<String> {
    let client = netease::shared_client();
    let url = fetch_stream_url(&client, id, quality).await?;
    let track = NeteaseTrackInfo {
        title: SharedString::from(name.unwrap_or_default()),
        artist: SharedString::from(artist.unwrap_or_default()),
        album: SharedString::default(),
        duration: duration.unwrap_or(0),
        id,
        album_id: 0,
        fee: 0,
        cover_url: SharedString::from(cover_url.unwrap_or_default()),
    };
    remember_online_track(url.clone(), track);
    Some(url)
}

/// The NetEase entry in the compile-time provider registry (A-1 step ②); the
/// body delegates to the free functions above.
pub struct NeteaseSource;

#[async_trait::async_trait]
impl super::OnlineSourceProvider for NeteaseSource {
    fn handles(&self, identity: &super::OnlineIdentity) -> bool {
        matches!(identity, super::OnlineIdentity::Netease { .. })
    }

    fn identify_path(&self, path: &Path) -> Option<super::OnlineTrackMatch> {
        let track = online_track_matching_path(path)?;
        Some(super::OnlineTrackMatch {
            identity: super::OnlineIdentity::Netease { id: track.id },
            title: track.title.to_string(),
            artist: track.artist.to_string(),
            album: track.album.to_string(),
        })
    }

    async fn refresh_url(
        &self,
        identity: &super::OnlineIdentity,
        ctx: &super::RefreshContext<'_>,
    ) -> Option<String> {
        // Registry routing guarantees the NetEase variant; the fallthrough is
        // type exhaustiveness only.
        let super::OnlineIdentity::Netease { id } = identity else {
            return None;
        };
        let (name, artist, duration, cover_url) = &ctx.display;
        refresh_restored_url(
            *id,
            ctx.netease_quality,
            name.clone(),
            artist.clone(),
            *duration,
            cover_url.clone(),
        )
        .await
    }
}
