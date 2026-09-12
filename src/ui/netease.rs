//! NetEase UI glue shared by the settings section, the search palette and
//! the playlists/discovery pages: JSON parsing into plain structs, QR image
//! rendering and queue/play helpers. Gated behind the `netease` cargo
//! feature; mirrors `ui::kugou`.

pub mod download;

pub use download::download_track_ui;

/// Localized "Download" label for the netease track rows. Defined once so the
/// i18n generator never sees a duplicate `NETEASE_DOWNLOAD` key.
pub fn download_label() -> cntp_i18n::I18nString {
    tr!("NETEASE_DOWNLOAD", "Download")
}

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use cntp_i18n::tr;
use gpui::{
    App, ClickEvent, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderImage,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smallvec::SmallVec;

use crate::{
    netease,
    playback::queue::{DataSource, OnlineIdentity, QueueItemData, QueueItemUIData},
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        components::{
            button::button,
            icons::{DOWNLOAD, STAR, STAR_FILLED, icon},
            managed_image::{ManagedImageKey, managed_image},
            tooltip::build_tooltip,
        },
        library::context_menus::{play_now, queue_item},
        lyrics::lrc::{LrcLine, parse_lrc},
        theme::Theme,
        util::format_duration,
    },
};

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

impl NeteaseTrackInfo {
    /// Right-hand label used in listings: "artist · m:ss".
    pub fn detail_label(&self) -> SharedString {
        let duration = format_duration(self.duration, false);
        if self.artist.is_empty() {
            duration.into()
        } else {
            format!("{} · {}", self.artist, duration).into()
        }
    }
}

/// One user (created or subscribed) playlist.
#[derive(Clone, Debug)]
pub struct NeteasePlaylistInfo {
    pub id: i64,
    pub name: SharedString,
    pub count: i64,
    pub cover_url: SharedString,
}

/// One chart entry from `/api/toplist`. The id doubles as a playlist id for
/// `playlist_track_all`.
#[derive(Clone, Debug)]
pub struct NeteaseRank {
    pub id: i64,
    pub name: SharedString,
    pub cover_url: SharedString,
    pub update_frequency: SharedString,
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
#[derive(Serialize, Deserialize, Clone)]
struct StreamMapEntry {
    url: String,
    id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    album_id: i64,
    #[serde(default)]
    fee: i64,
    #[serde(default)]
    duration: i64,
    #[serde(default)]
    cover_url: String,
}

const STREAM_MAP_CAP: usize = 200;

fn stream_map_path() -> PathBuf {
    crate::paths::data_dir().join("netease_stream_map.json")
}

fn stream_map() -> &'static Mutex<Vec<StreamMapEntry>> {
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

/// Bytes of the stream-map JSON last handed to the persistence task. Kept so
/// re-recording an unchanged registry (e.g. replaying the same song) skips
/// the disk write entirely instead of re-writing identical bytes.
static LAST_PERSISTED_STREAM_MAP: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();

/// Serializes stream-map persistence. Two overlapping truncate+write on the
/// same file can interleave into torn, unparseable JSON — which would drop
/// the whole registry on next launch — so every write takes this gate first.
/// tokio's mutex is fair (FIFO), so writers land in hand-off order and the
/// latest snapshot wins.
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
    // Off-thread and lock-free disk write; see the kugou twin for rationale.
    if let Some(json) = persist_stream_map_if_changed(json) {
        let path = stream_map_path();
        let gate = STREAM_MAP_WRITE_GATE.get_or_init(Default::default);
        crate::RUNTIME.spawn(async move {
            let _gate = gate.lock().await;
            if let Err(err) = tokio::fs::write(&path, &json).await {
                // Roll the baseline back so an identical later snapshot
                // retries instead of silently leaving the old file in place.
                if let Some(last) = LAST_PERSISTED_STREAM_MAP.get() {
                    last.lock().unwrap_or_else(|e| e.into_inner()).take();
                }
                tracing::warn!(%err, "failed to persist netease stream map");
            }
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

// ---------------------------------------------------------------------------
// JSON parsing (tolerant to the shape differences between the endpoints)
// ---------------------------------------------------------------------------

fn string_field(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(Value::String(s)) = value.get(*key)
            && !s.is_empty()
        {
            return s.clone();
        }
    }
    String::new()
}

fn i64_field(value: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        match value.get(*key) {
            Some(Value::Number(n)) => return n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => {
                if let Ok(parsed) = s.parse::<i64>() {
                    return parsed;
                }
            }
            _ => {}
        }
    }
    0
}

/// Artists: `ar[]` (cloudsearch / song detail) or `artists[]` (legacy shapes),
/// joined with " / ".
fn artist_field(value: &Value) -> String {
    for key in ["ar", "artists"] {
        if let Some(Value::Array(items)) = value.get(key) {
            let names: Vec<String> = items
                .iter()
                .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_string))
                .filter(|name| !name.is_empty())
                .collect();
            if !names.is_empty() {
                return names.join(" / ");
            }
        }
    }
    string_field(value, &["artist"])
}

