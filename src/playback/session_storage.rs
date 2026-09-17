use std::{io::BufReader, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncWriteExt, sync::watch};
use tracing::error;

use crate::playback::{events::RepeatState, queue::QueueItemData};

/// Queue snapshots are `Arc`-shared between the playback thread and this
/// worker: a session send bumps refcounts instead of deep-copying a
/// 100k-item queue, and the worker serializes its own ref without blocking
/// further sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackSessionData {
    pub queue: Arc<Vec<QueueItemData>>,
    pub original_queue: Arc<Vec<QueueItemData>>,
    pub queue_position: Option<usize>,
    pub shuffle: bool,
    pub repeat: RepeatState,
}

impl Default for PlaybackSessionData {
    fn default() -> Self {
        Self {
            queue: Arc::new(Vec::new()),
            original_queue: Arc::new(Vec::new()),
            queue_position: None,
            shuffle: false,
            repeat: RepeatState::NotRepeating,
        }
    }
}

pub struct PlaybackSessionStorageWorker {
    file_path: PathBuf,
    rx: watch::Receiver<PlaybackSessionData>,
}

impl PlaybackSessionStorageWorker {
    pub fn new(file_path: PathBuf, rx: watch::Receiver<PlaybackSessionData>) -> Self {
        Self { file_path, rx }
    }

    pub async fn run(mut self) {
        while self.rx.changed().await.is_ok() {
            // Clone the session out of the watch borrow before serializing:
            // with Arc'd queues this is two refcounts, and serialization no
            // longer blocks the playback thread's next send.
            let session = self.rx.borrow_and_update().clone();
            // Serialize off the async executor: a 100k-item queue takes tens of
            // ms of pure CPU, enough to stall other tasks sharing the runtime.
            // The loop awaits the result, so writes stay strictly ordered.
            let serialized_session = tokio::task::spawn_blocking(move || {
                serde_json::to_vec(&session).map(|mut json| {
                    json.push(b'\n');
                    json
                })
            })
            .await;

            let json = match serialized_session {
                Ok(Ok(json)) => json,
                Ok(Err(e)) => {
                    error!("Failed to serialize PlaybackSessionData: {}", e);
                    continue;
                }
                Err(e) => {
                    error!("Session serialization task failed: {}", e);
                    continue;
                }
            };

            // Write to a temporary file and rename it into place (same pattern
            // as `library/scan/record.rs`): an in-place truncate+rewrite can
            // leave a truncated playback_session.json behind when the process
            // dies mid-write. `fs::rename` replaces an existing target on
            // Windows, so the swap is atomic on every supported platform.
            let tmp_path = self.file_path.with_extension("json.tmp");

            let mut file = match fs::File::create(&tmp_path).await {
                Ok(file) => file,
                Err(e) => {
                    error!("Unable to create playback session temp file: {}", e);
                    continue;
                }
            };

            if let Err(e) = file.write_all(&json).await {
                error!("Failed to write playback session file: {}", e);
                let _ = fs::remove_file(&tmp_path).await;
                continue;
            }
            if let Err(e) = file.shutdown().await {
                error!("Failed to close playback session file: {}", e);
                let _ = fs::remove_file(&tmp_path).await;
                continue;
            }
            if let Err(e) = fs::rename(&tmp_path, &self.file_path).await {
                error!("Failed to rename playback session file into place: {}", e);
                let _ = fs::remove_file(&tmp_path).await;
            }
        }
    }

    pub fn load(file_path: &PathBuf) -> PlaybackSessionData {
        let file = match std::fs::File::open(file_path) {
            Ok(file) => file,
            Err(_) => return PlaybackSessionData::default(),
        };

        serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackSessionData, PlaybackSessionStorageWorker};
    use crate::{playback::events::RepeatState, test_support::TestDir};
    use std::{fs, sync::Arc};

    fn create_test_dir() -> TestDir {
        TestDir::new("meliora-session-storage-test")
    }

