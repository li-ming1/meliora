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

#[cfg(test)]
use database::{flush_album_artists, flush_track_artists};

pub fn start_scanner(
    pool: SqlitePool,
    settings: ScanSettings,
) -> (ScanInterface, UnboundedReceiver<ScanEvent>) {
    let (cmd_tx, command_rx) = channel(10);
    let (event_tx, events_rx) = unbounded_channel();

    // The scanner is the only writer to the library database, and its handle
    // used to be dropped: a panic anywhere in it left the UI stuck on
    // "scanning" with no hint of what happened. Watch the task so the failure
    // is at least logged and surfaced.
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
