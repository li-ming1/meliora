//! 酷狗共享在线源层：流 URL → 曲目注册表（「stream map」）、播放 URL 获取与喜欢列表缓存。
//! 由 `crate::ui::kugou` 下沉而来（A-1 第①步），因此播放线程与统计解析流时不必碰 UI；
//! UI 仍经 `crate::ui::kugou` 的再导出调用这些。整体受 `kugou` cargo feature 门控。

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use gpui::SharedString;
use serde_json::Value;

use super::stream_registry::{STREAM_MAP_CAP, StreamMapPersistence};
use crate::kugou;
use crate::sync::RecoveredRwLock;

/// 把秒数格式化为 "m:ss" / "mm:ss" / "h:mm:ss"，与 `ui::util::format_duration` 一致。
/// `online_sources` 禁止反向依赖 `crate::ui`，故此处保留纯格式化逻辑。
fn format_duration_text(secs: i64, pad_minutes: bool) -> String {
    let secs = secs.max(0);
    let hours = secs / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else if pad_minutes {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// 在后台解析或构造 `KugouTrackInfo` 时一次性算好的列表展示文本，避免渲染热路径
/// 每帧为每个可见行重复 `format!` 分配。
#[derive(Clone, Debug)]
struct PrecomputedTrackDisplay {
    artist_album: Option<SharedString>,
    duration_compact: SharedString,
    duration_padded: SharedString,
    detail: SharedString,
}

impl PrecomputedTrackDisplay {
    fn new(artist: &SharedString, album: &SharedString, duration: i64) -> Self {
        let secs = duration.max(0);
        let duration_compact: SharedString = format_duration_text(secs, false).into();
        // 达到 1 小时或分钟已占两位时补零版与紧凑版完全相同，直接复用同一个 SharedString。
        let duration_padded = if secs >= 600 {
            duration_compact.clone()
        } else {
            format_duration_text(secs, true).into()
        };
        let artist_album = (!artist.is_empty() && !album.is_empty())
            .then(|| SharedString::from(format!("{artist} · {album}")));
        let detail = if artist.is_empty() {
            duration_compact.clone()
        } else {
            SharedString::from(format!("{artist} · {duration_compact}"))
        };
        Self {
            artist_album,
            duration_compact,
            duration_padded,
            detail,
        }
    }
}

/// 搜索结果与歌单里展示的一条在线曲目。
#[derive(Clone, Debug)]
pub struct KugouTrackInfo {
    pub title: SharedString,
    pub artist: SharedString,
    pub album: SharedString,
    /// 时长，单位秒（未知为 0）。
    pub duration: i64,
    pub hash: String,
    pub mix_song_id: i64,
    pub album_id: i64,
    /// API 给的专辑封面绝对 URL（`{size}` 占位符已替换成固定像素宽度）。API 没给则为空。
    pub cover_url: SharedString,
    display: PrecomputedTrackDisplay,
}

impl PartialEq for KugouTrackInfo {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title
            && self.artist == other.artist
            && self.album == other.album
            && self.duration == other.duration
            && self.hash == other.hash
            && self.mix_song_id == other.mix_song_id
            && self.album_id == other.album_id
            && self.cover_url == other.cover_url
    }
}

impl KugouTrackInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        title: SharedString,
        artist: SharedString,
        album: SharedString,
        duration: i64,
        hash: String,
        mix_song_id: i64,
        album_id: i64,
        cover_url: SharedString,
    ) -> Self {
        let display = PrecomputedTrackDisplay::new(&artist, &album, duration);
        Self {
            title,
            artist,
            album,
            duration,
            hash,
            mix_song_id,
            album_id,
            cover_url,
            display,
        }
    }

    pub fn cached_artist_album_label(&self) -> Option<&SharedString> {
        self.display.artist_album.as_ref()
    }

    pub fn cached_duration_label(&self, pad_minutes: bool) -> &SharedString {
        if pad_minutes {
            &self.display.duration_padded
        } else {
            &self.display.duration_compact
        }
    }

    pub fn cached_detail_label(&self) -> &SharedString {
        &self.display.detail
    }
}

// ---------------------------------------------------------------------------
// 流 URL → 曲目注册表
//
// 酷狗的播放 URL 是不透明的签名 CDN 链接；队列（只带 URL）自己没法解析歌词/喜欢状态/
// 下载动作。每个解析出来的播放 URL 都同时记在这里的内存里和磁盘上，所以重启后恢复的队列
// 仍然能匹配上——即使会话恢复期的 URL 刷新撞上网络失败。对照上游的 stream-map 设计。
// ---------------------------------------------------------------------------

