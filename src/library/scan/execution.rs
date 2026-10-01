use std::{
    cell::Cell,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant, SystemTime},
};

use camino::Utf8PathBuf;
use cntp_i18n::tr;
use rustc_hash::{FxHashMap, FxHashSet};
use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::{
    sync::{
        Mutex,
        mpsc::{Receiver, UnboundedSender, WeakSender},
    },
    task::JoinHandle,
};
use tracing::{error, info, warn};

use super::{
    active_scan::{ActiveScan, DecodeFailureCounters, record_decode_failure},
    artist_match::ArtistMatcher,
    artwork::{
        ArtIdCache, ArtworkProcessor, FolderArtCandidates, FolderArtLoader, examine_folder_art,
        finalize_scan_art,
    },
    control::{PendingRescan, ScanCommand, ScanEvent, ScanMode},
    database::{
        AlbumCacheKey, TrackWriteOutcome, WriteCaches, flush_album_artists, flush_track_artists,
        relocate_track, sweep_orphan_artists, update_metadata,
    },
    discover::{FolderArtObservations, Relocation},
    record::{ScanRecord, write_checkpoint, write_scan_record},
    scanner::{ActiveCommandContext, ActiveCommandOutcome},
    watch::WatcherState,
};
use crate::{
    settings::scan::ScanSettings,
    toasts::{Toast, emit_toast},
};

const BATCH_SIZE: usize = 50;

/// Intermediate checkpoints serialize the whole record map, which costs
/// O(library size) per write (~0.3 s at 100k records, measured in
/// record.rs::tests::bench_checkpoint_serialization; the lock-held postcard
/// pass is only ~10 ms of that - zlib dominates). Writing one per 50-file
/// batch would let that dominate fast scans, so spawned checkpoint writes are
/// gated on this clock interval. The first checkpoint of a scan still goes out
/// immediately, cancellation writes one unconditionally in finish_cancelled,
/// and completion writes the full record in finish_completed - a crash loses
/// at most this much scan progress.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

/// Recoverable failures tallied over a single scan.
///
/// Every count here was previously an `expect`/`panic` that killed the scanner
/// task mid-flight. Since `start_scanner` drops its `JoinHandle`, that surfaced
/// to the user as a scan frozen on "scanning" forever with nothing in the UI.
/// A degraded scan now runs to completion (state returns to idle/watching) and
/// reports what was lost once, at the end.
#[derive(Default)]
struct ScanFailures {
    /// `BEGIN`/`COMMIT` refused: whole batches were dropped, and those tracks
    /// stay unindexed until a later scan picks them up.
    transaction: u64,
    /// A discovery worker panicked (`JoinError`) instead of returning a count.
    worker_panic: u64,
    /// Individual files dropped because no transaction could be opened.
    file: u64,
}

impl ScanFailures {
    fn is_empty(&self) -> bool {
        self.transaction == 0 && self.worker_panic == 0 && self.file == 0
    }

    /// One toast per scan, most informative category first: a worker panic
    /// means the scan is incomplete, a file count says how much was lost, and
    /// the transaction count is only the fall-back when nothing was dropped
    /// (a bare `COMMIT` failure). The full breakdown stays in the log.
    fn report(&self) {
        if self.is_empty() {
            return;
        }

        error!(
            "Scan finished degraded: {} panicked workers, {} files dropped, \
             {} transaction failures",
            self.worker_panic, self.file, self.transaction
        );

        // NOTE: the placeholder is deliberately not called `count` - `tr!`
        // reserves that name for plural selection and types it as `isize`.
        if self.worker_panic > 0 {
            emit_toast(Toast::error(tr!(
                "SCAN_FAILED_WORKER",
                "Library scan stopped early — {{failed}} scan workers failed. Please scan again.",
                failed = self.worker_panic
            )));
        } else if self.file > 0 {
            emit_toast(Toast::error(tr!(
                "SCAN_FAILED_FILES",
                "Library scan could not index {{failed}} files. Please scan again.",
                failed = self.file
            )));
        } else {
            emit_toast(Toast::error(tr!(
                "SCAN_FAILED_TRANSACTION",
                "Library scan hit {{failed}} database errors. Some files were not indexed — see the log for details.",
                failed = self.transaction
            )));
        }
    }
}

