use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use camino::{Utf8Path, Utf8PathBuf};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use rustc_hash::FxHashMap;
use sqlx::SqlitePool;
use tokio::{
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    task::{JoinHandle, spawn, spawn_blocking},
};
use tracing::{error, warn};

use super::{
    artwork::{ArtworkProcessor, FolderArtLoader, load_art_ids},
    control::ScanMode,
    database::WriteCaches,
    decode::{FileInformation, ScanReadError, read_metadata_for_path},
    discover::{
        DirectoryReadPolicy, DiscoveredPath, FolderArtObservations, Relocation, discover,
        rescan_discover,
    },
    disk,
    record::ScanRecord,
};
use crate::settings::scan::ScanSettings;

pub(super) struct ActiveScan {
    pub(super) scan_record: Arc<Mutex<ScanRecord>>,
    pub(super) artwork_processor: ArtworkProcessor,
    pub(super) folder_art_loader: FolderArtLoader,
    pub(super) folder_art_observations: FolderArtObservations,
    pub(super) meta_rx: Receiver<MetadataItem>,
    pub(super) decode_fail_rx: Receiver<(Utf8PathBuf, SystemTime, ScanReadError)>,
    pub(super) relocate_rx: Receiver<Relocation>,
    pub(super) cancel_flag: Arc<AtomicBool>,
    pub(super) slow_discover_task: Option<JoinHandle<u64>>,
    pub(super) metadata_tasks: Vec<JoinHandle<()>>,
    pub(super) discover_handle: JoinHandle<u64>,
    pub(super) artwork_handle: JoinHandle<()>,
    pub(super) caches: WriteCaches,
}