/// 注册表条目的磁盘镜像（纯字符串，JSON 友好）。
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
        KugouTrackInfo::new(
            SharedString::from(self.title.clone()),
            SharedString::from(self.artist.clone()),
            SharedString::from(self.album.clone()),
            self.duration,
            self.hash.clone(),
            self.mix_song_id,
            self.album_id,
            SharedString::from(self.cover_url.clone()),
        )
    }
}

fn stream_map_path() -> PathBuf {
    crate::paths::data_dir().join("kugou_stream_map.json")
}

fn stream_map() -> &'static RwLock<Vec<StreamMapEntry>> {
    static STREAM_MAP: OnceLock<RwLock<Vec<StreamMapEntry>>> = OnceLock::new();
    STREAM_MAP.get_or_init(|| {
        let entries = std::fs::read_to_string(stream_map_path())
            .ok()
            .and_then(|contents| {
                // 逐条目容错：单条损坏（如缺 url 键）只丢该条，不废整张注册表
                let values: Vec<serde_json::Value> = serde_json::from_str(&contents).ok()?;
                Some(
                    values
                        .into_iter()
                        .filter_map(|value| serde_json::from_value(value).ok())
                        .collect::<Vec<StreamMapEntry>>(),
                )
            })
            .unwrap_or_default();
        RwLock::new(entries)
    })
}

/// 流注册表持久化设施（去重基线 + 写闸门）。本 provider 专属 static 实例：
/// 基线绝不能与其他 provider 共享，否则去重跳写与失败回滚会互相误伤。
static STREAM_MAP_PERSISTENCE: StreamMapPersistence = StreamMapPersistence::new();

/// 记下 `url`（一条酷狗流）属于 `track`：同一曲目最新的条目放最上面，并逐出同一曲目更早的
/// URL。
pub fn remember_online_track(url: String, track: KugouTrackInfo) {
    // 锁内只改表：这张表 UI 线程与播放线程都在读，写守卫跨过任何慢操作都会
    // 拖住换歌。快照与序列化都不在这里做（见下）。
    {
        let mut guard = stream_map().write_recovered();
        // 0 表示端点省略了 id。把它当成真键会逐掉所有其他无 id 的曲目，所以未知 id 的条目
        // 只按 URL 去重，永不被 id 匹配（见 online_track_matching_path）。
        guard.retain(|e| {
            e.url != url && (track.mix_song_id == 0 || e.mix_song_id != track.mix_song_id)
        });
        guard.insert(0, StreamMapEntry::from((url, track)));
        guard.truncate(STREAM_MAP_CAP);
    }
    // 全表 clone + 54KB JSON 序列化交运行时，不在调用线上做：两个调用点
    // （UI 点歌与在线源取流）都处在 gpui `cx.spawn` 的 await 恢复点，也就是
    // **主线程**，200 条 clone + 序列化实测数量级 0.2-0.5ms ≈ 主窗一帧 draw
    // 的 10-20%。挪走之后写锁临界区也只剩三次比较级操作，与播放线程读者的
    // 竞争面同步缩小。
    //
    // 顺序性不变：落盘仍由 `persist_snapshot` 的写门串行化；两次快连点谁先
    // 落盘无所谓——注册表是缓存，陈旧只多一次 URL 刷新（与原
    // "racing writers" 语义一致）。
    // 在后台任务内直接持读锁序列化为 JSON 字节，省掉 200 条 × 多段 String 的全表深克隆；
    // 序列化本身不涉及 I/O，写锁/读锁临界区都在内存百微秒级。
    crate::RUNTIME.spawn(async move {
        let json = {
            let guard = stream_map().read_recovered();
            serde_json::to_vec(&*guard)
        };
        if let Ok(json) = json {
            STREAM_MAP_PERSISTENCE.persist_serialized(stream_map_path(), json, "kugou");
        }
    });
}

/// 刷新重登记时从既有注册表条目沿用专辑名（A4）。刷新路径的展示四元组没有 album
/// 字段，直接落空串会覆盖原始注册（点歌时的全量元数据）写下的专辑；此后按 URL 解析
/// （统计侧 `identify_path`）拿到空 album，upsert 再把库里正确的专辑抹成空串——该曲
/// 从按专辑统计消失。`remember_online_track` 以最新条目置顶，同曲（同 mix_song_id）
/// 旧条目在重登记发生**之前**取用、尚未被逐出，取到的就是原始注册的值；0 是端点
/// 省略 id 的哨兵，不得按 id 匹配（会抓到别的歌的专辑），无旧条目时返回空串。
fn previous_track_entry(mix_song_id: i64) -> Option<StreamMapEntry> {
    if mix_song_id == 0 {
        return None;
    }
    stream_map()
        .read_recovered()
        .iter()
        .find(|e| e.mix_song_id == mix_song_id)
        .cloned()
}

