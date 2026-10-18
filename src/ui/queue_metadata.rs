//! 队列条目的 UI 侧元数据旁表，以 [`QueueItemData::uid`] 为键（P2-B 第 2 步）。
//!
//! 每个队列条目的 `Entity<Option<QueueItemUIData>>` 句柄存在一张以条目 uid 为键的
//! `Global` map 里，所以一次查找能活过克隆（每个克隆都带同一个 uid）。过去住在
//! `playback::queue::QueueItemData` 里的那个「每条目实体槽位」已在 P2-B 第 6 步
//! 移除——本表是队列条目 UI 元数据的唯一居所。后台加载器（库查询 + 磁盘读 +
//! 实体回填）随之搬到这里：它们是 UI 语义，且它们离开之后 `playback/queue.rs`
//! 不再触碰任何 gpui 实体。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use futures::TryFutureExt as _;
use gpui::{App, AppContext, Entity, Global, SharedString};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{error, trace_span};

use crate::library::Pool;
use crate::library::db;
use crate::media::{
    lookup_table::try_open_media, metadata::Metadata, traits::MediaProviderFeatures,
};
use crate::playback::queue::{DataSource, QueueItemData, QueueItemUIData, UNKNOWN_DURATION};

/// 把队列条目 uid 映射到其 UI 元数据槽位的旁表。作为独立的 [`Global`] 存在
/// （与 `PlaybackInterface` / `StatsHandle` / `Pool` 同一模式），而不是
/// `models::Queue` 的一个字段——后者的实体会在每次插入时通知所有订阅者
/// （O(n) 扇出）。
#[derive(Default)]
pub struct QueueMetadataStore(FxHashMap<u64, Entity<Option<QueueItemUIData>>>);

impl Global for QueueMetadataStore {}

/// 从以 [`QueueItemData::uid`] 为键的表里返回该条目的 UI 元数据槽位，首次使用时
/// 创建空壳，并按三条分支播种（持久化恢复 → 库加载 → 磁盘元数据加载）。
pub fn queue_ui_data(cx: &mut App, item: &QueueItemData) -> Entity<Option<QueueItemUIData>> {
    let uid = item.uid();

    // ensure：查这个槽位，首次触到时创建（并插入）一个空壳。
    // `default_global` 惰性地把 store 具现化。
    let existing = cx
        .default_global::<QueueMetadataStore>()
        .0
        .get(&uid)
        .cloned();
    let entity = match existing {
        Some(entity) => entity,
        None => {
            let shell = cx.new(|_| None);
            cx.default_global::<QueueMetadataStore>()
                .0
                .insert(uid, shell.clone());
            shell
        }
    };

    // 快路径：槽位已持有数据，于是直接返回句柄，不克隆 path/persisted-ui/duration
    // 这几份载荷（它们只有下面的播种路径才需要）。
    if entity.read(cx).is_some() {
        return entity;
    }

    let track_id = item.get_db_id();
    let album_id = item.get_db_album_id();
    let path = item.get_path().clone();
    let persisted = item.persisted_restore();
    let duration = item.duration_cell();
    entity.update(cx, move |m, cx| {
        // 数据已经有了，就退出本函数
        if m.is_some() {
            return;
        }

        // 在线（酷狗）曲目不带 db id，所以恢复随队列条目一起持久化下来的元数据，
        // 而不是去做一次注定失败的磁盘读
        if let Some((name, artist, dur, cover)) = persisted
            && track_id.is_none()
        {
            duration.store(dur.unwrap_or(UNKNOWN_DURATION), Ordering::Relaxed);
            *m = Some(QueueItemUIData {
                album_id: None,
                name: name.map(SharedString::from),
                artist_name: artist.map(SharedString::from),
                source: DataSource::Metadata,
                duration: dur,
                cover_url: cover.map(SharedString::from),
            });
            return;
        }

        *m = Some(QueueItemUIData {
            album_id: None,
            name: None,
            artist_name: None,
            source: DataSource::Library,
            duration: None,
            cover_url: None,
        });

        // 数据库 id 已知时就能从数据库取数据
        if let (Some(track_id), Some(album_id)) = (track_id, album_id) {
            // 那两次库查询已离开 UI 线程（准则 §2.3/§14）：占位符一直渲染到
            // 后台加载回填，与下面那条磁盘元数据路径完全一致
            cx.notify();
            spawn_library_load(track_id, album_id, path, cx.entity(), duration.clone(), cx);
            return;
        }

        // 关键信息仍空缺时，试着从磁盘取元数据——慢得多，在 Windows 上尤其慢
        spawn_metadata_load(path, cx.entity(), duration.clone(), cx);
    });

    entity
}

/// 取该条目槽位实体里的时长，但仅当该实体已存在于表中——绝不作为副作用新建一个。
/// 在渲染期间读它会注册依赖，于是读取方视图会在元数据逐个落地时重新计算。
pub fn queue_loaded_duration(cx: &App, item: &QueueItemData) -> Option<i64> {
    let store = cx.try_global::<QueueMetadataStore>()?;
    let entity = store.0.get(&item.uid())?;
    entity.read(cx).as_ref()?.duration
}

