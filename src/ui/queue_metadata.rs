//! UI-side metadata side table for queue items, keyed by
//! [`QueueItemData::uid`] (P2-B step 2).
//!
//! Holds each queue item's `Entity<Option<QueueItemUIData>>` handle in a
//! `Global` map keyed by the item's uid, so a lookup survives clones (every
//! clone carries the same uid). The per-item entity slot that used to live
//! inside `playback::queue::QueueItemData` was removed in P2-B step 6 — this
//! table is the only home of queue-item UI metadata. The background loaders
//! (library query + disk read + entity backfill) moved here with it: they are
//! UI semantics, and with them gone `playback/queue.rs` no longer touches
//! gpui entities.

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

/// Side table mapping a queue item's uid to its UI metadata slot. Lives as an
/// independent [`Global`] (same pattern as `PlaybackInterface` / `StatsHandle`
/// / `Pool`) rather than a field of `models::Queue`, whose entity would
/// notify every subscriber on each insert (O(n) fan-out).
#[derive(Default)]
pub struct QueueMetadataStore(FxHashMap<u64, Entity<Option<QueueItemUIData>>>);

impl Global for QueueMetadataStore {}

/// Returns the item's UI metadata slot from the table keyed by
/// [`QueueItemData::uid`], creating an empty shell on first use and seeding it
/// through three branches (persisted restore → library load → disk-metadata
/// load).
pub fn queue_ui_data(cx: &mut App, item: &QueueItemData) -> Entity<Option<QueueItemUIData>> {
    let uid = item.uid();

    // ensure: look up the slot, creating (and inserting) an empty shell on
    // first touch. `default_global` lazily materializes the store.
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

    // Fast path: the slot already holds its data, so return the handle without
    // cloning the path/persisted-ui/duration payloads (those are only needed
    // by the seeding path below).
    if entity.read(cx).is_some() {
        return entity;
    }

    let track_id = item.get_db_id();
    let album_id = item.get_db_album_id();
    let path = item.get_path().clone();
    let persisted = item.persisted_restore();
    let duration = item.duration_cell();
    entity.update(cx, move |m, cx| {
        // if we already have the data, exit the function
        if m.is_some() {
            return;
        }

        // online (KuGou) tracks carry no db id, so restore the metadata that
        // was persisted with the queue item instead of failing a disk read
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

    entity
}

/// The duration from the item's slot entity, but only if that entity already
/// exists in the table — never spawns one as a side effect. Reading it during
/// a render registers a dependency, so the reading view recomputes as
/// metadata loads land.
pub fn queue_loaded_duration(cx: &App, item: &QueueItemData) -> Option<i64> {
    let store = cx.try_global::<QueueMetadataStore>()?;
    let entity = store.0.get(&item.uid())?;
    entity.read(cx).as_ref()?.duration
}

/// Clears the item's slot content (the shell and its uid key stay in the
/// table), so the data is re-fetched if the item is resolved through
/// [`queue_ui_data`] again.
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

/// Retains only the slots whose uid is in `live` (the queue panel's
/// `rescan_views_and_summary` passes the uids of the queue's current items);
/// everything else is dropped, releasing those entity handles. Takes
/// `&mut App` because gpui offers no `&App` path that can mutate a `Global`.
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

            if let Some(artist_name) = track
                .artist_names
                .clone()
                .or_else(|| album.artist_display_override.clone())
            {
                data.artist_name = Some(artist_name.0);
            }

            cx.notify();

            // artist information left blank, try retrieving the metadata from
            // disk (the fallback the old inline path ran)
            if data.artist_name.is_none() {
                spawn_metadata_load(path, cx.entity(), duration, cx);
            }
        });
    })
    .detach();
}
