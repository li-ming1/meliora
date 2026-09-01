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

    register_providers(vec![
        Box::new(LoftyProvider),
        Box::new(SymphoniaProvider),
    ]);

    crate::ui::app::run()
}
