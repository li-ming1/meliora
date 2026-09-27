use std::{
    io::{Error, ErrorKind},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use async_compression::tokio::bufread::ZlibDecoder;
use async_compression::tokio::write::ZlibEncoder;
use camino::Utf8PathBuf;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};
use tracing::{error, info};

/// Scan algorithm version. Bump to force a full rescan.
pub const SCAN_VERSION: u16 = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRecord {
    pub version: u16,
    pub records: FxHashMap<Utf8PathBuf, SystemTime>,
    pub directories: Vec<Utf8PathBuf>,
}

impl ScanRecord {
    pub fn new_current() -> Self {
        Self {
            version: SCAN_VERSION,
            records: FxHashMap::default(),
            directories: Vec::new(),
        }
    }

    pub fn is_version_mismatch(&self) -> bool {
        self.version != SCAN_VERSION
    }
}

pub async fn load_scan_record(path: &Path) -> ScanRecord {
    let mut file = match tokio::fs::File::open(path)
        .await
        .map(BufReader::new)
        .map(ZlibDecoder::new)
    {
        Ok(f) => f,
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                error!("Could not open scan record: {:?}", e);
                error!("Scanning will be slow until the scan record is rebuilt");
            }

            return ScanRecord::new_current();
        }
    };

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.unwrap_or_default();

    match postcard::from_bytes(&bytes) {
        Ok(scan_record) => scan_record,
        Err(e) => {
            error!("Could not read scan record: {:?}", e);
            error!("Scanning will be slow until the scan record is rebuilt");
            ScanRecord::new_current()
        }
    }
}

#[derive(Serialize)]
struct ScanRecordForWrite<'a> {
    version: u16,
    records: &'a FxHashMap<Utf8PathBuf, SystemTime>,
    directories: &'a [Utf8PathBuf],
}

/// Log one failed stage of an atomic record write, with the caller's
/// user-facing hint. `stage` completes the message, e.g. `write <label>`.
fn log_write_failure(stage: &str, error: &Error, failure_hint: Option<&'static str>) {
    error!("Could not {stage}: {:?}", error);
    if let Some(hint) = failure_hint {
        error!("{hint}");
    }
}

async fn write_record_atomic(
    path: &Path,
    data: Vec<u8>,
    label: &str,
    log_success: bool,
    failure_hint: Option<&'static str>,
) {
    let tmp_path = path.with_extension("hsr.tmp");

    let mut file = match tokio::fs::File::create(&tmp_path)
        .await
        .map(ZlibEncoder::new)
    {
        Ok(file) => file,
        // nothing was created, so there is no temp file to clean up
        Err(e) => {
            log_write_failure(&format!("create {label} file"), &e, failure_hint);
            return;
        }
    };

    if let Err(e) = file.write_all(&data).await {
        log_write_failure(&format!("write {label}"), &e, failure_hint);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return;
    }
    if let Err(e) = file.shutdown().await {
        log_write_failure(&format!("close {label}"), &e, failure_hint);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, path).await {
        log_write_failure(&format!("rename {label} into place"), &e, failure_hint);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return;
    }
    if log_success {
        info!("Scan record saved successfully");
    }
}

pub async fn write_checkpoint(
    checkpoint: Arc<Mutex<FxHashMap<Utf8PathBuf, SystemTime>>>,
    directories: Vec<Utf8PathBuf>,
    path: &Path,
) {
    let serialized = {
        let guard = checkpoint.lock().await;
        let view = ScanRecordForWrite {
            version: SCAN_VERSION,
            records: &guard,
            directories: &directories,
        };
        postcard::to_allocvec(&view)
    };

    match serialized {
        Ok(data) => write_record_atomic(path, data, "scan record checkpoint", false, None).await,
        Err(e) => error!("Could not serialize scan record checkpoint: {:?}", e),
    }
}