fn album_field(value: &Value) -> String {
    for key in ["al", "album"] {
        match value.get(key) {
            Some(Value::Object(album)) => {
                let name = album.get("name").and_then(Value::as_str).unwrap_or("");
                if !name.is_empty() {
                    return name.to_string();
                }
            }
            Some(Value::String(s)) if !s.is_empty() => return s.clone(),
            _ => {}
        }
    }
    String::new()
}

/// Album-art URL: `al.picUrl` / `album.picUrl`. The playlist endpoints may
/// hand out `1.jpg`-style size placeholders (`?param=...` suffix form), which
/// load as-is; NetEase also accepts a `?param=WxH` parameter, appended when
/// absent to keep the images small.
fn cover_url_field(value: &Value) -> SharedString {
    let pick = |url: &str| -> Option<SharedString> {
        (!url.is_empty()).then(|| {
            if url.contains("?param=") {
                SharedString::from(url)
            } else {
                SharedString::from(format!("{url}?param=256y256"))
            }
        })
    };
    // song shapes: nested album object (`al` on songs, `album` on legacy)
    for key in ["al", "album"] {
        if let Some(pic_url) = value
            .get(key)
            .and_then(|album| album.get("picUrl"))
            .and_then(Value::as_str)
            && let Some(cover) = pick(pic_url)
        {
            return cover;
        }
    }
    // playlist/chart shapes: top-level cover field (verified live: the
    // toplist items carry `coverImgUrl` without a size parameter)
    for key in ["coverImgUrl", "picUrl", "coverUrl"] {
        if let Some(url) = value.get(key).and_then(Value::as_str)
            && let Some(cover) = pick(url)
        {
            return cover;
        }
    }
    SharedString::default()
}

/// Parses one song object; entries without an id are skipped.
fn parse_song(item: &Value) -> Option<NeteaseTrackInfo> {
    let id = i64_field(item, &["id"]);
    if id == 0 {
        return None;
    }
    Some(NeteaseTrackInfo {
        title: SharedString::from(string_field(item, &["name", "title"])),
        artist: SharedString::from(artist_field(item)),
        album: SharedString::from(album_field(item)),
        // `dt`/`duration` are milliseconds
        duration: i64_field(item, &["dt", "duration"]) / 1000,
        id,
        album_id: item
            .get("al")
            .or_else(|| item.get("album"))
            .and_then(|album| album.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        fee: i64_field(item, &["fee"]),
        cover_url: cover_url_field(item),
    })
}

/// Parses the song arrays of the search (`/result/songs`), song detail
/// (`/songs`), daily recommend (`/data/dailySongs`) and playlist
/// (`/songs`) endpoints.
pub fn parse_tracks(body: &Value, list_pointer: &str) -> Vec<NeteaseTrackInfo> {
    body.pointer(list_pointer)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_song).collect())
        .unwrap_or_default()
}