/// 若 `path` 是经本应用到达的流，返回为它记下的在线曲目。匹配先用嵌在 URL 里的
/// `mixsongid`（队列去重已改走结构化 OnlineIdentity，此启发式仅剩本处的
/// 流 URL 回溯在用），再退化到逐字节 URL 比较，所以即使重定向目标
/// 与产生它的 URL 不同也仍能解析出来。
pub fn online_track_matching_path(path: &Path) -> Option<KugouTrackInfo> {
    let path_str = path.to_string_lossy();
    let path_mix = mixsongid_in_url(&path_str);
    let guard = stream_map().read_recovered();
    guard
        .iter()
        .find(|entry| {
            entry.url == path_str.as_ref()
                || path_mix.is_some_and(|id| id != 0 && id == entry.mix_song_id)
        })
        .map(StreamMapEntry::to_track)
}

/// 酷狗流 URL 以 `_mx<digits>_` 的形式内嵌 mixsong id（例如 `..._pi2_mx29048099_qu128_...`）。
/// 同一曲目的不同签名 URL 之间这个 id 稳定，所以它能可靠地标识队列里的一条在线曲目。
pub(crate) fn mixsongid_in_url(url: &str) -> Option<i64> {
    let idx = url.find("_mx")?;
    let rest = &url[idx + 3..];
    let digits = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())?;
    digits.parse().ok()
}

/// 从 `song_url` 响应里取出第一个可播放 URL。直链就在顶层的 `url`（一个数组）下，例如
/// `/v5/url` 的响应形如 `{ ..., "url": ["http://...", "http://..."] }`。
pub fn extract_song_url(body: &Value) -> Option<String> {
    match body.get("url") {
        Some(Value::Array(items)) => items
            .iter()
            .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string)),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// 标准 128 kbps 片段的音质档位——请求档位不可用时的兜底（例如没有 VIP）。
const FALLBACK_QUALITY: &str = "128";

/// 音质档位从低到高排列。`[`quality_ladder`]` 返回前缀，调用方按 `.rev()`
/// 逐级降档。
const QUALITY_ORDER: [&str; 4] = ["128", "320", "flac", "high"];

/// 从 `requested` 降到 128 的档位路径（升序存放，含两端）：调用方用
/// `.iter().rev()` 从请求档开始逐级尝试。未知档位只返回 128 这一段。
///
/// 播放取流与下载共用同一顺序（`ui::kugou::download` 引用它），避免两侧梯队
/// 漂移成"播放掉 128、下载却是 flac→320→128"。
pub fn quality_ladder(requested: &str) -> &'static [&'static str] {
    let highest = QUALITY_ORDER
        .iter()
        .position(|quality| *quality == requested)
        .unwrap_or(0);
    &QUALITY_ORDER[..=highest]
}

/// 实时播放路径的单请求时限档位。播放侧钩子给整个刷新 6s 总预算
/// （playback/thread.rs 的 `STREAM_REFRESH_TIMEOUT`），而网关 client 单请求 30s、
/// 失败再打一次 128k 兜底——最坏 60s：慢网下外层总在 6s 超时返回 `None`，内层
/// spawn 继续跑完但结果被整体丢弃（新 URL 白拿，下次重试再付一遍）。压到 3s/请求
/// 后：首档网络错误两连发 ≤6s 收敛；顺降路径（首档 Ok 无 URL → 中段档 Err →
/// 128 兜底）最坏三连发 3×3s=9s，第三发可能被外层 6s 总预算掐断——顺降路径不
/// 承诺预算内收敛。非实时路径（用户点歌等）传 `None` 维持网关 30s 档。
const REALTIME_URL_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// 带（可选）单请求时限的 `song_url`：超时折算成 [`kugou::KugouError::local`]，
/// 走调用方同一条失败/兜底路径。时限到期时 future 被 drop，底层的 HTTP 请求随之
/// 取消（不像外层总预算超时那样任由内层任务跑完再丢结果）。`None` = 不加时限，
/// 维持网关 client 的 30s 总体上界。
async fn song_url_bounded(
    client: &kugou::KugouClient,
    hash: &str,
    mix_song_id: i64,
    album_id: i64,
    quality: &str,
    per_request: Option<Duration>,
) -> Result<kugou::client::KugouResponse, kugou::KugouError> {
    let free_part = !client.has_active_vip();
    let call = client.song_url(hash, mix_song_id, album_id, quality, free_part);
    match per_request {
        Some(limit) => match tokio::time::timeout(limit, call).await {
            Ok(result) => result,
            Err(_elapsed) => Err(kugou::KugouError::local(
                "realtime URL request exceeded its budget",
            )),
        },
        None => call.await,
    }
}

