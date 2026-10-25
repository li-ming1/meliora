pub mod queries;
mod recorder;

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{AsyncApp, Entity, Global};
use tracing::warn;

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

/// 在途异步 flush 的计数：[`flush_rows_async`] 登记 +1、任务收尾 -1 并唤醒
/// 退出等待者。此前的裸 spawn 无人追踪——退出前某次 flush 的 spawn 尚未被
/// 调度执行时，那批行（≤60s 收听）随进程退出无声丢失且无日志。
static FLUSH_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
/// 退出等待的低频唤醒通道。计数才是事实源，这里只负责"睡了就叫醒"，
/// 唤醒丢失由等待循环里的计数复检兜底，不依赖唤醒语义的精确性。
static FLUSH_DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());
/// 退出路径等在途 flush 清零的上限：超时 warn 放行——不为一行收听统计
/// 拖死退出。
const FLUSH_EXIT_WAIT: Duration = Duration::from_millis(500);

/// 在 UI 线程之外持久化已完成的行；空批次是空操作。spawn 计入在途计数，
/// 退出路径经 [`drain_inflight_flushes_for_exit`] 等它清零。
pub fn flush_rows_async(pool: sqlx::SqlitePool, rows: Vec<ListenRow>) {
    if rows.is_empty() {
        return;
    }
    FLUSH_INFLIGHT.fetch_add(1, Ordering::Release);
    crate::RUNTIME.spawn(async move {
        queries::write_rows(&pool, rows).await;
        FLUSH_INFLIGHT.fetch_sub(1, Ordering::Release);
        FLUSH_DONE.1.notify_all();
    });
}

/// 等待在途的收听统计 flush 清零，最多 `timeout`（超时 warn 放行）。
fn wait_inflight_flushes(timeout: Duration) {
    // 锁毒化：等不了就不等，退出路径不为它加新的失败模式。
    let Ok(mut guard) = FLUSH_DONE.0.lock() else {
        return;
    };
    let deadline = Instant::now() + timeout;
    while FLUSH_INFLIGHT.load(Ordering::Acquire) > 0 {
        let now = Instant::now();
        if now >= deadline {
            warn!(
                in_flight = FLUSH_INFLIGHT.load(Ordering::Acquire),
                "exit drain: stats flush still in flight, proceeding (rows may be lost)"
            );
            return;
        }
        let (next, _) = FLUSH_DONE
            .1
            .wait_timeout(guard, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard = next;
    }
}

/// 退出路径的收口：先排空在途的收听统计 flush 再继续退出动作。两处退出
/// 路径（主窗释放与硬 cap 阀门 exit(70)）都经 settings::flush_pending_settings
/// 路过这里——spawn 出去还没轮到执行的 flush 不等走就会随进程退出无声
/// 丢失。刻意同步等待（Condvar 而非 tokio Notify）：exit(70) 的探针任务
/// 本来就跑在 RUNTIME 上，再 block_on 进运行时会 panic；清零由 RUNTIME
/// 的另一个工作线程完成并经条件变量唤醒（多线程运行时，2 个 worker）。
pub fn drain_inflight_flushes_for_exit() {
    wait_inflight_flushes(FLUSH_EXIT_WAIT);
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
        // 已知口径限制：mix_song_id=0（端点省略 id 的哨兵，见 online_sources/kugou.rs
        // 的注册表守卫）的曲目全部并进 kugou:0 一个键——去重侧已加 0 守卫（ui/kugou.rs），
        // 统计侧改键会制造历史孤儿行，维持现状。
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

#[cfg(test)]
mod tests {
    use super::{FLUSH_DONE, FLUSH_INFLIGHT, wait_inflight_flushes};
    use std::{
        sync::Mutex,
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    /// 串行化触碰在途计数 statics 的测试（#[test] 并行跑会互踩）。
    static INFIGHT_LOCK: Mutex<()> = Mutex::new(());

    /// 计数为零时立即返回：空闲退出不为统计多等一毫秒。
    #[test]
    fn wait_inflight_flushes_returns_immediately_when_idle() {
        let _guard = INFIGHT_LOCK.lock().unwrap();
        FLUSH_INFLIGHT.store(0, Ordering::Release);
        let started = Instant::now();
        wait_inflight_flushes(Duration::from_millis(500));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    /// 在途计数非零且无人清零：等到超时 warn 放行（退出被拖死比丢一行
    /// 统计更糟），计数原样保留交还给运行时。
    #[test]
    fn wait_inflight_flushes_times_out_when_counter_stuck() {
        let _guard = INFIGHT_LOCK.lock().unwrap();
        FLUSH_INFLIGHT.store(2, Ordering::Release);
        let started = Instant::now();
        wait_inflight_flushes(Duration::from_millis(100));
        assert!(started.elapsed() >= Duration::from_millis(90));
        assert_eq!(FLUSH_INFLIGHT.load(Ordering::Acquire), 2);
        FLUSH_INFLIGHT.store(0, Ordering::Release);
    }

    /// 在途任务收尾（计数清零 + 条件变量唤醒）必须早于超时返回：这是
    /// 退出等待的正常路径，超时只是兜底。
    #[test]
    fn wait_inflight_flushes_wakes_on_completion() {
        let _guard = INFIGHT_LOCK.lock().unwrap();
        FLUSH_INFLIGHT.store(1, Ordering::Release);
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            FLUSH_INFLIGHT.fetch_sub(1, Ordering::Release);
            FLUSH_DONE.1.notify_all();
        });
        let started = Instant::now();
        wait_inflight_flushes(Duration::from_secs(3));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "收尾唤醒必须在超时前返回"
        );
        assert_eq!(FLUSH_INFLIGHT.load(Ordering::Acquire), 0);
    }
}
