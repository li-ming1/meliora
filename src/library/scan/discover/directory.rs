use std::{
    fs::Metadata,
    sync::Arc,
    time::{Duration, SystemTime},
};

use camino::{Utf8Path, Utf8PathBuf};
use futures::{FutureExt, stream::FuturesUnordered};
use rustc_hash::FxHashMap;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::spawn_blocking,
};
use tracing::error;

use super::{FolderArtCandidate, SidecarLyrics};
use crate::{
    library::scan::{
        decode::{is_folder_art_name, is_hidden_file},
        fs_case::is_case_insensitive,
    },
    media::{lookup_table::can_be_read, traits::MediaProviderFeatures},
};

#[derive(Clone)]
pub(crate) struct DirectoryReadPolicy {
    pub(super) semaphores: Arc<Vec<Arc<Semaphore>>>,
    mounts: Arc<Vec<Utf8PathBuf>>,
    mount_to_channel: Arc<FxHashMap<Utf8PathBuf, usize>>,
    max_pending: usize,
}

impl DirectoryReadPolicy {
    pub(crate) fn normal(concurrency: usize) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            semaphores: Arc::new(vec![Arc::new(Semaphore::new(concurrency))]),
            mounts: Arc::new(Vec::new()),
            mount_to_channel: Arc::new(FxHashMap::default()),
            max_pending: concurrency * 2,
        }
    }

    pub(crate) fn slow(
        mounts: Vec<Utf8PathBuf>,
        mount_to_channel: FxHashMap<Utf8PathBuf, usize>,
        channel_count: usize,
    ) -> Self {
        let channel_count = channel_count.max(1);
        Self {
            semaphores: Arc::new(
                (0..channel_count)
                    .map(|_| Arc::new(Semaphore::new(1)))
                    .collect(),
            ),
            mounts: Arc::new(mounts),
            mount_to_channel: Arc::new(mount_to_channel),
            max_pending: channel_count * 4,
        }
    }

    pub(super) fn channel_for(&self, path: &Utf8Path) -> usize {
        self.mounts
            .iter()
            .find(|mount| path.as_std_path().starts_with(mount.as_std_path()))
            .and_then(|mount| self.mount_to_channel.get(mount))
            .copied()
            .unwrap_or(0)
            .min(self.semaphores.len() - 1)
    }

    /// 在服务于 `path` 的那个通道上取一个许可，让它的读取受到节流。
    async fn permit_for(&self, path: &Utf8Path) -> OwnedSemaphorePermit {
        Arc::clone(&self.semaphores[self.channel_for(path)])
            .acquire_owned()
            .await
            .expect("directory semaphore closed")
    }

    pub(crate) fn max_pending(&self) -> usize {
        self.max_pending
    }

    pub(super) async fn read(
        &self,
        directory: Utf8PathBuf,
    ) -> std::io::Result<ScanDirectorySnapshot> {
        let _permit = self.permit_for(&directory).await;
        spawn_blocking(move || read_scan_directory(&directory))
            .await
            .expect("directory read task panicked")
    }

    pub(super) async fn inspect(&self, path: Utf8PathBuf) -> std::io::Result<PathInspection> {
        let _permit = self.permit_for(&path).await;
        spawn_blocking(move || {
            let metadata = std::fs::metadata(&path)?;
            let scan_timestamp = metadata
                .is_file()
                .then(|| supported_scan_timestamp(&path, &metadata, sidecar_modified(&path)))
                .flatten();
            Ok(PathInspection {
                metadata,
                scan_timestamp,
            })
        })
        .await
        .expect("path inspection task panicked")
    }
}

pub(crate) type PendingDirectoryRead =
    futures::future::BoxFuture<'static, (Utf8PathBuf, std::io::Result<ScanDirectorySnapshot>)>;

pub(crate) fn schedule_directory_read(
    pending: &mut FuturesUnordered<PendingDirectoryRead>,
    policy: DirectoryReadPolicy,
    directory: Utf8PathBuf,
) {
    pending.push(
        async move {
            let result = policy.read(directory.clone()).await;
            (directory, result)
        }
        .boxed(),
    );
}

pub fn sidecar_lyrics_path(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let stem = path.file_stem()?;
    let parent = path.parent()?;
    Some(parent.join(format!("{}.lrc", stem)))
}

