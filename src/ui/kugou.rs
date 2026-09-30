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
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock, atomic::Ordering},
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use gpui::{App, RenderImage, SharedString};
use serde_json::Value;
use smallvec::SmallVec;

use crate::{
    kugou,
    playback::queue::{DataSource, OnlineIdentity, QueueItemData, QueueItemUIData},
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        library::context_menus::{play_now, queue_item},
        online_track_row::OnlineTrackDisplay,
    },
};

// ①步下沉：流注册表（stream map）、播放 URL 获取与 liked 缓存已搬至
// `crate::online_sources::kugou`（A-1 第①步，playback/stats 不再反向依赖
// crate::ui），此处按原可见性再导出，保证 ui 侧全部既有调用点零改动；
// ②步收编（trait 化）时清理这些再导出。
pub use crate::online_sources::kugou::{
    KugouTrackInfo, extract_song_url, fetch_stream_url, liked_set_contains,
    online_track_matching_path, remember_online_track,
};
pub(crate) use crate::online_sources::kugou::{
    LIKED_SET_INIT, fetch_liked_entries, liked_fileids, liked_load_lock, liked_set,
    mixsongid_in_url, refresh_liked_set_from_service, store_liked_entries,
};

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

/// Search keyword for the lyrics service: "title artist", or just the title
/// when the track has no artist.
fn lyric_search_keyword(track: &KugouTrackInfo) -> String {
    if track.artist.is_empty() {
        track.title.to_string()
    } else {
        format!("{} {}", track.title, track.artist)
    }
}

/// `(id, accesskey)` of the first `/candidates` entry that carries both
/// fields, or `None` when the service has no usable candidate.
fn first_lyric_candidate(search_body: &Value) -> Option<(String, String)> {
    search_body
        .pointer("/candidates")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|candidate| {
                let id = candidate.get("id").and_then(Value::as_str)?;
                let accesskey = candidate.get("accesskey").and_then(Value::as_str)?;
                Some((id.to_string(), accesskey.to_string()))
            })
        })
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
            let keyword = lyric_search_keyword(&track);

            // search the lyrics service for a candidate
            let search = client
                .search_lyric(&track.hash, &keyword, track.duration)
                .await
                .map_err(|err| err.to_string())?;

            let Some((id, accesskey)) = first_lyric_candidate(&search.body) else {
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
                emit_toast(Toast::success(tr!(
                    "KUGOU_VIP_CLAIMED",
                    "Daily VIP claimed"
                )));
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

    let active: Vec<_> = products
        .iter()
        .filter(|item| item.get("is_vip").and_then(Value::as_i64) == Some(1))
        .collect();
    if active.is_empty() {
        return "No VIP".into();
    }

    // Prefer the "concept" (svip) product for the expiry date, falling back
    // to the first active one.
    let preferred = active
        .iter()
        .find(|item| item.get("product_type").and_then(Value::as_str) == Some("svip"))
        .unwrap_or(&active[0]);
    let end = preferred
        .get("vip_end_time")
        .and_then(Value::as_str)
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
    SharedString::from(raw.replace("{size}", "256"))
}

/// Candidate key spellings of the mix-song id across the search, playlist and
/// rank endpoints (PascalCase legacy vs snake_case JSON).
const MIX_SONG_ID_KEYS: &[&str] = &[
    "MixSongID",
    "mixsong_id",
    "album_audio_id",
    "SongID",
    "mixsongid",
];
const ALBUM_ID_KEYS: &[&str] = &["AlbumID", "album_id"];

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
                        mix_song_id: i64_field(item, MIX_SONG_ID_KEYS),
                        album_id: i64_field(item, ALBUM_ID_KEYS),
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
        mix_song_id: i64_field(item, MIX_SONG_ID_KEYS),
        album_id: i64_field(item, ALBUM_ID_KEYS),
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

enum PlayIntent {
    Now,
    Queue,
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
        //
        // Bind the cache lookup to a local first: as a `match` scrutinee the
        // RwLock guard temporary would live for the whole match — across the
        // `None` arm's network `.await` — starving like-state writers for the
        // entire fetch (clippy::await_holding_lock).
        let cached = liked_fileids()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&hash)
            .copied();
        let fileid = match cached {
            Some(fid) => Some(fid),
            None => fetch_liked_entries().await.and_then(|entries| {
                let found = entries.iter().find(|(h, _)| *h == hash).map(|(_, f)| *f);
                store_liked_entries(entries);
                found
            }),
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

impl OnlineTrackDisplay for KugouTrackInfo {
    fn title(&self) -> &SharedString {
        &self.title
    }
    fn artist(&self) -> &SharedString {
        &self.artist
    }
    fn album(&self) -> &SharedString {
        &self.album
    }
    fn cover_url(&self) -> &SharedString {
        &self.cover_url
    }
    fn duration_secs(&self) -> i64 {
        self.duration
    }
    fn download_label() -> cntp_i18n::I18nString {
        download_label()
    }
}
