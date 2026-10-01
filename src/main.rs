// On Windows do NOT show a console window when opening the app
#![cfg_attr(
    all(not(test), not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use cntp_i18n::{I18N_MANAGER, tr_load};
use gpui::set_trace_enabled;
use std::sync::LazyLock;

use crate::media::{
    lofty::LoftyProvider, lookup_table::register_providers, symphonia::SymphoniaProvider,
};

mod controllers;
mod devices;
#[cfg(feature = "kugou")]
mod kugou;
mod library;
mod logging;
mod media;
#[cfg(not(test))]
mod mimalloc_stats;
#[cfg(feature = "netease")]
mod netease;
#[cfg(feature = "online_sources")]
mod online_sources;
mod paths;
mod playback;
mod power;
#[cfg(any(feature = "kugou", feature = "netease"))]
mod session_io;
mod settings;
mod stats;
#[cfg(test)]
mod test_support;
mod toasts;
pub mod ui;
#[cfg(feature = "online")]
mod updater;

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

/// Whether the page-reset overrides took effect (see
/// [`tune_mimalloc_page_reset`]).
#[cfg(not(test))]
pub static MIMALLOC_PAGE_RESET_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Decommit free page space eagerly instead of letting it ride in the arena:
/// v3's `page_reset` resets page space to the OS on every free, and
/// `abandoned_page_purge` purges a page the moment its thread abandons it.
/// With `purge_delay = 0` alone the collector only purges on its 5-minute
/// forced collect, so interactive churn parks tens of MB of free-but-committed
/// arena between collects. The vendored v3 tree does not export
/// `mi_option_get_name`, so the two options are addressed by enum index
/// (page_reset = 10, abandoned_page_purge = 11 in the v3 table; validated
/// indirectly by `tune_mimalloc_purge_delay`'s anchor: purge_delay sits at 15
/// with factory default 1000, which pins the table layout). Fail-safe: if the
/// read-back disagrees, the flag flips off and the probe reports it.
#[cfg(not(test))]
fn tune_mimalloc_page_reset() {
    use std::ffi::{c_int, c_long};

    unsafe extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
        fn mi_option_set(option: c_int, value: c_long);
    }

    const MI_OPTION_PAGE_RESET: c_int = 10;
    const MI_OPTION_ABANDONED_PAGE_PURGE: c_int = 11;
    // Both ship disabled; a non-zero read-back means the table does not match
    // the expected layout and touching it would clobber an unrelated option.
    let mut applied = unsafe { mi_option_get(MI_OPTION_PAGE_RESET) } == 0
        && unsafe { mi_option_get(MI_OPTION_ABANDONED_PAGE_PURGE) } == 0;
    if applied {
        unsafe {
            mi_option_set(MI_OPTION_PAGE_RESET, 1);
            mi_option_set(MI_OPTION_ABANDONED_PAGE_PURGE, 1);
        }
        applied = unsafe { mi_option_get(MI_OPTION_PAGE_RESET) } == 1
            && unsafe { mi_option_get(MI_OPTION_ABANDONED_PAGE_PURGE) } == 1;
    }
    MIMALLOC_PAGE_RESET_APPLIED.store(applied, std::sync::atomic::Ordering::Relaxed);
}

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
        #[derive(Default)]
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

        // `cb` must be set before the call; every counter is a pure out-param
        // and starts zeroed
        let mut counters = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            ..Default::default()
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

// mimalloc's own accounting, in MiB: `(committed, rss)`. Sampled next to the
// `[mem]` probe so per-track commit growth splits into "the allocator holds
// it" (mi_commit climbs in lockstep with private_mb — retention/fragmentation
// inside the heap) versus "something outside the heap grew" (private_mb
// climbs while mi_commit stays flat — D3D/driver territory).
// Only the `#[cfg(not(test))]` memory probe calls these; test builds swap in
// the counting allocator, so gate the declarations off there entirely.
#[cfg(not(test))]
#[link(name = "mimalloc")]
unsafe extern "C" {
    fn mi_process_info(
        elapsed_msecs: *mut usize,
        user_msecs: *mut usize,
        system_msecs: *mut usize,
        current_rss: *mut usize,
        peak_rss: *mut usize,
        current_commit: *mut usize,
        peak_commit: *mut usize,
        page_faults: *mut usize,
    );

    fn mi_collect(force: bool);
}

