//! KuGou shared online-source layer: the stream URL → track registry
//! ("stream map"), play-URL fetching and the liked-songs cache. A-1 step-①
//! sink from `crate::ui::kugou`, so the playback thread and stats resolve
//! streams without touching the UI; the UI keeps calling these through the
//! re-exports in `crate::ui::kugou`. Gated behind the `kugou` cargo feature.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::SharedString;
use serde_json::Value;

use crate::kugou;

/// One online track as shown in search results and playlists.
#[derive(Clone, Debug, PartialEq)]
pub struct KugouTrackInfo {
    pub title: SharedString,
    pub artist: SharedString,
    pub album: SharedString,
    /// Duration in seconds (0 when unknown).
    pub duration: i64,
    pub hash: String,
    pub mix_song_id: i64,
    pub album_id: i64,
    /// Absolute album-art URL from the API (the `{size}` placeholder is
    /// substituted with a fixed pixel width). Empty when the API gives none.
    pub cover_url: SharedString,
}

// ---------------------------------------------------------------------------
// Stream URL → track registry
//
// KuGou play URLs are opaque signed CDN links; the queue (which only carries
// the URL) cannot resolve lyrics / the like state / the download action on
// its own. Every resolved play URL is recorded here in memory AND on disk, so
// a restored queue still matches after a restart — even when the
// session-restore URL refresh hits a network failure. Mirrors NetEase's
// stream-map.
// ---------------------------------------------------------------------------

/// On-disk mirror of a registry entry (plain strings, JSON friendly).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct StreamMapEntry {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    duration: i64,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    mix_song_id: i64,
    #[serde(default)]
    album_id: i64,
    #[serde(default)]
    cover_url: String,
}

impl From<(String, KugouTrackInfo)> for StreamMapEntry {
    fn from((url, track): (String, KugouTrackInfo)) -> Self {
        Self {
            url,
            title: track.title.to_string(),
            artist: track.artist.to_string(),
            album: track.album.to_string(),
            duration: track.duration,
            hash: track.hash,
            mix_song_id: track.mix_song_id,
            album_id: track.album_id,
            cover_url: track.cover_url.to_string(),
        }
    }
}

impl StreamMapEntry {
    fn to_track(&self) -> KugouTrackInfo {
        KugouTrackInfo {
            title: SharedString::from(self.title.clone()),
            artist: SharedString::from(self.artist.clone()),
            album: SharedString::from(self.album.clone()),
            duration: self.duration,
            hash: self.hash.clone(),
            mix_song_id: self.mix_song_id,
            album_id: self.album_id,
            cover_url: SharedString::from(self.cover_url.clone()),
        }
    }
}

const STREAM_MAP_CAP: usize = 200;

fn stream_map_path() -> PathBuf {
    crate::paths::data_dir().join("kugou_stream_map.json")
}

fn stream_map() -> &'static RwLock<Vec<StreamMapEntry>> {
    static STREAM_MAP: OnceLock<RwLock<Vec<StreamMapEntry>>> = OnceLock::new();
    STREAM_MAP.get_or_init(|| {
        let entries = std::fs::read_to_string(stream_map_path())
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();
        RwLock::new(entries)
    })
}

/// Bytes of the stream-map JSON last handed to the persistence task. Kept so
/// re-recording an unchanged registry (e.g. replaying the same song) skips
/// the disk write entirely instead of re-writing identical bytes.
static LAST_PERSISTED_STREAM_MAP: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();

/// Serializes stream-map persistence. Two overlapping truncate+write on the
/// same file can interleave into torn, unparseable JSON — which would drop
/// the whole registry on next launch — so every write takes this gate first.
/// tokio's mutex is fair by poll order (not spawn order), so writers land
/// roughly in hand-off order; a stale final write only costs a URL refresh
/// on next launch (the registry is a cache).
static STREAM_MAP_WRITE_GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// Returns `json` back when it differs from the last persisted bytes (and
/// records it as the new baseline), or `None` when it is unchanged and the
/// write task can be skipped. The mutex is only ever held for a memcmp.
fn persist_stream_map_if_changed(json: Vec<u8>) -> Option<Vec<u8>> {
    let mut last = LAST_PERSISTED_STREAM_MAP
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(json.as_slice()) {
        return None;
    }
    *last = Some(json.clone());
    Some(json)
}