/// 音频文件的修改时间；若其 .lrc sidecar 更新则取 sidecar 的。
fn file_scan_timestamp_from_metadata(
    metadata: &Metadata,
    lyrics_timestamp: Option<SystemTime>,
) -> Option<SystemTime> {
    let audio_timestamp = metadata.modified().ok()?;
    let base_timestamp = match lyrics_timestamp {
        Some(lyrics_timestamp) if lyrics_timestamp > audio_timestamp => lyrics_timestamp,
        _ => audio_timestamp,
    };

    let presence_offset = if lyrics_timestamp.is_some() {
        // +1 微秒，这样 mtime 相同时新建一个 sidecar 仍能触发重扫
        Duration::from_micros(1)
    } else {
        Duration::ZERO
    };
    base_timestamp
        .checked_add(presence_offset)
        .or(Some(base_timestamp))
}

#[cfg(test)]
pub(crate) fn file_scan_timestamp(path: &Utf8Path) -> Option<SystemTime> {
    let metadata = std::fs::metadata(path).ok()?;
    file_scan_timestamp_from_metadata(&metadata, sidecar_modified(path))
}

fn sidecar_modified(path: &Utf8Path) -> Option<SystemTime> {
    sidecar_lyrics_path(path)
        .and_then(|lrc_path| std::fs::metadata(lrc_path).ok())
        .and_then(|metadata| metadata.modified().ok())
}

fn supported_scan_timestamp(
    path: &Utf8Path,
    metadata: &Metadata,
    lyrics_timestamp: Option<SystemTime>,
) -> Option<SystemTime> {
    let timestamp = file_scan_timestamp_from_metadata(metadata, lyrics_timestamp)?;
    can_be_read(
        path.as_std_path(),
        MediaProviderFeatures::PROVIDES_METADATA | MediaProviderFeatures::ALLOWS_INDEXING,
    )
    .unwrap_or(false)
    .then_some(timestamp)
}

pub(super) struct ScanDirectoryEntry {
    pub(super) path: Utf8PathBuf,
    pub(super) metadata: Metadata,
    pub(super) scan_timestamp: Option<SystemTime>,
    pub(super) sidecar_lyrics: SidecarLyrics,
}

pub(crate) struct ScanDirectorySnapshot {
    pub(super) entries: Vec<ScanDirectoryEntry>,
    pub(crate) folder_art: Option<FolderArtCandidate>,
}

pub(super) struct PathInspection {
    pub(super) metadata: Metadata,
    pub(super) scan_timestamp: Option<SystemTime>,
}

struct PendingDirectoryEntry {
    /// 解析后的路径（符号链接已 canonicalize 到目标）。
    path: Utf8PathBuf,
    /// 仅符号链接需要：链接自身的路径（sidecar/文件夹封面要就近于链接名而不是目标）。
    /// 普通文件两者相同，用 `None` 省掉每文件一次整串克隆。
    raw_path: Option<Utf8PathBuf>,
    metadata: Metadata,
}

fn normalized_file_name(path: &Utf8Path, case_insensitive: bool) -> Option<String> {
    let name = path.file_name()?;
    Some(if case_insensitive {
        name.to_lowercase()
    } else {
        name.to_string()
    })
}

/// 列举出来的一条目录项 → `PendingDirectoryEntry`。原先七条"记录并跳过"的分支
/// 摊在主循环里，占了 161 行函数体的 68 行；收成 `Option` 后循环只写一次
/// `else { continue }`。每条 `error!` 的文案与判据逐字保留，失败即放弃这一条
/// 的语义不变。
fn resolve_scan_entry(result: std::io::Result<std::fs::DirEntry>) -> Option<PendingDirectoryEntry> {
    let entry = match result {
        Ok(entry) => entry,
        Err(error) => {
            error!("Failed to read directory entry: {:?}", error);
            return None;
        }
    };
    let raw_std_path = entry.path();
    let raw_path = match Utf8PathBuf::try_from(raw_std_path.clone()) {
        Ok(path) => path,
        Err(error) => {
            error!(
                "Failed to convert path {:?} to UTF-8: {:?}",
                raw_std_path, error
            );
            return None;
        }
    };
    let file_type = match entry.file_type() {
        Ok(file_type) => file_type,
        Err(error) => {
            error!(
                "Failed to inspect directory entry {:?}: {:?}",
                raw_path, error
            );
            return None;
        }
    };
    let (path, raw_path, metadata) = if file_type.is_symlink() {
        let canonical = match raw_std_path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                error!("Failed to canonicalize path {:?}: {:?}", raw_path, error);
                return None;
            }
        };
        let path = match Utf8PathBuf::try_from(canonical) {
            Ok(path) => path,
            Err(error) => {
                error!(
                    "Failed to convert path {:?} to UTF-8: {:?}",
                    raw_path, error
                );
                return None;
            }
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                error!("Failed to inspect path {:?}: {:?}", path, error);
                return None;
            }
        };
        (path, Some(raw_path), metadata)
    } else {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                error!(
                    "Failed to inspect directory entry {:?}: {:?}",
                    raw_path, error
                );
                return None;
            }
        };
        (raw_path, None, metadata)
    };

    Some(PendingDirectoryEntry {
        path,
        raw_path,
        metadata,
    })
}

