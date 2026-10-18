//! 酷狗共享在线源层：流 URL → 曲目注册表（「stream map」）、播放 URL 获取与喜欢列表缓存。
//! 由 `crate::ui::kugou` 下沉而来（A-1 第①步），因此播放线程与统计解析流时不必碰 UI；
//! UI 仍经 `crate::ui::kugou` 的再导出调用这些。整体受 `kugou` cargo feature 门控。

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::SharedString;
use serde_json::Value;

use super::stream_registry::{STREAM_MAP_CAP, StreamMapPersistence};
use crate::kugou;
use crate::sync::RecoveredRwLock;

/// 搜索结果与歌单里展示的一条在线曲目。
#[derive(Clone, Debug, PartialEq)]
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
    crate::RUNTIME.spawn(async move {
        let snapshot = stream_map().read_recovered().clone();
        STREAM_MAP_PERSISTENCE.persist_snapshot(stream_map_path(), &snapshot, "kugou");
    });
}

/// 若 `path` 是经本应用到达的流，返回为它记下的在线曲目。匹配先用嵌在 URL 里的
/// `mixsongid`（与队列去重逻辑同一启发式），再退化到逐字节 URL 比较，所以即使重定向目标
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

/// 按请求的 `quality` 为酷狗曲目取一个可播放 URL，更高档位不可用时（例如没有 VIP）退化到
/// 标准 128 kbps 片段。完全没取回可播放 URL 时返回 `None`。实时播放与会话恢复期的 URL
/// 刷新共用。
pub async fn fetch_stream_url(
    client: &kugou::KugouClient,
    hash: &str,
    mix_song_id: i64,
    album_id: i64,
    quality: &str,
) -> Option<String> {
    let free_part = !client.has_active_vip();
    match client
        .song_url(hash, mix_song_id, album_id, quality, free_part)
        .await
    {
        Ok(resp) => {
            if let Some(url) = extract_song_url(&resp.body) {
                return Some(url);
            }
            tracing::debug!(hash, quality, "kugou: response carried no playable URL");
        }
        Err(error) => {
            tracing::debug!(%error, hash, quality, "kugou: URL request failed");
        }
    }
    if quality != FALLBACK_QUALITY {
        let resp = match client
            .song_url(hash, mix_song_id, album_id, FALLBACK_QUALITY, free_part)
            .await
        {
            Ok(resp) => resp,
            Err(error) => {
                tracing::warn!(
                    %error,
                    hash,
                    quality = FALLBACK_QUALITY,
                    "kugou: fallback URL request failed"
                );
                return None;
            }
        };
        let url = extract_song_url(&resp.body);
        if url.is_some() {
            tracing::debug!(
                hash,
                requested = quality,
                "kugou: fell back to the standard quality"
            );
        }
        return url;
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
                let Some(info) = resp.body.pointer("/data/info").and_then(Value::as_array) else {
                    break;
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

/// 串行化首次喜欢列表加载，让并发的 `online_track_is_liked` 查询（例如缓存预热前的快速换歌）
/// 等第一次完整分页拉取跑完，而不是各自发一份。
pub(crate) fn liked_load_lock() -> &'static tokio::sync::Mutex<()> {
    static LIKED_LOAD_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LIKED_LOAD_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 从服务刷新内存里的喜欢集（尽力而为）。拉取失败会保留先前的缓存与 init 旗标，于是下次
/// 查询是重试，而不是相信一个空列表（那会静默地把每首歌都取消喜欢直到重启）。
pub(crate) async fn refresh_liked_set_from_service() {
    let Some(entries) = fetch_liked_entries().await else {
        return;
    };
    store_liked_entries(entries);
}

/// 用一次分页拉取的结果同时替换两个喜欢缓存（hash 集 + hash→fileid 映射）。
pub(crate) fn store_liked_entries(entries: Vec<(String, i64)>) {
    let mut hashes = HashSet::with_capacity(entries.len());
    let mut fileids = HashMap::with_capacity(entries.len());
    for (hash, fileid) in entries {
        hashes.insert(hash.clone());
        fileids.insert(hash, fileid);
    }
    *liked_set().write_recovered() = hashes;
    *liked_fileids().write_recovered() = fileids;
    LIKED_SET_INIT.store(true, Ordering::Relaxed);
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
        let url = crate::RUNTIME
            .spawn(async move {
                fetch_stream_url(&client, &fetch_hash, mix_song_id, album_id, &kugou_quality).await
            })
            .await
            .ok()
            .flatten()?;

        let (name, artist, duration, cover) = ctx.display.clone();
        remember_online_track(
            url.clone(),
            KugouTrackInfo {
                title: name.unwrap_or_default().into(),
                artist: artist.unwrap_or_default().into(),
                album: gpui::SharedString::default(),
                duration: duration.unwrap_or(0),
                hash,
                mix_song_id,
                album_id,
                cover_url: cover.unwrap_or_default().into(),
            },
        );
        Some(url)
    }
}
