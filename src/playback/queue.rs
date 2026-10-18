use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use gpui::SharedString;

/// [`QueueItemData`] 时长槽里"长度尚不可知"的哨兵值。
pub const UNKNOWN_DURATION: i64 = i64::MIN;

/// [`QueueItemData`] 每实例身份的单调分配器。uid 在构造时（含反序列化）一次性
/// 分配，`Clone` 复制它，既不序列化也不参与 [`PartialEq`]；它作为 UI 侧元数据
/// 旁表（`ui::queue_metadata`）的键，因此克隆体携带同一 uid，查找照样命中。
static NEXT_UID: AtomicU64 = AtomicU64::new(0);

/// 随在线队列条目一起持久化的展示元数据：`(name, artist, duration, cover)`。
/// 队列条目的 `persisted_display` 与流注册表重注册路径共用此类型。
#[cfg(feature = "online_sources")]
pub type PersistedDisplay = PersistedUiRestore;

#[derive(Clone, Debug)]
pub struct QueueItemData {
    /// 每实例身份（见 [`NEXT_UID`]）：作为 UI 侧元数据旁表的键，本条目的每个克隆
    /// 都落到同一个条目。由 `Clone` 复制，不参与序列化，也不参与内容相等比较。
    uid: u64,
    /// 条目来源曲目的数据库 ID，若存在。
    db_id: Option<i64>,
    /// 条目来源专辑的数据库 ID，若存在。
    db_album_id: Option<i64>,
    /// 曲目文件的路径。
    path: PathBuf,
    /// 曲目长度（秒），一旦可知即写入：建队列时写（取自库内曲目 / 在线元数据），
    /// 该条目的 UI 元数据加载完成后回写（`ui::queue_metadata`），队列汇总因此
    /// 永远不必去碰那些实体。在此之前为 [`UNKNOWN_DURATION`]。
    duration: Arc<AtomicI64>,
    /// 在线（酷狗）曲目没有 db id，展示元数据若不随路径显式持久化就会在重启后丢失。
    /// 队列变更时写入，加载时恢复到 UI 数据。
    persisted_ui: Option<PersistedQueueUIData>,
    /// 在线音源身份，持久化它是为了让签名流 URL 在应用关闭期间过期的队列条目，
    /// 能在恢复时重新取到一个新的。
    #[cfg(feature = "online_sources")]
    online_identity: Option<OnlineIdentity>,
}

// ①步下沉：OnlineIdentity 的定义已整体搬至 `crate::online_sources`（A-1
// 第①步，playback → online_sources 为允许的依赖方向），此处按原 cfg 门再
// 导出，保持本模块与全部既有使用点不变；②步收编（trait 身份解析）时移除。
#[cfg(feature = "online_sources")]
pub use crate::online_sources::OnlineIdentity;

/// 展示元数据中必须跨重启存活的那份 serde 友好拷贝（在线曲目没有库条目可供
/// 重新推导）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedQueueUIData {
    name: Option<String>,
    artist_name: Option<String>,
    cover_url: Option<String>,
    duration: Option<i64>,
}

/// `PersistedQueueUIData::persisted_restore` 的载荷：
/// `(name, artist_name, duration, cover_url)`。
pub(crate) type PersistedUiRestore = (Option<String>, Option<String>, Option<i64>, Option<String>);

impl serde::Serialize for QueueItemData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        // `uid` 刻意不序列化：它是每会话的实例身份，每次反序列化都重新分配一个。
        let mut state = serializer.serialize_struct(
            "QueueItemData",
            5 + usize::from(cfg!(feature = "online_sources")),
        )?;
        state.serialize_field("db_id", &self.db_id)?;
        state.serialize_field("db_album_id", &self.db_album_id)?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("persisted_ui", &self.persisted_ui)?;
        state.serialize_field("duration", &self.known_duration())?;
        #[cfg(feature = "online_sources")]
        state.serialize_field("online_identity", &self.online_identity)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for QueueItemData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct QueueItemDataRaw {
            #[serde(default)]
            db_id: Option<i64>,
            #[serde(default)]
            db_album_id: Option<i64>,
            path: PathBuf,
            #[serde(default)]
            persisted_ui: Option<PersistedQueueUIData>,
            #[serde(default)]
            duration: Option<i64>,
            #[cfg(feature = "online_sources")]
            #[serde(default)]
            online_identity: Option<OnlineIdentity>,
        }

        let raw = QueueItemDataRaw::deserialize(deserializer)?;
        let persisted_duration = raw
            .persisted_ui
            .as_ref()
            .and_then(|p| p.duration)
            .or(raw.duration);
        Ok(QueueItemData {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            db_id: raw.db_id,
            db_album_id: raw.db_album_id,
            path: raw.path,
            persisted_ui: raw.persisted_ui,
            duration: Arc::new(AtomicI64::new(
                persisted_duration.unwrap_or(UNKNOWN_DURATION),
            )),
            #[cfg(feature = "online_sources")]
            online_identity: raw.online_identity,
        })
    }
}