/// Open a transaction if the previous commit closed it. Leaves `tx` as `None`
/// when the database refuses, and counts the miss: callers then skip the write
/// instead of panicking.
///
/// Takes the fields separately rather than `&mut self` so callers can hold the
/// resulting transaction alongside other fields of the scan.
async fn ensure_tx(
    tx: &mut Option<Transaction<'static, Sqlite>>,
    pool: &SqlitePool,
    failures: &mut ScanFailures,
) {
    if tx.is_none() {
        match pool.begin().await {
            Ok(opened) => *tx = Some(opened),
            Err(error) => {
                error!("could not begin scan transaction: {:?}", error);
                failures.transaction += 1;
            }
        }
    }
}

pub(super) struct ScanExecutionContext<'a> {
    pub(super) pool: &'a SqlitePool,
    pub(super) scan_settings: &'a mut ScanSettings,
    pub(super) command_rx: &'a mut Receiver<ScanCommand>,
    pub(super) cmd_tx: &'a WeakSender<ScanCommand>,
    pub(super) event_tx: &'a UnboundedSender<ScanEvent>,
    pub(super) pending_start: &'a mut Option<bool>,
    pub(super) pending_rescan: &'a mut Option<PendingRescan>,
    pub(super) watcher: &'a mut WatcherState,
    pub(super) mode: ScanMode,
    pub(super) tracks_deleted: bool,
    pub(super) checkpoint_dirs: Vec<Utf8PathBuf>,
    pub(super) checkpoint_path: &'a PathBuf,
    pub(super) scan_record_path: &'a PathBuf,
    pub(super) started_at: Instant,
}

pub(super) struct ScanExecution<'a> {
    active: ActiveScan,
    context: ScanExecutionContext<'a>,
    scanned: u64,
    processed: u64,
    skipped_duplicate: u64,
    decode_failures: DecodeFailureCounters,
    artist_matcher: ArtistMatcher,
    tx: Option<Transaction<'static, Sqlite>>,
    items_in_tx: usize,
    failures: ScanFailures,
    cancelled: bool,
    discovery_complete: bool,
    discovered_total: u64,
    pending_commit: Vec<(Utf8PathBuf, SystemTime)>,
    pending_relocations: Vec<Relocation>,
    scan_checkpoint: Arc<Mutex<FxHashMap<Utf8PathBuf, SystemTime>>>,
    checkpoint_handle: Option<JoinHandle<()>>,
    /// Last time a ScanProgress event was emitted; progress is throttled so a
    /// 100k-file scan doesn't flood the header with notify-per-5-files.
    last_progress_report: Cell<Option<Instant>>,
    /// Last time a checkpoint write was spawned; clock-gated by
    /// CHECKPOINT_INTERVAL because each write serializes the whole record map.
    last_checkpoint_write: Cell<Option<Instant>>,
}

impl<'a> ScanExecution<'a> {
    pub(super) async fn new(active: ActiveScan, context: ScanExecutionContext<'a>) -> Self {
        let mut failures = ScanFailures::default();
        // A scan that cannot even open a transaction still has to run to
        // completion so the watcher is refreshed and the UI leaves the
        // "scanning" state; every write is skipped and counted instead.
        let tx = match context.pool.begin().await {
            Ok(tx) => Some(tx),
            Err(error) => {
                error!("could not begin scan transaction: {:?}", error);
                failures.transaction += 1;
                None
            }
        };

        Self {
            active,
            context,
            scanned: 0,
            processed: 0,
            skipped_duplicate: 0,
            decode_failures: DecodeFailureCounters::default(),
            artist_matcher: ArtistMatcher::new(),
            tx,
            items_in_tx: 0,
            failures,
            cancelled: false,
            discovery_complete: false,
            discovered_total: 0,
            pending_commit: Vec::with_capacity(BATCH_SIZE),
            pending_relocations: Vec::new(),
            scan_checkpoint: Arc::new(Mutex::new(FxHashMap::default())),
            checkpoint_handle: None,
            last_progress_report: Cell::new(None),
            last_checkpoint_write: Cell::new(None),
        }
    }

