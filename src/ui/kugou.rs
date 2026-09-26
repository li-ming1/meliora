//! KuGou UI glue shared by the settings section, the search palette and the
//! playlists page: JSON parsing into plain structs, QR image rendering and
//! queue/play helpers. Gated behind the `kugou` cargo feature.

pub mod download;

pub use download::download_track_ui;

/// Localized "Download" label for the kugou track rows. Defined once so the
/// i18n generator never sees a duplicate `KUGOU_DOWNLOAD` key.
pub fn download_label() -> cntp_i18n::I18nString {
    tr!("KUGOU_DOWNLOAD", "Download")
}

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::ui::design::ICON_SM;
use cntp_i18n::tr;
use gpui::{
    App, ClickEvent, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderImage,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use serde_json::Value;
use smallvec::SmallVec;

use crate::{
    kugou,
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
        theme::Theme,
        util::format_duration,
    },
};

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

impl KugouTrackInfo {
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

/// One user playlist.
#[derive(Clone, Debug)]
pub struct KugouPlaylistInfo {
    pub global_collection_id: String,
    /// Numeric `listid` used by the `/v4/get_list_all_file` endpoint to fetch
    /// the songs of this playlist.
    pub listid: i64,
    pub name: SharedString,
    pub count: i64,
}

/// One rank entry from `/ocean/v6/rank/list`. Fetched with `withsong: 0`,
/// so entries carry no song previews — just the name and cover.
#[derive(Clone, Debug)]
pub struct KugouRank {
    pub rankid: i64,
    pub name: SharedString,
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

/// Fetches the eligible lyric for an online track via `search_lyric` →
/// `lyric_krc` (karaoke, with per-word timing and translations), falling back
/// to `lyric_lrc` when the karaoke payload is missing or decrypts to nothing.
/// Returns the lyric source that should be shown, or `Ok(None)` when the
/// service has no lyrics for the track.
///
/// The HTTP calls MUST run on the Tokio runtime: reqwest's DNS resolver
/// panics ("there is no reactor running") when polled outside of one, e.g.
/// from the gpui main-thread executor. That panic crashed the whole app, so
/// the request is wrapped in `RUNTIME::spawn` like every other kugou call.
pub async fn fetch_online_lyric(track: &KugouTrackInfo) -> Result<Option<OnlineLyric>, String> {
    let client = kugou::shared_client();
    let track = track.clone();

    crate::RUNTIME
        .spawn(async move {
            let keyword = if track.artist.is_empty() {
                track.title.to_string()
            } else {
                format!("{} {}", track.title, track.artist)
            };

            // search the lyrics service for a candidate
            let search = client
                .search_lyric(&track.hash, &keyword, track.duration)
                .await
                .map_err(|err| err.to_string())?;

            let candidate = search
                .body
                .pointer("/candidates")
                .and_then(Value::as_array)
                .and_then(|items| {
                    items.iter().find_map(|candidate| {
                        let id = candidate.get("id").and_then(Value::as_str)?;
                        let accesskey = candidate.get("accesskey").and_then(Value::as_str)?;
                        Some((id.to_string(), accesskey.to_string()))
                    })
                });

            let Some((id, accesskey)) = candidate else {
                return Ok(None);
            };

            // prefer KRC: decrypt here, then hand the plain text up so the
            // UI side never has to decrypt again
            let krc = client
                .lyric_krc(&id, &accesskey)
                .await
                .map_err(|err| err.to_string())?;
            if let Some(plain) = crate::ui::lyrics::krc::decrypt_krc(&krc) {
                return Ok(Some(OnlineLyric::Krc(plain)));
            }

            let lrc = client
                .lyric_lrc(&id, &accesskey)
                .await
                .map_err(|err| err.to_string())?;
            Ok((!lrc.is_empty()).then_some(OnlineLyric::Lrc(lrc)))
        })
        .await
        .map_err(|err| err.to_string())?
}

/// A fetched online lyric: plain LRC text or decrypted KRC plain text.
#[derive(Clone)]
pub enum OnlineLyric {
    Lrc(String),
    Krc(String),
}

impl OnlineLyric {
    /// Human-readable text for the plain-text fallback branch of the lyrics
    /// panel (used when no timed line could be parsed).
    pub fn describe(&self) -> String {
        match self {
            OnlineLyric::Lrc(text) | OnlineLyric::Krc(text) => text.clone(),
        }
    }
}

/// Claims today's free KuGou VIP in the background (best-effort, fire and
/// forget). Called after login so the account can play full-length tracks; a
/// no-op when not logged in. Runs on the Tokio runtime like every other
/// kugou call.
pub fn claim_daily_vip_async() {
    let client = kugou::shared_client();
    crate::RUNTIME.spawn(async move {
        use crate::kugou::api::VipClaimOutcome;
        match client.ensure_daily_vip().await {
            VipClaimOutcome::Claimed => {
                tracing::info!("kugou: daily VIP claimed");
                emit_toast(Toast::success(tr!("KUGOU_VIP_CLAIMED", "Daily VIP claimed")));
            }
            VipClaimOutcome::AlreadyClaimed => {
                tracing::debug!("kugou: VIP already claimed for today");
            }
            VipClaimOutcome::NotLoggedIn => {
                tracing::debug!("kugou: skipping daily VIP claim (not logged in)");
            }
            VipClaimOutcome::Failed { reason } => {
                tracing::warn!("kugou: daily VIP claim failed: {reason}");
                // The daily claim is what keeps membership alive; its failing
                // (usually an expired login) is exactly what the user needs to
                // know about instead of discovering a dead VIP line later.
                emit_toast(Toast::warning(tr!(
                    "KUGOU_VIP_CLAIM_FAILED",
                    "Daily VIP claim failed"
                )));
            }
        }
    });
}

/// Builds a short human-readable VIP status line from the cached
/// `/v1/get_union_vip` payload, e.g. "VIP · till 2026-08-24" or "No VIP".
/// Falls back to "No VIP" when there is no cached detail yet.
pub fn vip_status_line() -> SharedString {
    let Some(detail) = kugou::shared_client().vip_detail() else {
        return "No VIP".into();
    };

    // Active products live under `data.busi_vip[]`; the top-level `is_vip`
    // on this endpoint stays 0 even while `busi_vip` entries are active, so
    // membership is decided by scanning the product array.
    let products = detail
        .pointer("/data/busi_vip")
        .or_else(|| detail.get("busi_vip"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut active = products
        .iter()
        .filter(|item| item.get("is_vip").and_then(Value::as_i64) == Some(1))
        .collect::<Vec<_>>();
    if active.is_empty() {
        return "No VIP".into();
    }

    // Prefer the "concept" (svip) product for the expiry date, falling back
    // to the first active one.
    active.sort_by(|a, b| {
        let a_svip = a.get("product_type").and_then(Value::as_str) == Some("svip");
        let b_svip = b.get("product_type").and_then(Value::as_str) == Some("svip");
        b_svip.cmp(&a_svip)
    });

    let end = active
        .first()
        .and_then(|item| item.get("vip_end_time").and_then(Value::as_str))
        .map(str::to_string);

    match end {
        Some(end) => SharedString::from(format!("VIP · till {end}")),
        None => "VIP".into(),
    }
}

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

/// Artists come back either as a plain string (`Singer` on the web search
/// endpoint) or as an array of objects, depending on the endpoint.
fn artist_field(value: &Value) -> String {
    match value
        .get("Artist")
        .or_else(|| value.get("singer"))
        .or_else(|| value.get("singerinfo"))
        .or_else(|| value.get("author_name"))
    {
        Some(Value::String(s)) if !s.is_empty() => return s.clone(),
        Some(Value::Array(items)) => {
            let names: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.get("ArtistName")
                        .or_else(|| item.get("name"))
                        .or_else(|| item.get("Name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            if !names.is_empty() {
                return names.join(", ");
            }
        }
        _ => {}
    }
    string_field(value, &["Singer", "SingerName", "singer_name", "artist"])
}

fn title_field(value: &Value) -> String {
    let mut title = string_field(
        value,
        &["SongName", "songname", "OriSongName", "name", "Name"],
    );
    if title.is_empty() {
        let filename = string_field(value, &["filename", "FileName"]);
        if let Some(stripped) = filename.strip_suffix(".mp3") {
            title = stripped.to_string();
        }
    }
    title
}

/// Album name. Search results carry it as a plain `AlbumName` string, while
/// the playlist (`/v4`) endpoint nests it inside an `albuminfo` object.
fn album_field(value: &Value) -> String {
    if let Some(info) = value.get("albuminfo").and_then(Value::as_object)
        && let Some(name) = info.get("name").and_then(Value::as_str)
        && !name.is_empty()
    {
        return name.to_string();
    }
    string_field(value, &["AlbumName", "album_name"])
}

fn duration_field(value: &Value) -> i64 {
    let duration = i64_field(value, &["Duration", "duration"]);
    if duration == 0 {
        // some endpoints report milliseconds instead of seconds
        let timelength = i64_field(value, &["Timelength", "timelength", "timelen"]);
        if timelength > 0 {
            return timelength / 1000;
        }
    }
    duration
}

/// Album-art URL from the track record. KuGou CDN links carry a literal
/// `{size}` placeholder (e.g. `http://imge.kugou.com/stdmusic/{size}/...`),
/// which we substitute with a fixed width so the image loads directly.
fn cover_url_field(value: &Value) -> SharedString {
    let raw = string_field(value, &["Image", "img", "cover", "album_pic", "pic"]);
    if raw.is_empty() {
        return SharedString::default();
    }
    let sized = raw.replace("{size}", "256");
    if sized == raw {
        return SharedString::from(sized);
    }
    SharedString::from(sized)
}

/// Parses the track arrays of the search (`/data/lists`) and playlist
/// (`/data/songs`) endpoints. Entries without a hash are skipped.
pub fn parse_tracks(body: &Value, list_pointer: &str) -> Vec<KugouTrackInfo> {
    body.pointer(list_pointer)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let hash = string_field(item, &["FileHash", "hash"]);
                    if hash.is_empty() {
                        return None;
                    }
                    Some(KugouTrackInfo {
                        title: SharedString::from(title_field(item)),
                        artist: SharedString::from(artist_field(item)),
                        album: SharedString::from(album_field(item)),
                        duration: duration_field(item),
                        hash,
                        mix_song_id: i64_field(
                            item,
                            &[
                                "MixSongID",
                                "mixsong_id",
                                "album_audio_id",
                                "SongID",
                                "mixsongid",
                            ],
                        ),
                        album_id: i64_field(item, &["AlbumID", "album_id"]),
                        cover_url: cover_url_field(item),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses the rank list (`/data/info`). Entries without a rankid are skipped.
pub fn parse_ranks(body: &Value) -> Vec<KugouRank> {
    body.pointer("/data/info")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let rankid = i64_field(item, &["rankid"]);
                    if rankid == 0 {
                        return None;
                    }
                    // the rank endpoint ships covers under `img_9` with a
                    // `{size}` placeholder
                    let raw_cover = string_field(item, &["img_9", "album_img_9"]);
                    let cover_url = if raw_cover.is_empty() {
                        cover_url_field(item)
                    } else {
                        SharedString::from(raw_cover.replace("{size}", "256"))
                    };
                    Some(KugouRank {
                        rankid,
                        name: SharedString::from(string_field(item, &["rankname", "name"])),
                        cover_url,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses one track object, tolerating the schema variations across search,
/// playlist and rank endpoints. Rank audio data lives under `deprecated`
/// (hash + duration in ms), the cover under `album_info`.
fn parse_track(item: &Value) -> KugouTrackInfo {
    let audio = item
        .get("deprecated")
        .cloned()
        .unwrap_or_else(|| item.clone());
    let album_cover = item
        .pointer("/album_info/sizable_cover")
        .or_else(|| item.get("sizable_cover"))
        .and_then(Value::as_str)
        // some endpoints leave a {size} placeholder in the cover path
        .map(|url| SharedString::from(url.replace("{size}", "256")))
        .unwrap_or_default();
    // duration: seconds on search/playlist endpoints; ranks ship
    // `deprecated.duration` in ms, the daily recommend `time_length` in s
    let duration = {
        let secs = duration_field(item);
        if secs > 0 {
            secs
        } else {
            let deprecated_ms = i64_field(&audio, &["Duration", "duration"]);
            if deprecated_ms > 0 {
                deprecated_ms / 1000
            } else {
                i64_field(item, &["time_length"])
            }
        }
    };
    let artist = {
        let direct = artist_field(item);
        if !direct.is_empty() {
            direct
        } else {
            item.get("authors")
                .and_then(Value::as_array)
                .map(|authors| {
                    authors
                        .iter()
                        .filter_map(|a| a.get("author_name").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .unwrap_or_default()
        }
    };
    KugouTrackInfo {
        title: SharedString::from(title_field(item)),
        artist: SharedString::from(artist),
        album: SharedString::from(album_field(item)),
        duration,
        hash: string_field(&audio, &["FileHash", "hash"]),
        mix_song_id: i64_field(
            item,
            &[
                "MixSongID",
                "mixsong_id",
                "album_audio_id",
                "SongID",
                "mixsongid",
            ],
        ),
        album_id: i64_field(item, &["AlbumID", "album_id"]),
        cover_url: if album_cover.is_empty() {
            cover_url_field(&audio)
        } else {
            album_cover
        },
    }
}

/// Parses the songs of one rank (`/data/songlist`).
pub fn parse_rank_tracks(body: &Value) -> Vec<KugouTrackInfo> {
    body.pointer("/data/songlist")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let track = parse_track(item);
                    (!track.hash.is_empty()).then_some(track)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses the daily recommend tracks (`/data/song_list`).
pub fn parse_recommend_tracks(body: &Value) -> Vec<KugouTrackInfo> {
    body.pointer("/data/song_list")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let track = parse_track(item);
                    (!track.hash.is_empty()).then_some(track)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses the user playlist list (`/data/info`).
pub fn parse_playlists(body: &Value) -> Vec<KugouPlaylistInfo> {
    body.pointer("/data/info")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = match item.get("global_collection_id") {
                        Some(Value::String(s)) if !s.is_empty() => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        _ => string_field(item, &["global_collection_id", "specialid"]),
                    };
                    if id.is_empty() {
                        return None;
                    }
                    Some(KugouPlaylistInfo {
                        global_collection_id: id,
                        listid: i64_field(item, &["listid", "list_create_listid"]),
                        name: SharedString::from(string_field(item, &["name", "Name"])),
                        count: i64_field(item, &["count", "total", "m_count"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// KuGou stream URLs embed the mixsong id as `_mx<digits>_` (e.g.
/// `..._pi2_mx29048099_qu128_...`). The id is stable across differently-signed
/// URLs for the same track, so it identifies a queued online track reliably.
fn mixsongid_in_url(url: &str) -> Option<i64> {
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

enum PlayIntent {
    Now,
    Queue,
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

async fn fetch_play_url(
    client: &kugou::KugouClient,
    track: &KugouTrackInfo,
    quality: &str,
) -> Option<String> {
    fetch_stream_url(
        client,
        &track.hash,
        track.mix_song_id,
        track.album_id,
        quality,
    )
    .await
}

/// Mix-song ids whose play-URL fetch is currently in flight, plus ids whose
/// fetch recently succeeded (tagged with the intent it served). Concurrent
/// clicks on the same song coalesce into the first fetch so the track can't
/// be queued twice while the (slow) URL request is still outstanding, and a
/// short same-intent cooldown after a success keeps a click burst from
/// running the full fetch + open churn several times — GPUI delivers one
/// click event per click of a multi-click sequence, so a double-click is
/// two `play_track` calls a few hundred ms apart.
struct PlayFetchDedup {
    in_flight: HashSet<i64>,
    recent: HashMap<i64, (u8, Instant)>,
}

/// How long a successful fetch suppresses an identical-intent re-request.
const FETCH_COOLDOWN: Duration = Duration::from_millis(800);

static PENDING_FETCHES: OnceLock<RwLock<PlayFetchDedup>> = OnceLock::new();

fn pending_fetches() -> &'static RwLock<PlayFetchDedup> {
    PENDING_FETCHES.get_or_init(|| {
        RwLock::new(PlayFetchDedup {
            in_flight: HashSet::new(),
            recent: HashMap::new(),
        })
    })
}

fn play_track(cx: &mut App, track: &KugouTrackInfo, intent: PlayIntent) {
    let quality = cx
        .global::<SettingsGlobal>()
        .model
        .read(cx)
        .playback
        .online_quality
        .as_str()
        .to_string();
    let track = track.clone();
    cx.spawn(async move |cx| {
        // Coalesce concurrent clicks on the same song: only the first click
        // fetches and queues; later clicks in the same window are dropped so
        // a double-click can't enqueue the track twice. The intent tag keeps
        // the cooldown from swallowing a deliberate play right after a
        // queue-add of the same song.
        let intent_key = matches!(intent, PlayIntent::Now) as u8;
        {
            let mut dedup = pending_fetches().write().unwrap_or_else(|e| e.into_inner());
            dedup
                .recent
                .retain(|_, (_, at)| at.elapsed() < FETCH_COOLDOWN);
            let duplicate = dedup
                .recent
                .get(&track.mix_song_id)
                .is_some_and(|&(seen_intent, _)| seen_intent == intent_key)
                || !dedup.in_flight.insert(track.mix_song_id);
            if duplicate {
                return;
            }
        }
        tracing::info!(
            title = %track.title,
            hash = %track.hash,
            "kugou play_track: fetching play URL"
        );

        let client = kugou::shared_client();
        let fetch_track = track.clone();
        let request = crate::RUNTIME
            .spawn(async move { fetch_play_url(&client, &fetch_track, &quality).await })
            .await;

        let Some(url) = request.ok().flatten() else {
            // Clear the in-flight tag only: a failed fetch must stay
            // retryable on the next click.
            pending_fetches()
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .in_flight
                .remove(&track.mix_song_id);
            tracing::warn!(hash = %track.hash, "kugou play_track: no playable URL in response");
            emit_toast(Toast::warning(tr!(
                "KUGOU_NO_URL",
                "KuGou returned no playable URL for this track"
            )));
            return;
        };
        tracing::info!(
            hash = %track.hash,
            url = %url,
            "kugou play_track: got play URL"
        );

        // remember the URL → track mapping so the lyrics view can resolve
        // lyrics for the running HTTP stream
        remember_online_track(url.clone(), track.clone());

        cx.update(|cx| {
            // Re-clicking the same track must not pile up queue duplicates:
            // if an entry for this online track already exists, refresh its
            // (expiring) URL in place and jump to it.
            let queue_data = cx
                .global::<crate::ui::models::Models>()
                .queue
                .read(cx)
                .data
                .clone();
            let existing = queue_data
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .position(|item| {
                    crate::media::is_http_path(item.get_path())
                        && mixsongid_in_url(&item.get_path().to_string_lossy())
                            .is_some_and(|id| id == track.mix_song_id)
                });

            if let Some(index) = existing {
                // Re-check under the write lock (the queue may have changed
                // since the read above): skip the refresh if the item is gone
                // instead of panicking.
                let mut guard = queue_data.write().unwrap_or_else(|e| e.into_inner());
                let replaced = Arc::make_mut(&mut guard)
                    .get_mut(index)
                    .map(|item| item.replace_path(PathBuf::from(url)))
                    .is_some();
                if !replaced {
                    tracing::warn!(
                        index,
                        "kugou play_track: queue item gone before URL refresh"
                    );
                }

                if matches!(intent, PlayIntent::Now) {
                    cx.global::<crate::playback::interface::PlaybackInterface>()
                        .jump(index);
                }
                return;
            }

            let item = online_queue_item(cx, url, &track);

            match intent {
                PlayIntent::Now => play_now(cx, item),
                PlayIntent::Queue => queue_item(cx, item),
            }
        });

        {
            let mut dedup = pending_fetches().write().unwrap_or_else(|e| e.into_inner());
            dedup.in_flight.remove(&track.mix_song_id);
            dedup
                .recent
                .insert(track.mix_song_id, (intent_key, Instant::now()));
        }
    })
    .detach();
}

/// Fetches the play URL for `track` and starts playing it.
pub fn play_track_now(cx: &mut App, track: &KugouTrackInfo) {
    play_track(cx, track, PlayIntent::Now);
}

fn online_queue_item(cx: &mut App, url: String, track: &KugouTrackInfo) -> QueueItemData {
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
    .with_online_identity(OnlineIdentity::Kugou {
        hash: track.hash.clone(),
        mix_song_id: track.mix_song_id,
        album_id: track.album_id,
    })
}

/// Adds `track` to the KuGou "liked songs" playlist (listid 2) and toasts
/// the outcome. The request runs on the Tokio runtime like every other
/// kugou call.
pub fn like_track(cx: &mut App, track: &KugouTrackInfo) {
    let track = track.clone();
    cx.spawn(async move |cx| {
        let client = kugou::shared_client();
        let name = track.title.to_string();
        let hash = track.hash.clone();
        let album_id = track.album_id;
        let mixsongid = track.mix_song_id;
        let request = crate::RUNTIME
            .spawn(async move {
                client
                    .playlist_add_songs(2, &[(name, hash, album_id, mixsongid)])
                    .await
            })
            .await;

        cx.update(|_cx| match request {
            Ok(Ok(response)) => {
                let error_code = response
                    .body
                    .get("error_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                let status = response
                    .body
                    .get("status")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if error_code == 0 && status == 1 {
                    liked_set()
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(track.hash.clone());
                    emit_toast(Toast::success(tr!(
                        "KUGOU_LIKE_ADDED",
                        "Added to your KuGou liked songs"
                    )));
                } else {
                    let err = response
                        .body
                        .get("error_msg")
                        .or_else(|| response.body.get("errmsg"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string();
                    emit_toast(Toast::warning(tr!(
                        "KUGOU_LIKE_FAILED",
                        "Could not like track: {{err}}",
                        err = err
                    )));
                }
            }
            Ok(Err(err)) => {
                emit_toast(Toast::warning(tr!(
                    "KUGOU_LIKE_FAILED",
                    err = err.to_string()
                )));
            }
            Err(err) => {
                emit_toast(Toast::error(tr!(
                    "KUGOU_LIKE_TASK_FAILED",
                    "Like request failed: {{err}}",
                    err = err.to_string()
                )));
            }
        });
    })
    .detach();
}

/// Fetches the play URL for `track` and appends it to the queue.
pub fn queue_track(cx: &mut App, track: &KugouTrackInfo) {
    play_track(cx, track, PlayIntent::Queue);
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

/// Wraps `liked_entries_raw` on the Tokio runtime (see `fetch_online_lyric`
/// for why every kugou network call must be spawned onto it). `None` when the
/// fetch failed or the task was cancelled.
async fn fetch_liked_entries() -> Option<Vec<(String, i64)>> {
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
static LIKED_SET_INIT: AtomicBool = AtomicBool::new(false);

fn liked_set() -> &'static RwLock<HashSet<String>> {
    LIKED_SET.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Fileids for the hashes currently in the liked list. The remove API
/// (`delete_songs`) needs the fileid, which only the paged list fetch
/// provides — caching it beside the hash set lets unlike skip that fetch
/// entirely in the common case instead of re-pulling up to 10 pages of full
/// song entities per unlike.
static LIKED_FILEIDS: OnceLock<RwLock<HashMap<String, i64>>> = OnceLock::new();

fn liked_fileids() -> &'static RwLock<HashMap<String, i64>> {
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
fn liked_load_lock() -> &'static tokio::sync::Mutex<()> {
    static LIKED_LOAD_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LIKED_LOAD_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Refreshes the in-memory liked-set from the service (best-effort). A failed
/// fetch leaves the previous cache and the init flag untouched, so the next
/// query retries instead of trusting an empty list (which would silently
/// unlike every track until restart).
async fn refresh_liked_set_from_service() {
    let Some(entries) = fetch_liked_entries().await else {
        return;
    };
    store_liked_entries(entries);
}

/// Replaces both liked caches (hash set + hash→fileid map) from one paged
/// fetch result.
fn store_liked_entries(entries: Vec<(String, i64)>) {
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

/// Fires a background refresh of the liked-set. Called when the user opens
/// the KuGou "liked songs" playlist so the play-bar star is immediately
/// accurate for those tracks.
pub fn prime_liked_cache() {
    crate::RUNTIME.spawn(async {
        // Same lock as the lazy load in `online_track_is_liked` so the two
        // entry points cannot double-fetch the whole liked list.
        let _guard = liked_load_lock().lock().await;
        refresh_liked_set_from_service().await;
    });
}

/// True when `hash` is already in the user's KuGou liked-songs list. Uses the
/// cached set once it has been primed; only falls back to a network refresh
/// the first time. Concurrent first queries coalesce into one fetch: the
/// latecomers wait on `liked_load_lock` and then read whatever the first
/// loader produced; a failed load leaves the init flag unset so a later
/// query retries.
pub async fn online_track_is_liked(hash: &str) -> bool {
    let hash = hash.to_string();
    if !LIKED_SET_INIT.load(Ordering::Relaxed) {
        let _guard = liked_load_lock().lock().await;
        // Re-check under the lock: the query that primed the cache may have
        // finished while we waited.
        if !LIKED_SET_INIT.load(Ordering::Relaxed) {
            refresh_liked_set_from_service().await;
        }
    }
    liked_set_contains(&hash)
}

/// Removes `track` from the user's KuGou liked-songs list and toasts the
/// outcome. The request runs on the Tokio runtime like every other kugou
/// call.
pub fn unlike_track(cx: &mut App, track: &KugouTrackInfo) {
    let track = track.clone();
    cx.spawn(async move |cx| {
        let client = kugou::shared_client();
        let hash = track.hash.clone();
        // The fileid usually sits in the cache primed by the last full liked
        // fetch; only a miss (like made on another device since then, or the
        // cache never primed) pays for a fresh paged fetch.
        let fileid = match liked_fileids()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&hash)
            .copied()
        {
            Some(fid) => Some(fid),
            None => match fetch_liked_entries().await {
                Some(entries) => {
                    let found = entries.iter().find(|(h, _)| *h == hash).map(|(_, f)| *f);
                    store_liked_entries(entries);
                    found
                }
                None => None,
            },
        };

        let request = match fileid {
            Some(fid) => crate::RUNTIME
                .spawn(async move { client.playlist_remove_songs(2, &[fid]).await })
                .await
                .map(|result| result.map(|_| true)),
            // Not found within the fetched pages (or the fetch itself failed
            // and produced no candidate); treated as a successful removal.
            // The server may still hold the like when the list exceeds the
            // 10-page fetch window, so leave a trace in the log.
            None => {
                tracing::warn!(
                    hash = %hash,
                    "kugou unlike: fileid not found in fetched liked pages; \
                     treating as already removed (cache may be incomplete)"
                );
                Ok(Ok(true))
            }
        };

        cx.update(|_cx| match request {
            Ok(Ok(_)) => {
                liked_set()
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&hash);
                liked_fileids()
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&hash);
                emit_toast(Toast::success(tr!(
                    "KUGOU_LIKE_REMOVED",
                    "Removed from your KuGou liked songs"
                )))
            }
            Ok(Err(err)) => emit_toast(Toast::warning(tr!(
                "KUGOU_LIKE_REMOVED_FAILED",
                "Could not unlike track: {{err}}",
                err = err.to_string()
            ))),
            Err(err) => emit_toast(Toast::error(tr!(
                "KUGOU_UNLIKE_TASK_FAILED",
                "Unlike request failed: {{err}}",
                err = err.to_string()
            ))),
        });
    })
    .detach();
}

/// Renders the login URL as a black-on-white QR code image. Uses the same
/// frame construction as the album art pipeline.
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
    build_qr_render_image(&kugou::api::qr_login_url(key))
}

/// Shared online-track row used by both the playlists page and the discovery
/// (ranks / daily recommend) page. The like/play/download callbacks carry the
/// per-view semantics; the visual skeleton is identical.
pub(crate) fn kugou_track_row<F1, F2, F3>(
    theme: Theme,
    track: &KugouTrackInfo,
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
        .on_click(move |event, window, cx| {
            // GPUI delivers one click event per click of a multi-click
            // sequence; only the first click starts playback so a
            // double-click doesn't run the fetch + open churn twice.
            if event.click_count() > 1 {
                return;
            }
            on_play(event, window, cx)
        })
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
                managed_image(
                    ("thumb", index),
                    ManagedImageKey::HttpCover(track.cover_url.clone()),
                )
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
                .child(icon(if liked { STAR_FILLED } else { STAR }).size(ICON_SM))
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
                .child(icon(DOWNLOAD).size(ICON_SM))
                .text_color(theme.text_secondary)
                .tooltip(build_tooltip(download_label()))
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_download(event, window, cx);
                }),
        )
}