impl Display for QueueItemData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.path.to_str().unwrap_or("invalid path"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueItemUIData {
    /// 曲目关联的专辑 ID，若存在。
    pub album_id: Option<i64>,
    /// 曲目名称，若已知。
    pub name: Option<SharedString>,
    /// 艺术家名称，若已知。
    pub artist_name: Option<SharedString>,
    /// 曲目的元数据是取自文件还是数据库。
    pub source: DataSource,
    /// 曲目时长，单位秒。
    pub duration: Option<i64>,
    /// 在线（酷狗）封面图 URL，离线曲目为空。
    pub cover_url: Option<SharedString>,
}

/// 抽出在线展示元数据里需要跨重启存活的那部分。
#[cfg(feature = "online_sources")]
fn persist_from(ui: QueueItemUIData) -> Option<PersistedQueueUIData> {
    if ui.name.is_none()
        && ui.artist_name.is_none()
        && ui.cover_url.is_none()
        && ui.duration.is_none()
    {
        return None;
    }
    Some(PersistedQueueUIData {
        name: ui.name.map(|s| s.to_string()),
        artist_name: ui.artist_name.map(|s| s.to_string()),
        cover_url: ui.cover_url.map(|s| s.to_string()),
        duration: ui.duration,
    })
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum DataSource {
    /// 元数据直接读自文件。
    Metadata,
    /// 元数据读自音乐库数据库。
    Library,
}

impl PartialEq for QueueItemData {
    // 只比内容：`uid` 是实例身份（内容相同的两个条目仍是不同实例），故刻意排除。
    fn eq(&self, other: &Self) -> bool {
        self.db_id == other.db_id
            && self.db_album_id == other.db_album_id
            && self.path == other.path
    }
}

impl QueueItemData {
    /// 用给定信息新建一个 `QueueItemData`。这里不创建任何 UI 元数据实体——
    /// `ui::queue_metadata` 旁表在首次使用时惰性建一个，与反序列化路径一致。
    /// 可在 UI 线程之外调用（Shuffle All 这类批量构建）：每曲目一个 GPUI 实体
    /// 纯属启动开销，预先建个空壳实体什么也没换来——原先独立的 `lazy` 构造器
    /// 已并入这一个。
    pub fn new(path: PathBuf, db_id: Option<i64>, db_album_id: Option<i64>) -> Self {
        QueueItemData {
            path,
            db_id,
            db_album_id,
            persisted_ui: None,
            duration: Arc::new(AtomicI64::new(UNKNOWN_DURATION)),
            // 所有非反序列化构造的唯一分配点：每构造一个实例就取一个新 uid。
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            #[cfg(feature = "online_sources")]
            online_identity: None,
        }
    }

    /// 创建一个 UI 元数据就已知的队列条目（例如从远端服务取回的在线曲目），
    /// 因此后续不再安排数据库或磁盘查找。这里同样不创建实体：元数据被播进
    /// `persisted_ui`（外加时长单元），旁表首次解析（`ui::queue_metadata`）时
    /// 经 persisted-restore 分支据此重建实体内容——与本构造器原先的即时填充
    /// 逐字段一致（`album_id: None` 与 `source: Metadata` 是两个调用点传入的、
    /// 也是该分支恢复出来的值）。
    #[cfg(feature = "online_sources")]
    pub fn with_ui_data(path: PathBuf, ui_data: QueueItemUIData) -> Self {
        let duration = Arc::new(AtomicI64::new(ui_data.duration.unwrap_or(UNKNOWN_DURATION)));
        let persisted_ui = persist_from(ui_data);
        QueueItemData {
            persisted_ui,
            duration,
            ..Self::new(path, None, None)
        }
    }

    /// 挂上在线音源身份，使过期的流 URL 之后（例如会话恢复时）能够重新取回。
    #[cfg(feature = "online_sources")]
    pub fn with_online_identity(mut self, identity: OnlineIdentity) -> Self {
        self.online_identity = Some(identity);
        self
    }

    /// 若本条目是 HTTP 流，给出其在线音源身份。
    #[cfg(feature = "online_sources")]
    pub fn online_identity(&self) -> Option<&OnlineIdentity> {
        self.online_identity.as_ref()
    }

    /// 随条目持久化的展示元数据：(name, artist, duration, cover)。
    #[cfg(feature = "online_sources")]
    pub fn persisted_display(&self) -> Option<PersistedDisplay> {
        self.persisted_restore()
    }

    /// 本条目的每实例身份：UI 侧元数据旁表（`ui::queue_metadata`）存放本条目
    /// UI 状态槽所用的键。克隆之间保持稳定。
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// 共享时长单元的克隆：旁表派发的后台加载器据此把曲目长度写回，无需持有
    /// 条目本体（写的正是 [`Self::set_known_duration`] 透写的那个单元）。
    pub fn duration_cell(&self) -> Arc<AtomicI64> {
        self.duration.clone()
    }

    /// 把已持久化的 UI 载荷 (name, artist, duration, cover) 作为普通元组给出，
    /// 与私有的 `PersistedQueueUIData` 解耦，好让 UI 侧元数据旁表
    /// （`ui::queue_metadata`）跑同一条恢复分支。不加 feature 门控：
    /// `persisted_ui` 本身也没有。
    pub(crate) fn persisted_restore(&self) -> Option<PersistedUiRestore> {
        self.persisted_ui.as_ref().map(|p| {
            (
                p.name.clone(),
                p.artist_name.clone(),
                p.duration,
                p.cover_url.clone(),
            )
        })
    }

    /// 已知的曲目长度（秒），元数据仍在加载时为 `None`。开销极低：一次原子读，
    /// 除引用计数外没有锁。
    pub fn known_duration(&self) -> Option<i64> {
        match self.duration.load(Ordering::Relaxed) {
            UNKNOWN_DURATION => None,
            secs => Some(secs),
        }
    }

    /// 写入曲目长度（秒）；传 `None` 即复位为 [`UNKNOWN_DURATION`]。
    pub fn set_known_duration(&self, secs: Option<i64>) {
        self.duration
            .store(secs.unwrap_or(UNKNOWN_DURATION), Ordering::Relaxed);
    }

    /// 返回队列条目的文件路径。
    pub fn get_path(&self) -> &PathBuf {
        &self.path
    }

    /// 替换所存路径。酷狗在线流程用它刷新已入队曲目的（会过期的）流 URL，
    /// 而不是再排入一个重复条目。
    pub fn replace_path(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// 返回队列条目的专辑 ID，若存在。
    pub fn get_db_album_id(&self) -> Option<i64> {
        self.db_album_id
    }

    /// 返回队列条目的曲目 ID，若存在。
    pub fn get_db_id(&self) -> Option<i64> {
        self.db_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item(tag: &str) -> QueueItemData {
        QueueItemData::new(
            PathBuf::from(format!("/tmp/meliora-uid-test-{tag}.flac")),
            Some(7),
            None,
        )
    }

    #[test]
    fn uid_is_unique_per_construction() {
        // 每次构造都取一个全新的实例身份，即便两条目内容相同——
        // ui::queue_metadata 旁表正是以它作键
        let a = sample_item("a");
        let b = sample_item("a");
        assert_ne!(a.uid(), b.uid());
        // 内容相等刻意忽略 uid
        assert_eq!(a, b);
    }

    #[test]
    fn uid_is_shared_by_clones() {
        let a = sample_item("a");
        assert_eq!(a.uid(), a.clone().uid());
    }

    #[test]
    fn deserialization_allocates_a_fresh_uid() {
        let a = sample_item("a");
        let json = serde_json::to_string(&a).unwrap();
        let restored: QueueItemData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, a);
        assert_ne!(restored.uid(), a.uid());
    }

    #[cfg(feature = "online_sources")]
    #[test]
    fn with_ui_data_seeds_persisted_ui_for_first_frame_restore() {
        // 即时创建的实体空壳被移除后，在线曲目的展示元数据改由旁表首帧从
        // 已持久化的种子恢复——本测试锁定的就是这个种子本身
        let item = QueueItemData::with_ui_data(
            PathBuf::from("/tmp/meliora-online.flac"),
            QueueItemUIData {
                album_id: None,
                name: Some("Track".into()),
                artist_name: Some("Artist".into()),
                source: DataSource::Metadata,
                duration: Some(42),
                cover_url: None,
            },
        );
        let (name, artist, duration, _cover) = item.persisted_restore().expect("seeded");
        assert_eq!(name.as_deref(), Some("Track"));
        assert_eq!(artist.as_deref(), Some("Artist"));
        assert_eq!(duration, Some(42));
    }
}
