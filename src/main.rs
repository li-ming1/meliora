// On Windows do NOT show a console window when opening the app
#![cfg_attr(
    all(not(test), not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use cntp_i18n::{I18N_MANAGER, tr_load};
use gpui::set_trace_enabled;
use std::sync::LazyLock;

use crate::media::{
    lofty::LoftyProvider,
    lookup_table::register_providers,
    symphonia::SymphoniaProvider,
};

mod controllers;
mod devices;
#[cfg(feature = "kugou")]
mod kugou;
#[cfg(feature = "netease")]
mod netease;
mod library;
mod logging;
mod media;
mod paths;
mod playback;
mod power;
mod settings;
#[cfg(test)]
mod test_support;
mod toasts;
mod ui;

const VERSION_STRING: &str = env!("MELIORA_VERSION_STRING");

// count allocations during testing, needed for testing the allocation behavior of the playback
// pipeline
#[cfg(test)]
#[global_allocator]
static ALLOC_GUARD: test_support::alloc_guard::CountingAllocator =
    test_support::alloc_guard::CountingAllocator;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .max_blocking_threads(4)
        .build()
        .unwrap()
});

/// Current process memory, in MiB: (private, virtual/working-set).
/// Used by the `[mem]` probe to watch for long-run growth / unreclaimed
/// high-water marks while playback stays up.
pub(crate) fn process_memory_mb() -> (u64, u64) {
    use sysinfo::ProcessesToUpdate;

    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut sys = sysinfo::System::new_with_specifics(sysinfo::RefreshKind::default());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
    match sys.process(pid) {
        Some(process) => (
            process.memory() / (1024 * 1024),
            process.virtual_memory() / (1024 * 1024),
        ),
        None => (0, 0),
    }
}

/// Background memory probe: samples process memory every 30 seconds so a long
/// playback session leaves a curve in the log (private vs working set) that
/// separates a real leak from allocator/GPU pool retention.
fn spawn_memory_probe() {
    crate::RUNTIME.spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let (private, working) = process_memory_mb();
            tracing::info!(
                private_mb = private,
                working_mb = working,
                covers_mb = disk_cover_cache_mb(),
                "[mem] periodic"
            );
        }
    });
}

/// Total bytes of the on-disk online-cover cache, in MiB. Tracks how many
/// distinct covers the session has touched, independent of process memory.
#[cfg(feature = "online_sources")]
fn disk_cover_cache_mb() -> u64 {
    let dir = crate::paths::data_dir().join("image-cache");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum::<u64>()
        / (1024 * 1024)
}

#[cfg(not(feature = "online_sources"))]
fn disk_cover_cache_mb() -> u64 {
    0
}

#[cfg(target_os = "windows")]
fn init_windows_restart() -> anyhow::Result<()> {
    use windows::{
        Win32::System::Recovery::{RESTART_NO_REBOOT, RegisterApplicationRestart},
        core::PWSTR,
    };

    unsafe { RegisterApplicationRestart(PWSTR::null(), RESTART_NO_REBOOT)? }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    init_windows_restart()?;

    // disable the GPUI mini profiler immediately to avoid unnecessary allocations
    set_trace_enabled(false);

    // move any data/log dirs left under the legacy `li-ming1/meliora` and
    // `mailliw/hummingbird` names so logins and caches survive the renames
    crate::paths::migrate_legacy_li_ming1_dirs();
    crate::paths::migrate_legacy_dirs();

    I18N_MANAGER.load_source(tr_load!());
    crate::logging::init()?;

    tracing::info!("version {VERSION_STRING}");

    #[cfg(not(test))]
    spawn_memory_probe();

    register_providers(vec![
        Box::new(LoftyProvider),
        Box::new(SymphoniaProvider),
    ]);

    // Bound the online image-cache to its 30-day age window even when no
    // cover has been written yet this session.
    #[cfg(feature = "online_sources")]
    crate::media::http_source::prune_image_cache_background();

    crate::ui::app::run()
}
