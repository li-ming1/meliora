use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use futures::TryFutureExt as _;
use gpui::{App, AppContext, Entity, SharedString};
use tracing::{error, trace_span};

use crate::library::Pool;
use crate::library::db;
use crate::media::{
    lookup_table::try_open_media, metadata::Metadata, traits::MediaProviderFeatures,
};

/// Sentinel for "length not known yet" in [`QueueItemData`]'s duration slot.
pub const UNKNOWN_DURATION: i64 = i64::MIN;

#[derive(Clone, Debug)]
pub struct QueueItemData {
    // hardcore: three layers are all required — `Arc` shares one entity slot
    // across every clone of this item (the entity is only creatable on the UI
    // thread), `RwLock` for lazy creation + `drop_data` reset, `Option` marks
    // "entity not created yet" after deserialization.
    /// The UI data associated with the queue item.
    data: Arc<RwLock<Option<Entity<Option<QueueItemUIData>>>>>,
    /// The database ID of track the item is from, if it exists.
    db_id: Option<i64>,
    /// The database ID of album the item is from, if it exists.
    db_album_id: Option<i64>,
    /// The path to the track file.
    path: PathBuf,
    /// Track length in seconds once known: written at queue-build time (from
    /// the library track / online metadata) and written back whenever the
    /// per-item metadata entity loads, so the queue summary never needs to
    /// touch those entities. [`UNKNOWN_DURATION`] until then.
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

impl serde::Serialize for QueueItemData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
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
            data: Arc::new(RwLock::new(None)),
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
    fn eq(&self, other: &Self) -> bool {
        self.db_id == other.db_id
            && self.db_album_id == other.db_album_id
            && self.path == other.path
    }
}

impl QueueItemData {
    /// Creates a new `QueueItemData` instance with the given information.
    pub fn new(cx: &mut App, path: PathBuf, db_id: Option<i64>, db_album_id: Option<i64>) -> Self {
        QueueItemData {
            path,
            db_id,
            db_album_id,
            data: Arc::new(RwLock::new(Some(cx.new(|_| None)))),
            persisted_ui: None,
            duration: Arc::new(AtomicI64::new(UNKNOWN_DURATION)),
            #[cfg(feature = "online_sources")]
            online_identity: None,
        }
    }

    /// Creates a queue item with no metadata entity yet; `get_data` creates
    /// it on first use, mirroring the deserialization path. Bulk queue builds
    /// (Shuffle All) use this off the UI thread where `cx.new` is unavailable
    /// and one GPUI entity per track would be pure startup cost.
    pub fn lazy(path: PathBuf, db_id: Option<i64>, db_album_id: Option<i64>) -> Self {
        QueueItemData {
            path,
            db_id,
            db_album_id,
            data: Arc::new(RwLock::new(None)),
            persisted_ui: None,
            duration: Arc::new(AtomicI64::new(UNKNOWN_DURATION)),
            #[cfg(feature = "online_sources")]
            online_identity: None,
        }
    }

