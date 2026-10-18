use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use camino::Utf8PathBuf;
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
    notify::{
        RecommendedWatcher, RecursiveMode,
        event::{AccessKind, AccessMode, EventKind},
    },
};
use rustc_hash::FxHashSet;
use tokio::{sync::mpsc::WeakSender, task::spawn_blocking};
use tracing::{error, info, warn};

use crate::{
    library::scan::{control::ScanCommand, decode::is_folder_art_name},
    media::{lookup_table::can_be_read, traits::MediaProviderFeatures},
    settings::scan::ScanSettings,
};

/// 合并成一批防抖事件的文件系统活动。
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(2);
/// 单个事件批次触及的目录超过这个数就改为全量扫描。
const STORM_TARGET_CAP: usize = 200;
/// 监视器状态刷新的频率，同时重试当初监视不上的根目录。
const WATCH_RETRY_INTERVAL: Duration = Duration::from_secs(10);

pub(super) struct LibraryWatcher {
    // 保留它是为了 Drop 时能停掉 notify 线程
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    watched: FxHashSet<Utf8PathBuf>,
    unwatched: FxHashSet<Utf8PathBuf>,
    /// 已经告过警的根目录，这样重试不会刷屏日志。
    warned: FxHashSet<Utf8PathBuf>,
}

impl LibraryWatcher {
    fn new(roots: &[Utf8PathBuf], cmd_tx: WeakSender<ScanCommand>) -> Option<Self> {
        let mut debouncer =
            match new_debouncer(DEBOUNCE_WINDOW, None, move |result: DebounceEventResult| {
                handle_debounced(result, &cmd_tx)
            }) {
                Ok(debouncer) => debouncer,
                Err(e) => {
                    error!("Could not start library watcher: {:?}", e);
                    return None;
                }
            };

        let mut watched = FxHashSet::default();
        let mut unwatched = FxHashSet::default();
        let mut warned = FxHashSet::default();
        let mut seen = FxHashSet::default();
        for root in roots {
            let Some(canonical) = root.canonicalize_utf8().ok() else {
                unwatched.insert(root.clone());
                continue;
            };

            if !seen.insert(canonical.clone()) {
                continue;
            }

            match debouncer.watch(canonical.as_std_path(), RecursiveMode::Recursive) {
                Ok(()) => {
                    watched.insert(canonical);
                }
                Err(e) => {
                    warn!("Could not watch {:?}: {:?}", canonical, e);
                    #[cfg(target_os = "linux")]
                    warn!("If this is a watch count limit, raise fs.inotify.max_user_watches");
                    warned.insert(canonical.clone());
                    unwatched.insert(canonical);
                }
            }
        }

        Some(Self {
            _debouncer: debouncer,
            watched,
            unwatched,
            warned,
        })
    }

    pub(super) fn is_active(&self) -> bool {
        !self.watched.is_empty()
    }

    pub(super) fn watched_roots(&self) -> Vec<Utf8PathBuf> {
        self.watched.iter().cloned().collect()
    }

    pub(super) fn unwatched_roots(&self) -> Vec<Utf8PathBuf> {
        self.unwatched.iter().cloned().collect()
    }

    /// 探测之后：停止监视已丢失的根，尝试重新监视恢复的根。有任何新根被监视则为 true。
    pub(super) fn apply_probe(
        &mut self,
        lost: Vec<Utf8PathBuf>,
        recoverable: Vec<Utf8PathBuf>,
    ) -> bool {
        for root in lost {
            self.watched.remove(&root);
            self.unwatched.insert(root);
        }

        let mut recovered = false;
        for canonical in recoverable {
            if self.watched.contains(&canonical) {
                continue;
            }

            match self
                ._debouncer
                .watch(canonical.as_std_path(), RecursiveMode::Recursive)
            {
                Ok(()) => {
                    info!("Now watching {:?}", canonical);
                    self.warned.remove(&canonical);
                    self.watched.insert(canonical);
                    recovered = true;
                }
                Err(e) => {
                    if self.warned.insert(canonical.clone()) {
                        warn!("Could not watch {:?}: {:?}", canonical, e);
                    }
                    self.unwatched.insert(canonical);
                }
            }
        }

        recovered
    }
}

/// 在阻塞线程上检查根目录可用性，免得死掉的网络挂载把扫描器卡住。
pub(super) fn probe_roots(
    watched: &[Utf8PathBuf],
    unwatched: &[Utf8PathBuf],
) -> (Vec<Utf8PathBuf>, Vec<Utf8PathBuf>) {
    let lost: Vec<Utf8PathBuf> = watched
        .iter()
        .filter(|root| matches!(root.as_std_path().try_exists(), Ok(false)))
        .cloned()
        .collect();
    let recoverable: Vec<Utf8PathBuf> = unwatched
        .iter()
        .filter_map(|root| root.canonicalize_utf8().ok())
        .collect();
    (lost, recoverable)
}

