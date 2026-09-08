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
mod stats;
#[cfg(test)]
mod test_support;
mod toasts;
pub mod ui;

const VERSION_STRING: &str = env!("MELIORA_VERSION_STRING");

// count allocations during testing, needed for testing the allocation behavior of the playback
// pipeline
#[cfg(test)]
#[global_allocator]
static ALLOC_GUARD: test_support::alloc_guard::CountingAllocator =
    test_support::alloc_guard::CountingAllocator;

// mimalloc for non-test builds: the NT heap never returns mid-size free
// segments to the OS, so interactive churn ratchets private bytes up even
// with no leak; mimalloc decommits idle pages so committed memory falls back
// after activity stops. Tests keep the counting allocator above.
#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Set mimalloc's purge delay to 0 as the program default: freed pages are
/// decommitted immediately instead of after the factory delay, keeping the
/// interactive memory peak low (measured 468 -> 407 MB under a heavy
/// browse-and-click session). The option is set through the C API because the
/// environment cannot be injected after process start. `purge_delay` sits at
/// enum index 15 in the vendored v3 mimalloc tree (libmimalloc-sys 0.1.49);
/// the factory default of 10 ms doubles as a sanity check so a future enum
/// reshuffle fails safe instead of clobbering an unrelated option.
#[cfg(not(test))]
fn tune_mimalloc_purge_delay() {
    use std::ffi::{c_int, c_long};

    unsafe extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
        fn mi_option_set(option: c_int, value: c_long);
    }

    const MI_OPTION_PURGE_DELAY: c_int = 15;
    // The header comment claims a 10 ms default, but v3's options.c table
    // actually ships 1000 ms.
    const PURGE_DELAY_FACTORY_DEFAULT: c_long = 1000;
    unsafe {
        if mi_option_get(MI_OPTION_PURGE_DELAY) != PURGE_DELAY_FACTORY_DEFAULT {
            return;
        }
        mi_option_set(MI_OPTION_PURGE_DELAY, 0);
        let applied = mi_option_get(MI_OPTION_PURGE_DELAY);
        MIMALLOC_PURGE_DELAY_APPLIED.store(applied == 0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether the purge-delay override took effect, reported by the memory probe
/// once logging is up.
#[cfg(not(test))]
pub static MIMALLOC_PURGE_DELAY_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .max_blocking_threads(4)
        .build()
        .unwrap()
});

/// Current process memory, in MiB: `(private_committed, working_set)`.
/// `private_committed` is the *commit charge* (what task manager shows / what
/// the OS actually reserves), not the resident set; resident pages are far more
/// volatile and under-reported the long-run curve in past logs.
pub(crate) fn process_memory_mb() -> (u64, u64) {
    #[cfg(target_os = "windows")]
    {
        use core::ffi::c_void;

        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_nonpaged_pool_usage: usize,
            quota_nonpaged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }

        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut c_void;
            fn GetProcessMemoryInfo(
                process: *mut c_void,
                counters: *mut ProcessMemoryCounters,
                size: u32,
            ) -> i32;
        }

        let mut counters = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_nonpaged_pool_usage: 0,
            quota_nonpaged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
            private_usage: 0,
        };
        // SAFETY: buffer is the correct size and type; the pseudo-handle is
        // always valid for querying the current process.
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            )
        };
        if ok != 0 {
            let committed = (counters.private_usage / (1024 * 1024)) as u64;
            let working = (counters.working_set_size / (1024 * 1024)) as u64;
            (committed, working)
        } else {
            (0, 0)
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
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
}

/// Samples and logs process memory at a named low-frequency UI event so a
/// later `[mem]` step can be attributed to the user action that preceded it:
/// the 30-second probe alone cannot tell browsing from playback, and the
/// 2026-09 logs show unattributable +35..155 MB steps during browsing.
pub fn log_mem_event(event: &str) {
    let (private, working) = process_memory_mb();
    tracing::info!(
        event,
        private_mb = private,
        working_mb = working,
        render_cache_mb = crate::ui::components::managed_image::render_cache_mb(),
        "[mem] ui event"
    );
}

/// Background memory probe: samples process memory every 30 seconds so a long
/// playback session leaves a curve in the log (committed private bytes vs
/// working set) that separates a real leak from cache/cache-size growth. A
/// "step" (net growth over a rolling window) is logged explicitly so every
/// activity-driven bump in long-run memory is attributable instead of silent.
#[cfg(not(test))]
fn spawn_memory_probe() {
    // Net committed growth over the last 10 minutes considered a "step".
    const STEP_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
    const STEP_MIN_MB: i64 = 20;

    crate::RUNTIME.spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut baseline: Option<(std::time::Instant, u64)> = None;
        loop {
            tick.tick().await;
            let (private, working) = process_memory_mb();
            let covers = disk_cover_cache_mb();
            let render_cache =
                crate::ui::components::managed_image::render_cache_mb();

            let step_alert = match baseline {
                Some((at, from_mb)) if at.elapsed() >= STEP_WINDOW => {
                    let delta = private as i64 - from_mb as i64;
                    // Slide the window forward to the current sample so a
                    // sustained ramp keeps being measured against the same
                    // reference instead of resetting to zero.
                    baseline = Some((std::time::Instant::now(), private));
                    (delta >= STEP_MIN_MB).then_some(delta)
                }
                Some(_) => None,
                None => {
                    baseline = Some((std::time::Instant::now(), private));
                    None
                }
            };

            if let Some(delta) = step_alert {
                tracing::warn!(
                    step_mb = delta,
                    private_mb = private,
                    render_cache_mb = render_cache,
                    covers_mb = covers,
                    "mem step: committed grew in last 10 min"
                );
            } else {
                #[cfg(not(test))]
                let purge0 = MIMALLOC_PURGE_DELAY_APPLIED.load(std::sync::atomic::Ordering::Relaxed);
                #[cfg(not(test))]
                tracing::info!(
                    private_mb = private,
                    working_mb = working,
                    covers_mb = covers,
                    render_cache_mb = render_cache,
                    purge_delay0 = purge0,
                    "[mem] periodic"
                );
                #[cfg(test)]
                tracing::info!(
                    private_mb = private,
                    working_mb = working,
                    covers_mb = covers,
                    render_cache_mb = render_cache,
                    "[mem] periodic"
                );
            }
        }
    });
}

/// Total bytes of the on-disk online-cover cache, in MiB. Tracks how many
/// distinct covers the session has touched, independent of process memory.
#[cfg(all(not(test), feature = "online_sources"))]
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

#[cfg(all(not(test), not(feature = "online_sources")))]
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

    #[cfg(not(test))]
    tune_mimalloc_purge_delay();

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
