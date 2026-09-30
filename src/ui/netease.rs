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
    path::PathBuf,
    sync::{
        Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use cntp_i18n::tr;
use gpui::{App, RenderImage, SharedString};
use serde_json::Value;

use crate::{
    playback::queue::{DataSource, OnlineIdentity, QueueItemData, QueueItemUIData},
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        library::context_menus::{play_now, queue_item},
        lyrics::lrc::{LrcLine, parse_lrc},
        online_common::{
            FETCH_COOLDOWN, PlayIntent, i64_field, parse_pointer_list, string_field,
            write_pending_fetches,
        },
        online_track_row::OnlineTrackDisplay,
    },
};

// ①步下沉：流注册表（stream map）、播放 URL 获取与恢复期 URL 刷新已搬至
// `crate::online_sources::netease`（A-1 第①步，playback/stats 不再反向依赖
// crate::ui），此处按原可见性再导出，保证 ui 侧全部既有调用点零改动；
// ②步收编（trait 化）时清理这些再导出。
// （`refresh_restored_url` 的 ui 侧消费者已全部切到 crate::online_sources，
// 故此处不再重复再导出；`StreamMapEntry` 无需具名，经 `stream_map` 再导出
// 即可访问其 pub(crate) 字段。）
pub(crate) use crate::online_sources::netease::stream_map;
pub use crate::online_sources::netease::{
    NeteaseTrackInfo, extract_song_url, fetch_stream_url, online_track_matching_path,
    remember_online_track,
};

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
// JSON parsing (tolerant to the shape differences between the endpoints)
// ---------------------------------------------------------------------------

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
    parse_pointer_list(body, list_pointer, parse_song)
}

/// Parses the user playlist list (`/playlist` of user_playlist).
pub fn parse_playlists(body: &Value) -> Vec<NeteasePlaylistInfo> {
    parse_pointer_list(body, "/playlist", |item| {
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
}

/// Parses the chart list (`/list` of toplist).
pub fn parse_ranks(body: &Value) -> Vec<NeteaseRank> {
    parse_pointer_list(body, "/list", |item| {
        let id = i64_field(item, &["id"]);
        if id == 0 {
            return None;
        }
        Some(NeteaseRank {
            id,
            name: SharedString::from(string_field(item, &["name"])),
            cover_url: cover_url_field(item),
            update_frequency: SharedString::from(string_field(item, &["updateFrequency"])),
        })
    })
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

fn play_track(cx: &mut App, track: &NeteaseTrackInfo, intent: PlayIntent) {
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
        // a double-click can't enqueue the track twice. The intent tag keeps
        // the cooldown from swallowing a deliberate play right after a
        // queue-add of the same song.
        let intent_key = matches!(intent, PlayIntent::Now) as u8;
        {
            let mut dedup = write_pending_fetches();
            dedup.recent.retain(|_, (_, at)| at.elapsed() < FETCH_COOLDOWN);
            let duplicate = dedup
                .recent
                .get(&track.id)
                .is_some_and(|&(seen_intent, _)| seen_intent == intent_key)
                || !dedup.in_flight.insert(track.id);
            if duplicate {
                return;
            }
        }
        tracing::info!(title = %track.title, id = track.id, "netease play_track: fetching play URL");

        let client = crate::netease::shared_client();
        let fetch_track = track.clone();
        let request = crate::RUNTIME
            .spawn(async move { fetch_stream_url(&client, fetch_track.id, &quality).await })
            .await;

        let Some(url) = request.ok().flatten() else {
            // Clear the in-flight tag only: a failed fetch must stay
            // retryable on the next click.
            write_pending_fetches().in_flight.remove(&track.id);
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
                .unwrap_or_else(|e| e.into_inner())
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
                        "netease play_track: queue item gone before URL refresh"
                    );
                }

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

        {
            let mut dedup = write_pending_fetches();
            dedup.in_flight.remove(&track.id);
            dedup.recent.insert(track.id, (intent_key, Instant::now()));
        }
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

/// Poison-recovering locks for the liked-id set, same rationale as
/// `write_pending_fetches`: a panicking holder must not wedge every later
/// liked-state query or update behind a poisoned lock.
fn write_liked_ids() -> RwLockWriteGuard<'static, HashSet<i64>> {
    liked_ids().write().unwrap_or_else(|e| e.into_inner())
}

fn read_liked_ids() -> RwLockReadGuard<'static, HashSet<i64>> {
    liked_ids().read().unwrap_or_else(|e| e.into_inner())
}

/// True when the cache holds `id` as liked.
pub fn liked_set_contains(id: i64) -> bool {
    read_liked_ids().contains(&id)
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
    *write_liked_ids() = ids.into_iter().collect();
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
        let request = crate::RUNTIME
            .spawn(async move { client.like(id, true).await })
            .await;

        cx.update(|_cx| match request {
            Ok(Ok(_)) => {
                write_liked_ids().insert(id);
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
        let request = crate::RUNTIME
            .spawn(async move { client.like(id, false).await })
            .await;

        cx.update(|_cx| match request {
            Ok(Ok(_)) => {
                write_liked_ids().remove(&id);
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

/// Timestamp tolerance (ms) for pairing a `tlyric` line with a body line: a
/// translation pairs with the nearest body line whose timestamp is within
/// this window; anything farther away never pairs.
const TRANSLATION_MATCH_WINDOW_MS: u64 = 500;

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
                candidate.time_ms.abs_diff(line.time_ms) <= TRANSLATION_MATCH_WINDOW_MS
            })
            .min_by_key(|candidate| candidate.time_ms.abs_diff(line.time_ms));
        if let Some(best) = best
            && !best.text.is_empty()
        {
            line.translation = Some(best.text.clone());
        }
    }
}

/// Text at the `"/<kind>/lyric"` JSON pointer of a `lyric_new` body; a
/// missing or non-string field comes back as an empty string.
fn lyric_pointer_text(body: &Value, kind: &str) -> String {
    body.pointer(&format!("/{kind}/lyric"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
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

            let yrc = lyric_pointer_text(&response.body, "yrc");
            let lrc = lyric_pointer_text(&response.body, "lrc");
            let tlyric = lyric_pointer_text(&response.body, "tlyric");

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

/// Renders a QR code for the given QR login key, encoded from the login URL.
pub fn build_login_qr(key: &str) -> anyhow::Result<Arc<RenderImage>> {
    crate::ui::online_common::build_qr_render_image(&crate::netease::api::qr_login_url(key))
}

impl OnlineTrackDisplay for NeteaseTrackInfo {
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
        assert_eq!(
            playlists[0].cover_url,
            "https://p1.music.126.net/x/y.jpg?param=120y120"
        );
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
        assert_eq!(
            tracks[0].cover_url,
            "https://p2.music.126.net/z.jpg?param=256y256"
        );
        assert_eq!(tracks[0].duration, 269);
        assert_eq!(tracks[0].artist, "周杰伦");
    }
}