    pub(super) async fn run(mut self) -> Option<ScanRecord> {
        if !self.process_events().await {
            return None;
        }

        self.stop_and_join_workers().await;
        self.drain_decode_failures().await;
        self.drain_relocations().await;

        if self.cancelled {
            Some(self.finish_cancelled().await)
        } else {
            Some(self.finish_completed().await)
        }
    }

    async fn process_events(&mut self) -> bool {
        loop {
            tokio::select! {
                command = self.context.command_rx.recv() => {
                    let outcome = ActiveCommandContext {
                        scan_settings: self.context.scan_settings,
                        cmd_tx: self.context.cmd_tx,
                        pending_start: self.context.pending_start,
                        pending_rescan: self.context.pending_rescan,
                        watcher: self.context.watcher,
                    }
                    .handle(command)
                    .await;
                    match outcome {
                        ActiveCommandOutcome::Cancel => {
                            self.cancelled = true;
                            self.active.cancel_flag.store(true, Ordering::Relaxed);
                            self.active.meta_rx.close();
                            self.active.decode_fail_rx.close();
                            // unblock discovery if it's stuck on a full relocate channel
                            self.active.relocate_rx.close();
                            break;
                        }
                        ActiveCommandOutcome::Shutdown => return false,
                        ActiveCommandOutcome::Continue => {}
                    }
                }

                _ = self.context.watcher.retry_tick() => {
                    let (recovered, _) = self
                        .context
                        .watcher
                        .refresh(self.context.scan_settings, self.context.cmd_tx)
                        .await;
                    if recovered {
                        self.context.pending_start.get_or_insert(false);
                    }
                }

                result = &mut self.active.discover_handle, if !self.discovery_complete => {
                    match result {
                        Ok(total) => self.discovered_total = total,
                        Err(error) => {
                            // A panicking discovery worker must not take the
                            // whole scan down; finish with whatever was found.
                            error!("discover task panicked: {:?}", error);
                            self.failures.worker_panic += 1;
                        }
                    }
                    self.discovery_complete = true;

                    if self.discovered_total == 0 {
                        info!("Nothing new to scan");
                    }
                }

                Some((path, timestamp, class)) = self.active.decode_fail_rx.recv(),
                    if !self.cancelled =>
                {
                    self.processed += 1;
                    record_decode_failure(
                        &self.scan_checkpoint,
                        &self.active.scan_record,
                        &mut self.decode_failures,
                        &path,
                        timestamp,
                        class,
                    )
                    .await;
                }

                Some((old, new, timestamp)) = self.active.relocate_rx.recv(),
                    if !self.cancelled =>
                {
                    self.relocate(old, new, timestamp).await;
                }

                item = self.active.meta_rx.recv() => {
                    let Some((path, timestamp, (metadata, length, art))) = item else {
                        self.commit_final_batch().await;
                        break;
                    };

                    ensure_tx(&mut self.tx, self.context.pool, &mut self.failures).await;
                    let result = match self.tx.as_mut() {
                        Some(tx) => Some(
                            update_metadata(
                                tx,
                                &metadata,
                                &path,
                                length,
                                &art,
                                self.context.mode.force_albums(),
                                &mut self.active.caches,
                            )
                            .await,
                        ),
                        None => None,
                    };
                    self.active
                        .artwork_processor
                        .mark_resolved(&art, &self.active.caches.art_ids);

                    self.processed += 1;
                    match result {
                        Some(Ok(outcome)) => {
                            // record skipped files so later scans don't re-read them until mtime
                            // changes
                            self.pending_commit.push((path, timestamp));
                            self.items_in_tx += 1;
                            match outcome {
                                TrackWriteOutcome::Written => self.scanned += 1,
                                TrackWriteOutcome::SkippedDuplicateFolder => {
                                    self.skipped_duplicate += 1;
                                }
                            }
                        }
                        Some(Err(err)) => {
                            error!(
                                "Failed to update metadata for file: {:?}, error: {}",
                                path, err
                            );
                        }
                        None => self.failures.file += 1,
                    }

                    if self.items_in_tx >= BATCH_SIZE {
                        self.commit_scan_batch().await;
                    }

                    self.report_progress();
                }
            }
        }

        true
    }