impl ActiveScan {
    pub(super) async fn start(
        pool: &SqlitePool,
        scan_settings: &ScanSettings,
        mode: &ScanMode,
        scan_record: ScanRecord,
        full_available_paths: Vec<Utf8PathBuf>,
    ) -> Self {
        let scan_record = Arc::new(Mutex::new(scan_record));

        let parallelism = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(FALLBACK_PARALLELISM);
        let num_workers = normal_worker_count(parallelism);

        let meta_capacity = if scan_settings.slow_disk_mode {
            SLOW_DISK_META_CAPACITY
        } else {
            num_workers * META_CAPACITY_PER_WORKER
        };
        let existing_art_ids = load_art_ids(pool).await;
        let artwork_processor = ArtworkProcessor::new(existing_art_ids.keys().copied());
        let folder_art_loader = FolderArtLoader::new(artwork_processor.concurrency());
        let folder_art_observations = FolderArtObservations::default();
        let (meta_tx, meta_rx) = channel::<MetadataItem>(meta_capacity);
        let (raw_meta_tx, raw_meta_rx) =
            channel::<RawMetadataItem>(artwork_processor.concurrency() * RAW_ITEMS_PER_WORKER);
        let artwork_handle = spawn(run_artwork_pipeline(
            raw_meta_rx,
            meta_tx,
            artwork_processor.clone(),
            folder_art_loader.clone(),
        ));
        let (decode_fail_tx, decode_fail_rx) =
            channel::<(Utf8PathBuf, SystemTime, ScanReadError)>(meta_capacity);
        // case-only renames found during discovery
        let (relocate_tx, relocate_rx) = channel::<Relocation>(64);

        let cancel_flag = Arc::new(AtomicBool::new(false));

        let spawn_discover = |path_tx: Sender<DiscoveredPath>,
                              relocate_tx: Sender<Relocation>,
                              cancel: Arc<AtomicBool>,
                              read_policy: DirectoryReadPolicy|
         -> JoinHandle<u64> {
            let settings = scan_settings.clone();
            let paths = full_available_paths.clone();
            let scan_record = scan_record.clone();
            let folder_art = folder_art_observations.clone();
            match mode {
                ScanMode::Full { .. } => {
                    let mut settings = settings;
                    settings.paths = paths;
                    spawn(discover(
                        settings,
                        scan_record,
                        path_tx,
                        relocate_tx,
                        cancel,
                        read_policy,
                        folder_art,
                    ))
                }
                ScanMode::Targeted {
                    paths,
                    respect_record,
                    recursive,
                } => {
                    let paths = paths.clone();
                    let recursive = *recursive;
                    let record = respect_record.then(|| scan_record.clone());
                    spawn(rescan_discover(
                        paths,
                        record,
                        recursive,
                        path_tx,
                        relocate_tx,
                        cancel,
                        read_policy,
                        folder_art,
                    ))
                }
            }
        };

        let mut slow_discover_task = None;
        let mut metadata_tasks = Vec::new();

        let discover_handle = if scan_settings.slow_disk_mode {
            let paths_for_disks = scan_settings.paths.clone();
            let (disk_groups, mounts_sorted, mount_to_channel) =
                spawn_blocking(move || disk::group_paths_by_disk(&paths_for_disks))
                    .await
                    .expect("disk grouping task panicked");
            let num_disks = disk_groups.len().max(1);
            let read_policy = DirectoryReadPolicy::slow(
                mounts_sorted.clone(),
                mount_to_channel.clone(),
                num_disks,
            );

            let (disk_txs, disk_rxs): (Vec<_>, Vec<_>) =
                (0..num_disks).map(|_| channel(64)).unzip();

            let (path_tx, mut path_rx) = channel::<DiscoveredPath>(64);

            let cancel_for_discover = Arc::clone(&cancel_flag);
            let discover_task =
                spawn_discover(path_tx, relocate_tx, cancel_for_discover, read_policy);
            slow_discover_task = Some(discover_task);

            let router_cancel = Arc::clone(&cancel_flag);
            let router_disk_txs = disk_txs.clone();
            let router = spawn_blocking(move || {
                let mut dir_cache: FxHashMap<Utf8PathBuf, usize> = FxHashMap::default();
                let mut routed: u64 = 0;

                while let Some(discovered) = path_rx.blocking_recv() {
                    if router_cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    let parent = discovered.path.parent().map(|path| path.to_path_buf());
                    let disk_idx = parent
                        .as_ref()
                        .and_then(|path| dir_cache.get(path).copied())
                        .or_else(|| {
                            let mount_point = mounts_sorted.iter().find(|mount| {
                                discovered
                                    .path
                                    .as_std_path()
                                    .starts_with(mount.as_std_path())
                            })?;
                            let channel = mount_to_channel
                                .get(mount_point)
                                .copied()
                                .unwrap_or_else(|| {
                                    warn!(
                                        "no physical device ID for mount point {:?}, routing to fallback channel 0",
                                        mount_point
                                    );
                                    0
                                });
                            if let Some(parent) = &parent {
                                dir_cache.insert(parent.clone(), channel);
                            }
                            Some(channel)
                        })
                        .unwrap_or(0);

                    if router_disk_txs[disk_idx].blocking_send(discovered).is_err() {
                        break;
                    }
                    routed += 1;
                }

                routed
            });

            for rx in disk_rxs {
                let raw_meta_tx = raw_meta_tx.clone();
                let decode_fail_tx = decode_fail_tx.clone();
                let cancel_flag = Arc::clone(&cancel_flag);
                metadata_tasks.push(spawn(run_metadata_pipeline(
                    rx,
                    raw_meta_tx,
                    decode_fail_tx,
                    cancel_flag,
                    1,
                )));
            }

            router
        } else {
            let (path_tx, path_rx) = channel::<DiscoveredPath>(64);

            let cancel_for_discover = Arc::clone(&cancel_flag);
            let discover_handle = spawn_discover(
                path_tx,
                relocate_tx,
                cancel_for_discover,
                DirectoryReadPolicy::normal(num_workers),
            );

            let raw_meta_tx = raw_meta_tx.clone();
            let decode_fail_tx = decode_fail_tx.clone();
            let cancel_flag = Arc::clone(&cancel_flag);
            metadata_tasks.push(spawn(run_metadata_pipeline(
                path_rx,
                raw_meta_tx,
                decode_fail_tx,
                cancel_flag,
                num_workers,
            )));

            discover_handle
        };

        // drop senders so channels close when workers finish
        drop(raw_meta_tx);
        drop(decode_fail_tx);

        Self {
            scan_record,
            artwork_processor,
            folder_art_loader,
            folder_art_observations,
            meta_rx,
            decode_fail_rx,
            relocate_rx,
            cancel_flag,
            slow_discover_task,
            metadata_tasks,
            discover_handle,
            artwork_handle,
            caches: WriteCaches {
                art_ids: existing_art_ids,
                ..WriteCaches::default()
            },
        }
    }
}

/// Caps metadata readers so peak memory and disk contention stay bounded on many-core machines.
const MAX_METADATA_WORKERS: usize = 16;

/// Core count assumed when `available_parallelism` is unavailable.
const FALLBACK_PARALLELISM: usize = 4;

/// Metadata channel capacity in slow-disk mode, independent of worker count.
const SLOW_DISK_META_CAPACITY: usize = 64;

/// Metadata channel capacity added per metadata worker in normal mode.
const META_CAPACITY_PER_WORKER: usize = 8;

/// Raw (undecoded) artwork items buffered per artwork-decode worker; both the
/// raw channel capacity in [`ActiveScan::start`] and the in-flight cap in
/// [`run_artwork_pipeline`] derive from this.
const RAW_ITEMS_PER_WORKER: usize = 2;