pub(super) fn arm(
    settings: &ScanSettings,
    cmd_tx: &WeakSender<ScanCommand>,
) -> Option<LibraryWatcher> {
    if !settings.watch_for_changes || settings.paths.is_empty() {
        return None;
    }
    LibraryWatcher::new(&settings.paths, cmd_tx.clone())
}

fn try_send_command(cmd_tx: &WeakSender<ScanCommand>, command: ScanCommand) -> bool {
    cmd_tx
        .upgrade()
        .is_some_and(|cmd_tx| cmd_tx.blocking_send(command).is_ok())
}

fn handle_debounced(result: DebounceEventResult, cmd_tx: &WeakSender<ScanCommand>) {
    let events = match result {
        Ok(events) => events,
        Err(errors) => {
            // 错误批次可能已经丢了事件——用全量扫描补回来
            for e in &errors {
                warn!("Library watcher error: {:?}", e);
            }
            let _ = try_send_command(cmd_tx, ScanCommand::Scan);
            return;
        }
    };

    // 忽略纯访问事件，但保留 close-write——某些后端用它代替 modify
    let events: Vec<DebouncedEvent> = events
        .into_iter()
        .filter(|e| {
            e.need_rescan()
                || matches!(
                    e.event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(_)
                        | EventKind::Remove(_)
                        | EventKind::Access(AccessKind::Close(AccessMode::Write))
                )
        })
        .collect();

    if events.is_empty() {
        return;
    }

    if events.iter().any(|e| e.need_rescan()) {
        let _ = try_send_command(cmd_tx, ScanCommand::Scan);
        return;
    }

    let paths: Vec<PathBuf> = events
        .iter()
        .flat_map(|e| e.event.paths.iter().cloned())
        .collect();
    let (targets, force_rescan) = affected_targets(&paths);
    if targets.is_empty() {
        return;
    }

    let command = if targets.len() > STORM_TARGET_CAP {
        ScanCommand::Scan
    } else {
        ScanCommand::RescanPaths {
            paths: targets.into_iter().collect(),
            respect_record: !force_rescan,
            // 移入的文件夹不会为其内容发事件——要递归进去找
            recursive: true,
        }
    };

    if !try_send_command(cmd_tx, command) {
        warn!("Could not queue watcher rescan, scanner is gone");
    }
}

/// 把事件路径映射成重扫目标：媒体/封面/歌词用父文件夹，目录用自身。那个旗标报告
/// 是否有路径属于专辑封面，它会让随后的重扫忽略扫描记录。
fn affected_targets(paths: &[PathBuf]) -> (FxHashSet<Utf8PathBuf>, bool) {
    let mut targets = FxHashSet::default();
    let mut force_rescan = false;
    let mut non_utf8: usize = 0;

    for path in paths {
        let Ok(path) = Utf8PathBuf::try_from(path.clone()) else {
            non_utf8 += 1;
            continue;
        };

        let is_media = can_be_read(
            path.as_std_path(),
            MediaProviderFeatures::PROVIDES_METADATA | MediaProviderFeatures::ALLOWS_INDEXING,
        )
        .unwrap_or(false);
        let is_lyrics = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lrc"));
        let is_album_art = is_album_art(path.as_std_path());

        if is_media || is_lyrics || is_album_art {
            force_rescan |= is_album_art;
            if let Some(parent) = path.parent() {
                targets.insert(parent.to_path_buf());
            }
            continue;
        }

        match path.as_std_path().try_exists() {
            Ok(true) => {
                if path.is_dir() {
                    targets.insert(path);
                }
            }
            Ok(false) => {
                targets.insert(path);
            }
            Err(_) => {}
        }
    }

    if non_utf8 > 0 {
        warn!("Dropped {} non-UTF-8 watcher event path(s)", non_utf8);
    }

    (targets, force_rescan)
}

fn is_album_art(path: &Path) -> bool {
    let stem = path.file_stem().and_then(|stem| stem.to_str());
    let extension = path.extension().and_then(|extension| extension.to_str());

    stem.zip(extension)
        .and_then(|(stem, extension)| is_folder_art_name(stem, extension))
        .is_some()
}

/// 持有跨扫描之间存续的监视器状态。
#[derive(Default)]
pub(super) struct WatcherState {
    watcher: Option<LibraryWatcher>,
    config: Option<(bool, Vec<Utf8PathBuf>)>,
    probe: Option<WatchProbe>,
    retry: Option<tokio::time::Interval>,
}

/// 后台根目录检查。持有监视器，好让慢挂载不会拖住扫描器。
type WatchProbe = tokio::task::JoinHandle<(LibraryWatcher, bool)>;