    async fn relocate(&mut self, old: Utf8PathBuf, new: Utf8PathBuf, timestamp: SystemTime) {
        ensure_tx(&mut self.tx, self.context.pool, &mut self.failures).await;
        let result = match self.tx.as_mut() {
            Some(tx) => Some(relocate_track(tx, &mut self.artist_matcher, &old, &new).await),
            None => None,
        };

        match result {
            Some(Ok(updated)) => {
                if !updated.is_empty() {
                    let _ = self
                        .context
                        .event_tx
                        .send(ScanEvent::PlaylistsUpdated(updated));
                }
                self.pending_relocations.push((old, new, timestamp));
            }
            Some(Err(error)) => {
                error!(
                    "Failed to relocate track from {:?} to {:?}: {:?}",
                    old, new, error
                );
            }
            None => self.failures.file += 1,
        }
    }

    /// Whether this scan still has uncommitted work: unflushed write caches,
    /// items in the open transaction, or relocations waiting on the record.
    /// The same condition guards the final commit on both the completed and
    /// the cancelled path.
    fn has_pending_writes(&self) -> bool {
        !self.active.caches.pending_albums.is_empty()
            || !self.active.caches.pending_tracks.is_empty()
            || self.items_in_tx > 0
            || !self.pending_relocations.is_empty()
    }

    async fn commit_final_batch(&mut self) {
        if !self.has_pending_writes() {
            return;
        }

        commit_batch(
            self.context.pool,
            &mut self.tx,
            &mut self.artist_matcher,
            &mut self.active.caches,
            PendingCommitState {
                pending_commit: &mut self.pending_commit,
                pending_relocations: &mut self.pending_relocations,
                scan_record: &self.active.scan_record,
                scan_checkpoint: &self.scan_checkpoint,
            },
            CommitOptions {
                update_checkpoint: false,
                update_record: true,
                run_retry: true,
                label: "final scan",
            },
        )
        .await;
    }

    async fn commit_scan_batch(&mut self) {
        commit_batch(
            self.context.pool,
            &mut self.tx,
            &mut self.artist_matcher,
            &mut self.active.caches,
            PendingCommitState {
                pending_commit: &mut self.pending_commit,
                pending_relocations: &mut self.pending_relocations,
                scan_record: &self.active.scan_record,
                scan_checkpoint: &self.scan_checkpoint,
            },
            CommitOptions {
                update_checkpoint: true,
                update_record: true,
                run_retry: false,
                label: "scan batch",
            },
        )
        .await;

        self.start_checkpoint_write().await;
        ensure_tx(&mut self.tx, self.context.pool, &mut self.failures).await;
        self.items_in_tx = 0;
    }