    /// Creates a queue item whose UI metadata is already known (e.g. online
    /// tracks fetched from a remote service), so no database or disk lookup
    /// is scheduled later.
    #[cfg(feature = "online_sources")]
    pub fn with_ui_data(cx: &mut App, path: PathBuf, ui_data: QueueItemUIData) -> Self {
        let persisted_ui = persist_from(ui_data.clone());
        let duration = Arc::new(AtomicI64::new(ui_data.duration.unwrap_or(UNKNOWN_DURATION)));
        QueueItemData {
            path,
            db_id: None,
            db_album_id: None,
            data: Arc::new(RwLock::new(Some(cx.new(|_| Some(ui_data))))),
            persisted_ui,
            duration,
            online_identity: None,
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
    pub fn persisted_display(
        &self,
    ) -> Option<(Option<String>, Option<String>, Option<i64>, Option<String>)> {
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

    pub fn set_known_duration(&self, secs: Option<i64>) {
        self.duration
            .store(secs.unwrap_or(UNKNOWN_DURATION), Ordering::Relaxed);
    }

    /// Duration from the metadata entity, but only if that entity already
    /// exists: never spawns one as a side effect. Reading it during the queue
    /// summary's render registers a dependency, so the summary recomputes as
    /// metadata loads land.
    pub fn loaded_duration(&self, cx: &App) -> Option<i64> {
        let entity = self
            .data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .clone();
        entity.read(cx).as_ref()?.duration
    }

    /// Helper to lazily initialize the UI data entity if it was deserialized.
    fn ensure_entity(&self, cx: &mut App) {
        if self
            .data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            let mut data = self.data.write().unwrap_or_else(|e| e.into_inner());
            if data.is_none() {
                *data = Some(cx.new(|_| None));
            }
        }
    }

    /// Returns a copy of the UI data after ensuring that the metadata is loaded (or going to be
    /// loaded).
    pub fn get_data(&self, cx: &mut App) -> Entity<Option<QueueItemUIData>> {
        self.ensure_entity(cx);
        let model = self
            .data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .unwrap()
            .clone();

        // Fast path: the metadata entity already holds its data, so return the
        // handle without cloning the path/persisted-ui/duration payloads (those
        // are only needed by the loading path below).
        if model.read(cx).is_some() {
            return model;
        }

        let track_id = self.db_id;
        let album_id = self.db_album_id;
        let path = self.path.clone();
        let persisted = self.persisted_ui.clone();
        let duration = self.duration.clone();
        model.update(cx, move |m, cx| {
            // if we already have the data, exit the function
            if m.is_some() {
                return;
            }

            // online (KuGou) tracks carry no db id, so restore the metadata that
            // was persisted with the queue item instead of failing a disk read
            if let Some(persisted) = persisted
                && track_id.is_none()
            {
                duration.store(
                    persisted.duration.unwrap_or(UNKNOWN_DURATION),
                    Ordering::Relaxed,
                );
                *m = Some(QueueItemUIData {
                    album_id: None,
                    name: persisted.name.map(SharedString::from),
                    artist_name: persisted.artist_name.map(SharedString::from),
                    source: DataSource::Metadata,
                    duration: persisted.duration,
                    cover_url: persisted.cover_url.map(SharedString::from),
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

            // if the database ids are known we can get the data from the database
            if let (Some(track_id), Some(album_id)) = (track_id, album_id) {
                // the two library queries left the UI thread (doctrine
                // §2.3/§14): the placeholder renders until the background load
                // backfills, exactly like the disk-metadata path below
                cx.notify();
                spawn_library_load(track_id, album_id, path, cx.entity(), duration.clone(), cx);
                return;
            }

            // vital information left blank, try retriving the metadata from disk
            // much slower, especially on windows
            spawn_metadata_load(path, cx.entity(), duration.clone(), cx);
        });

        model
    }

    /// Drop the UI data from the queue item. This means the data must be retrieved again from disk
    /// if the item is used with get_data again.
    pub fn drop_data(&self, cx: &mut App) {
        if let Some(model) = self.data.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            model.update(cx, |m, cx| {
                *m = None;
                cx.notify();
            });
        }
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

    pub fn slot_key(&self, cx: &mut App) -> usize {
        self.ensure_entity(cx);
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .unwrap()
            .entity_id()
            .as_u64() as usize
    }

    pub fn existing_slot_key(&self) -> Option<usize> {
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|e| e.entity_id().as_u64() as usize)
    }
}

#[tracing::instrument(level = "trace")]
fn read_metadata(path: &Path) -> anyhow::Result<QueueItemUIData> {
    let mut stream = try_open_media(path, MediaProviderFeatures::PROVIDES_METADATA)?
        .ok_or_else(|| anyhow::anyhow!("no metadata provider for {}", path.display()))?;
    stream.start_playback()?;

    let mut metadata = stream.read_metadata()?;
    // online (KuGou) paths are URLs — a filename fallback would parse the URL
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

/// Background-loads a queue item's display metadata from disk, filling `entity`
/// when done. Avoids blocking the UI thread on slow Windows reads.
pub fn spawn_metadata_load(
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

/// Background-loads a queue item's display metadata from the library database
/// (one track row plus one album row), filling `entity` when done. Mirrors
/// [`spawn_metadata_load`]: the synchronous `LibraryAccess` calls this replaces
/// parked the UI thread on two `RUNTIME.block_on`s per queue row's first
/// render (doctrine §2.3/§14). A row that vanished from the library, or one
/// whose artist stays blank after the load, falls back to the disk metadata
/// load exactly as the previous inline path did.
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
            return; // the load task panicked: the placeholder stays
        };
        entity.update(cx, |m, cx| {
            let (Some(track), Some(album)) = (track, album) else {
                spawn_metadata_load(path, cx.entity(), duration, cx);
                return;
            };

            let Some(data) = m.as_mut() else {
                // the entity was dropped meanwhile; nothing to fill
                return;
            };
            data.name = Some(track.title.clone().0);
            data.album_id = Some(album.id);
            data.duration = Some(track.duration);
            duration.store(track.duration, Ordering::Relaxed);

            if let Some(artist_name) = track.artist_names.clone() {
                data.artist_name = Some(artist_name.0);
            } else if let Some(artist_name) = album.artist_display_override.clone() {
                data.artist_name = Some(artist_name.0);
            }

            cx.notify();

            // artist information left blank, try retrieving the metadata from
            // disk (the fallback the old inline path ran)
            if data.artist_name.is_none() {
                spawn_metadata_load(path, cx.entity(), duration.clone(), cx);
            }
        });
    })
    .detach();
}