pub(super) fn normal_worker_count(parallelism: usize) -> usize {
    parallelism.saturating_sub(1).clamp(1, MAX_METADATA_WORKERS)
}

pub(super) async fn run_metadata_pipeline(
    mut input: Receiver<DiscoveredPath>,
    meta_tx: Sender<RawMetadataItem>,
    decode_fail_tx: Sender<(Utf8PathBuf, SystemTime, ScanReadError)>,
    cancel_flag: Arc<AtomicBool>,
    concurrency: usize,
) {
    let concurrency = concurrency.max(1);
    let mut pending = FuturesUnordered::new();
    let mut input_open = true;

    while input_open || !pending.is_empty() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            discovered = input.recv(), if input_open && pending.len() < concurrency => {
                match discovered {
                    Some(discovered) => {
                        pending.push(spawn_blocking(move || {
                            let result = read_metadata_for_path(&discovered.path);
                            (discovered, result)
                        }));
                    }
                    None => input_open = false,
                }
            }
            Some(result) = pending.next(), if !pending.is_empty() => {
                let (discovered, result) = match result {
                    Ok(result) => result,
                    Err(e) => {
                        error!("Metadata reader task failed: {:?}", e);
                        continue;
                    }
                };

                match result {
                    Ok(info) => {
                        if meta_tx
                            .send(RawMetadataItem { discovered, info })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(class) => {
                        warn!(
                            "Could not read metadata for file {:?}: {:?}",
                            discovered.path, class
                        );
                        if decode_fail_tx
                            .send((discovered.path, discovered.timestamp, class))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub(super) struct RawMetadataItem {
    pub(super) discovered: DiscoveredPath,
    pub(super) info: FileInformation,
}

pub(super) type MetadataItem = (Utf8PathBuf, SystemTime, FileInformation);

// No cancel_flag by design: on cancel, `meta_rx.close()` in execution.rs makes
// the `output.send` below fail, and the metadata tasks dropping their
// `raw_meta_tx` clones close `input` — a flag check could only fire where
// those two paths already end this loop (the metadata pipeline owns the flag).
pub(super) async fn run_artwork_pipeline(
    mut input: Receiver<RawMetadataItem>,
    output: Sender<MetadataItem>,
    processor: ArtworkProcessor,
    folder_art_loader: FolderArtLoader,
) {
    let max_pending = processor.concurrency() * 2;
    let mut pending = FuturesUnordered::new();
    let mut input_open = true;

    while input_open || !pending.is_empty() {
        tokio::select! {
            item = input.recv(), if input_open && pending.len() < max_pending => {
                match item {
                    Some(RawMetadataItem {
                        discovered,
                        mut info,
                    }) => {
                        let processor = processor.clone();
                        let folder_art_loader = folder_art_loader.clone();
                        pending.push(async move {
                            if info.2.representative
                                && let Some(candidate) = discovered.folder_art
                            {
                                info.2.folder = folder_art_loader.load(candidate).await;
                            }

                            processor.process_file_art(&mut info.2).await;

                            (discovered.path, discovered.timestamp, info)
                        }
                        .boxed());
                    }
                    None => input_open = false,
                }
            }
            Some(item) = pending.next(), if !pending.is_empty() => {
                if output.send(item).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[derive(Default)]
pub(super) struct DecodeFailureCounters {
    pub(super) missing: u64,
    pub(super) transient: u64,
    pub(super) corrupt: u64,
}

impl DecodeFailureCounters {
    fn count(&mut self, class: ScanReadError) {
        match class {
            ScanReadError::Missing => self.missing += 1,
            ScanReadError::Transient => self.transient += 1,
            ScanReadError::Corrupt => self.corrupt += 1,
        }
    }
}

pub(super) fn apply_decode_failure(
    records: &mut FxHashMap<Utf8PathBuf, SystemTime>,
    path: &Utf8Path,
    timestamp: SystemTime,
    class: ScanReadError,
) {
    match class {
        ScanReadError::Missing | ScanReadError::Transient => {
            records.remove(path);
        }
        ScanReadError::Corrupt => {
            records.insert(path.to_path_buf(), timestamp);
        }
    }
}

pub(super) async fn record_decode_failure(
    scan_checkpoint: &Mutex<FxHashMap<Utf8PathBuf, SystemTime>>,
    scan_record: &Mutex<ScanRecord>,
    counters: &mut DecodeFailureCounters,
    path: &Utf8Path,
    timestamp: SystemTime,
    class: ScanReadError,
) {
    counters.count(class);
    if class == ScanReadError::Corrupt {
        scan_checkpoint
            .lock()
            .await
            .insert(path.to_path_buf(), timestamp);
    }
    let mut sr = scan_record.lock().await;
    apply_decode_failure(&mut sr.records, path, timestamp, class);
}
