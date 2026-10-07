use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use gpui::SharedString;

/// Sentinel for "length not known yet" in [`QueueItemData`]'s duration slot.
pub const UNKNOWN_DURATION: i64 = i64::MIN;

/// Monotonic allocator for [`QueueItemData`]'s per-instance identity. Uids are
/// assigned once at construction (including deserialization), copied by
/// `Clone`, never serialized and never compared by [`PartialEq`]; they key the
/// UI-side metadata side table (`ui::queue_metadata`) so a lookup survives
/// clones, which carry the same uid.
static NEXT_UID: AtomicU64 = AtomicU64::new(0);

/// Display metadata persisted with an online queue item:
/// `(name, artist, duration, cover)`. Shared by the queue item's
/// `persisted_display` and the stream registry re-registration path.
#[cfg(feature = "online_sources")]
pub type PersistedDisplay = (Option<String>, Option<String>, Option<i64>, Option<String>);

#[derive(Clone, Debug)]
pub struct QueueItemData {
    /// Per-instance identity (see [`NEXT_UID`]): keys the UI-side metadata
    /// side table so every clone of this item resolves to the same entry.
    /// Copied by `Clone`, excluded from serialization and content equality.
    uid: u64,
    /// The database ID of track the item is from, if it exists.
    db_id: Option<i64>,
    /// The database ID of album the item is from, if it exists.
    db_album_id: Option<i64>,
    /// The path to the track file.
    path: PathBuf,
    /// Track length in seconds once known: written at queue-build time (from
    /// the library track / online metadata) and written back when the item's
    /// UI metadata load lands (`ui::queue_metadata`), so the queue summary
    /// never needs to touch those entities. [`UNKNOWN_DURATION`] until then.
    duration: Arc<AtomicI64>,
    /// Online (KuGou) tracks have no db id, so their display metadata would be
    /// lost across a restart unless persisted explicitly alongside the path.
    /// Written on queue-mutation and restored into the UI data on load.
    persisted_ui: Option<PersistedQueueUIData>,
    /// Online-provider identity, persisted so a queue item whose signed stream
    /// URL expired while the app was off can re-fetch a fresh one on restore.
    #[cfg(feature = "online_sources")]
    online_identity: Option<OnlineIdentity>,
}

// ①步下沉：OnlineIdentity 的定义已整体搬至 `crate::online_sources`（A-1
// 第①步，playback → online_sources 为允许的依赖方向），此处按原 cfg 门再
// 导出，保持本模块与全部既有使用点不变；②步收编（trait 身份解析）时移除。
#[cfg(feature = "online_sources")]
pub use crate::online_sources::OnlineIdentity;

/// Serde-friendly copy of the display metadata that must survive a restart
/// for online tracks (which have no library entry to re-derive it from).
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
        // `uid` is deliberately not serialized: it is per-session instance
        // identity, re-allocated fresh on every deserialization.
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
    /// The album ID associated with the track, if it exists.
    pub album_id: Option<i64>,
    /// The name of the track, if it is known.
    pub name: Option<SharedString>,
    /// The name of the artist, if it is known.
    pub artist_name: Option<SharedString>,
    /// Whether the track's metadata is known from the file or the database.
    pub source: DataSource,
    /// The duration of the track in seconds.
    pub duration: Option<i64>,
    /// Online (KuGou) album-art URL, empty for offline tracks.
    pub cover_url: Option<SharedString>,
}

/// Extracts the restart-surviving subset of online display metadata.
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
    /// The metadata was read directly from the file.
    Metadata,
    /// The metadata was read from the library database.
    Library,
}

impl PartialEq for QueueItemData {
    // Content-only equality: `uid` is instance identity (two content-equal
    // items are distinct instances), so it is deliberately excluded here.
    fn eq(&self, other: &Self) -> bool {
        self.db_id == other.db_id
            && self.db_album_id == other.db_album_id
            && self.path == other.path
    }
}