impl WatcherState {
    /// 设置变更后重建监视器。返回 true 表示监视已激活。
    pub(super) async fn rearm(
        &mut self,
        settings: &ScanSettings,
        cmd_tx: &WeakSender<ScanCommand>,
    ) -> bool {
        // 有一次探测正持有监视器——等它结束
        if self.probe.is_some() {
            return false;
        }

        let unchanged = self.config.as_ref().is_some_and(|(enabled, paths)| {
            *enabled == settings.watch_for_changes && *paths == settings.paths
        });
        if self.watcher.is_some() && unchanged {
            return false;
        }

        let was_active = self.watcher.as_ref().is_some_and(LibraryWatcher::is_active);
        let config = (settings.watch_for_changes, settings.paths.clone());
        let settings = settings.clone();
        let cmd_tx = cmd_tx.clone();
        self.watcher = spawn_blocking(move || arm(&settings, &cmd_tx))
            .await
            .unwrap_or_default();
        if self.watcher.is_some() {
            self.config = Some(config);
        } else {
            self.config = None;
        }
        !was_active && self.watcher.as_ref().is_some_and(LibraryWatcher::is_active)
    }

    fn spawn_probe(&mut self) {
        if self.probe.is_some() {
            return;
        }
        let Some(mut watcher) = self.watcher.take() else {
            return;
        };
        self.probe = Some(spawn_blocking(move || {
            let watched = watcher.watched_roots();
            let unwatched = watcher.unwatched_roots();
            let (lost, recoverable) = probe_roots(&watched, &unwatched);
            let recovered = watcher.apply_probe(lost, recoverable);
            (watcher, recovered)
        }));
    }

    /// 若一次已完成的探测带回了某个根，则为 `Some(true)`。
    async fn poll_probe(&mut self) -> Option<bool> {
        if !self.probe.as_ref().is_some_and(|probe| probe.is_finished()) {
            return None;
        }
        match self.probe.take()?.await {
            Ok((watcher, recovered)) => {
                self.watcher = Some(watcher);
                Some(recovered)
            }
            // 探测 panic 并把监视器一起带了下去
            Err(e) => {
                error!("Watcher probe failed: {:?}", e);
                self.watcher = None;
                Some(false)
            }
        }
    }

    /// 应用设置变更并在后台探测根目录。返回 (recovered, watching)。
    pub(super) async fn refresh(
        &mut self,
        settings: &ScanSettings,
        cmd_tx: &WeakSender<ScanCommand>,
    ) -> (bool, bool) {
        let recovered = self.poll_probe().await.unwrap_or(false);
        let rearmed = self.rearm(settings, cmd_tx).await;
        let watching = self.watcher.as_ref().is_some_and(LibraryWatcher::is_active);
        self.spawn_probe();
        (rearmed || recovered, watching)
    }