/// 首档与梯队中段网络错误共用的 128 兜底单发：直接落 [`FALLBACK_QUALITY`] 再试
/// 一次判终，不逐档续试——「最坏超时次数不放大」由两条错误路径共同维持。
async fn fallback_quality_url_once(
    client: &kugou::KugouClient,
    hash: &str,
    mix_song_id: i64,
    album_id: i64,
    per_request_timeout: Option<Duration>,
) -> Option<String> {
    match song_url_bounded(
        client,
        hash,
        mix_song_id,
        album_id,
        FALLBACK_QUALITY,
        per_request_timeout,
    )
    .await
    {
        Ok(resp) => extract_song_url(&resp.body),
        Err(error) => {
            tracing::warn!(
                %error,
                hash,
                quality = FALLBACK_QUALITY,
                "kugou: fallback URL request failed"
            );
            None
        }
    }
}

/// 按请求的 `quality` 为酷狗曲目取一个可播放 URL，更高档位不可用时（例如没有 VIP）退化到
/// 标准 128 kbps 片段。完全没取回可播放 URL 时返回 `None`。实时播放与会话恢复期的 URL
/// 刷新共用；`per_request_timeout` 是单请求时限档位（REALTIME_URL_REQUEST_TIMEOUT），
/// 传 `None` 维持网关 30s 档。
pub async fn fetch_stream_url(
    client: &kugou::KugouClient,
    hash: &str,
    mix_song_id: i64,
    album_id: i64,
    quality: &str,
    per_request_timeout: Option<Duration>,
) -> Option<String> {
    let mut ladder = quality_ladder(quality).iter().rev();
    let Some(requested) = ladder.next().copied() else {
        return None;
    };

    match song_url_bounded(
        client,
        hash,
        mix_song_id,
        album_id,
        requested,
        per_request_timeout,
    )
    .await
    {
        Ok(resp) => {
            if let Some(url) = extract_song_url(&resp.body) {
                return Some(url);
            }
            // 该档位存在但没给 URL（多数曲目没有 flac/high 变体）：顺降一档
            tracing::debug!(
                hash,
                quality = requested,
                "kugou: response carried no playable URL"
            );
        }
        Err(error) => {
            // 网络/超时失败不逐档重试（最坏会放大到 4×超时），直接落 128 判终
            tracing::debug!(%error, hash, quality = requested, "kugou: URL request failed");
            if requested == FALLBACK_QUALITY {
                return None;
            }
            return fallback_quality_url_once(
                client,
                hash,
                mix_song_id,
                album_id,
                per_request_timeout,
            )
            .await;
        }
    }

    for candidate in ladder {
        match song_url_bounded(
            client,
            hash,
            mix_song_id,
            album_id,
            candidate,
            per_request_timeout,
        )
        .await
        {
            Ok(resp) => {
                if let Some(url) = extract_song_url(&resp.body) {
                    tracing::debug!(
                        hash,
                        requested = quality,
                        quality = candidate,
                        "kugou: fell back to a lower quality"
                    );
                    return Some(url);
                }
            }
            Err(error) => {
                // 与首档错误同策略：中段档位的网络/超时失败不逐档续试，直接落 128
                // 兜底一次判终；已在 128 档则无路可退（REG-1：原先 warn + return
                // None 会放弃本可播的 128 终档，与首档「网络失败落 128」不对称）。
                tracing::warn!(
                    %error,
                    hash,
                    quality = candidate,
                    "kugou: URL request failed during fallback"
                );
                if *candidate == FALLBACK_QUALITY {
                    return None;
                }
                return fallback_quality_url_once(
                    client,
                    hash,
                    mix_song_id,
                    album_id,
                    per_request_timeout,
                )
                .await;
            }
        }
    }
    None
}

/// 用户酷狗喜欢列表的 `listid`。
const LIKED_LIST_ID: i64 = 2;
/// 喜欢列表的分页拉取上界：最多这些页，每页 [`LIKED_LIST_PAGE_SIZE`] 条；不满一页即结束
/// 遍历。
const LIKED_LIST_MAX_PAGES: i64 = 10;
const LIKED_LIST_PAGE_SIZE: i64 = 100;