#[cfg(not(test))]
fn mimalloc_memory_mb() -> (u64, u64) {
    let mut elapsed: usize = 0;
    let mut user: usize = 0;
    let mut system: usize = 0;
    let mut rss: usize = 0;
    let mut peak_rss: usize = 0;
    let mut commit: usize = 0;
    let mut peak_commit: usize = 0;
    let mut page_faults: usize = 0;
    // SAFETY: all pointers are valid out-params; the call is pure statistics.
    unsafe {
        mi_process_info(
            &mut elapsed,
            &mut user,
            &mut system,
            &mut rss,
            &mut peak_rss,
            &mut commit,
            &mut peak_commit,
            &mut page_faults,
        );
    }
    (commit as u64 / (1024 * 1024), rss as u64 / (1024 * 1024))
}

/// Forces a full purge and reports how many MiB of committed heap it bought
/// back. A large reclaim means the per-track ratchet is idle-page
/// fragmentation the allocator simply hadn't decommitted yet (harmless — the
/// pages get reused); a ~0 reclaim at a rising commit means live allocations
/// are accumulating and the leak is real.
#[cfg(not(test))]
fn mimalloc_force_collect_reclaim_mb(before: u64) -> u64 {
    // SAFETY: stats-adjacent maintenance call; safe per mimalloc docs.
    unsafe { mi_collect(true) };
    let (after, _) = mimalloc_memory_mb();
    before.saturating_sub(after)
}

/// Samples and logs process memory at a named low-frequency UI event so a
/// later `[mem]` step can be attributed to the user action that preceded it:
/// the 30-second probe alone cannot tell browsing from playback, and the
/// 2026-09 logs show unattributable +35..155 MB steps during browsing.
pub fn log_mem_event(event: &str) {
    let (private, working) = process_memory_mb();
    let (img_entries, img_mb) = crate::ui::caching::image_cache_stats();
    tracing::info!(
        event,
        private_mb = private,
        working_mb = working,
        render_cache_mb = crate::ui::components::managed_image::render_cache_mb(),
        img_cache_mb = img_mb,
        img_cache_entries = img_entries,
        "[mem] ui event"
    );
}

/// Background memory probe: samples process memory every 30 seconds so a long
/// playback session leaves a curve in the log (committed private bytes vs
/// working set) that separates a real leak from cache/cache-size growth. A
/// "step" (net growth over a rolling window) is logged explicitly so every
/// activity-driven bump in long-run memory is attributable instead of silent.
/// Atlas observability (paired with the MELIORA PERMANENT OBSERVABILITY PATCH
/// block in gpui_windows' directx_atlas.rs): per texture kind [polychrome,
/// monochrome, subpixel], five counters each — pages created, pages dropped
/// (emptied + freed back to the driver), pages live, free_listed,
/// tiles_by_key length. Read in the [mem] periodic probe so glyph-page
/// growth (the 2026-09-27 leak class) is visible in every long-session log.
///
/// Gated behind the `atlas-probe` cargo feature: the symbol is exported by
/// the *locally patched* gpui-ce checkout only. Upstream (and therefore CI,
/// which fetches the pristine rev) has no `meliora_atlas_probe`, and
/// gpui_windows is not even compiled on non-Windows targets — an ungated
/// extern here would fail the link everywhere outside this machine.
#[cfg(all(target_os = "windows", feature = "atlas-probe", not(test)))]
fn atlas_probe_snapshot() -> [u64; 15] {
    // The symbol lives in the gpui_windows rlib already in the link graph;
    // no #[link] attribute — MSVC would demand a matching .lib import lib.
    unsafe extern "C" {
        fn meliora_atlas_probe(out: *mut u64, len: usize) -> usize;
    }
    let mut out = [0u64; 15];
    unsafe {
        meliora_atlas_probe(out.as_mut_ptr(), out.len());
    }
    out
}

/// Spawns a fresh copy of this executable with the original CLI arguments.
/// Returns `false` when the spawn failed (callers continue degraded). Shared
/// by the memory hard-cap valve (exits immediately with a sentinel code) and
/// the updater's "restart to apply" (quits through gpui so the `on_app_quit`
/// flushes still run); session state rides in `playback_session.json`, which
/// the storage worker persists continuously, so the restart gap is ~2 s.
pub(crate) fn spawn_replacement() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .spawn()
                .ok()
        })
        .is_some()
}