/// Records that `url` (a KuGou stream) belongs to `track`, keeping the newest
/// entry for a track on top and evicting older URLs for the same track.
pub fn remember_online_track(url: String, track: KugouTrackInfo) {
    // Only mutate + snapshot under the lock; serialization and the disk write
    // both happen outside of it: the map is read on both the UI thread and
    // the playback thread, so holding the write guard across either lets a
    // slow FS stall song changes.
    let snapshot = {
        let mut guard = stream_map().write().unwrap_or_else(|e| e.into_inner());
        guard.retain(|e| e.url != url && e.mix_song_id != track.mix_song_id);
        guard.insert(0, StreamMapEntry::from((url, track)));
        guard.truncate(STREAM_MAP_CAP);
        guard.clone()
    };
    let Some(json) = serde_json::to_vec(&snapshot).ok() else {
        return;
    };
    // Write off-thread and outside the lock. Two racing writers may persist
    // in either order; a stale file only costs a URL refresh on next launch.
    if let Some(json) = persist_stream_map_if_changed(json) {
        let path = stream_map_path();
        let gate = STREAM_MAP_WRITE_GATE.get_or_init(Default::default);
        crate::RUNTIME.spawn(async move {
            let _gate = gate.lock().await;
            if let Err(err) = tokio::fs::write(&path, &json).await {
                // Roll the baseline back so an identical later snapshot
                // retries instead of silently leaving the old file in place.
                // Only when the baseline is still our own bytes: a newer
                // writer may have updated it meanwhile.
                if let Some(last) = LAST_PERSISTED_STREAM_MAP.get() {
                    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                    if last.as_deref() == Some(json.as_slice()) {
                        last.take();
                    }
                }
                tracing::warn!(%err, "failed to persist kugou stream map");
            }
        });
    }
}

/// If `path` is a stream reached through this app, returns the remembered
/// online track for it. Matching keys off the `mixsongid` embedded in the URL
/// (same heuristic as the queue de-dup logic) before falling back to a
/// byte-for-byte URL compare, so it still resolves when the redirect target
/// differs from the URL that produced it.
pub fn online_track_matching_path(path: &Path) -> Option<KugouTrackInfo> {
    let path_str = path.to_string_lossy();
    let guard = stream_map().read().unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .find(|entry| {
            entry.url == path_str.as_ref()
                || mixsongid_in_url(&path_str).is_some_and(|id| id == entry.mix_song_id)
        })
        .map(StreamMapEntry::to_track)
}

/// KuGou stream URLs embed the mixsong id as `_mx<digits>_` (e.g.
/// `..._pi2_mx29048099_qu128_...`). The id is stable across differently-signed
/// URLs for the same track, so it identifies a queued online track reliably.
pub(crate) fn mixsongid_in_url(url: &str) -> Option<i64> {
    let idx = url.find("_mx")?;
    let rest = &url[idx + 3..];
    let digits = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())?;
    digits.parse().ok()
}

/// Extracts the first playable URL out of a `song_url` response. The direct
/// links live at the top level under `url` (an array), e.g. the `/v5/url`
/// response has `{ ..., "url": ["http://...", "http://..."] }`.
pub fn extract_song_url(body: &Value) -> Option<String> {
    match body.get("url") {
        Some(Value::Array(items)) => items
            .iter()
            .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string)),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Fetches a playable URL for a KuGou track at the requested `quality`,
/// falling back to the standard 128 kbps clip when the higher tier is
/// unavailable (e.g. no VIP). Returns `None` when no playable URL comes back
/// at all. Shared by live playback and the session-restore URL refresh.
pub async fn fetch_stream_url(
    client: &kugou::KugouClient,
    hash: &str,
    mix_song_id: i64,
    album_id: i64,
    quality: &str,
) -> Option<String> {
    let free_part = !client.has_active_vip();
    if let Ok(resp) = client
        .song_url(hash, mix_song_id, album_id, quality, free_part)
        .await
        && let Some(url) = extract_song_url(&resp.body)
    {
        return Some(url);
    }
    if quality != "128" {
        let Ok(resp) = client
            .song_url(hash, mix_song_id, album_id, "128", free_part)
            .await
        else {
            return None;
        };
        return extract_song_url(&resp.body);
    }
    None
}

