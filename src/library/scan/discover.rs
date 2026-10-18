mod cleanup;
mod directory;
mod full;
mod targeted;

#[cfg(test)]
mod helpers;
#[cfg(test)]
#[path = "discover/tests.rs"]
mod tests;

use std::{
    sync::{Arc, Mutex as StdMutex},
    time::SystemTime,
};

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;
use tokio::sync::mpsc::Sender;

use crate::library::scan::fs_case::{fold_path, same_file};
use crate::sync::RecoveredMutex;

#[cfg(test)]
pub(crate) use directory::read_scan_directory;
pub(crate) use directory::{DirectoryReadPolicy, sidecar_lyrics_path};
pub use full::{cleanup_stale_tracks, discover};
pub use targeted::{reconcile_rescan_paths, rescan_discover};

pub(super) use cleanup::{
    canonicalize_or_keep, delete_tracks, fold_excluded_roots, is_missing, is_under_excluded,
    missing_paths,
};
#[cfg(test)]
pub(super) use directory::file_scan_timestamp;
pub(super) use directory::{PendingDirectoryRead, schedule_directory_read};

/// 纯大小写改名，且保留旧时间戳，这样被修改过的内容仍会被重扫。
pub(crate) type Relocation = (Utf8PathBuf, Utf8PathBuf, SystemTime);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FolderArtCandidate {
    pub(crate) path: Utf8PathBuf,
    pub(crate) rank: u8,
}

/// 关于某曲目 `.lrc` sidecar 的目录列举知识，随事件一起携带，好让解码那一趟跳过
/// 对列举已证明不存在的文件做探测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarLyrics {
    /// 没有目录级的 lrc 知识支撑这个路径（定向扫描、直接传入的文件目标）——
    /// 解码那一趟保留自己的探测。
    Unknown,
    /// 列举（或对符号链接的直接 stat）找到了这个 sidecar。
    Present,
    /// 列举证明曲目旁边没有 sidecar。
    Absent,
}

#[derive(Debug)]
pub(crate) struct DiscoveredPath {
    pub(crate) path: Utf8PathBuf,
    pub(crate) timestamp: SystemTime,
    pub(crate) folder_art: Option<FolderArtCandidate>,
    pub(crate) sidecar_lyrics: SidecarLyrics,
}

pub(crate) type FolderArtObservationMap = FxHashMap<Utf8PathBuf, Option<FolderArtCandidate>>;

#[derive(Clone, Default)]
pub(crate) struct FolderArtObservations {
    inner: Arc<StdMutex<FolderArtObservationMap>>,
}

impl FolderArtObservations {
    pub(crate) fn record(&self, directory: Utf8PathBuf, candidate: Option<FolderArtCandidate>) {
        self.inner.lock_recovered().insert(directory, candidate);
    }

    pub(crate) fn snapshot(&self) -> FolderArtObservationMap {
        self.inner.lock_recovered().clone()
    }

    /// `directory` 的上一次观测。外层 `None` 表示该目录还没被观测过；内层 `None`
    /// 表示已观测过但没有文件夹封面候选。
    pub(crate) fn get(&self, directory: &Utf8Path) -> Option<Option<FolderArtCandidate>> {
        self.inner.lock_recovered().get(directory).cloned()
    }
}

pub(super) type FoldedIndex = FxHashMap<Utf8PathBuf, Vec<(Utf8PathBuf, SystemTime)>>;

pub(super) enum DiscoverAction {
    Skip,
    Scan(SystemTime),
    /// 可能是纯大小写改名——放开记录锁之后用 `same_file` 确认。
    Relocate {
        candidates: Vec<(Utf8PathBuf, SystemTime)>,
        ts: SystemTime,
    },
}

pub(super) fn classify(
    path: &Utf8Path,
    timestamp: SystemTime,
    records: &FxHashMap<Utf8PathBuf, SystemTime>,
    folded_index: &FoldedIndex,
) -> (DiscoverAction, Vec<(Utf8PathBuf, SystemTime)>) {
    if let Some(last_scan) = records.get(path) {
        let action = if *last_scan == timestamp {
            DiscoverAction::Skip
        } else {
            DiscoverAction::Scan(timestamp)
        };
        let stale = other_recorded_spellings(folded_index, path);
        return (action, stale);
    }

    let mut candidates = other_recorded_spellings(folded_index, path);
    if !candidates.is_empty() {
        // 优先时间戳相同的候选——搬迁这类候选可以跳过一次元数据重读
        candidates.sort_by_key(|(_, old_timestamp)| *old_timestamp != timestamp);
        return (
            DiscoverAction::Relocate {
                candidates,
                ts: timestamp,
            },
            Vec::new(),
        );
    }

    (DiscoverAction::Scan(timestamp), Vec::new())
}

/// 应用一次纯大小写改名。若元数据仍需读取则返回时间戳。
pub(super) async fn apply_relocation(
    path: &Utf8Path,
    timestamp: SystemTime,
    candidates: Vec<(Utf8PathBuf, SystemTime)>,
    relocate_tx: &Sender<Relocation>,
) -> Result<Option<SystemTime>, ()> {
    let Some((old, old_timestamp)) = confirm_relocation(candidates, path) else {
        return Ok(Some(timestamp));
    };

    relocate_tx
        .send((old, path.to_path_buf(), old_timestamp))
        .await
        .map_err(|_| ())?;
    Ok((old_timestamp != timestamp).then_some(timestamp))
}

fn other_recorded_spellings(
    index: &FoldedIndex,
    path: &Utf8Path,
) -> Vec<(Utf8PathBuf, SystemTime)> {
    index
        .get(&fold_path(path))
        .map(|candidates| {
            candidates
                .iter()
                .filter(|(key, _)| key.as_path() != path)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn confirm_relocation(
    candidates: Vec<(Utf8PathBuf, SystemTime)>,
    path: &Utf8Path,
) -> Option<(Utf8PathBuf, SystemTime)> {
    candidates.into_iter().find(|(old, _)| same_file(old, path))
}