/// 把一条已解析的目录项落成快照条目（sidecar 歌词存在性 + 扫描时间戳）。从
/// `read_scan_directory` 的物化映射里原样搬来，三分支的判据不变：符号链接的
/// 目标可能在本目录之外，所以直接 stat 它的 sidecar 而不是信任本目录的列举；
/// stat 失败并不能证明文件不存在，保留解码那一趟的探测；绝大多数目录没有
/// `.lrc`，空表短路省去每文件三次堆分配。
fn materialize_scan_entry(
    entry: PendingDirectoryEntry,
    lyrics_timestamps: &FxHashMap<String, SystemTime>,
    case_insensitive: bool,
) -> ScanDirectoryEntry {
    let naming = entry.raw_path.as_deref().unwrap_or(&entry.path);
    let (lyrics_timestamp, sidecar_lyrics) = if entry.raw_path.is_some() {
        let lyrics_timestamp = sidecar_modified(&entry.path);
        let sidecar_lyrics = if lyrics_timestamp.is_some() {
            SidecarLyrics::Present
        } else {
            SidecarLyrics::Unknown
        };
        (lyrics_timestamp, sidecar_lyrics)
    } else if lyrics_timestamps.is_empty() {
        (None, SidecarLyrics::Absent)
    } else {
        let lyrics_timestamp = sidecar_lyrics_path(naming)
            .and_then(|path| normalized_file_name(&path, case_insensitive))
            .and_then(|name| lyrics_timestamps.get(&name).copied());
        let sidecar_lyrics = if lyrics_timestamp.is_some() {
            SidecarLyrics::Present
        } else {
            SidecarLyrics::Absent
        };
        (lyrics_timestamp, sidecar_lyrics)
    };

    let scan_timestamp = entry
        .metadata
        .is_file()
        .then(|| supported_scan_timestamp(&entry.path, &entry.metadata, lyrics_timestamp))
        .flatten();

    ScanDirectoryEntry {
        path: entry.path,
        metadata: entry.metadata,
        scan_timestamp,
        sidecar_lyrics,
    }
}

pub(crate) fn read_scan_directory(dir: &Utf8Path) -> std::io::Result<ScanDirectorySnapshot> {
    let case_insensitive = is_case_insensitive(dir);
    let mut pending = Vec::new();
    let mut lyrics_timestamps = FxHashMap::default();
    let mut folder_art: Option<FolderArtCandidate> = None;

    for result in std::fs::read_dir(dir)? {
        let Some(entry) = resolve_scan_entry(result) else {
            continue;
        };

        // sidecar/封面都按“链接名”就近查找：symlink 用 raw，普通文件即 path。
        let naming = entry.raw_path.as_deref().unwrap_or(&entry.path);
        let is_lyrics = naming.extension().is_some_and(|extension| {
            extension == "lrc" || (case_insensitive && extension.eq_ignore_ascii_case("lrc"))
        });
        if entry.metadata.is_file()
            && is_lyrics
            && let Some(name) = normalized_file_name(naming, case_insensitive)
            && let Ok(modified) = entry.metadata.modified()
        {
            lyrics_timestamps.insert(name, modified);
        }

        let art_rank = naming
            .extension()
            .zip(naming.file_stem())
            .and_then(|(extension, stem)| is_folder_art_name(stem, extension));
        if entry.metadata.is_file()
            && let Some(rank) = art_rank
            && !is_hidden_file(&entry.metadata)
        {
            let candidate = FolderArtCandidate {
                path: naming.to_owned(),
                rank,
            };
            if folder_art
                .as_ref()
                .is_none_or(|current| (rank, &candidate.path) < (current.rank, &current.path))
            {
                folder_art = Some(candidate);
            }
        }

        pending.push(entry);
    }

    let entries = pending
        .into_iter()
        .map(|entry| materialize_scan_entry(entry, &lyrics_timestamps, case_insensitive))
        .collect();
    Ok(ScanDirectorySnapshot {
        entries,
        folder_art,
    })
}