pub async fn write_scan_record(scan_record: &ScanRecord, path: &Path) {
    const HINT: &str = "Scan record will not be saved, this may cause rescans on restart";
    match postcard::to_allocvec(&scan_record) {
        Ok(data) => write_record_atomic(path, data, "scan record", true, Some(HINT)).await,
        Err(e) => {
            error!("Could not serialize scan record: {:?}", e);
            error!("{HINT}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use std::time::{Duration, UNIX_EPOCH};

    async fn assert_loads_record(path: &Path, expected_path: &Utf8PathBuf, timestamp: SystemTime) {
        let loaded = load_scan_record(path).await;
        assert_eq!(loaded.version, SCAN_VERSION);
        assert_eq!(loaded.records.get(expected_path), Some(&timestamp));
        assert_eq!(loaded.directories, vec![Utf8PathBuf::from("/music")]);
    }

    #[tokio::test]
    async fn missing_or_corrupt_record_returns_current() {
        let dir = TestDir::new("scan-record-test");
        let record = load_scan_record(&dir.join("missing.hsr")).await;
        assert!(!record.is_version_mismatch());
        assert!(record.records.is_empty());
        assert!(record.directories.is_empty());
        let path = dir.join("corrupt.hsr");
        std::fs::write(&path, b"not valid postcard data").unwrap();
        let record = load_scan_record(&path).await;
        assert!(!record.is_version_mismatch());
        assert!(record.records.is_empty());
    }

    #[tokio::test]
    async fn write_and_load_roundtrip() {
        let dir = TestDir::new("scan-record-test");
        let path = dir.join("record.hsr");
        let mut record = ScanRecord::new_current();
        let t1 = UNIX_EPOCH + Duration::from_secs(1_234_567_890);
        let p1 = Utf8PathBuf::from("/music/track.flac");
        record.records.insert(p1.clone(), t1);
        record.directories.push(Utf8PathBuf::from("/music"));

        write_scan_record(&record, &path).await;
        assert_loads_record(&path, &p1, t1).await;
    }

    #[tokio::test]
    async fn checkpoint_writes_loadable_record() {
        let dir = TestDir::new("scan-record-test");
        let path = dir.join("checkpoint.hsr");
        let mut checkpoint = FxHashMap::default();
        let t1 = UNIX_EPOCH + Duration::from_secs(1_000);
        let p1 = Utf8PathBuf::from("/music/a.flac");
        checkpoint.insert(p1.clone(), t1);

        let guard = Arc::new(Mutex::new(checkpoint));
        write_checkpoint(guard, vec![Utf8PathBuf::from("/music")], &path).await;
        assert_loads_record(&path, &p1, t1).await;
    }

    /// Checkpoint serialization cost benchmark, hand-run with:
    /// `cargo test --release --features kugou -- bench_checkpoint --ignored --nocapture`
    ///
    /// Measures exactly what `write_checkpoint` does per invocation, minus disk
    /// IO (which happens off-lock in `write_record_atomic`):
    /// 1. `postcard::to_allocvec` of the full map - this is the portion where
    ///    the checkpoint `Mutex` is held, so it also blocks `commit_batch`'s
    ///    `merge_checkpoint_records` for that long;
    /// 2. zlib compression through the same `async_compression` tokio
    ///    `ZlibEncoder` pipeline used by `write_record_atomic`.
    ///
    /// Decision rule from the perf audit: <50 ms per checkpoint at 100k records
    /// -> keep per-batch checkpointing and record the numbers here;
    /// >=100 ms -> gate checkpoint writes on a >=2 s clock interval in
    /// execution.rs. In between: keep unless further evidence appears.
    ///
    /// Measured 2026-09-12 (release, median of 20 rounds after 2 warmup;
    /// two runs agreed within ~10%, second run quoted):
    /// -  50k records: postcard 4.49 ms (lock held), postcard+zlib 140.26 ms;
    ///   sizes 3.72 MiB -> 0.67 MiB zlib
    /// - 100k records: postcard 8.81 ms (lock held), postcard+zlib 265.66 ms;
    ///   sizes 7.44 MiB -> 1.40 MiB zlib
    ///
    /// Decision: 100k lands at ~266-294 ms (both runs >=100 ms), so per-batch
    /// checkpointing was replaced with a 2 s clock gate in execution.rs
    /// (CHECKPOINT_INTERVAL). The first checkpoint of a scan still fires on
    /// the first batch commit, and cancellation/completion writes are
    /// unconditional. Note zlib is ~97% of the cost; the lock-held postcard
    /// pass is only ~9 ms, so the gate primarily removes wasted background
    /// compression work, not lock contention.
    #[test]
    #[ignore = "benchmark: run with --ignored"]
    fn bench_checkpoint_serialization() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            for count in [50_000usize, 100_000] {
                let (records, directories) = synthetic_records(count);
                let view = ScanRecordForWrite {
                    version: SCAN_VERSION,
                    records: &records,
                    directories: &directories,
                };

                const WARMUP_ROUNDS: usize = 2;
                const ROUNDS: usize = 20;
                let mut serialize_samples = Vec::new();
                let mut serialize_zlib_samples = Vec::new();
                let mut raw_len = 0usize;
                let mut zlib_len = 0usize;

                for round in 0..WARMUP_ROUNDS + ROUNDS {
                    let start = std::time::Instant::now();
                    let raw = postcard::to_allocvec(&view).unwrap();
                    let serialize_ms = start.elapsed().as_secs_f64() * 1e3;

                    let start = std::time::Instant::now();
                    let mut encoder = ZlibEncoder::new(Vec::with_capacity(raw.len() / 4));
                    encoder.write_all(&raw).await.unwrap();
                    encoder.shutdown().await.unwrap();
                    let zlib_ms = start.elapsed().as_secs_f64() * 1e3;

                    if round >= WARMUP_ROUNDS {
                        serialize_samples.push(serialize_ms);
                        serialize_zlib_samples.push(serialize_ms + zlib_ms);
                        raw_len = raw.len();
                        zlib_len = encoder.into_inner().len();
                    }
                }

                let median = |mut v: Vec<f64>| {
                    v.sort_by(|a, b| a.total_cmp(b));
                    v[v.len() / 2]
                };
                println!("{count} records:");
                println!(
                    "  postcard only (lock held) median: {:7.2} ms",
                    median(serialize_samples)
                );
                println!(
                    "  postcard + zlib (full checkpoint) median: {:7.2} ms",
                    median(serialize_zlib_samples)
                );
                println!(
                    "  sizes: postcard {:.2} MiB, zlib {:.2} MiB",
                    raw_len as f64 / (1024.0 * 1024.0),
                    zlib_len as f64 / (1024.0 * 1024.0)
                );
            }
        });
    }

    /// Synthetic but structurally faithful record map: real-ish library paths
    /// (artist/album nesting, ~70-100 byte keys) and spread mtimes, all
    /// deterministic so every run measures identical input. `FxHashMap` has no
    /// random seed, so postcard output (and thus size) is run-stable too.
    fn synthetic_records(count: usize) -> (FxHashMap<Utf8PathBuf, SystemTime>, Vec<Utf8PathBuf>) {
        let mut records = FxHashMap::default();
        for i in 0..count {
            let artist = i / 500;
            let album = (i / 25) % 20;
            let track = i % 25;
            records.insert(
                Utf8PathBuf::from(format!(
                    "/music/Artist {artist:03}/Album {album:02} (Deluxe Edition)/{track:02} - Song Title {i:06}.flac"
                )),
                UNIX_EPOCH + Duration::from_secs(1_300_000_000 + (i as u64 * 7919) % 900_000_000),
            );
        }
        (records, vec![Utf8PathBuf::from("/music")])
    }
}
