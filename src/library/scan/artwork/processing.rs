use std::{
    collections::hash_map::Entry,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};

use camino::Utf8PathBuf;
use futures::future::{BoxFuture, FutureExt, Shared};
use rustc_hash::FxHashMap;
use tokio::{sync::Semaphore, task::spawn_blocking};
use tracing::warn;

use super::ArtIdCache;
use crate::library::scan::{
    decode::{FileArt, ProcessedArt, ScannedArt, process_owned_album_art},
    discover::FolderArtCandidate,
};
use crate::sync::RecoveredMutex;

/// 一次已在途的文件夹封面加载，由同一路径的所有请求方共享。
type FolderArtFuture = Shared<BoxFuture<'static, Option<Arc<Vec<u8>>>>>;

/// 给并发图片解码封顶——每个已在途的条目都持有一张全尺寸 JPEG 和一张缩略图，
/// 所以限制并发解码数以免内存占用过高。
const MAX_DECODE_WORKERS: usize = 4;

/// 单个文件夹封面路径的加载器状态。
enum FolderArtLoadState {
    /// 加载正在进行；请求方通过共享 future 加入。
    Loading(FolderArtFuture),
    /// 加载已结束；字节只在还持有 `Arc` 期间存活，弱句柄过期就重新发起一次加载。
    Ready(Weak<Vec<u8>>),
}

#[derive(Clone)]
pub struct FolderArtLoader {
    states: Arc<Mutex<FxHashMap<Utf8PathBuf, FolderArtLoadState>>>,
    permits: Arc<Semaphore>,
    /// 真正发起文件读取的次数（扫描内缓存命中不计）。完成日志用它度量
    /// "零变更扫描仍会重读文件夹封面"这一已知成本的规模。
    reads: Arc<AtomicUsize>,
}

impl FolderArtLoader {
    pub fn new(concurrency: usize) -> Self {
        Self {
            states: Arc::new(Mutex::new(FxHashMap::default())),
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn read_count(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    pub async fn load(&self, candidate: FolderArtCandidate) -> Option<ScannedArt> {
        let path = candidate.path;
        let future = {
            let mut states = self.states.lock_recovered();
            match states.entry(path.clone()) {
                Entry::Occupied(mut entry) => match entry.get() {
                    FolderArtLoadState::Loading(future) => future.clone(),
                    FolderArtLoadState::Ready(bytes) => match bytes.upgrade() {
                        Some(bytes) => return Some(ScannedArt::folder(bytes, candidate.rank)),
                        None => {
                            let future = self.load_future(path.clone());
                            entry.insert(FolderArtLoadState::Loading(future.clone()));
                            future
                        }
                    },
                },
                Entry::Vacant(entry) => {
                    let future = self.load_future(path.clone());
                    entry.insert(FolderArtLoadState::Loading(future.clone()));
                    future
                }
            }
        };

        let bytes = match future.await {
            Some(bytes) => bytes,
            None => {
                // 读取失败：丢掉这条死条目，让下一个请求重新发起一次加载，
                // 而不是永远 join 到一个已经解析成 None 的共享 future
                // （重试语义与上面那条弱句柄 Ready 的分支相同）。
                self.states.lock_recovered().remove(&path);
                return None;
            }
        };
        self.states
            .lock_recovered()
            .insert(path, FolderArtLoadState::Ready(Arc::downgrade(&bytes)));
        Some(ScannedArt::folder(bytes, candidate.rank))
    }

    fn load_future(&self, path: Utf8PathBuf) -> FolderArtFuture {
        let permits = Arc::clone(&self.permits);
        let reads = Arc::clone(&self.reads);
        async move {
            let _permit = permits.acquire_owned().await.ok()?;
            reads.fetch_add(1, Ordering::Relaxed);
            spawn_blocking(move || std::fs::read(path).ok().map(Arc::new))
                .await
                .ok()
                .flatten()
        }
        .boxed()
        .shared()
    }
}

/// 单个封面哈希的解码状态。
enum ArtworkState {
    /// artwork 行已存在于数据库；不保留任何缓冲区。
    Existing,
    /// 解码正在进行；请求方通过共享 future 加入。
    Processing(ArtworkFuture),
}

/// 一次已在途的封面解码，由同一哈希的所有请求方共享。
type ArtworkFuture = Shared<BoxFuture<'static, Option<Arc<ProcessedArt>>>>;

/// 每个新的封面哈希只在 Tokio 的有界阻塞池上转换一次。
#[derive(Clone)]
pub struct ArtworkProcessor {
    states: Arc<Mutex<FxHashMap<u64, ArtworkState>>>,
    permits: Arc<Semaphore>,
    concurrency: usize,
}

impl ArtworkProcessor {
    pub fn new(existing_hashes: impl IntoIterator<Item = u64>) -> Self {
        let states = Arc::new(Mutex::new(
            existing_hashes
                .into_iter()
                .map(|hash| (hash, ArtworkState::Existing))
                .collect(),
        ));
        // 一半核心用于解码封面，并做钳位以保证峰值内存有界。
        let concurrency = std::thread::available_parallelism()
            .map(|count| (count.get() / 2).clamp(1, MAX_DECODE_WORKERS))
            .unwrap_or(1);

        Self {
            states,
            permits: Arc::new(Semaphore::new(concurrency)),
            concurrency,
        }
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    pub async fn process_file_art(&self, art: &mut FileArt) {
        let embedded = art
            .embedded
            .as_mut()
            .and_then(|candidate| self.prepare(candidate));
        let folder = art
            .folder
            .as_mut()
            .and_then(|candidate| self.prepare(candidate));

        let (embedded, folder) = tokio::join!(
            Self::await_processed(embedded),
            Self::await_processed(folder)
        );

        if let Some(candidate) = &mut art.embedded {
            candidate.processed = embedded;
        }
        if let Some(candidate) = &mut art.folder {
            candidate.processed = folder;
        }
    }

    fn prepare(&self, candidate: &mut ScannedArt) -> Option<ArtworkFuture> {
        let raw = candidate.raw.take()?;

        let mut states = self.states.lock_recovered();
        match states.entry(candidate.hash) {
            Entry::Occupied(entry) => match entry.get() {
                ArtworkState::Existing => None,
                ArtworkState::Processing(result) => Some(result.clone()),
            },
            Entry::Vacant(entry) => {
                let permits = Arc::clone(&self.permits);
                let result = async move {
                    let _permit = permits.acquire_owned().await.ok()?;
                    spawn_blocking(move || process_owned_album_art(raw))
                        .await
                        .map_err(|e| warn!("Artwork processing task failed: {:?}", e))
                        .ok()?
                        .map(|(image, thumb)| Arc::new(ProcessedArt { image, thumb }))
                        .map_err(|e| warn!("Failed to process album art: {:?}", e))
                        .ok()
                }
                .boxed()
                .shared();

                entry.insert(ArtworkState::Processing(result.clone()));
                Some(result)
            }
        }
    }

    async fn await_processed(result: Option<ArtworkFuture>) -> Option<Arc<ProcessedArt>> {
        match result {
            Some(result) => result.await,
            None => None,
        }
    }

    /// 哈希已被数据库写入者解析完毕后，丢掉处理好的缓冲区。
    pub fn mark_resolved(&self, art: &FileArt, ids: &ArtIdCache) {
        let mut states = self.states.lock_recovered();
        for candidate in [&art.embedded, &art.folder].into_iter().flatten() {
            if ids.contains_key(&candidate.hash) {
                states.insert(candidate.hash, ArtworkState::Existing);
            }
        }
    }
}
