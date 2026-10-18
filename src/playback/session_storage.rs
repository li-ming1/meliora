use std::{io::BufReader, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncWriteExt, sync::watch};
use tracing::{error, warn};

use crate::playback::{events::RepeatState, queue::QueueItemData};

/// 队列快照在播放线程与本 worker 之间以 `Arc` 共享：发送会话只加引用计数，
/// 不会深拷贝 10 万条队列；worker 序列化自己那一份引用，不阻塞后续发送。
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
            // 序列化前先把会话从 watch 借用里克隆出来：队列是 Arc 的，克隆只加
            // 两个引用计数，序列化不再阻塞播放线程的下一次发送。
            let session = self.rx.borrow_and_update().clone();
            // 序列化放到异步执行器之外：10 万条队列要吃几十 ms 的纯 CPU，足以拖住
            // 共享同一 runtime 的其它任务。循环会 await 结果，写入因此严格有序。
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

            // 先写临时文件再改名就位（与 `library/scan/record.rs` 同一套路）：就地
            // truncate + 重写，进程写到一半死掉时会留下被截断的
            // playback_session.json。`fs::rename` 在 Windows 上可替换已存在的目标，
            // 因此在所有受支持平台上这次交换都是原子的。
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
            // 首次启动没有会话文件，保持静默。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return PlaybackSessionData::default();
            }
            Err(e) => {
                warn!(
                    error = %e,
                    file = %file_path.display(),
                    "failed to open playback session file; starting with empty session"
                );
                return PlaybackSessionData::default();
            }
        };

        match serde_json::from_reader(BufReader::new(file)) {
            Ok(session) => session,
            // serde_json::Error 的 Display 自带行/列号，可直接指认损坏位置。
            Err(e) => {
                warn!(
                    error = %e,
                    file = %file_path.display(),
                    "playback session file is corrupt; starting with empty session"
                );
                PlaybackSessionData::default()
            }
        }
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

    /// 任何不可用的输入，`load` 都必须给出完全中性的默认值：没有队列条目、没有
    /// 位置、随机播放关闭、循环关闭。
    fn assert_default(session: &PlaybackSessionData) {
        let default = PlaybackSessionData::default();
        assert!(session.queue.is_empty());
        assert!(session.original_queue.is_empty());
        assert_eq!(session.queue_position, default.queue_position);
        assert_eq!(session.shuffle, default.shuffle);
        assert_eq!(session.repeat, default.repeat);
    }

    #[test]
    fn load_returns_default_when_file_is_missing() {
        let dir = create_test_dir();
        let path = dir.join("session.json");

        let session = PlaybackSessionStorageWorker::load(&path);
        assert_default(&session);
    }

    #[test]
    fn load_returns_default_when_json_is_invalid() {
        let dir = create_test_dir();
        let path = dir.join("session.json");
        fs::write(&path, "{not valid json").unwrap();

        let session = PlaybackSessionStorageWorker::load(&path);
        assert_default(&session);
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

    /// GPUI_HARDCORE §31 的启动路径实测依据：`load` 在首帧之前跑在主线程上，所以
    /// 这里量化"恢复超大队列到底贵在哪"（解析 queue + original_queue，各 10k 条，
    /// 另加 app.rs 为 watch 通道付出的那次 `queue.clone()`）。
    /// 运行方式：cargo test --release --features kugou -- bench_large_session --ignored --nocapture
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

        // 预热 + 10 轮计时，报告中位数
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