#[cfg(not(test))]
fn spawn_memory_probe() {
    // Net committed growth over the last 10 minutes considered a "step".
    const STEP_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
    const STEP_MIN_MB: i64 = 20;
    const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    /// Soft private-commit budget (MiB) for the watchdog. Tuned from the
    /// 2026-09-27 soak (110+ track changes, cover browsing, lyrics+queue
    /// open): sustained plateau ~370-385 MB, burst peaks ~440 MB settling
    /// back within minutes. Above 420 MB the purge can no longer be explained
    /// by live churn — it means a ratchet (or a fresh leak), so purge hard and
    /// warn every 4th sample.
    const MEMORY_SOFT_CAP_MB: u64 = 420;
    /// Hard private-commit ceiling (MiB). The leak fixes bound every known
    /// growth path, but "无论运行多久都不可能超过" needs an enforced
    /// mechanism against *unknown* future bugs, and no in-process action can
    /// reclaim, e.g., glyph atlas pages (eviction trips the 2026-09-08
    /// dangling-scene crash class). So the terminal valve restarts the
    /// process: two consecutive samples (≥60 s) past the cap mean the
    /// commit is sustained past the line — spawn a replacement exe (session
    /// state lives in playback_session.json, settings save on change) and
    /// exit. Growth between samples is single-digit MB at post-fix rates,
    /// so the bound holds to within ~10 MB regardless of the bug. Guarded by
    /// a minimum-uptime so a startup transient cannot loop-restart; both
    /// values are env-overridable for testing.
    const MEMORY_HARD_CAP_MB: u64 = 500;
    const HARD_CAP_CONSECUTIVE_SAMPLES: u32 = 2;
    const HARD_CAP_MIN_UPTIME: std::time::Duration = std::time::Duration::from_secs(300);

    fn env_override(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    let hard_cap_mb = env_override("MELIORA_MEMORY_HARD_CAP_MB", MEMORY_HARD_CAP_MB);
    let hard_cap_min_uptime = env_override(
        "MELIORA_MEMORY_HARD_CAP_MIN_UPTIME_SECS",
        HARD_CAP_MIN_UPTIME.as_secs(),
    );

    crate::RUNTIME.spawn(async move {
        let started_at = std::time::Instant::now();
        let mut hard_cap_breaches: u32 = 0;
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut baseline: Option<(std::time::Instant, u64)> = None;
        let mut tick_count: u64 = 0;
        loop {
            tick.tick().await;
            let (private, working) = process_memory_mb();
            let covers = disk_cover_cache_mb();
            let render_cache = crate::ui::components::managed_image::render_cache_mb();
            let render_cache_entries = crate::ui::components::managed_image::render_cache_entries();
            let (img_entries, img_mb) = crate::ui::caching::image_cache_stats();
            let funnel = crate::ui::components::managed_image::tile_drop_stats();
            let (mi_commit_mb, mi_rss_mb) = mimalloc_memory_mb();
            // mi_process_info's commit mirrors the OS charge (it is refilled
            // from GetProcessMemoryInfo), so the real mimalloc accounting
            // comes from the stats API: heap = what the allocator owns,
            // non-heap = driver/D3D/atlas/stack commit.
            let heap_committed_mb = crate::mimalloc_stats::committed_mb().unwrap_or(mi_commit_mb);
            // Every 10th sample (~5 min), force a full purge and log what it
            // bought back: large reclaim = idle-page fragmentation, ~0 = the
            // growth is live data and the leak is real.
            tick_count += 1;
            let scheduled_collect = tick_count % 10 == 0;
            // Memory-cap watchdog: past the soft private-commit budget, purge
            // on every sample instead of waiting for the 5-minute cadence, and
            // say so in the log. The budget sits above the measured steady
            // state (~300 MB with lyrics+queue+grid open) and below the known
            // churn drift, so a breach means fragmentation the eager
            // page-reset options failed to hold down (or a fresh leak).
            let watchdog_purge = private >= MEMORY_SOFT_CAP_MB;
            let mi_collect_reclaim_mb = (scheduled_collect || watchdog_purge)
                .then(|| mimalloc_force_collect_reclaim_mb(mi_commit_mb));
            if watchdog_purge && tick_count % 4 == 0 {
                tracing::warn!(
                    private_mb = private,
                    soft_cap_mb = MEMORY_SOFT_CAP_MB,
                    reclaimed_mb = mi_collect_reclaim_mb,
                    "mem watchdog: private commit over soft cap, forced mimalloc purge"
                );
            }
            // Same cadence as the forced collect: mimalloc's own size-bin
            // statistics read right after a full purge, where stranded pages
            // are at their minimum. A large stranded commit there is
            // fragmentation the collector cannot return; a large live total
            // is real retained data, and the top bins say which sizes.
            if tick_count % 10 == 0 {
                crate::mimalloc_stats::log_snapshot(mi_collect_reclaim_mb);
            }

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
                // capture the allocator state right after suspicious growth
                crate::mimalloc_stats::dump_once();
                tracing::warn!(
                    step_mb = delta,
                    private_mb = private,
                    render_cache_mb = render_cache,
                    img_cache_mb = img_mb,
                    covers_mb = covers,
                    tiles_leaked = funnel.5,
                    "mem step: committed grew in last 10 min"
                );
            }

            // Hard-cap valve: sustained commit past the ceiling restarts the
            // process (see MEMORY_HARD_CAP_MB). One sample can be a burst
            // spike; two consecutive mean it is not coming back down on its
            // own.
            if private >= hard_cap_mb {
                hard_cap_breaches += 1;
            } else {
                hard_cap_breaches = 0;
            }
            let hard_cap_restart = hard_cap_breaches >= HARD_CAP_CONSECUTIVE_SAMPLES
                && started_at.elapsed().as_secs() >= hard_cap_min_uptime;
            if hard_cap_restart {
                tracing::error!(
                    private_mb = private,
                    hard_cap_mb = hard_cap_mb,
                    "memory hard cap breached for two consecutive samples; \
                     restarting to bound private commit"
                );
                if spawn_replacement() {
                    std::process::exit(70);
                }
                tracing::error!("memory hard cap: replacement spawn failed; continuing degraded");
                hard_cap_breaches = 0;
            } else {
                #[cfg(not(test))]
                let purge0 =
                    MIMALLOC_PURGE_DELAY_APPLIED.load(std::sync::atomic::Ordering::Relaxed);
                #[cfg(not(test))]
                let page_reset0 =
                    MIMALLOC_PAGE_RESET_APPLIED.load(std::sync::atomic::Ordering::Relaxed);
                // Only resolved when the atlas-probe feature is on (locally
                // patched gpui checkout); None elsewhere keeps the field in
                // the log shape without touching the missing extern symbol.
                #[cfg(all(target_os = "windows", feature = "atlas-probe"))]
                let atlas_field = Some(atlas_probe_snapshot());
                #[cfg(not(all(target_os = "windows", feature = "atlas-probe")))]
                let atlas_field: Option<[u64; 15]> = None;
                #[cfg(not(test))]
                tracing::info!(
                    private_mb = private,
                    working_mb = working,
                    heap_commit_mb = heap_committed_mb,
                    non_heap_mb = private.saturating_sub(heap_committed_mb),
                    mi_rss_mb = mi_rss_mb,
                    mi_collect_reclaim_mb = mi_collect_reclaim_mb,
                    atlas = ?atlas_field,
                    covers_mb = covers,
                    render_cache_mb = render_cache,
                    render_cache_entries = render_cache_entries,
                    backdrop_cache_mb = crate::ui::components::managed_image::backdrop_cache_stats().1,
                    backdrop_cache_entries =
                        crate::ui::components::managed_image::backdrop_cache_stats().0,
                    img_cache_mb = img_mb,
                    img_cache_entries = img_entries,
                    funnel_pending = funnel.0,
                    funnel_pushed = funnel.1,
                    tiles_reclaimed = funnel.2,
                    tiles_kept_by_cache = funnel.3,
                    tiles_kept_holders = funnel.4,
                    tiles_leaked = funnel.5,
                    purge_delay0 = purge0,
                    page_reset0 = page_reset0,
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
    #[cfg(not(test))]
    tune_mimalloc_page_reset();

    // move any data/log dirs left under the legacy `li-ming1/meliora` and
    // `mailliw/hummingbird` names so logins and caches survive the renames
    crate::paths::migrate_legacy_li_ming1_dirs();
    crate::paths::migrate_legacy_dirs();

    I18N_MANAGER.load_source(tr_load!());
    crate::logging::init()?;

    tracing::info!("version {VERSION_STRING}");

    #[cfg(not(test))]
    spawn_memory_probe();

    register_providers(vec![Box::new(LoftyProvider), Box::new(SymphoniaProvider)]);

    // Bound the online image-cache to its 30-day age window even when no
    // cover has been written yet this session.
    #[cfg(feature = "online_sources")]
    crate::media::http_source::prune_image_cache_background();

    crate::ui::app::run()
}