    pub(super) async fn retry_tick(&mut self) {
        match &mut self.retry {
            Some(interval) => {
                interval.tick().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    pub(super) fn set_retry(&mut self, watch_for_changes: bool) {
        if watch_for_changes && self.retry.is_none() {
            let mut retry = tokio::time::interval(WATCH_RETRY_INTERVAL);
            retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            self.retry = Some(retry);
        } else if !watch_for_changes {
            self.retry = None;
        }
    }

    /// 摘下一个已在途的探测：派生出去的任务不会被取消，它的监视器会一直发事件
    /// 直到探测自然结束，而这期间重新装配可能让同一批根被监听两次。
    pub(super) fn stop_probe(&mut self) {
        self.probe = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use notify_debouncer_full::{
        DebouncedEvent,
        notify::{
            Event,
            event::{AccessKind, AccessMode, CreateKind, EventKind, Flag},
        },
    };
    use tokio::sync::mpsc::channel;

    use super::*;
    use crate::test_support::{TestDir, register_test_media_providers, strip_verbatim_prefix};

    fn handle_for_test(result: DebounceEventResult, tx: &tokio::sync::mpsc::Sender<ScanCommand>) {
        let weak_tx = tx.downgrade();
        handle_debounced(result, &weak_tx);
    }

    #[test]
    fn media_lyrics_and_art_events_map_to_parent_directory() {
        register_test_media_providers();
        for (name, contents) in [
            ("track.flac", Some(b"".as_slice())),
            ("track.lrc", Some(b"".as_slice())),
            ("cover.JPG", None),
            ("gone.flac", None),
        ] {
            let dir = TestDir::new("watch-test");
            let file = dir.join(name);
            if let Some(contents) = contents {
                std::fs::write(&file, contents).unwrap();
            }

            let (targets, force_rescan) = affected_targets(std::slice::from_ref(&file));
            let mut expected = FxHashSet::default();
            // `affected_targets` 把事件映射到普通的父目录（不做规范化），所以在
            // Windows 上要拿同一目录的非 verbatim 拼写来比对
            expected.insert(strip_verbatim_prefix(
                dir.utf8_path().canonicalize_utf8().unwrap(),
            ));
            assert_eq!(targets, expected);
            assert_eq!(force_rescan, name == "cover.JPG");
        }
    }

    #[test]
    fn non_media_files_are_dropped() {
        register_test_media_providers();
        let dir = TestDir::new("watch-test");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"").unwrap();

        assert!(affected_targets(&[file]).0.is_empty());
    }

    #[test]
    fn deleted_directory_maps_to_itself() {
        register_test_media_providers();
        let dir = TestDir::new("watch-test");

        let targets = affected_targets(&[dir.join("artist")]).0;
        assert_eq!(targets.len(), 1);
        assert!(targets.contains(&dir.utf8_join("artist")));
    }

    #[test]
    fn existing_directory_maps_to_itself() {
        register_test_media_providers();
        let dir = TestDir::new("watch-test");
        let sub = dir.join("album");
        std::fs::create_dir(&sub).unwrap();

        let targets = affected_targets(std::slice::from_ref(&sub)).0;
        assert_eq!(targets.len(), 1);
        assert!(targets.contains(&Utf8PathBuf::try_from(sub).unwrap()));
    }

    #[test]
    fn access_events_are_dropped() {
        register_test_media_providers();
        let dir = TestDir::new("watch-test");
        let file = dir.join("track.flac");
        std::fs::write(&file, b"").unwrap();
        let (tx, mut rx) = channel(1);
        let event = DebouncedEvent::new(
            Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read))).add_path(file),
            Instant::now(),
        );

        // 只开/关而没有写入并不改变内容——不重扫
        handle_for_test(Ok(vec![event]), &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn write_events_queue_record_aware_rescans() {
        register_test_media_providers();
        for kind in [
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Create(CreateKind::File),
        ] {
            let dir = TestDir::new("watch-test");
            let file = dir.join("track.flac");
            std::fs::write(&file, b"").unwrap();
            let (tx, mut rx) = channel(1);
            let event = DebouncedEvent::new(Event::new(kind).add_path(file), Instant::now());

            handle_for_test(Ok(vec![event]), &tx);
            match rx.blocking_recv() {
                Some(ScanCommand::RescanPaths {
                    paths,
                    respect_record,
                    recursive,
                }) => {
                    assert!(respect_record);
                    assert!(recursive);
                    assert_eq!(paths.len(), 1);
                }
                other => panic!("expected a record-aware rescan command, got {:?}", other),
            }
        }
    }

    #[test]
    fn rescan_flag_promotes_to_full_scan() {
        let (tx, mut rx) = channel(1);
        let event = DebouncedEvent::new(
            Event::new(EventKind::Create(CreateKind::File)).set_flag(Flag::Rescan),
            Instant::now(),
        );

        handle_for_test(Ok(vec![event]), &tx);
        assert!(matches!(rx.blocking_recv(), Some(ScanCommand::Scan)));
    }

    #[test]
    fn storm_of_directories_promotes_to_full_scan() {
        register_test_media_providers();
        let (tx, mut rx) = channel(1);
        let events: Vec<DebouncedEvent> = (0..=STORM_TARGET_CAP)
            .map(|i| {
                DebouncedEvent::new(
                    Event::new(EventKind::Create(CreateKind::Folder))
                        .add_path(PathBuf::from(format!("/music/dir{i}"))),
                    Instant::now(),
                )
            })
            .collect();

        handle_for_test(Ok(events), &tx);
        assert!(matches!(rx.blocking_recv(), Some(ScanCommand::Scan)));
    }

    #[test]
    fn album_art_batch_forces_rescan_of_recorded_files() {
        let dir = TestDir::new("watch-test");
        let file = dir.join("folder.png");
        let (tx, mut rx) = channel(1);
        let event = DebouncedEvent::new(
            Event::new(EventKind::Create(CreateKind::File)).add_path(file),
            Instant::now(),
        );

        handle_for_test(Ok(vec![event]), &tx);
        match rx.blocking_recv() {
            Some(ScanCommand::RescanPaths {
                respect_record,
                recursive,
                ..
            }) => {
                assert!(!respect_record);
                assert!(recursive);
            }
            other => panic!("expected an artwork rescan command, got {:?}", other),
        }
    }

    #[test]
    fn error_batch_promotes_to_full_scan() {
        let (tx, mut rx) = channel(1);
        handle_for_test(
            Err(vec![notify_debouncer_full::notify::Error::generic(
                "test error",
            )]),
            &tx,
        );
        assert!(matches!(rx.blocking_recv(), Some(ScanCommand::Scan)));
    }
}