/// 清空该条目槽位的内容（空壳与它的 uid 键仍留在表里），于是条目再次经
/// [`queue_ui_data`] 解析时数据会被重新取回。
pub fn queue_drop_data(cx: &mut App, item: &QueueItemData) {
    let Some(entity) = cx
        .try_global::<QueueMetadataStore>()
        .and_then(|store| store.0.get(&item.uid()).cloned())
    else {
        return;
    };
    entity.update(cx, |m, cx| {
        *m = None;
        cx.notify();
    });
}

/// 只保留 uid 在 `live` 里的槽位（队列面板的 `rescan_views_and_summary` 传入
/// 当前队列条目的 uid 集合）；其余全部丢弃，从而释放那些实体句柄。取
/// `&mut App`，因为 gpui 没有能用 `&App` 修改 `Global` 的路径。
pub(crate) fn gc_retaining(cx: &mut App, live: impl Iterator<Item = u64>) {
    let live: FxHashSet<u64> = live.collect();
    let store = cx.default_global::<QueueMetadataStore>();
    store.0.retain(|uid, _| live.contains(uid));
}

#[tracing::instrument(level = "trace")]
fn read_metadata(path: &Path) -> anyhow::Result<QueueItemUIData> {
    let mut stream = try_open_media(path, MediaProviderFeatures::PROVIDES_METADATA)?
        .ok_or_else(|| anyhow::anyhow!("no metadata provider for {}", path.display()))?;
    stream.start_playback()?;

    let mut metadata = stream.read_metadata()?;
    // 在线（酷狗）路径是 URL——用文件名兜底会把 URL 解析成显示名
    if !crate::media::is_http_path(path) {
        metadata.fill_from_filename(path);
    }
    let Metadata {
        name,
        artist,
        album_artist,
        ..
    } = metadata;
    let ui_data = QueueItemUIData {
        name: name.as_ref().map(Into::into),
        artist_name: artist.as_ref().or(album_artist.as_ref()).map(Into::into),
        source: DataSource::Metadata,
        album_id: None,
        duration: stream.duration_ms().ok().map(|ms| ms as i64 / 1_000),
        cover_url: None,
    };

    Ok(ui_data)
}

/// 在后台从磁盘加载一个队列条目的显示元数据，完成时填充 `entity`。避免把 UI
/// 线程阻塞在 Windows 的慢读取上。
fn spawn_metadata_load(
    path: PathBuf,
    entity: Entity<Option<QueueItemUIData>>,
    duration: Arc<AtomicI64>,
    cx: &mut App,
) {
    let span = trace_span!("read_metadata_outer", path = %path.display());
    let task = crate::RUNTIME.spawn_blocking(move || read_metadata(&path));
    cx.spawn(async move |cx| match task.err_into().await.flatten() {
        Err(err) => error!(parent: span, ?err, "Failed to read metadata: {err}"),
        Ok(metadata) => {
            duration.store(
                metadata.duration.unwrap_or(UNKNOWN_DURATION),
                Ordering::Relaxed,
            );
            entity.update(cx, |m, cx| {
                *m = Some(metadata);
                cx.notify();
            });
        }
    })
    .detach();
}

/// 在后台从库数据库加载一个队列条目的显示元数据（一行曲目加一行专辑），完成时
/// 填充 `entity`。与 [`spawn_metadata_load`] 同形：它取代的那些同步
/// `LibraryAccess` 调用，会让 UI 线程在每行队列首次渲染时停在两次
/// `RUNTIME.block_on` 上（准则 §2.3/§14）。已从库里消失的行，或加载后艺人仍为空的
/// 行，都照旧的 inline 路径那样回退到磁盘元数据加载。
fn spawn_library_load(
    track_id: i64,
    album_id: i64,
    path: PathBuf,
    entity: Entity<Option<QueueItemUIData>>,
    duration: Arc<AtomicI64>,
    cx: &mut App,
) {
    let pool = cx.global::<Pool>().0.clone();
    let task = crate::RUNTIME.spawn(async move {
        let track = db::get_track_by_id(&pool, track_id).await;
        let album = db::get_album_by_id(&pool, album_id).await;
        (track.ok(), album.ok())
    });
    cx.spawn(async move |cx| {
        let Ok((track, album)) = task.await else {
            return; // 加载任务 panic：占位项留在原位
        };
        entity.update(cx, |m, cx| {
            let (Some(track), Some(album)) = (track, album) else {
                spawn_metadata_load(path, cx.entity(), duration, cx);
                return;
            };

            let Some(data) = m.as_mut() else {
                // 实体其间已被丢弃；没什么可填的
                return;
            };
            data.name = Some(track.title.clone().0);
            data.album_id = Some(album.id);
            data.duration = Some(track.duration);
            duration.store(track.duration, Ordering::Relaxed);

            if let Some(artist_name) = track
                .artist_names
                .clone()
                .or_else(|| album.artist_display_override.clone())
            {
                data.artist_name = Some(artist_name.0);
            }

            cx.notify();

            // 艺人信息仍然空缺，试着从磁盘取元数据（旧 inline 路径跑的就是这个兜底）
            if data.artist_name.is_none() {
                spawn_metadata_load(path, cx.entity(), duration, cx);
            }
        });
    })
    .detach();
}
