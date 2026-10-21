//! 酷狗 UI 胶水层，供设置分区、搜索面板与歌单页共用：把 JSON 解析成朴素结构体、
//! 渲染二维码图片，以及队列/播放辅助函数。整体受 `kugou` cargo feature 门控。

pub mod download;

pub use download::download_track_ui;

/// 酷狗曲目行本地化后的「下载」标签。只定义一次，i18n 生成器因此绝不会看到
/// 重复的 `KUGOU_DOWNLOAD` 键。
pub fn download_label() -> cntp_i18n::I18nString {
    tr!("KUGOU_DOWNLOAD", "Download")
}

use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use gpui::{App, SharedString};
use serde_json::Value;

use crate::sync::RecoveredRwLock;
use crate::{
    kugou,
    playback::queue::{DataSource, OnlineIdentity, QueueItemData, QueueItemUIData},
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        library::context_menus::{play_now, queue_item},
        online_common::{
            PlayIntent, claim_fetch, i64_field, parse_pointer_list, settle_fetch, string_field,
            write_pending_fetches,
        },
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

/// 单个用户歌单。
#[derive(Clone, Debug)]
pub struct KugouPlaylistInfo {
    pub global_collection_id: String,
    /// `/v4/get_list_all_file` 端点用来取本歌单歌曲的数字 `listid`。
    pub listid: i64,
    pub name: SharedString,
    pub count: i64,
}

/// `/ocean/v6/rank/list` 返回的一条榜单条目。抓取时带 `withsong: 0`，
/// 所以条目不含歌曲预览——只有名字和封面。
#[derive(Clone, Debug)]
pub struct KugouRank {
    pub rankid: i64,
    pub name: SharedString,
    pub cover_url: SharedString,
}

/// 歌词服务的搜索关键词：「标题 艺人」，曲目没有艺人时只用标题。
fn lyric_search_keyword(track: &KugouTrackInfo) -> String {
    if track.artist.is_empty() {
        track.title.to_string()
    } else {
        format!("{} {}", track.title, track.artist)
    }
}

/// 第一个同时带这两个字段的 `/candidates` 条目的 `(id, accesskey)`；
/// 服务没有可用候选时为 `None`。
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

/// 经 `search_lyric` → `lyric_krc`（卡拉OK，含逐字时间戳与译文）为在线曲目取回
/// 可用的歌词，卡拉OK载荷缺失或解密后为空时回退到 `lyric_lrc`。返回应当展示的
/// 歌词来源，服务里这首歌没有歌词时返回 `None`；抓取错误会记录并返回 `None`。
///
/// 这些 HTTP 调用必须在 Tokio 运行时上跑：reqwest 的 DNS 解析器在没有 reactor
/// 运行的时候被 poll 会 panic（"there is no reactor running"），例如从 gpui 的
/// 主线程执行器上调用。那个 panic 会崩掉整个应用，所以请求和所有其他酷狗调用
/// 一样包在 `RUNTIME::spawn` 里。
pub async fn fetch_online_lyric(track: &KugouTrackInfo) -> Option<OnlineLyric> {
    let client = kugou::shared_client();
    let track = track.clone();

    match crate::RUNTIME
        .spawn(async move {
            let keyword = lyric_search_keyword(&track);

            // 在歌词服务里搜索候选
            let search = client
                .search_lyric(&track.hash, &keyword, track.duration)
                .await
                .map_err(|err| err.to_string())?;

            let Some((id, accesskey)) = first_lyric_candidate(&search.body) else {
                return Ok(None);
            };

            // 优先 KRC：在这里解密，然后把明文交上去，UI 侧就不必再解密一次
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
            Ok::<_, String>((!lrc.is_empty()).then_some(OnlineLyric::Lrc(lrc)))
        })
        .await
    {
        Ok(Ok(lyric)) => lyric,
        Ok(Err(err)) => {
            tracing::debug!("kugou: lyric fetch failed: {err}");
            None
        }
        Err(err) => {
            tracing::debug!("kugou: lyric fetch task failed: {err}");
            None
        }
    }
}

/// 抓取到的在线歌词：普通 LRC 文本，或解密后的 KRC 明文。
#[derive(Clone)]
pub enum OnlineLyric {
    Lrc(String),
    Krc(String),
}

impl OnlineLyric {
    /// 供歌词面板纯文本回退分支使用的可读文本（一行带时间的歌词都解析不出来
    /// 时才用）。
    pub fn describe(&self) -> String {
        match self {
            OnlineLyric::Lrc(text) | OnlineLyric::Krc(text) => text.clone(),
        }
    }
}

/// 在后台领取今日份的免费酷狗 VIP（尽力而为，发射后不管）。登录后调用，
/// 让账号能播整曲；未登录时是空操作。和其他所有酷狗调用一样跑在 Tokio
/// 运行时上。
pub fn claim_daily_vip_async() {
    let client = kugou::shared_client();
    crate::RUNTIME.spawn(async move {
        // 启动错峰：领取是启动期最不紧急的请求；先让出一下，让扫描/更新器的
        // 流量先走。
        tokio::time::sleep(Duration::from_secs(2)).await;
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
                // 每日领取正是维持会员身份的东西；它失败（通常是登录已过期）
                // 恰恰是用户需要当场知道的事，而不是日后才发现 VIP 通道已经死了。
                emit_toast(Toast::warning(tr!(
                    "KUGOU_VIP_CLAIM_FAILED",
                    "Daily VIP claim failed"
                )));
            }
        }
    });
}