/// Lists the `(hash, fileid)` pairs in the user's KuGou liked-songs list
/// (`listid: 2`). Runs on the Tokio runtime; `None` when not logged in or on
/// any error (best-effort) — a `None` result says nothing about the list, so
/// callers must not treat it as "the list is empty". `fileid` is the
/// in-playlist key required by the `/v4/delete_songs` endpoint.
async fn liked_entries_raw() -> Option<Vec<(String, i64)>> {
    let client = kugou::shared_client();
    let mut out = Vec::new();
    for page in 1..=10 {
        match client.playlist_tracks(2, page, 100).await {
            Ok(resp) => {
                let Some(info) = resp.body.pointer("/data/info").and_then(Value::as_array) else {
                    break;
                };
                for item in info {
                    let hash = item.get("hash").and_then(Value::as_str).unwrap_or("");
                    let fileid = item.get("fileid").and_then(Value::as_i64).unwrap_or(0);
                    if !hash.is_empty() && fileid != 0 {
                        out.push((hash.to_string(), fileid));
                    }
                }
                if info.len() < 100 {
                    break;
                }
            }
            // A failed page leaves the list incomplete: report the failure so
            // callers neither replace the liked cache with partial data nor
            // treat a missing fileid as "already removed".
            Err(_) => return None,
        }
    }
    Some(out)
}

/// Wraps `liked_entries_raw` on the Tokio runtime (see the UI-side
/// `fetch_online_lyric` for why every kugou network call must be spawned onto
/// it). `None` when the fetch failed or the task was cancelled.
pub(crate) async fn fetch_liked_entries() -> Option<Vec<(String, i64)>> {
    crate::RUNTIME
        .spawn(liked_entries_raw())
        .await
        .ok()
        .flatten()
}

/// In-memory set of hashes currently in the user's KuGou liked-songs list.
/// Kept so the play-bar like button lights up instantly instead of waiting on
/// a network round-trip every time the track changes.
static LIKED_SET: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
/// Set once the cache has been populated; avoids re-fetching the whole liked
/// list for every online track when the user simply has an empty one.
pub(crate) static LIKED_SET_INIT: AtomicBool = AtomicBool::new(false);

/// The in-memory liked-hash set. `pub(crate)`: the UI-side like/unlike
/// actions patch it directly through the re-export in `crate::ui::kugou`.
pub(crate) fn liked_set() -> &'static RwLock<HashSet<String>> {
    LIKED_SET.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Fileids for the hashes currently in the liked list. The remove API
/// (`delete_songs`) needs the fileid, which only the paged list fetch
/// provides — caching it beside the hash set lets unlike skip that fetch
/// entirely in the common case instead of re-pulling up to 10 pages of full
/// song entities per unlike.
static LIKED_FILEIDS: OnceLock<RwLock<HashMap<String, i64>>> = OnceLock::new();

pub(crate) fn liked_fileids() -> &'static RwLock<HashMap<String, i64>> {
    LIKED_FILEIDS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// True when the cache holds `hash` as liked.
pub fn liked_set_contains(hash: &str) -> bool {
    liked_set()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(hash)
}

/// Serializes the initial liked-list load so concurrent `online_track_is_liked`
/// queries (e.g. rapid track changes before the cache is primed) wait for the
/// first full paged fetch to finish instead of each firing their own.
pub(crate) fn liked_load_lock() -> &'static tokio::sync::Mutex<()> {
    static LIKED_LOAD_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LIKED_LOAD_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Refreshes the in-memory liked-set from the service (best-effort). A failed
/// fetch leaves the previous cache and the init flag untouched, so the next
/// query retries instead of trusting an empty list (which would silently
/// unlike every track until restart).
pub(crate) async fn refresh_liked_set_from_service() {
    let Some(entries) = fetch_liked_entries().await else {
        return;
    };
    store_liked_entries(entries);
}

/// Replaces both liked caches (hash set + hash→fileid map) from one paged
/// fetch result.
pub(crate) fn store_liked_entries(entries: Vec<(String, i64)>) {
    let mut hashes = HashSet::with_capacity(entries.len());
    let mut fileids = HashMap::with_capacity(entries.len());
    for (hash, fileid) in entries {
        hashes.insert(hash.clone());
        fileids.insert(hash, fileid);
    }
    *liked_set().write().unwrap_or_else(|e| e.into_inner()) = hashes;
    *liked_fileids().write().unwrap_or_else(|e| e.into_inner()) = fileids;
    LIKED_SET_INIT.store(true, Ordering::Relaxed);
}