    #[test]
    fn load_returns_default_when_file_is_missing() {
        let dir = create_test_dir();
        let path = dir.join("session.json");

        let session = PlaybackSessionStorageWorker::load(&path);
        let default = PlaybackSessionData::default();

        assert!(session.queue.is_empty());
        assert!(session.original_queue.is_empty());
        assert_eq!(session.queue_position, default.queue_position);
        assert_eq!(session.shuffle, default.shuffle);
        assert_eq!(session.repeat, default.repeat);
    }

    #[test]
    fn load_returns_default_when_json_is_invalid() {
        let dir = create_test_dir();
        let path = dir.join("session.json");
        fs::write(&path, "{not valid json").unwrap();

        let session = PlaybackSessionStorageWorker::load(&path);
        let default = PlaybackSessionData::default();

        assert!(session.queue.is_empty());
        assert!(session.original_queue.is_empty());
        assert_eq!(session.queue_position, default.queue_position);
        assert_eq!(session.shuffle, default.shuffle);
        assert_eq!(session.repeat, default.repeat);
    }

    #[test]
    fn load_reads_valid_session_file() {
        let dir = create_test_dir();
        let path = dir.join("session.json");
        let expected = PlaybackSessionData {
            queue: Arc::new(Vec::new()),
            original_queue: Arc::new(Vec::new()),
            queue_position: Some(3),
            shuffle: true,
            repeat: RepeatState::RepeatingOne,
        };

        fs::write(&path, serde_json::to_vec(&expected).unwrap()).unwrap();

        let session = PlaybackSessionStorageWorker::load(&path);

        assert!(session.queue.is_empty());
        assert!(session.original_queue.is_empty());
        assert_eq!(session.queue_position, expected.queue_position);
        assert_eq!(session.shuffle, expected.shuffle);
        assert_eq!(session.repeat, expected.repeat);
    }

    /// Startup-path evidence for GPUI_HARDCORE §31: `load` runs on the main
    /// thread before the first frame, so this quantifies what a huge restored
    /// queue actually costs (parse of queue + original_queue, 10k items each,
    /// plus the extra `queue.clone()` app.rs pays for the watch channel).
    /// Run with: cargo test --release --features kugou -- bench_large_session --ignored --nocapture
    #[test]
    #[ignore = "benchmark: run with --ignored"]
    fn bench_large_session_load() {
        let dir = create_test_dir();
        let path = dir.join("session-10k.json");

        let item = |id: i64| {
            serde_json::json!({
                "db_id": id,
                "db_album_id": id / 10,
                "path": format!(r"C:\Music\artist\album\track-{id}.flac"),
            })
        };
        let queue: Vec<_> = (1..=10_000).map(item).collect();
        let session = serde_json::json!({
            "queue": queue,
            "original_queue": (1..=10_000).map(|id| item(id + 100_000)).collect::<Vec<_>>(),
            "queue_position": 4999,
            "shuffle": false,
            "repeat": "NotRepeating",
        });
        fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();
        let file_bytes = fs::metadata(&path).unwrap().len();
        println!("session file: {file_bytes} bytes (20k queue items)");

        // warm-up + 10 measured rounds, report the median
        let mut rounds: Vec<std::time::Duration> = Vec::with_capacity(11);
        for _ in 0..11 {
            let start = std::time::Instant::now();
            let loaded = PlaybackSessionStorageWorker::load(&path);
            let parse = start.elapsed();
            let start = std::time::Instant::now();
            let _queue_clone = loaded.queue.clone();
            let clone = start.elapsed();
            rounds.push(parse);
            if rounds.len() == 1 {
                println!("first round: parse {parse:?}, queue clone {clone:?}");
            }
            assert_eq!(loaded.queue.len(), 10_000);
        }
        rounds.sort();
        println!("median parse (20k items): {:?}", rounds[rounds.len() / 2]);
    }
}