/// Parses the user playlist list (`/playlist` of user_playlist).
pub fn parse_playlists(body: &Value) -> Vec<NeteasePlaylistInfo> {
    body.pointer("/playlist")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = i64_field(item, &["id"]);
                    if id == 0 {
                        return None;
                    }
                    Some(NeteasePlaylistInfo {
                        id,
                        name: SharedString::from(string_field(item, &["name"])),
                        count: i64_field(item, &["trackCount"]),
                        cover_url: cover_url_field(item),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses the chart list (`/list` of toplist).
pub fn parse_ranks(body: &Value) -> Vec<NeteaseRank> {
    body.pointer("/list")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = i64_field(item, &["id"]);
                    if id == 0 {
                        return None;
                    }
                    Some(NeteaseRank {
                        id,
                        name: SharedString::from(string_field(item, &["name"])),
                        cover_url: cover_url_field(item),
                        update_frequency: SharedString::from(string_field(item, &[
                            "updateFrequency",
                        ])),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Extracts the first playable URL out of a `song_url` response
/// (`data[0].url`), together with whether it is only a trial clip
/// (`freeTrialInfo` present).
pub fn extract_song_url(body: &Value) -> Option<(String, bool)> {
    body.pointer("/data/0")
        .and_then(|entry| {
            entry
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(|url| {
                    let trial = entry.get("freeTrialInfo").is_some_and(|info| !info.is_null());
                    (url.to_string(), trial)
                })
        })
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

enum PlayIntent {
    Now,
    Queue,
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
    let levels: [&str; 2] = if quality == "standard" {
        [quality, ""]
    } else {
        [quality, "standard"]
    };
    for &level in levels.iter().filter(|l| !l.is_empty()) {
        if let Ok(resp) = client.song_url(id, level).await
            && let Some((url, _trial)) = extract_song_url(&resp.body)
        {
            return Some(url);
        }
    }
    None
}

async fn fetch_play_url(
    client: &netease::NeteaseClient,
    track: &NeteaseTrackInfo,
    quality: &str,
) -> Option<String> {
    fetch_stream_url(client, track.id, quality).await
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

/// Song ids whose play-URL fetch is currently in flight. Concurrent clicks on
/// the same song coalesce into the first fetch so the track can't be queued
/// twice while the (slow) URL request is still outstanding.
static PENDING_FETCHES: OnceLock<RwLock<HashSet<i64>>> = OnceLock::new();

fn pending_fetches() -> &'static RwLock<HashSet<i64>> {
    PENDING_FETCHES.get_or_init(|| RwLock::new(HashSet::new()))
}

fn play_track(cx: &mut App, track: &NeteaseTrackInfo, intent: PlayIntent) {
    tracing::info!(title = %track.title, id = track.id, "netease play_track: fetching play URL");
    let quality = cx
        .global::<SettingsGlobal>()
        .model
        .read(cx)
        .playback
        .netease_quality
        .as_str()
        .to_string();
    let track = track.clone();
    cx.spawn(async move |cx| {
        // Coalesce concurrent clicks on the same song: only the first click
        // fetches and queues; later clicks in the same window are dropped so
        // a double-click can't enqueue the track twice.
        if !pending_fetches()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(track.id)
        {
            return;
        }

        let client = crate::netease::shared_client();
        let fetch_track = track.clone();
        let request = crate::RUNTIME
            .spawn(async move { fetch_play_url(&client, &fetch_track, &quality).await })
            .await;

        let Some(url) = request.ok().flatten() else {
            pending_fetches()
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&track.id);
            tracing::warn!(id = track.id, "netease play_track: no playable URL in response");
            emit_toast(Toast::warning(tr!(
                "NETEASE_NO_URL",
                "NetEase returned no playable URL for this track"
            )));
            return;
        };
        tracing::info!(id = track.id, url = %url, "netease play_track: got play URL");

        // remember the URL → track mapping so the lyrics view and the play
        // bar can resolve lyrics / like state for the running HTTP stream
        remember_online_track(url.clone(), track.clone());

        cx.update(|cx| {
            // Re-clicking the same track must not pile up queue duplicates:
            // if an entry for this online track already exists, refresh its
            // (expiring) URL in place and jump to it. The stream map is
            // locked once for the whole pass instead of once per queue item
            // (each locking + scanning up to STREAM_MAP_CAP entries). Map
            // URLs are unique (see `remember_online_track`), so "an entry
            // with this url and id exists" matches the old per-item
            // `track_id_for_path` lookup. Nesting the map lock under the
            // queue lock is safe: nothing takes the queue lock while
            // holding the map lock.
            let queue_data = cx.global::<crate::ui::models::Models>().queue.read(cx).data.clone();
            let map = stream_map().lock().unwrap_or_else(|e| e.into_inner());
            let existing = queue_data
                .read()
                .expect("poisoned queue")
                .iter()
                .position(|item| {
                    let path = item.get_path();
                    if !crate::media::is_http_path(path) {
                        return false;
                    }
                    let path_str = path.to_string_lossy();
                    map.iter()
                        .any(|entry| entry.id == track.id && entry.url == path_str.as_ref())
                });

            if let Some(index) = existing {
                queue_data
                    .write()
                    .expect("poisoned queue")
                    [index]
                    .replace_path(PathBuf::from(url));

                if matches!(intent, PlayIntent::Now) {
                    cx.global::<crate::playback::interface::PlaybackInterface>().jump(index);
                }
                return;
            }

            let item = online_queue_item(cx, url, &track);

            match intent {
                PlayIntent::Now => play_now(cx, item),
                PlayIntent::Queue => queue_item(cx, item),
            }
        });

        pending_fetches()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&track.id);
    })
    .detach();
}

/// Fetches the play URL for `track` and starts playing it.
pub fn play_track_now(cx: &mut App, track: &NeteaseTrackInfo) {
    play_track(cx, track, PlayIntent::Now);
}

/// Fetches the play URL for `track` and appends it to the queue.
pub fn queue_track(cx: &mut App, track: &NeteaseTrackInfo) {
    play_track(cx, track, PlayIntent::Queue);
}

fn online_queue_item(cx: &mut App, url: String, track: &NeteaseTrackInfo) -> QueueItemData {
    QueueItemData::with_ui_data(
        cx,
        PathBuf::from(url),
        QueueItemUIData {
            album_id: None,
            name: Some(track.title.clone()),
            artist_name: Some(track.artist.clone()),
            source: DataSource::Metadata,
            duration: Some(track.duration),
            cover_url: (!track.cover_url.is_empty()).then(|| track.cover_url.clone()),
        },
    )
    .with_online_identity(OnlineIdentity::Netease { id: track.id })
}

// ---------------------------------------------------------------------------
// Liked (red-heart) songs
// ---------------------------------------------------------------------------

/// In-memory set of song ids currently in the user's NetEase liked-songs
/// list. Kept so the play-bar like button lights up instantly instead of
/// waiting on a network round-trip every time the track changes.
static LIKED_IDS: OnceLock<RwLock<HashSet<i64>>> = OnceLock::new();
/// Set once the cache has been populated; avoids re-fetching the whole liked
/// list for every online track when the user simply has an empty one.
static LIKED_SET_INIT: AtomicBool = AtomicBool::new(false);

fn liked_ids() -> &'static RwLock<HashSet<i64>> {
    LIKED_IDS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// True when the cache holds `id` as liked.
pub fn liked_set_contains(id: i64) -> bool {
    liked_ids()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&id)
}

/// Serializes the initial liked-list load so concurrent `online_track_is_liked`
/// queries (e.g. rapid track changes before the cache is primed) wait for the
/// first fetch to finish instead of each firing their own.
fn liked_load_lock() -> &'static tokio::sync::Mutex<()> {
    static LIKED_LOAD_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LIKED_LOAD_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Refreshes the in-memory liked-set from the service (best-effort). A failed
/// fetch leaves the previous cache and the init flag untouched, so the next
/// query retries instead of trusting an empty list (which would silently
/// unlike every track until restart).
async fn refresh_liked_set_from_service() {
    let client = crate::netease::shared_client();
    let Some(uid) = client.user_id() else {
        return;
    };
    let Some(ids) = crate::RUNTIME
        .spawn(async move { client.like_list(uid).await })
        .await
        .ok()
        .and_then(Result::ok)
    else {
        return;
    };
    *liked_ids().write().unwrap_or_else(|e| e.into_inner()) = ids.into_iter().collect();
    LIKED_SET_INIT.store(true, Ordering::Relaxed);
}

/// Fires a background refresh of the liked-set. Called at startup (when
/// logged in) so the play-bar star is immediately accurate.
pub fn prime_liked_cache() {
    crate::RUNTIME.spawn(async {
        // Same lock as the lazy load in `online_track_is_liked` so the two
        // entry points cannot double-fetch the whole liked list.
        let _guard = liked_load_lock().lock().await;
        refresh_liked_set_from_service().await;
    });
}

/// True when `id` is in the user's NetEase liked-songs list. Uses the cached
/// set once it has been primed; only falls back to a network refresh the
/// first time. Concurrent first queries coalesce into one fetch: the
/// latecomers wait on `liked_load_lock` and then read whatever the first
/// loader produced; a failed load leaves the init flag unset so a later
/// query retries.
pub async fn online_track_is_liked(id: i64) -> bool {
    if !LIKED_SET_INIT.load(Ordering::Relaxed) {
        let _guard = liked_load_lock().lock().await;
        // Re-check under the lock: the query that primed the cache may have
        // finished while we waited.
        if !LIKED_SET_INIT.load(Ordering::Relaxed) {
            refresh_liked_set_from_service().await;
        }
    }
    liked_set_contains(id)
}

fn require_login_toast() {
    emit_toast(Toast::warning(tr!(
        "NETEASE_LOGIN_REQUIRED_LIKE",
        "Log in to NetEase Cloud Music in Settings to like songs."
    )));
}

/// Likes (red-hearts) `track` and toasts the outcome. The request runs on
/// the Tokio runtime like every other netease call.
pub fn like_track(cx: &mut App, track: &NeteaseTrackInfo) {
    let client = crate::netease::shared_client();
    if !client.logged_in() {
        require_login_toast();
        return;
    }
    let id = track.id;
    cx.spawn(async move |cx| {
        let client = crate::netease::shared_client();
        let request = crate::RUNTIME.spawn(async move { client.like(id, true).await }).await;

        cx.update(|_cx| match request {
            Ok(Ok(_)) => {
                liked_ids()
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id);
                emit_toast(Toast::success(tr!(
                    "NETEASE_LIKE_ADDED",
                    "Added to your NetEase liked songs"
                )));
            }
            Ok(Err(err)) => {
                emit_toast(Toast::warning(tr!(
                    "NETEASE_LIKE_FAILED",
                    "Could not like track: {{err}}",
                    err = err.to_string()
                )));
            }
            Err(err) => {
                emit_toast(Toast::error(tr!(
                    "NETEASE_LIKE_TASK_FAILED",
                    "Like request failed: {{err}}",
                    err = err.to_string()
                )));
            }
        });
    })
    .detach();
}

/// Unlikes `track` and toasts the outcome.
pub fn unlike_track(cx: &mut App, track: &NeteaseTrackInfo) {
    let client = crate::netease::shared_client();
    if !client.logged_in() {
        require_login_toast();
        return;
    }
    let id = track.id;
    cx.spawn(async move |cx| {
        let client = crate::netease::shared_client();
        let request = crate::RUNTIME.spawn(async move { client.like(id, false).await }).await;

        cx.update(|_cx| match request {
            Ok(Ok(_)) => {
                liked_ids()
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                emit_toast(Toast::success(tr!(
                    "NETEASE_LIKE_REMOVED",
                    "Removed from your NetEase liked songs"
                )));
            }
            Ok(Err(err)) => {
                emit_toast(Toast::warning(tr!(
                    "NETEASE_LIKE_REMOVED_FAILED",
                    "Could not unlike track: {{err}}",
                    err = err.to_string()
                )));
            }
            Err(err) => {
                emit_toast(Toast::error(tr!(
                    "NETEASE_UNLIKE_TASK_FAILED",
                    "Unlike request failed: {{err}}",
                    err = err.to_string()
                )));
            }
        });
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Lyrics
// ---------------------------------------------------------------------------

/// A fetched NetEase lyric: the raw text (plain-text fallback of the lyrics
/// panel) plus the pre-parsed lines (word-level when YRC was available and
/// translations merged from `tlyric`).
#[derive(Clone)]
pub struct NeteaseLyric {
    pub content: String,
    pub lines: Vec<LrcLine>,
}

/// Merges line-timed translations into `lines` by nearest timestamp.
fn merge_translation(lines: &mut [LrcLine], translation: &str) {
    if translation.trim().is_empty() {
        return;
    }
    let Some(translated) = parse_lrc(translation) else {
        return;
    };
    for line in lines.iter_mut() {
        let best = translated
            .iter()
            .filter(|candidate| {
                candidate.time_ms.abs_diff(line.time_ms) <= 500
            })
            .min_by_key(|candidate| candidate.time_ms.abs_diff(line.time_ms));
        if let Some(best) = best
            && !best.text.is_empty()
        {
            line.translation = Some(best.text.clone());
        }
    }
}

/// Fetches the lyric for an online track via `lyric_new`: prefers the
/// word-level YRC karaoke payload (translations merged from `tlyric`),
/// falling back to plain LRC. Returns `Ok(None)` when the service has no
/// lyrics for the track.
///
/// The HTTP calls MUST run on the Tokio runtime (see `fetch_online_lyric` in
/// `ui::kugou` for why).
pub async fn fetch_online_lyric(track: &NeteaseTrackInfo) -> Result<Option<NeteaseLyric>, String> {
    let client = crate::netease::shared_client();
    let track = track.clone();

    crate::RUNTIME
        .spawn(async move {
            let response = client
                .lyric_new(track.id)
                .await
                .map_err(|err| err.to_string())?;

            let yrc = response
                .body
                .pointer("/yrc/lyric")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let lrc = response
                .body
                .pointer("/lrc/lyric")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let tlyric = response
                .body
                .pointer("/tlyric/lyric")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            if !yrc.trim().is_empty() {
                let mut lines = crate::ui::lyrics::yrc::parse_yrc(&yrc).unwrap_or_default();
                merge_translation(&mut lines, &tlyric);
                if !lines.is_empty() {
                    return Ok(Some(NeteaseLyric {
                        content: yrc,
                        lines,
                    }));
                }
            }

            if !lrc.trim().is_empty() {
                let mut lines = parse_lrc(&lrc).unwrap_or_default();
                merge_translation(&mut lines, &tlyric);
                if !lines.is_empty() {
                    return Ok(Some(NeteaseLyric {
                        content: lrc,
                        lines,
                    }));
                }
            }

            Ok(None)
        })
        .await
        .map_err(|err| err.to_string())?
}

// ---------------------------------------------------------------------------
// QR rendering + track row
// ---------------------------------------------------------------------------

/// Renders a URL as a black-on-white QR code image. Uses the same frame
/// construction as the album art pipeline.
pub fn build_qr_render_image(url: &str) -> anyhow::Result<Arc<RenderImage>> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    let mut image: image::RgbaImage = code
        .render::<image::Rgba<u8>>()
        .quiet_zone(true)
        .min_dimensions(320, 320)
        .build();

    crate::ui::components::managed_image::rgb_to_bgr(&mut image);

    let mut frames: SmallVec<[_; 1]> = SmallVec::new();
    frames.push(image::Frame::new(image));
    Ok(Arc::new(RenderImage::new(frames)))
}

/// Renders a QR code for the given QR login key, encoded from the login URL.
pub fn build_login_qr(key: &str) -> anyhow::Result<Arc<RenderImage>> {
    build_qr_render_image(&crate::netease::api::qr_login_url(key))
}

/// Shared online-track row used by the playlists and discovery pages. The
/// like/play/download callbacks carry the per-view semantics; the visual
/// skeleton is identical to the KuGou row.
pub(crate) fn netease_track_row<F1, F2, F3>(
    theme: Theme,
    track: &NeteaseTrackInfo,
    index: usize,
    id_prefix: &'static str,
    show_cover: bool,
    pad_minutes: bool,
    liked: bool,
    on_play: F1,
    on_like: F2,
    on_download: F3,
) -> impl IntoElement
where
    F1: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F2: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F3: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let title: SharedString = if track.title.is_empty() {
        tr!("UNKNOWN_TRACK").into()
    } else {
        track.title.clone()
    };
    let artist: SharedString = if track.artist.is_empty() {
        tr!("UNKNOWN_ARTIST").into()
    } else {
        track.artist.clone()
    };
    let detail: SharedString = if track.album.is_empty() {
        artist
    } else {
        SharedString::from(format!("{artist} · {}", track.album))
    };

    div()
        .id((id_prefix, index))
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(4.0))
        .py(px(8.0))
        .pl(px(6.0))
        .border_b_1()
        .border_color(theme.border_color)
        .cursor_pointer()
        .hover({
            let theme = theme.clone();
            move |this| this.bg(theme.queue_item_hover)
        })
        .on_click(on_play)
        .child(
            div()
                .text_xs()
                .text_color(theme.text_secondary)
                .w(px(28.0))
                .flex_shrink(0.0)
                .child((index + 1).to_string()),
        )
        .when(show_cover, |this| {
            this.child(
                // composite (name, index) id: zero per-frame allocation; the
                // image is scoped under this row's `.id((id_prefix, index))`
                managed_image(("thumb", index), ManagedImageKey::HttpCover(track.cover_url.clone()))
                    .thumb(),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink(1.0)
                .overflow_x_hidden()
                .gap(px(1.0))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .overflow_x_hidden()
                        .text_ellipsis()
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_secondary)
                        .overflow_x_hidden()
                        .text_ellipsis()
                        .child(detail),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_secondary)
                .ml_auto()
                .flex_shrink(0.0)
                .child(format_duration(track.duration, pad_minutes)),
        )
        .child(
            button()
                // composite ids scoped under the row's `.id((id_prefix, index))`
                .id(("like", index))
                .child(icon(if liked { STAR_FILLED } else { STAR }).size(px(14.0)))
                .text_color(if liked {
                    theme.liked_song
                } else {
                    theme.text_secondary
                })
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_like(event, window, cx);
                }),
        )
        .child(
            button()
                .id(("download", index))
                .child(icon(DOWNLOAD).size(px(14.0)))
                .text_color(theme.text_secondary)
                .tooltip(build_tooltip(download_label()))
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_download(event, window, cx);
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ranks_reads_top_level_cover_img_url() {
        // live-verified shape: toplist items carry a top-level `coverImgUrl`
        let body: Value = serde_json::json!({
            "list": [{
                "id": 19723756,
                "name": "飙升榜",
                "updateFrequency": "每天更新",
                "coverImgUrl": "https://p3.music.126.net/rIi7Qzy2i2Y_1QD7cd0MYA==/109951170048506929.jpg"
            }]
        });
        let ranks = parse_ranks(&body);
        assert_eq!(ranks.len(), 1);
        assert_eq!(ranks[0].name, "飙升榜");
        assert_eq!(
            ranks[0].cover_url,
            "https://p3.music.126.net/rIi7Qzy2i2Y_1QD7cd0MYA==/109951170048506929.jpg?param=256y256"
        );
    }

    #[test]
    fn parse_playlists_reads_cover_img_url() {
        let body: Value = serde_json::json!({
            "playlist": [{
                "id": 42,
                "name": "我喜欢的音乐",
                "trackCount": 10,
                "coverImgUrl": "https://p1.music.126.net/x/y.jpg?param=120y120"
            }]
        });
        let playlists = parse_playlists(&body);
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].cover_url, "https://p1.music.126.net/x/y.jpg?param=120y120");
    }

    #[test]
    fn parse_song_keeps_nested_album_pic() {
        let body: Value = serde_json::json!({
            "result": { "songs": [{
                "id": 186016, "name": "晴天",
                "ar": [{ "id": 6452, "name": "周杰伦" }],
                "al": { "id": 18918, "name": "叶惠美", "picUrl": "https://p2.music.126.net/z.jpg" },
                "dt": 269306, "fee": 8
            }]}
        });
        let tracks = parse_tracks(&body, "/result/songs");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].cover_url, "https://p2.music.126.net/z.jpg?param=256y256");
        assert_eq!(tracks[0].duration, 269);
        assert_eq!(tracks[0].artist, "周杰伦");
    }
}