/// 列出用户酷狗喜欢列表（`listid: 2`）里的 `(hash, fileid)` 对。跑在 Tokio 运行时上；未登录
/// 或任何错误（尽力而为）时返回 `None`——`None` 不说明列表的任何情况，所以调用方不能把它
/// 当作「列表是空的」。`fileid` 是 `/v4/delete_songs` 端点要求的歌单内键。
async fn liked_entries_raw() -> Option<Vec<(String, i64)>> {
    let client = kugou::shared_client();
    let mut out = Vec::new();
    for page in 1..=LIKED_LIST_MAX_PAGES {
        match client
            .playlist_tracks(LIKED_LIST_ID, page, LIKED_LIST_PAGE_SIZE)
            .await
        {
            Ok(resp) => {
                let info = match resp.body.pointer("/data/info").and_then(Value::as_array) {
                    Some(info) => info,
                    // 第一页就缺 /data/info 是「载荷变形」而非「列表为空」：
                    // 与 Err 路径同语义返回 None，绝不落地空缓存并置
                    // LIKED_SET_INIT（否则本会话星标全灭且不自愈）。后续页的
                    // 缺失维持 break（不满一页的正常结束）。
                    None if page == 1 => return None,
                    None => break,
                };
                out.extend(info.iter().filter_map(|item| {
                    let hash = item
                        .get("hash")
                        .and_then(Value::as_str)
                        .filter(|hash| !hash.is_empty())?;
                    let fileid = item
                        .get("fileid")
                        .and_then(Value::as_i64)
                        .filter(|fileid| *fileid != 0)?;
                    Some((hash.to_string(), fileid))
                }));
                if info.len() < LIKED_LIST_PAGE_SIZE as usize {
                    break;
                }
            }
            // 有一页失败会让列表不完整：上报失败，这样调用方既不会用部分数据替换喜欢缓存，
            // 也不会把缺失的 fileid 当作「已经移除」。
            Err(_) => return None,
        }
    }
    Some(out)
}

/// 在 Tokio 运行时上包一层 `liked_entries_raw`（为什么每个 kugou 网络调用都必须 spawn 到它
/// 上面，见 UI 侧的 `fetch_online_lyric`）。拉取失败或任务被取消时返回 `None`。
pub(crate) async fn fetch_liked_entries() -> Option<Vec<(String, i64)>> {
    crate::RUNTIME
        .spawn(liked_entries_raw())
        .await
        .ok()
        .flatten()
}

/// 内存里「当前在用户酷狗喜欢列表中」的 hash 集合。留着它是为了让播放栏的喜欢按钮立即点亮，
/// 而不必每次换歌都等一个网络往返。
static LIKED_SET: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
/// 缓存填充过一次后置位；避免用户本来就是空列表时，每条在线曲目都重拉整个喜欢列表。
pub(crate) static LIKED_SET_INIT: AtomicBool = AtomicBool::new(false);

/// 内存里的喜欢 hash 集。`pub(crate)`：UI 侧的喜欢/取消喜欢动作经 `crate::ui::kugou` 的再
/// 导出直接改它。
pub(crate) fn liked_set() -> &'static RwLock<HashSet<String>> {
    LIKED_SET.get_or_init(|| RwLock::new(HashSet::new()))
}

/// 当前在喜欢列表里的 hash 对应的 fileid。移除 API（`delete_songs`）要 fileid，而它只有分页
/// 列表拉取才提供——把它与 hash 集并排缓存后，常见情况下取消喜欢能完全跳过那次拉取，不必
/// 每次取消都重拉最多 10 页完整歌曲实体。
static LIKED_FILEIDS: OnceLock<RwLock<HashMap<String, i64>>> = OnceLock::new();

pub(crate) fn liked_fileids() -> &'static RwLock<HashMap<String, i64>> {
    LIKED_FILEIDS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 缓存把 `hash` 记为已喜欢时返回 true。
pub fn liked_set_contains(hash: &str) -> bool {
    liked_set().read_recovered().contains(hash)
}

/// 喜欢缓存的世代计数。`invalidate_liked_cache` 在 UI 线程同步调用，拿不了
/// `liked_load_lock`（tokio 锁需要 `.await`），世代计数是它与在途抓取之间唯一的
/// 串行化交接点：invalidate 递增世代；抓取方开工前捕获、落地前比对，世代已变即
/// 整份丢弃——旧账号的整份抓取结果绝不回填进已清空的缓存。
static LIKED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 当前喜欢缓存世代（抓取方在开工前捕获）。
pub(crate) fn liked_generation() -> u64 {
    LIKED_GENERATION.load(Ordering::Relaxed)
}

/// 串行化首次喜欢列表加载，让并发的 `online_track_is_liked` 查询（例如缓存预热前的快速换歌）
/// 等第一次完整分页拉取跑完，而不是各自发一份。
pub(crate) fn liked_load_lock() -> &'static tokio::sync::Mutex<()> {
    static LIKED_LOAD_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LIKED_LOAD_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 从服务刷新内存里的喜欢集（尽力而为）。拉取失败会保留先前的缓存与 init 旗标，于是下次
