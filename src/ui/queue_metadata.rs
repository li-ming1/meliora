//! UI-side metadata side table for queue items, keyed by
//! [`QueueItemData::uid`] (P2-B step 2).
//!
//! Transitional sibling of the per-item entity slot inside
//! `playback::queue::QueueItemData`: this table holds the same
//! `Entity<Option<QueueItemUIData>>` handles in a `Global` map keyed by the
//! item's uid instead of behind a per-item `Arc<RwLock<..>>`, so a lookup
//! survives clones (which share one legacy slot but carry the same uid).
//!
//! This wave is pure addition with no callers; the queue UI migrates onto it
//! in the next step, after which the legacy per-item slot and its `get_data`
//! path are removed. The metadata loaders
//! ([`spawn_metadata_load`](crate::playback::queue::spawn_metadata_load) /
//! [`spawn_library_load`](crate::playback::queue::spawn_library_load)) stay
//! in `playback::queue` as `pub(crate)` and are called from here — ui →
//! playback is the allowed dependency direction.

#![allow(dead_code)] // 过渡期旁表：本波零接线，P2-B 后续步骤接线后移除

use std::sync::atomic::Ordering;

use gpui::{App, AppContext, Entity, Global, SharedString};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::playback::queue::{
    DataSource, QueueItemData, QueueItemUIData, UNKNOWN_DURATION, spawn_library_load,
    spawn_metadata_load,
};

/// Side table mapping a queue item's uid to its UI metadata slot. Lives as an
/// independent [`Global`] (same pattern as `PlaybackInterface` / `StatsHandle`
/// / `Pool`) rather than a field of `models::Queue`, whose entity would
/// notify every subscriber on each insert (O(n) fan-out).
#[derive(Default)]
pub struct QueueMetadataStore(FxHashMap<u64, Entity<Option<QueueItemUIData>>>);

impl Global for QueueMetadataStore {}

/// Side-table counterpart of `QueueItemData::get_data`: returns the item's UI
/// metadata slot from the table keyed by [`QueueItemData::uid`], creating an
/// empty shell on first use and seeding it through the same three branches as
/// the legacy path (persisted restore → library load → disk-metadata load).
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

/// Side-table counterpart of `QueueItemData::loaded_duration`: the duration
/// from the slot entity, but only if that entity already exists in the table —
/// never spawns one as a side effect. Reading it during a render registers a
/// dependency, so the reading view recomputes as metadata loads land.
pub fn queue_loaded_duration(cx: &App, item: &QueueItemData) -> Option<i64> {
    let store = cx.try_global::<QueueMetadataStore>()?;
    let entity = store.0.get(&item.uid())?;
    entity.read(cx).as_ref()?.duration
}

/// Side-table counterpart of `QueueItemData::drop_data`: clears the slot's
/// content (the shell and its uid key stay in the table), so the data is
/// re-fetched if the item is resolved through [`queue_ui_data`] again.
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
