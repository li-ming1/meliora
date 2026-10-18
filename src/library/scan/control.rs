use camino::Utf8PathBuf;
use gpui::{App, Entity, Global};
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tracing::error;

use crate::{
    library::playlist::{PlaylistEvent, PlaylistInfoTransfer},
    settings::scan::{MissingFolderPolicy, ScanSettings},
};

#[derive(Debug, PartialEq, Clone)]
pub enum ScanEvent {
    Cleaning,
    PlaylistsUpdated(Vec<i64>),
    WaitingForMissingFolderDecision { paths: Vec<Utf8PathBuf> },
    ScanProgress { current: u64, total: u64 },
    ScanCompleteWatching,
    ScanCompleteIdle,
    TargetedRescanComplete,
}

#[derive(Debug, Clone)]
pub(super) enum ScanCommand {
    Scan,
    /// 忽略扫描记录的全量重扫（schema 变更、SCAN_VERSION 变化）。
    ForceScan,
    /// 重扫指定路径；`respect_record` 会跳过未变化的文件，`recursive` 会向下遍历子目录。
    RescanPaths {
        paths: Vec<Utf8PathBuf>,
        respect_record: bool,
        recursive: bool,
    },
    ResolveMissingFolders(MissingFolderDecision),
    UpdateSettings(ScanSettings),
    Stop,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MissingFolderDecision {
    KeepInLibrary,
    DeleteFromLibrary,
}

#[derive(Debug, Clone)]
pub(super) enum ScanMode {
    Full {
        is_force: bool,
    },
    Targeted {
        paths: Vec<Utf8PathBuf>,
        respect_record: bool,
        recursive: bool,
    },
}

impl ScanMode {
    /// 强制全量扫描会把专辑从头重建。定向重扫不得清掉专辑封面。
    pub(super) fn force_albums(&self) -> bool {
        matches!(self, ScanMode::Full { is_force: true })
    }

    pub(super) fn completion_event(&self, watching: bool) -> ScanEvent {
        if watching {
            ScanEvent::ScanCompleteWatching
        } else if matches!(self, ScanMode::Targeted { .. }) {
            ScanEvent::TargetedRescanComplete
        } else {
            ScanEvent::ScanCompleteIdle
        }
    }
}

/// 向扫描器发送命令。命令都是幂等请求，所以通道满时直接丢弃（记日志），
/// 而不是在发送时阻塞 UI 线程。
pub struct ScanInterface {
    cmd_tx: Sender<ScanCommand>,
}

impl ScanInterface {
    pub(super) fn new(cmd_tx: Sender<ScanCommand>) -> Self {
        ScanInterface { cmd_tx }
    }

    /// 发送一条命令；通道满或已关闭时只记日志而不报错。
    fn try_send(&self, command: ScanCommand, label: &str) {
        if let Err(err) = self.cmd_tx.try_send(command) {
            error!("could not send {label}: {err}");
        }
    }

    pub fn scan(&self) {
        self.try_send(ScanCommand::Scan, "scan start command");
    }

    pub fn force_scan(&self) {
        self.try_send(ScanCommand::ForceScan, "force re-scan start command");
    }

    pub fn rescan_paths(&self, paths: Vec<Utf8PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.try_send(
            ScanCommand::RescanPaths {
                paths,
                respect_record: false,
                recursive: false,
            },
            "rescan-paths command",
        );
    }

    pub fn stop(&self) {
        self.try_send(ScanCommand::Stop, "scan stop command");
    }

    pub fn update_settings(&self, settings: ScanSettings) {
        self.try_send(
            ScanCommand::UpdateSettings(settings),
            "scan settings update command",
        );
    }

    pub fn resolve_missing_folders(&self, decision: MissingFolderDecision) {
        self.try_send(
            ScanCommand::ResolveMissingFolders(decision),
            "missing folder resolution",
        );
    }

    /// 启动扫描事件桥。镜像出去的 UI 实体（`scan_state`、`playlist_tracker`）由 UI
    /// 调用方传入，因此本模块保持对 `crate::ui` 零 import；循环体本身没有变化。
    pub fn start_broadcast(
        &self,
        mut events_rx: UnboundedReceiver<ScanEvent>,
        scan_state: Entity<ScanEvent>,
        playlist_tracker: Entity<PlaylistInfoTransfer>,
        cx: &mut App,
    ) {
        cx.spawn(async move |cx| {
            while let Some(event) = events_rx.recv().await {
                if let ScanEvent::PlaylistsUpdated(playlist_ids) = event {
                    if !playlist_ids.is_empty() {
                        playlist_tracker.update(cx, |_, cx| {
                            for playlist_id in playlist_ids {
                                cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
                            }
                        });
                    }
                    continue;
                }

                scan_state.update(cx, |m, cx| {
                    *m = event;
                    cx.notify()
                });
            }
        })
        .detach();
    }
}

impl Global for ScanInterface {}

pub(super) struct PendingRescan {
    pub(super) paths: Vec<Utf8PathBuf>,
    pub(super) respect_record: bool,
    pub(super) recursive: bool,
}

pub(super) fn queue_pending_rescan(
    pending: &mut Option<PendingRescan>,
    paths: Vec<Utf8PathBuf>,
    respect_record: bool,
    recursive: bool,
) {
    if paths.is_empty() {
        return;
    }

    match pending {
        Some(merged) => {
            merged.paths.extend(paths);
            // 队列里只要有一次重扫忽略记录，整批都忽略
            merged.respect_record &= respect_record;
            merged.recursive |= recursive;
        }
        None => {
            *pending = Some(PendingRescan {
                paths,
                respect_record,
                recursive,
            })
        }
    }
}

pub(super) async fn resolve_missing_folder_action(
    command_rx: &mut Receiver<ScanCommand>,
    event_tx: &UnboundedSender<ScanEvent>,
    scan_settings: &mut ScanSettings,
    missing_paths: Vec<Utf8PathBuf>,
    pending_start: &mut Option<bool>,
    pending_rescan: &mut Option<PendingRescan>,
) -> MissingFolderPolicy {
    match scan_settings.missing_folder_policy {
        MissingFolderPolicy::Ask => {}
        already_decided => return already_decided,
    }

    let _ = event_tx.send(ScanEvent::WaitingForMissingFolderDecision {
        paths: missing_paths,
    });

    loop {
        match command_rx.recv().await {
            Some(ScanCommand::ResolveMissingFolders(decision)) => {
                break match decision {
                    MissingFolderDecision::KeepInLibrary => MissingFolderPolicy::KeepInLibrary,
                    MissingFolderDecision::DeleteFromLibrary => {
                        MissingFolderPolicy::DeleteFromLibrary
                    }
                };
            }
            Some(ScanCommand::UpdateSettings(s)) => {
                *scan_settings = s;
                match scan_settings.missing_folder_policy {
                    MissingFolderPolicy::Ask => {}
                    decided => break decided,
                }
            }
            Some(ScanCommand::Stop) => break MissingFolderPolicy::KeepInLibrary,
            Some(ScanCommand::Scan) => {
                pending_start.get_or_insert(false);
            }
            Some(ScanCommand::ForceScan) => {
                *pending_start = Some(true);
            }
            Some(ScanCommand::RescanPaths {
                paths,
                respect_record,
                recursive,
            }) => {
                queue_pending_rescan(pending_rescan, paths, respect_record, recursive);
            }
            None => break MissingFolderPolicy::KeepInLibrary,
        }
    }
}