    async fn start_checkpoint_write(&mut self) {
        if self
            .checkpoint_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return;
        }
        if let Some(last) = self.last_checkpoint_write.get()
            && last.elapsed() < CHECKPOINT_INTERVAL
        {
            return;
        }
        if let Some(handle) = self.checkpoint_handle.take() {
            let _ = handle.await;
        }
        self.last_checkpoint_write.set(Some(Instant::now()));
        let checkpoint = Arc::clone(&self.scan_checkpoint);
        let directories = self.context.checkpoint_dirs.clone();
        let path = self.context.checkpoint_path.clone();
        self.checkpoint_handle = Some(tokio::spawn(async move {
            write_checkpoint(checkpoint, directories, &path).await;
        }));
    }

    fn report_progress(&self) {
        if !self.processed.is_multiple_of(5) {
            return;
        }
        // The header repaints per event; 250 ms is well past what a progress
        // bar needs, so shed the rest at the source instead of per-notify.
        const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
        let now = Instant::now();
        if let Some(last) = self.last_progress_report.get()
            && now.duration_since(last) < PROGRESS_INTERVAL
        {
            return;
        }
        self.last_progress_report.set(Some(now));
        let total = if self.discovery_complete {
            self.discovered_total
        } else {
            u64::MAX
        };
        let _ = self.context.event_tx.send(ScanEvent::ScanProgress {
            current: self.processed,
            total,
        });
    }

    async fn stop_and_join_workers(&mut self) {
        self.active.cancel_flag.store(true, Ordering::Relaxed);

        if !self.discovery_complete {
            if let Err(error) = (&mut self.active.discover_handle).await {
                error!("discover task panicked: {:?}", error);
                self.failures.worker_panic += 1;
            }
            self.discovery_complete = true;
        }
        if let Some(task) = self.active.slow_discover_task.take()
            && let Err(error) = task.await
        {
            error!("slow discover task panicked: {:?}", error);
            self.failures.worker_panic += 1;
        }
        for task in self.active.metadata_tasks.drain(..) {
            if let Err(error) = task.await {
                error!("Metadata pipeline task failed: {:?}", error);
            }
        }
        if let Err(error) = (&mut self.active.artwork_handle).await {
            error!("Artwork pipeline task failed: {:?}", error);
        }
    }

    async fn drain_decode_failures(&mut self) {
        while let Ok((path, timestamp, class)) = self.active.decode_fail_rx.try_recv() {
            record_decode_failure(
                &self.scan_checkpoint,
                &self.active.scan_record,
                &mut self.decode_failures,
                &path,
                timestamp,
                class,
            )
            .await;
        }
    }

    async fn drain_relocations(&mut self) {
        while let Ok((old, new, timestamp)) = self.active.relocate_rx.try_recv() {
            // `relocate` opens the transaction itself and counts the miss if it
            // cannot, so a failed BEGIN here no longer kills the scan.
            self.relocate(old, new, timestamp).await;
        }
    }

    async fn finish_cancelled(mut self) -> ScanRecord {
        if self.has_pending_writes() {
            commit_batch(
                self.context.pool,
                &mut self.tx,
                &mut self.artist_matcher,
                &mut self.active.caches,
                PendingCommitState {
                    pending_commit: &mut self.pending_commit,
                    pending_relocations: &mut self.pending_relocations,
                    scan_record: &self.active.scan_record,
                    scan_checkpoint: &self.scan_checkpoint,
                },
                CommitOptions {
                    update_checkpoint: true,
                    update_record: false,
                    run_retry: true,
                    label: "cancelled scan",
                },
            )
            .await;
            self.pending_commit.clear();
            self.pending_relocations.clear();
        }
        drop(self.tx.take());

        info!(
            "Scan cancelled after {} files in {} seconds, writing checkpoint only.",
            self.scanned,
            self.context.started_at.elapsed().as_secs_f32()
        );

        self.finalize_artwork().await;
        sweep_orphan_artists(self.context.pool).await;
        self.finish_checkpoint_write().await;
        write_checkpoint(
            Arc::clone(&self.scan_checkpoint),
            self.context.checkpoint_dirs.clone(),
            self.context.checkpoint_path,
        )
        .await;

        let scan_record = self.take_scan_record().await;
        self.refresh_watcher_and_complete().await;
        scan_record
    }

    async fn finish_completed(mut self) -> ScanRecord {
        self.commit_pending_relocations().await;
        self.finalize_artwork().await;
        sweep_orphan_artists(self.context.pool).await;

        info!(
            "Scan complete, {} files scanned in {} seconds, writing record. \
             (skipped: {} duplicate-folder; unreadable: {} missing, {} transient, {} corrupt)",
            self.scanned,
            self.context.started_at.elapsed().as_secs_f32(),
            self.skipped_duplicate,
            self.decode_failures.missing,
            self.decode_failures.transient,
            self.decode_failures.corrupt,
        );

        self.finish_checkpoint_write().await;
        let scan_record = self.take_scan_record().await;
        write_scan_record(&scan_record, self.context.scan_record_path).await;

        // full scan record is written - checkpoint can go
        if let Err(error) = tokio::fs::remove_file(self.context.checkpoint_path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!("Failed to delete scan record checkpoint: {:?}", error);
        }

        self.refresh_watcher_and_complete().await;
        scan_record
    }

    async fn commit_pending_relocations(&mut self) {
        if self.pending_relocations.is_empty() {
            return;
        }
        let Some(tx) = self.tx.take() else {
            error!("No scan transaction available to commit relocations");
            self.failures.transaction += 1;
            self.pending_relocations.clear();
            return;
        };
        if let Err(error) = tx.commit().await {
            error!("Failed to commit relocation transaction: {:?}", error);
            self.failures.transaction += 1;
            self.pending_relocations.clear();
            return;
        }

        let mut scan_record = self.active.scan_record.lock().await;
        for (old, new, timestamp) in self.pending_relocations.drain(..) {
            scan_record.records.remove(&old);
            scan_record.records.entry(new).or_insert(timestamp);
        }
    }

    async fn finalize_artwork(&mut self) {
        finalize_artwork(
            self.context.pool,
            &self.context.mode,
            &self.active.folder_art_observations,
            &self.active.folder_art_loader,
            &self.active.artwork_processor,
            &self.active.caches.albums,
            &mut self.active.caches.folder_art_candidates,
            &mut self.active.caches.art_ids,
            &mut self.active.caches.examined_albums,
            self.context.tracks_deleted,
        )
        .await;
    }

    async fn finish_checkpoint_write(&mut self) {
        if let Some(handle) = self.checkpoint_handle.take() {
            let _ = handle.await;
        }
    }

    async fn take_scan_record(&mut self) -> ScanRecord {
        match Arc::try_unwrap(std::mem::replace(
            &mut self.active.scan_record,
            Arc::new(Mutex::new(ScanRecord::new_current())),
        )) {
            Ok(mutex) => mutex.into_inner(),
            // A worker we failed to join still holds a clone. Copy the record
            // rather than killing an otherwise finished scan over bookkeeping.
            Err(shared) => shared.lock().await.clone(),
        }
    }

    async fn refresh_watcher_and_complete(&mut self) {
        let (recovered, watching) = self
            .context
            .watcher
            .refresh(self.context.scan_settings, self.context.cmd_tx)
            .await;
        if recovered {
            self.context.pending_start.get_or_insert(false);
        }
        let _ = self
            .context
            .event_tx
            .send(self.context.mode.completion_event(watching));
        self.failures.report();
    }
}

