pub mod queries;
mod recorder;

use std::sync::{Arc, Mutex};

use gpui::{AsyncApp, Entity, Global};

#[cfg(feature = "online_sources")]
use crate::online_sources::{OnlineIdentity, identify_path};
use crate::ui::models::Queue;

pub use recorder::ListenRow;
use recorder::StatsRecorder;

/// 收听统计开始的年份；热力图的年份翻页器到此为止。
pub fn epoch_year() -> i32 {
    2026
}

/// [`epoch_year`] 年 8 月 1 日本地午夜的 Unix 秒，即收听统计开始追踪的第一天。
/// 早于此的一切都被每个查询和热力图窗口忽略（该功能于 2026-08 上线）。
pub fn epoch_ts() -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(epoch_year(), 8, 1, 0, 0, 0)
        .earliest()
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

/// 附在收听行上的展示元数据：切歌时从在线 provider 注册表抓取，本地曲目则在写入
/// 时从音乐库解析。
#[derive(Clone, Debug, Default)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// 共享的记录器句柄：播放事件循环在 UI 线程喂它，应用退出钩子把它排空。
pub struct StatsHandle(pub Arc<Mutex<StatsRecorder>>);

impl Global for StatsHandle {}

impl StatsHandle {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(StatsRecorder::new())))
    }
}

/// 在 UI 线程之外持久化已完成的行；空批次是空操作。
pub fn flush_rows_async(pool: sqlx::SqlitePool, rows: Vec<ListenRow>) {
    if rows.is_empty() {
        return;
    }
    crate::RUNTIME.spawn(async move {
        queries::write_rows(&pool, rows).await;
    });
}

/// 刚刚开始的曲目的身份 + 展示元数据，作为 `listen_event.track_key` 的键。本地
/// 文件用路径作键；在线流用 provider 身份作键，并在缺失时退回 URL 哈希。
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
            q.data
                .read()
                .ok()
                .and_then(|items| items.iter().find(|item| item.get_path() == path).cloned())
        });

        let identity = item.as_ref().and_then(|item| item.online_identity());
        let key = match identity {
            Some(OnlineIdentity::Kugou { mix_song_id, .. }) => format!("kugou:{mix_song_id}"),
            None => return (format!("hash:{}", url_hash(path)), None),
        };

        // 编译期注册表会查每个已编译进来 provider 的流映射；命中就意味着这个身份
        // （URL 只存在于产生它的那个服务的映射里）。注册表不认识该 URL 时，退回
        // 队列条目已持久化的展示元数据。
        let meta = identify_path(path)
            .map(|m| TrackMeta {
                title: m.title,
                artist: m.artist,
                album: m.album,
            })
            .or_else(|| {
                item.as_ref().and_then(|item| item.persisted_display()).map(
                    |(name, artist, _, _)| TrackMeta {
                        title: name.unwrap_or_default(),
                        artist: artist.unwrap_or_default(),
                        album: String::new(),
                    },
                )
            });
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
