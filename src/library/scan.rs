mod active_scan;
pub(crate) mod artist_match;
pub(crate) mod artwork;
mod control;
pub(crate) mod database;
pub(crate) mod decode;
mod discover;
mod disk;
mod execution;
mod fs_case;
mod record;
mod scanner;
mod watch;

use cntp_i18n::tr;
use sqlx::SqlitePool;
use tokio::sync::mpsc::{UnboundedReceiver, channel, unbounded_channel};
use tracing::error;

use crate::{
    settings::scan::ScanSettings,
    toasts::{Toast, emit_toast},
};

pub use control::{MissingFolderDecision, ScanEvent, ScanInterface};

/// 扫描器命令队列的容量：超出容量的发送会被丢弃（由发送端记日志），
/// 而不是阻塞 UI 线程。
const COMMAND_CHANNEL_CAPACITY: usize = 10;

#[cfg(test)]
use database::{flush_album_artists, flush_track_artists};

pub fn start_scanner(
    pool: SqlitePool,
    settings: ScanSettings,
) -> (ScanInterface, UnboundedReceiver<ScanEvent>) {
    let (cmd_tx, command_rx) = channel(COMMAND_CHANNEL_CAPACITY);
    let (event_tx, events_rx) = unbounded_channel();

    // 扫描器是库内容（曲目/专辑/歌手/封面）的唯一写入者，而它的 handle 过去是被丢弃的：
    // 它内部任何一处 panic 都会让 UI 卡在「正在扫描」上，且完全不提示发生了什么。盯着这个任务，
    // 至少让失败被记录并暴露出来。
    let handle = crate::RUNTIME.spawn(scanner::run_scanner(
        pool,
        settings,
        command_rx,
        cmd_tx.downgrade(),
        event_tx,
    ));
    crate::RUNTIME.spawn(async move {
        if let Err(error) = handle.await {
            error!("library scanner task panicked: {error}");
            emit_toast(Toast::error(tr!(
                "SCAN_CRASHED",
                "The library scanner stopped unexpectedly. See the log for details."
            )));
        }
    });

    (ScanInterface::new(cmd_tx), events_rx)
}

#[cfg(test)]
#[path = "scan/tests.rs"]
mod tests;