async fn retry_artists(
    pool: &SqlitePool,
    matcher: &mut ArtistMatcher,
    pending_albums: &mut FxHashSet<i64>,
    pending_tracks: &mut FxHashSet<i64>,
) {
    if pending_albums.is_empty() && pending_tracks.is_empty() {
        return;
    }
    matcher.clear();
    let Ok(mut conn) = pool.acquire().await else {
        return;
    };
    if let Err(e) = flush_album_artists(&mut conn, matcher, pending_albums).await {
        error!("Failed to recompute album artists after commit: {:?}", e);
    }
    if let Err(e) = flush_track_artists(&mut conn, matcher, pending_tracks).await {
        error!("Failed to recompute track artists after commit: {:?}", e);
    }
}

async fn merge_checkpoint_records(
    scan_checkpoint: &Mutex<FxHashMap<Utf8PathBuf, SystemTime>>,
    pending_commit: &[(Utf8PathBuf, SystemTime)],
    pending_relocations: &[Relocation],
) {
    let mut checkpoint = scan_checkpoint.lock().await;
    for (path, timestamp) in pending_commit {
        checkpoint.insert(path.clone(), *timestamp);
    }
    for (old, new, timestamp) in pending_relocations {
        checkpoint.remove(old);
        checkpoint.entry(new.clone()).or_insert(*timestamp);
    }
}

async fn merge_scan_record(
    scan_record: &Mutex<ScanRecord>,
    pending_commit: &mut Vec<(Utf8PathBuf, SystemTime)>,
    pending_relocations: &mut Vec<Relocation>,
) {
    let mut record = scan_record.lock().await;
    for (path, timestamp) in pending_commit.drain(..) {
        record.records.insert(path, timestamp);
    }
    for (old, new, timestamp) in pending_relocations.drain(..) {
        record.records.remove(&old);
        record.records.entry(new).or_insert(timestamp);
    }
}