impl QueueItemData {
    /// Creates a new `QueueItemData` instance with the given information. No
    /// UI metadata entity is created here — the `ui::queue_metadata` side
    /// table creates one lazily on first use, mirroring the deserialization
    /// path. Usable off the UI thread (bulk builds like Shuffle All): one
    /// GPUI entity per track would be pure startup cost, and eagerly building
    /// an empty shell entity bought nothing — the former separate `lazy`
    /// constructor merged into this one.
    pub fn new(path: PathBuf, db_id: Option<i64>, db_album_id: Option<i64>) -> Self {
        QueueItemData {
            path,
            db_id,
            db_album_id,
            persisted_ui: None,
            duration: Arc::new(AtomicI64::new(UNKNOWN_DURATION)),
            // Single allocation point for all non-deserialized constructions:
            // one fresh uid per constructed instance.
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            #[cfg(feature = "online_sources")]
            online_identity: None,
        }
    }

    /// Creates a queue item whose UI metadata is already known (e.g. online
    /// tracks fetched from a remote service), so no database or disk lookup
    /// is scheduled later. No entity is created here either: the metadata is
    /// seeded into `persisted_ui` (plus the duration cell), and the first
    /// side-table resolution (`ui::queue_metadata`) rebuilds the entity
    /// content from it via the persisted-restore branch — field-for-field
    /// identical to the eager fill this constructor used to perform
    /// (`album_id: None` and `source: Metadata` are what both call sites pass
    /// and what the branch restores).
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

    /// Attaches the online-provider identity so an expired stream URL can be
    /// re-fetched later (e.g. on a session restore).
    #[cfg(feature = "online_sources")]
    pub fn with_online_identity(mut self, identity: OnlineIdentity) -> Self {
        self.online_identity = Some(identity);
        self
    }

    /// The online-provider identity, if this item is an HTTP stream.
    #[cfg(feature = "online_sources")]
    pub fn online_identity(&self) -> Option<&OnlineIdentity> {
        self.online_identity.as_ref()
    }

    /// Display metadata persisted with the item: (name, artist, duration, cover).
    #[cfg(feature = "online_sources")]
    pub fn persisted_display(&self) -> Option<PersistedDisplay> {
        self.persisted_ui.as_ref().map(|p| {
            (
                p.name.clone(),
                p.artist_name.clone(),
                p.duration,
                p.cover_url.clone(),
            )
        })
    }

    /// This item's per-instance identity: the key under which the UI-side
    /// metadata side table (`ui::queue_metadata`) stores this item's UI state
    /// slot. Stable across clones.
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// Clone of the shared duration cell, so background loaders handed out by
    /// the side table can write the track length back without owning the item
    /// (same cell [`Self::set_known_duration`] writes through).
    pub fn duration_cell(&self) -> Arc<AtomicI64> {
        self.duration.clone()
    }

    /// The persisted UI payload (name, artist, duration, cover) as a plain
    /// tuple, decoupled from the private `PersistedQueueUIData` so the
    /// UI-side metadata table (`ui::queue_metadata`) can run the same
    /// restore branch. Not feature-gated: `persisted_ui` itself is not.
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

    /// Length in seconds once it is known, `None` while metadata is still
    /// loading. Cheap: an atomic load, no locks beyond the refcount.
    pub fn known_duration(&self) -> Option<i64> {
        match self.duration.load(Ordering::Relaxed) {
            UNKNOWN_DURATION => None,
            secs => Some(secs),
        }
    }

    /// Stores the length in seconds; `None` resets it to [`UNKNOWN_DURATION`].
    pub fn set_known_duration(&self, secs: Option<i64>) {
        self.duration
            .store(secs.unwrap_or(UNKNOWN_DURATION), Ordering::Relaxed);
    }

    /// Returns the file path of the queue item.
    pub fn get_path(&self) -> &PathBuf {
        &self.path
    }

    /// Replaces the stored path. Used by the kugou online flow to refresh the
    /// (expiring) stream URL of an already-queued track instead of queueing a
    /// duplicate entry.
    pub fn replace_path(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// Returns the album ID of the queue item, if it exists.
    pub fn get_db_album_id(&self) -> Option<i64> {
        self.db_album_id
    }

    /// Returns the track ID of the queue item, if it exists.
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
        // every construction gets a fresh instance identity even for
        // content-equal items — the ui::queue_metadata side table keys on it
        let a = sample_item("a");
        let b = sample_item("a");
        assert_ne!(a.uid(), b.uid());
        // content equality deliberately ignores uid
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
        // since the eager entity shell was removed, an online track's display
        // metadata is restored from the persisted seed on the side table's
        // first frame — this test locks the seed itself
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