/// 从缓存的 `/v1/get_union_vip` 载荷拼出一行简短可读的 VIP 状态，
/// 例如 "VIP · till 2026-08-24" 或 "No VIP"。还没有缓存详情时回退到 "No VIP"。
pub fn vip_status_line() -> SharedString {
    let Some(detail) = kugou::shared_client().vip_detail() else {
        return "No VIP".into();
    };

    // 生效的产品住在 `data.busi_vip[]` 下；本端点顶层的 `is_vip` 即便 `busi_vip`
    // 条目正生效也仍为 0，所以会员身份靠扫描产品数组来判定。
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

    // 到期日优先取 "concept"（svip）那个产品，取不到再回退到第一个生效的。
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

/// 艺人的返回形态取决于端点：要么是纯字符串（web 搜索端点上的 `Singer`），
/// 要么是对象数组。
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

/// 专辑名。搜索结果把它作为纯 `AlbumName` 字符串带回来，而歌单（`/v4`）端点把它
/// 嵌在 `albuminfo` 对象里。
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
        // 有些端点报的是毫秒而不是秒
        let timelength = i64_field(value, &["Timelength", "timelength", "timelen"]);
        if timelength > 0 {
            return timelength / 1000;
        }
    }
    duration
}

/// 从曲目记录取专辑封面 URL。酷狗 CDN 链接里带一个字面的 `{size}` 占位符
/// （例如 `http://imge.kugou.com/stdmusic/{size}/...`），我们把它替换成固定宽度，
/// 图片才能直接加载。
fn cover_url_field(value: &Value) -> SharedString {
    let raw = string_field(value, &["Image", "img", "cover", "album_pic", "pic"]);
    if raw.is_empty() {
        return SharedString::default();
    }
    SharedString::from(raw.replace("{size}", "256"))
}

/// 混歌曲目 id 在搜索、歌单与榜单端点里的各种候选拼写
/// （PascalCase 旧写法 vs snake_case JSON）。
const MIX_SONG_ID_KEYS: &[&str] = &[
    "MixSongID",
    "mixsong_id",
    "album_audio_id",
    "SongID",
    "mixsongid",
];
const ALBUM_ID_KEYS: &[&str] = &["AlbumID", "album_id"];