fn clear_failed_batch(
    artist_matcher: &mut ArtistMatcher,
    caches: &mut WriteCaches,
    pending_commit: &mut Vec<(Utf8PathBuf, SystemTime)>,
    pending_relocations: &mut Vec<Relocation>,
) {
    pending_commit.clear();
    pending_relocations.clear();
    caches.pending_albums.clear();
    caches.pending_tracks.clear();
    artist_matcher.clear();
    caches.albums.clear();
    caches.paths.clear();
    caches.force_encountered.clear();
    caches.folder_art_candidates.clear();
    caches.art_ids.clear();
}

struct PendingCommitState<'a> {
    pending_commit: &'a mut Vec<(Utf8PathBuf, SystemTime)>,
    pending_relocations: &'a mut Vec<Relocation>,
    scan_record: &'a Mutex<ScanRecord>,
    scan_checkpoint: &'a Mutex<FxHashMap<Utf8PathBuf, SystemTime>>,
}

struct CommitOptions {
    update_checkpoint: bool,
    /// When false, leave the shared scan record alone and keep pending lists for the caller.
    update_record: bool,
    run_retry: bool,
    label: &'static str,
}

/// Flush pending artists, commit, and update records. Clear uncommitted caches on failure.
async fn commit_batch<'tx, 'state>(
    pool: &SqlitePool,
    tx: &mut Option<sqlx::Transaction<'tx, sqlx::Sqlite>>,
    artist_matcher: &mut ArtistMatcher,
    caches: &mut WriteCaches,
    state: PendingCommitState<'state>,
    options: CommitOptions,
) {
    // With no transaction (an earlier BEGIN failed) there is nothing to flush
    // into: drop the batch and let the next scan retry these files.
    let Some(mut tx) = tx.take() else {
        error!("No scan transaction available to commit {}", options.label);
        clear_failed_batch(
            artist_matcher,
            caches,
            state.pending_commit,
            state.pending_relocations,
        );
        return;
    };

    if let Err(e) = flush_album_artists(&mut tx, artist_matcher, &mut caches.pending_albums).await {
        error!("Failed to recompute album artists: {:?}", e);
    }
    if let Err(e) = flush_track_artists(&mut tx, artist_matcher, &mut caches.pending_tracks).await {
        error!("Failed to recompute track artists: {:?}", e);
    }

    match tx.commit().await {
        Ok(()) => {
            if options.update_checkpoint {
                merge_checkpoint_records(
                    state.scan_checkpoint,
                    state.pending_commit,
                    state.pending_relocations,
                )
                .await;
            }
            if options.update_record {
                merge_scan_record(
                    state.scan_record,
                    state.pending_commit,
                    state.pending_relocations,
                )
                .await;
            }
        }
        Err(e) => {
            error!("Failed to commit {} transaction: {:?}", options.label, e);
            clear_failed_batch(
                artist_matcher,
                caches,
                state.pending_commit,
                state.pending_relocations,
            );
        }
    }

    if options.run_retry {
        retry_artists(
            pool,
            artist_matcher,
            &mut caches.pending_albums,
            &mut caches.pending_tracks,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_artwork(
    pool: &SqlitePool,
    mode: &ScanMode,
    folder_art_observations: &FolderArtObservations,
    folder_art_loader: &FolderArtLoader,
    artwork_processor: &ArtworkProcessor,
    album_cache: &FxHashMap<AlbumCacheKey, i64>,
    folder_art_candidates: &mut FolderArtCandidates,
    art_ids: &mut ArtIdCache,
    examined_albums: &mut FxHashSet<i64>,
    tracks_deleted: bool,
) {
    if let Err(e) = examine_folder_art(
        pool,
        &folder_art_observations.snapshot(),
        folder_art_loader,
        artwork_processor,
        examined_albums,
        folder_art_candidates,
        art_ids,
    )
    .await
    {
        error!("Failed to examine folder art: {:?}", e);
    }

    let touched: FxHashSet<i64> = album_cache.values().copied().collect();
    if let Err(e) = finalize_scan_art(
        pool,
        mode.force_albums(),
        &touched,
        examined_albums,
        folder_art_candidates,
        tracks_deleted,
    )
    .await
    {
        error!("Failed to finalize scan artwork: {:?}", e);
    }
}
