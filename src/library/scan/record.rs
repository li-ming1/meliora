use std::{
    fs::File,
    io::{Error, ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use async_compression::tokio::bufread::ZlibDecoder;
use camino::Utf8PathBuf;
use flate2::{Compression, write::ZlibEncoder};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, BufReader},
    sync::Mutex,
    task::spawn_blocking,
};
use tracing::{error, info};

/// 扫描算法版本。递增它即可强制一次全量重扫。
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
    let file = match tokio::fs::File::open(path)
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

    // 数据目录里的文件可能被同步盘/备份工具替换成高压缩比数据：无上限解压会在
    // 启动路径上直接撞内存上限，且损坏文件会被每次启动重复读取。
    const MAX_SCAN_RECORD_BYTES: u64 = 64 * 1024 * 1024;
    let mut bytes = Vec::new();
    let _ = file
        .take(MAX_SCAN_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .await;
    if bytes.len() as u64 > MAX_SCAN_RECORD_BYTES {
        error!("Could not read scan record: decompressed size exceeds the limit");
        error!("Scanning will be slow until the scan record is rebuilt");
        return ScanRecord::new_current();
    }

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

/// 记录原子写入中某一个失败阶段的日志，并带上调用方给用户的提示。`stage` 用来补全
/// 消息，例如 `write <label>`。
fn log_write_failure(stage: &str, error: &Error, failure_hint: Option<&'static str>) {
    error!("Could not {stage}: {:?}", error);
    if let Some(hint) = failure_hint {
        error!("{hint}");
    }
}

/// 用 zlib 压缩 `data`，并经由临时文件原子替换 `path`，保留写入流水线那套逐阶段失败
/// 日志。它经 `write_record_atomic` 在阻塞池上运行。
fn compress_write_rename(
    path: PathBuf,
    data: Vec<u8>,
    label: String,
    failure_hint: Option<&'static str>,
) -> Result<(), ()> {
    let tmp_path = path.with_extension("hsr.tmp");

    let file = match File::create(&tmp_path) {
        Ok(file) => file,
        // 什么都没创建出来，因此没有临时文件需要清理
        Err(e) => {
            log_write_failure(&format!("create {label} file"), &e, failure_hint);
            return Err(());
        }
    };
    let mut encoder = ZlibEncoder::new(file, Compression::default());

    if let Err(e) = encoder.write_all(&data) {
        log_write_failure(&format!("write {label}"), &e, failure_hint);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(());
    }
    let file = match encoder.finish() {
        Ok(file) => file,
        Err(e) => {
            log_write_failure(&format!("close {label}"), &e, failure_hint);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(());
        }
    };
    // 改名之前先关闭句柄，这样 Windows 绝不会看到源文件仍处于打开状态
    drop(file);

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        log_write_failure(&format!("rename {label} into place"), &e, failure_hint);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(());
    }
    Ok(())
}

async fn write_record_atomic(
    path: &Path,
    data: Vec<u8>,
    label: &str,
    log_success: bool,
    failure_hint: Option<&'static str>,
) {
    let label = label.to_string();
    let path = path.to_path_buf();

    // 10 万条记录下 zlib 占记录写入成本的约 97%，所以压缩+写入+改名这后半段放到
    // 阻塞池上跑，而不是扫描工作线程；阶段失败在函数内部记录日志（并删掉临时文件）。
    // tmp 路径与第二次 label clone 留在 move 闭包之外：下面那条 join-panic 分支
    // 还得做清理和日志。
    let tmp_path = path.with_extension("hsr.tmp");
    let task_label = label.clone();
    match spawn_blocking(move || compress_write_rename(path, data, task_label, failure_hint)).await
    {
        Ok(Ok(())) => {
            if log_success {
                info!("Scan record saved successfully");
            }
        }
        Ok(Err(())) => {}
        // 阻塞任务在改名前 panic——丢掉可能只写了一半的临时文件，保留旧记录
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            error!("Could not write {label}: {:?}", error);
            if let Some(hint) = failure_hint {
                error!("{hint}");
            }
        }
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

    /// 检查点序列化成本基准，手工运行方式：
    /// `cargo test --release --features kugou -- bench_checkpoint --ignored --nocapture`
    ///
    /// 测的正是 `write_checkpoint` 每次调用所做的事，只不含磁盘 IO（那一段发生在
    /// `write_record_atomic` 的锁之外）：
    /// 1. 对整张表做 `postcard::to_allocvec`——这是持有检查点 `Mutex` 的那一段，
    ///    所以它同样会让 `commit_batch` 的 `merge_checkpoint_records` 一并阻塞这么久；
    /// 2. 走 `write_record_atomic` 所用的同一条 `flate2` `ZlibEncoder` 流水线做
    ///    zlib 压缩。
    ///
    /// 性能审计给出的决策规则：10 万条记录下单个检查点 <50 ms -> 保留每批次写检查点
    /// 并把数字记在这里；>=100 ms -> 在 execution.rs 里用 >=2 s 的时钟间隔给检查点写入
    /// 加闸门。介于两者之间：除非出现进一步证据，否则保留。
    ///
    /// 2026-09-12 实测（release，2 轮预热后取 20 轮的中位数；两次运行相符在 ~10% 以内，
    /// 引用第二次）：
    /// - 50k 条记录：postcard 4.49 ms（持锁），postcard+zlib 140.26 ms；
    ///   体积 3.72 MiB -> 0.67 MiB zlib
    /// - 100k 条记录：postcard 8.81 ms（持锁），postcard+zlib 265.66 ms；
    ///   体积 7.44 MiB -> 1.40 MiB zlib
    ///
    /// 结论：100k 落在 ~266-294 ms（两次运行都 >=100 ms），所以每批次写检查点被换成
    /// execution.rs 里的 2 s 时钟闸门（CHECKPOINT_INTERVAL）。一次扫描的首个检查点仍在
    /// 首次批次提交时触发，取消/完成时的写入无条件。注意 zlib 约占成本的 97%；持锁的
    /// postcard 那一趟只有 ~9 ms，所以这道闸门主要去掉的是无谓的后台压缩工作，
    /// 而不是锁竞争。
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
                    let mut encoder =
                        ZlibEncoder::new(Vec::with_capacity(raw.len() / 4), Compression::default());
                    encoder.write_all(&raw).unwrap();
                    let compressed = encoder.finish().unwrap();
                    let zlib_ms = start.elapsed().as_secs_f64() * 1e3;

                    if round >= WARMUP_ROUNDS {
                        serialize_samples.push(serialize_ms);
                        serialize_zlib_samples.push(serialize_ms + zlib_ms);
                        raw_len = raw.len();
                        zlib_len = compressed.len();
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

    /// 合成的但结构上如实的记录表：真实感的库路径（artist/album 嵌套、键长约
    /// 70-100 字节）与分散的 mtime，全部确定，好让每次运行测的是同一份输入。
    /// `FxHashMap` 没有随机种子，因此 postcard 输出（连同体积）在运行间也是稳定的。
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