/// 解析搜索（`/data/lists`）与歌单（`/data/songs`）端点的曲目数组。没有 hash 的
/// 条目会被跳过。
pub fn parse_tracks(body: &Value, list_pointer: &str) -> Vec<KugouTrackInfo> {
    parse_pointer_list(body, list_pointer, |item| {
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
}

/// 解析榜单列表（`/data/info`）。没有 rankid 的条目会被跳过。
pub fn parse_ranks(body: &Value) -> Vec<KugouRank> {
    parse_pointer_list(body, "/data/info", |item| {
        let rankid = i64_field(item, &["rankid"]);
        if rankid == 0 {
            return None;
        }
        // 榜单端点把封面放在 `img_9` 下，且带 `{size}` 占位符
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
}

/// 解析单个曲目对象，容忍搜索、歌单与榜单端点之间的 schema 差异。榜单的音频数据
/// 住在 `deprecated` 下（hash + 毫秒时长），封面在 `album_info` 下。
fn parse_track(item: &Value) -> KugouTrackInfo {
    let audio = item.get("deprecated").unwrap_or(item);
    let album_cover = item
        .pointer("/album_info/sizable_cover")
        .or_else(|| item.get("sizable_cover"))
        .and_then(Value::as_str)
        // 有些端点在封面路径里留下 {size} 占位符
        .map(|url| SharedString::from(url.replace("{size}", "256")))
        .unwrap_or_default();
    // 时长：搜索/歌单端点用秒；榜单把 `deprecated.duration` 装成毫秒，
    // 每日推荐的 `time_length` 用秒
    let duration = {
        let secs = duration_field(item);
        if secs > 0 {
            secs
        } else {
            let deprecated_ms = i64_field(audio, &["Duration", "duration"]);
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
        hash: string_field(audio, &["FileHash", "hash"]),
        mix_song_id: i64_field(item, MIX_SONG_ID_KEYS),
        album_id: i64_field(item, ALBUM_ID_KEYS),
        cover_url: if album_cover.is_empty() {
            cover_url_field(audio)
        } else {
            album_cover
        },
    }
}

/// 解析单个榜单的歌曲（`/data/songlist`）。
pub fn parse_rank_tracks(body: &Value) -> Vec<KugouTrackInfo> {
    parse_pointer_list(body, "/data/songlist", |item| {
        let track = parse_track(item);
        (!track.hash.is_empty()).then_some(track)
    })
}

/// 解析每日推荐曲目（`/data/song_list`）。
pub fn parse_recommend_tracks(body: &Value) -> Vec<KugouTrackInfo> {
    parse_pointer_list(body, "/data/song_list", |item| {
        let track = parse_track(item);
        (!track.hash.is_empty()).then_some(track)
    })
}

/// 解析用户歌单列表（`/data/info`）。
pub fn parse_playlists(body: &Value) -> Vec<KugouPlaylistInfo> {
    parse_pointer_list(body, "/data/info", |item| {
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
        // 合并对同一首歌的并发点击：只有第一次点击会抓取并入队；同一窗口内的
        // 后续点击直接丢弃（claim_fetch，与在线胶水层共用）。
        {
            let mut dedup = write_pending_fetches();
            if !claim_fetch(&mut dedup, track.mix_song_id, intent, Instant::now()) {
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
            // 只清掉「已在途」标记：失败的抓取必须能在下一次点击时重试。
            write_pending_fetches().in_flight.remove(&track.mix_song_id);
            tracing::warn!(hash = %track.hash, "kugou play_track: no playable URL in response");
            emit_toast(Toast::warning(tr!(
                "KUGOU_NO_URL",
                "KuGou returned no playable URL for this track"
            )));
            return;
        };
        tracing::info!(
            hash = %track.hash,
            url = %crate::media::redact_media_path(&url),
            "kugou play_track: got play URL"
        );

        // 记住 URL → 曲目的映射，歌词视图才能给正在跑的 HTTP 流解析歌词
        remember_online_track(url.clone(), track.clone());

        cx.update(|cx| {
            // 重复点击同一曲目绝不能在队列里堆出重复条目：这首在线曲目的条目
            // 若已存在，就地刷新它（会过期的）URL 并跳转过去。
            let queue_data = cx
                .global::<crate::ui::models::Models>()
                .queue
                .read(cx)
                .data
                .clone();
            let existing = queue_data.read_recovered().iter().position(|item| {
                crate::media::is_http_path(item.get_path())
                    && mixsongid_in_url(&item.get_path().to_string_lossy())
                        .is_some_and(|id| id == track.mix_song_id)
            });

            if let Some(index) = existing {
                // 在写锁下重新检查（自上面那次读以来队列可能已变化）：只有该索引
                // 处的条目仍是同一首曲目时才刷新，否则一次交错的变更会让这个 URL
                // 落到别的歌上。
                let mut guard = queue_data.write_recovered();
                let entries = Arc::make_mut(&mut guard);
                let same_track = entries.get(index).is_some_and(|item| {
                    crate::media::is_http_path(item.get_path())
                        && mixsongid_in_url(&item.get_path().to_string_lossy())
                            .is_some_and(|id| id == track.mix_song_id)
                });
                let replaced = same_track
                    && entries
                        .get_mut(index)
                        .map(|item| item.replace_path(PathBuf::from(&url)))
                        .is_some();
                if !replaced {
                    tracing::warn!(
                        index,
                        "kugou play_track: queue item changed before URL refresh"
                    );
                }

                if replaced {
                    if matches!(intent, PlayIntent::Now) {
                        cx.global::<crate::playback::interface::PlaybackInterface>()
                            .jump(index);
                    }
                    return;
                }
                // 身份已经漂移：往下走，插入一个全新条目
            }

            let item = online_queue_item(url, &track);

            match intent {
                PlayIntent::Now => play_now(cx, item),
                PlayIntent::Queue => queue_item(cx, item),
            }
        });

        {
            let mut dedup = write_pending_fetches();
            settle_fetch(&mut dedup, track.mix_song_id, intent, Instant::now());
        }
    })
    .detach();
}

/// 取 `track` 的播放 URL 并开始播放。
pub fn play_track_now(cx: &mut App, track: &KugouTrackInfo) {
    play_track(cx, track, PlayIntent::Now);
}

fn online_queue_item(url: String, track: &KugouTrackInfo) -> QueueItemData {
    QueueItemData::with_ui_data(
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

/// 把 `track` 加入酷狗「我喜欢」歌单（listid 2）并把结果做成 toast。
/// 请求和其他所有酷狗调用一样跑在 Tokio 运行时上。
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
                    liked_set().write_recovered().insert(track.hash.clone());
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

/// 取 `track` 的播放 URL 并追加到队列。
pub fn queue_track(cx: &mut App, track: &KugouTrackInfo) {
    play_track(cx, track, PlayIntent::Queue);
}

/// 在后台发起一次收藏集刷新。用户打开酷狗「我喜欢」歌单时调用，让播放栏的星标
/// 对这些曲目立刻准确。
///
/// 由 `online_common::prime_online_liked_cache` 内联而来（2026-10-15）：共享体的
/// 两个闭包参数从来只是注入本模块自己的锁与刷新函数，只剩一个提供方之后它们
/// 就是纯间接层（§12）。
pub fn prime_liked_cache() {
    crate::RUNTIME.spawn(async {
        // 启动分阶段：等过首帧窗口再发出这次整份收藏列表的预热，与更新器的
        // 启动延迟同一纪律——这是后台预热，不是首帧工作。
        tokio::time::sleep(Duration::from_secs(2)).await;
        // 与 `online_track_is_liked` 里的惰性加载用同一把锁，这样两个入口
        // 不可能各自把整份收藏列表抓一遍。
        let _guard = liked_load_lock().lock().await;
        refresh_liked_set_from_service().await;
    });
}

/// `hash` 是否已在用户的酷狗「我喜欢」列表里。缓存一旦预热就用它；只有第一次
/// 才回退到网络刷新。并发的首次查询合并为一次抓取：后来者在 `liked_load_lock`
/// 上等待，然后读第一个加载者产出的结果；加载失败时不置初始化旗标，
/// 让后续查询重试。
pub async fn online_track_is_liked(hash: &str) -> bool {
    let hash = hash.to_string();
    if !LIKED_SET_INIT.load(Ordering::Relaxed) {
        let _guard = liked_load_lock().lock().await;
        // 在锁下重新检查：把缓存预热好的那次查询可能已在我们等待期间完成。
        if !LIKED_SET_INIT.load(Ordering::Relaxed) {
            refresh_liked_set_from_service().await;
        }
    }
    liked_set_contains(&hash)
}

/// 把 `track` 从用户的酷狗「我喜欢」列表里移除并把结果做成 toast。
/// 请求和其他所有酷狗调用一样跑在 Tokio 运行时上。
pub fn unlike_track(cx: &mut App, track: &KugouTrackInfo) {
    let track = track.clone();
    cx.spawn(async move |cx| {
        let client = kugou::shared_client();
        let hash = track.hash.clone();
        // fileid 通常就在上一次整份收藏抓取预热好的缓存里；只有未命中
        // （此后在别的设备上点过赞，或缓存从未预热）才要付一次新的分页抓取。
        //
        // 先把缓存查找绑到一个局部变量上：若直接作 `match` 的 scrutinee，
        // RwLock 守卫这个临时量会活过整个 match——穿过 `None` 分支里那次网络
        // `.await`——于是整次抓取期间收藏状态的写入方都会被饿死
        // （clippy::await_holding_lock）。
        let cached = liked_fileids().read_recovered().get(&hash).copied();
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
            // 在抓到的分页里没找到（或抓取本身失败、没产出候选）；按移除成功处理。
            // 列表超出 10 页抓取窗口时服务端可能仍留着这条收藏，所以在日志里留痕。
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
                liked_set().write_recovered().remove(&hash);
                liked_fileids().write_recovered().remove(&hash);
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

/// `parse_tracks` 成本基准。为什么存在：每个在线页面都把它的 JSON 投影成
/// `KugouTrackInfo`，而且是在 `this.update(cx, …)` **里面**——即 gpui 主线程
/// （`ui/library/kugou_playlists.rs` 的分页取曲与单曲解析、`ui/search/model.rs`
/// 的在线搜索；榜单页另走 `parse_rank_tracks`，它也在主线程）——
/// 只有网络 `.json()` 那一跳在线程外。准则 §3 禁止在渲染线程上解析，但 §1 禁止
/// 没有数字就动手，所以这里把它测出来。两种 schema 框住现实：乐观的一种是每个
/// 字段都在第一个候选键上命中，悲观的一种是把候选列表走到最后一种拼写
/// （且 `duration` 以毫秒的*数字字符串*送达，迫使解析一次）。
#[cfg(test)]
mod parse_costs {
    use super::*;

    const PAGE: usize = 100;
    const ROUNDS: usize = 300;

    /// 填充键用来顶替真实酷狗曲目对象里除了我们读的那些之外还带的约 20 个字段：
    /// 它们决定对象 map 的规模，而 `BTreeMap` 的查找开销正是随这个规模变化的。
    const FILLER: &[&str] = &[
        "KSR_DECODE",
        "MVhash",
        "PubTime",
        "ownercount",
        "sourceid",
        "typesong",
        "res_groupid",
        "ftitle",
        "FNVAudio",
        "a_id",
        "user_id",
        "is_vip",
        "pay_type",
        "album_status",
        "singerId",
        "bytes",
        "_nr",
        "_rk",
    ];

    fn object(pairs: &[(&str, Value)]) -> Value {
        let mut map = serde_json::Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_owned(), value.clone());
        }
        for (i, key) in FILLER.iter().enumerate() {
            map.insert((*key).to_owned(), Value::String(format!("v{i}")));
        }
        Value::Object(map)
    }

    fn text(value: impl Into<String>) -> Value {
        Value::String(value.into())
    }

    fn number(value: i64) -> Value {
        Value::Number(value.into())
    }

    fn fixture(pascal: bool) -> Value {
        let cover = "http://imgtest.kugou.com/get?cover={size}&hash=abc";
        let items = (0..PAGE)
            .map(|i| {
                if pascal {
                    let artist = Value::Array(vec![object(&[(
                        "ArtistName",
                        text(format!("歌手 {}", i % 40)),
                    )])]);
                    object(&[
                        ("FileHash", text("9ae8bf8fa997c0c4bc68fa6342cb85b7")),
                        ("SongName", text(format!("轨道 {i} 的歌名"))),
                        ("Artist", artist),
                        ("AlbumName", text(format!("专辑 {}", i % 12))),
                        ("Duration", number(180 + i as i64)),
                        ("Image", text(cover)),
                        ("MixSongID", number(1_000_000 + i as i64)),
                        ("AlbumID", number(500_000 + i as i64)),
                    ])
                } else {
                    object(&[
                        ("hash", text("9ae8bf8fa997c0c4bc68fa6342cb85b7")),
                        ("name", text(format!("轨道 {i} 的歌名"))),
                        ("author_name", text(format!("歌手 {}", i % 40))),
                        ("album_name", text(format!("专辑 {}", i % 12))),
                        ("timelength", text(format!("{}", (180 + i) * 1000))),
                        ("pic", text(cover)),
                        ("mixsongid", number(1_000_000 + i as i64)),
                        ("album_id", number(500_000 + i as i64)),
                    ])
                }
            })
            .collect::<Vec<_>>();
        let mut info = serde_json::Map::new();
        info.insert("info".to_owned(), Value::Array(items));
        let mut data = serde_json::Map::new();
        data.insert("data".to_owned(), Value::Object(info));
        Value::Object(data)
    }

    fn report(label: &str, body: &Value) {
        let mut microseconds = Vec::with_capacity(ROUNDS);
        let mut items = 0usize;
        for round in 0..ROUNDS {
            let started = std::time::Instant::now();
            let parsed = parse_tracks(body, "/data/info");
            let elapsed = started.elapsed().as_micros() as u64;
            items += parsed.len();
            // 第 0 轮跑在冷的分配器/分支布局上。
            if round > 0 {
                microseconds.push(elapsed);
            }
        }
        microseconds.sort_unstable();
        let at = |p: usize| microseconds[microseconds.len() * p / 100];
        println!(
            "parse_tracks page={PAGE} schema={label} min={}us p50={}us p95={}us max={}us items={items}",
            at(0),
            at(50),
            at(95),
            microseconds[microseconds.len() - 1],
        );
    }

    #[test]
    #[ignore = "cost bench: JSON projection of one online page, explicit run only"]
    fn report_online_page_parse_costs() {
        report("pascal_first_hit", &fixture(true));
        report("snake_last_hit", &fixture(false));
    }
}