/// 查询是重试，而不是相信一个空列表（那会静默地把每首歌都取消喜欢直到重启）。
pub(crate) async fn refresh_liked_set_from_service() {
    // 抓取开工前捕获世代：抓取期间发生 invalidate（登录/登出/换号）则结果整份作废。
    let generation = liked_generation();
    let Some(entries) = fetch_liked_entries().await else {
        return;
    };
    store_liked_entries(entries, generation);
}

/// 用一次分页拉取的结果同时替换两个喜欢缓存（hash 集 + hash→fileid 映射）。
/// `generation` 是抓取开工前捕获的世代：与当前不等（抓取期间发生了 invalidate）
/// 就整份丢弃且不置 init 旗标，下次惰性刷新拿新账号数据。比对与**两份写入
/// 加旗标**都持双写守卫完成（set→fileids，与 invalidate 同序），两条路径由此
/// 真正互斥——只守卫 set 半边的话，invalidate 落在 set 写入与 fileids 写入
/// 之间会留下「hash 集已清、fileids/INIT 却是旧账号」的半清状态（2026-10-25
/// 终审抓到的残余窗口），INIT 错置为 true 还会让惰性刷新整个会话不再自愈。
pub(crate) fn store_liked_entries(entries: Vec<(String, i64)>, generation: u64) {
    let mut hashes = HashSet::with_capacity(entries.len());
    let mut fileids = HashMap::with_capacity(entries.len());
    for (hash, fileid) in entries {
        hashes.insert(hash.clone());
        fileids.insert(hash, fileid);
    }
    let mut set_guard = liked_set().write_recovered();
    if generation != LIKED_GENERATION.load(Ordering::Relaxed) {
        return;
    }
    let mut fileids_guard = liked_fileids().write_recovered();
    *set_guard = hashes;
    *fileids_guard = fileids;
    // INIT 旗标必须留在双写守卫域内置位（与上方文档逐字对齐）：挪到 drop 之后，
    // invalidate 的「清空 + INIT=false」就能完整插进双写完成与旗标置位之间，把
    // 缓存钉死在「已初始化但为空」，惰性刷新整个会话不再自愈（CONC-1）。
    LIKED_SET_INIT.store(true, Ordering::Relaxed);
    drop(fileids_guard);
    drop(set_guard);
}

/// 账号身份变了（登录/登出/换号）时清空喜欢缓存：hash 集、hash→fileid 映射与
/// init 旗标一并复位（并发形态照 `store_liked_entries`：毒化可恢复写锁 + Relaxed
/// 旗标）。喜欢列表是以登录用户为键的服务端状态——星标是否点亮、unlike 要用的
/// fileid 都只对当前账号有效，换号后残留会让星标按旧账号点亮、取消喜欢删到旧
/// 账号的列表。清空、世代递增、fileids 清空与 INIT=false 全程持双写守卫
/// （set→fileids，与 `store_liked_entries` 同序，两条路径由此真正互斥）：旧写法
/// 把 fileids/INIT 留在守卫域外，在途 store 落在「双写完成与 INIT=false 之间」
/// 会把缓存钉死在「已初始化但为空」，惰性刷新整个会话不再自愈（CONC-1，
/// 2026-11-14）。在途抓取拿不了这把锁，靠世代在落地前自行丢弃。
pub(crate) fn invalidate_liked_cache() {
    let mut set_guard = liked_set().write_recovered();
    set_guard.clear();
    LIKED_GENERATION.fetch_add(1, Ordering::Relaxed);
    // fileids 清空与 INIT=false 收进同一段 set 写守卫（fileids 嵌套内层，锁序
    // set→fileids 与 store 同序；全仓普查仅此两处嵌套取锁，无其它锁序）。
    let mut fileids_guard = liked_fileids().write_recovered();
    fileids_guard.clear();
    LIKED_SET_INIT.store(false, Ordering::Relaxed);
    drop(fileids_guard);
    drop(set_guard);
}

/// 编译期 provider 注册表里的酷狗条目（A-1 第②步）；各方法体委托给上面的自由函数。
pub struct KugouSource;

#[async_trait::async_trait]
impl super::OnlineSourceProvider for KugouSource {
    fn handles(&self, identity: &super::OnlineIdentity) -> bool {
        matches!(identity, super::OnlineIdentity::Kugou { .. })
    }

    fn identify_path(&self, path: &Path) -> Option<super::OnlineTrackMatch> {
        let track = online_track_matching_path(path)?;
        Some(super::OnlineTrackMatch {
            identity: super::OnlineIdentity::Kugou {
                hash: track.hash.clone(),
                mix_song_id: track.mix_song_id,
                album_id: track.album_id,
            },
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
        // 酷狗是唯一编进来的 provider，所以注册表交给这个 trait 的永远是酷狗变体。
        let super::OnlineIdentity::Kugou {
            hash,
            mix_song_id,
            album_id,
        } = identity;

        let client = kugou::shared_client();
        let hash = hash.clone();
        let mix_song_id = *mix_song_id;
        let album_id = *album_id;
        let kugou_quality = ctx.kugou_quality.to_string();
        // spawn 出去的拉取用一份克隆；原件进下面的注册表条目。
        let fetch_hash = hash.clone();
        // 实时档单请求时限：本刷新的主要消费者是播放钩子的 6s 总预算
        // （STREAM_REFRESH_TIMEOUT），30s 档在慢网下永远赶不上；会话恢复路径
        // 失败的条目由播放线程按需重试兜底，同受此档约束无害。
        let url = crate::RUNTIME
            .spawn(async move {
                fetch_stream_url(
                    &client,
                    &fetch_hash,
                    mix_song_id,
                    album_id,
                    &kugou_quality,
                    Some(REALTIME_URL_REQUEST_TIMEOUT),
                )
                .await
            })
            .await
            .ok()
            .flatten()?;

        let (name, artist, duration, cover) = ctx.display.clone();
        // 展示四元组缺失的字段沿用注册表里原始注册的同曲条目，而不是落空串/0
        // 覆盖它们（A4 的 album 守卫 + 四轮审计 F3 把 title/artist/duration/cover
        // 补齐：unwrap_or_default 的空串经 upsert_meta 会抹掉库里正确的艺人）。
        // 必须在 remember_online_track 之前取：重登记会逐出同曲旧条目。
        let previous = previous_track_entry(mix_song_id);
        let album = previous
            .as_ref()
            .map(|e| SharedString::from(e.album.clone()))
            .unwrap_or_default();
        remember_online_track(
            url.clone(),
            KugouTrackInfo::new(
                name.or_else(|| previous.as_ref().map(|e| e.title.clone()))
                    .unwrap_or_default()
                    .into(),
                artist
                    .or_else(|| previous.as_ref().map(|e| e.artist.clone()))
                    .unwrap_or_default()
                    .into(),
                album,
                duration
                    .or(previous.as_ref().map(|e| e.duration))
                    .unwrap_or(0),
                hash,
                mix_song_id,
                album_id,
                cover
                    .or_else(|| previous.as_ref().map(|e| e.cover_url.clone()))
                    .unwrap_or_default()
                    .into(),
            ),
        );
        Some(url)
    }
}

#[cfg(test)]
mod liked_cache_tests {
    use super::*;

    /// 世代守卫（A1）：换号/登出时 invalidate 清空缓存并递增世代，在途抓取随后
    /// 带着旧世代完成——必须整份丢弃，不得把旧账号的喜欢集回填进已清空的缓存、
    /// 也不得置 init 旗标（否则星标按旧账号点亮，且不再自动重拉）。
    /// 同时钉住 CONC-1 的两条路径终态：store 落地后两份缓存与 INIT 旗标同步就位；
    /// invalidate 后 INIT=false 且 set/fileids 两份缓存全空——「已初始化但为空」
    /// 的钉死态正是本断言要堵的回归口（真实并发交错不可串行构造，只钉终态）。
    /// 串行执行依赖：本文件唯一触碰这些全局的测试；并行测试间无共享。
    #[test]
    fn stale_generation_store_is_discarded_after_invalidate() {
        // 基线：当前世代的结果正常落地（两份缓存 + INIT 旗标同步就位）。
        store_liked_entries(vec![("hash-a".into(), 11)], liked_generation());
        assert!(LIKED_SET_INIT.load(Ordering::Relaxed));
        assert!(liked_set_contains("hash-a"));
        assert!(liked_fileids().read_recovered().contains_key("hash-a"));

        // 换号：清空 + 世代递增，两份缓存与 INIT 旗标一并复位。
        invalidate_liked_cache();
        assert!(!LIKED_SET_INIT.load(Ordering::Relaxed));
        assert!(!liked_set_contains("hash-a"));
        assert!(liked_set().read_recovered().is_empty());
        assert!(liked_fileids().read_recovered().is_empty());

        // invalidate 之前开工的在途抓取此刻完成（携带旧世代）：整份丢弃。
        let stale_generation = liked_generation() - 1;
        store_liked_entries(vec![("hash-old".into(), 22)], stale_generation);
        assert!(!liked_set_contains("hash-old"), "旧世代的结果必须被丢弃");
        assert!(
            !liked_fileids().read_recovered().contains_key("hash-old"),
            "fileid 旁表同样不得回填"
        );
        assert!(
            !LIKED_SET_INIT.load(Ordering::Relaxed),
            "丢弃时不得置 init 旗标"
        );

        // 下一次刷新携带新世代：正常落地。
        store_liked_entries(vec![("hash-new".into(), 33)], liked_generation());
        assert!(liked_set_contains("hash-new"));
        assert!(liked_fileids().read_recovered().contains_key("hash-new"));
        assert!(LIKED_SET_INIT.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod url_parse_tests {
    //! 在线播放最后一跳的解析钉子：extract_song_url 认两种响应形态、
    //! mixsongid_in_url 从流 URL 里回溯身份（十五轮测试缺口矩阵的高危项）。
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_song_url_reads_array_string_and_skips_empties() {
        // /v5/url 形态：url 是数组，第一个非空者胜
        assert_eq!(
            extract_song_url(&json!({"url": ["", "http://a", "http://b"]})),
            Some("http://a".to_string())
        );
        // 字符串直连形态
        assert_eq!(
            extract_song_url(&json!({"url": "http://direct"})),
            Some("http://direct".to_string())
        );
        // 全空数组、url 缺失、url 为空串：None（调用方走兜底）
        assert_eq!(extract_song_url(&json!({"url": ["", ""]})), None);
        assert_eq!(extract_song_url(&json!({"other": 1})), None);
        assert_eq!(extract_song_url(&json!({"url": ""})), None);
    }

    #[test]
    fn mixsongid_in_url_extracts_digits_after_marker() {
        assert_eq!(mixsongid_in_url("http://x/_mx42abc?a=1"), Some(42));
        // 标记在结尾、后面全是数字
        assert_eq!(mixsongid_in_url("http://x/file_mx7"), Some(7));
        // 无标记、标记后无数字
        assert_eq!(mixsongid_in_url("http://x/file.mp3"), None);
        assert_eq!(mixsongid_in_url("http://x/_mx"), None);
    }

    #[test]
    fn kugou_track_info_precomputes_display_labels() {
        let short = KugouTrackInfo::new(
            "歌名".into(),
            "歌手".into(),
            "专辑".into(),
            185,
            "H1".into(),
            1,
            2,
            "".into(),
        );
        assert_eq!(
            short.cached_artist_album_label().map(|s| s.as_ref()),
            Some("歌手 · 专辑")
        );
        assert_eq!(short.cached_duration_label(false).as_ref(), "3:05");
        assert_eq!(short.cached_duration_label(true).as_ref(), "03:05");
        assert_eq!(short.cached_detail_label().as_ref(), "歌手 · 3:05");

        let long_no_artist = KugouTrackInfo::new(
            "长曲".into(),
            "".into(),
            "专辑".into(),
            3665,
            "H2".into(),
            2,
            3,
            "".into(),
        );
        assert!(long_no_artist.cached_artist_album_label().is_none());
        assert_eq!(
            long_no_artist.cached_duration_label(false).as_ref(),
            "1:01:05"
        );
        assert_eq!(
            long_no_artist.cached_duration_label(true).as_ref(),
            "1:01:05"
        );
        assert_eq!(long_no_artist.cached_detail_label().as_ref(), "1:01:05");

        // 负时长钳位到 0；599s 仍需补零，600s 起两版共享同一串
        let neg = KugouTrackInfo::new(
            "负".into(),
            "歌手".into(),
            "".into(),
            -5,
            "H3".into(),
            3,
            0,
            "".into(),
        );
        assert!(neg.cached_artist_album_label().is_none());
        assert_eq!(neg.cached_duration_label(false).as_ref(), "0:00");
        assert_eq!(neg.cached_duration_label(true).as_ref(), "00:00");
        assert_eq!(neg.cached_detail_label().as_ref(), "歌手 · 0:00");

        let border_599 = KugouTrackInfo::new(
            "A".into(),
            "B".into(),
            "C".into(),
            599,
            "H4".into(),
            4,
            0,
            "".into(),
        );
        assert_eq!(border_599.cached_duration_label(false).as_ref(), "9:59");
        assert_eq!(border_599.cached_duration_label(true).as_ref(), "09:59");

        let border_600 = KugouTrackInfo::new(
            "A".into(),
            "B".into(),
            "C".into(),
            600,
            "H4".into(),
            4,
            0,
            "".into(),
        );
        assert_eq!(border_600.cached_duration_label(false).as_ref(), "10:00");
        assert_eq!(border_600.cached_duration_label(true).as_ref(), "10:00");
        assert_ne!(border_599, border_600);
        assert_eq!(
            border_600,
            KugouTrackInfo::new(
                "A".into(),
                "B".into(),
                "C".into(),
                600,
                "H4".into(),
                4,
                0,
                "".into(),
            )
        );
    }
}
