//! Direct readout of mimalloc v3's aggregated size-bin statistics, so the
//! `[mem]` probe can separate live allocation bytes from allocator page
//! retention without an external profiler: `committed - live` after a forced
//! purge is stranded pages, and the per-bin breakdown identifies *which
//! allocation sizes* hold the retained memory.
//!
//! The `mi_stats_t` mirror below is layout-pinned to the vendored v3 tree
//! (libmimalloc-sys 0.1.49, `MI_STAT_VERSION` 5); the structure self-reports
//! its size and version, so a dependency bump that changes the layout makes
//! `read()` return `None` instead of logging garbage.

#![allow(non_camel_case_types, non_snake_case)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::sync::atomic::Ordering;

const MI_STAT_VERSION: usize = 5;
const MI_BIN_HUGE: usize = 73;
const MI_CBIN_COUNT: usize = 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct mi_stat_count_t {
    total: i64,
    peak: i64,
    current: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct mi_stat_counter_t {
    total: i64,
}

#[repr(C)]
#[allow(dead_code)] // full-layout mirror: most fields exist for layout, only a few are read
struct mi_stats_t {
    size: usize,
    version: usize,
    // MI_STAT_FIELDS(), in the exact order of include/mimalloc-stats.h.
    pages: mi_stat_count_t,
    reserved: mi_stat_count_t,
    committed: mi_stat_count_t,
    reset: mi_stat_counter_t,
    purged: mi_stat_counter_t,
    page_committed: mi_stat_count_t,
    pages_abandoned: mi_stat_count_t,
    threads: mi_stat_count_t,
    malloc_normal: mi_stat_count_t,
    malloc_huge: mi_stat_count_t,
    malloc_requested: mi_stat_count_t,
    mmap_calls: mi_stat_counter_t,
    commit_calls: mi_stat_counter_t,
    reset_calls: mi_stat_counter_t,
    purge_calls: mi_stat_counter_t,
    arena_count: mi_stat_counter_t,
    malloc_normal_count: mi_stat_counter_t,
    malloc_huge_count: mi_stat_counter_t,
    malloc_guarded_count: mi_stat_counter_t,
    arena_rollback_count: mi_stat_counter_t,
    arena_purges: mi_stat_counter_t,
    pages_extended: mi_stat_counter_t,
    pages_retire: mi_stat_counter_t,
    page_searches: mi_stat_counter_t,
    page_searches_count: mi_stat_counter_t,
    segments: mi_stat_count_t,
    segments_abandoned: mi_stat_count_t,
    segments_cache: mi_stat_count_t,
    segments_reserved: mi_stat_count_t,
    heaps: mi_stat_count_t,
    theaps: mi_stat_count_t,
    pages_reclaim_on_alloc: mi_stat_counter_t,
    pages_reclaim_on_free: mi_stat_counter_t,
    pages_reabandon_full: mi_stat_counter_t,
    pages_unabandon_busy_wait: mi_stat_counter_t,
    heaps_delete_wait: mi_stat_counter_t,
    _stat_reserved: [mi_stat_count_t; 4],
    _stat_counter_reserved: [mi_stat_counter_t; 4],
    malloc_bins: [mi_stat_count_t; MI_BIN_HUGE + 1],
    page_bins: [mi_stat_count_t; MI_BIN_HUGE + 1],
    chunk_bins: [mi_stat_count_t; MI_CBIN_COUNT],
}

unsafe extern "C" {
    fn mi_stats_get(stats: *mut mi_stats_t) -> bool;
    fn mi_subproc_current() -> mi_subproc_id_t;
    fn mi_subproc_heap_stats_print_out(
        subproc: mi_subproc_id_t,
        out: mi_output_fun,
        arg: *mut c_void,
    );
}

/// `mi_subproc_id_t`: an abstract pointer-sized handle.
#[repr(C)]
#[derive(Clone, Copy)]
struct mi_subproc_id_t {
    _id: *mut c_void,
}

/// `mi_output_fun`: mimalloc feeds the stats table through this callback in
/// arbitrarily-sized fragments.
type mi_output_fun = extern "C" fn(msg: *const c_char, arg: *mut c_void);

thread_local! {
    static DUMP_LINE: RefCell<String> = const { RefCell::new(String::new()) };
}

extern "C" fn dump_out(msg: *const c_char, _arg: *mut c_void) {
    if msg.is_null() {
        return;
    }
    // SAFETY: mimalloc passes a NUL-terminated string for the duration of the call.
    let chunk = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy();
    DUMP_LINE.with(|buf| {
        let mut buf = buf.borrow_mut();
        for part in chunk.split_inclusive('\n') {
            buf.push_str(part);
            if part.ends_with('\n') {
                let line = buf.trim_end();
                if !line.is_empty() {
                    tracing::info!("[mem] mi stats dump | {}", line);
                }
                buf.clear();
            }
        }
    });
}

/// Logs mimalloc's complete statistics table (per heap: peak / total /
/// current per category plus the page counters). Called from the probe's
/// `mem step` alert so the allocator state right after suspicious growth is
/// captured. A three-dump budget per process keeps a browsing session from
/// spamming the log; later calls are no-ops.
pub fn dump_once() {
    static DUMP_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if DUMP_COUNT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < 3).then_some(n + 1)
        })
        .is_err()
    {
        return;
    }
    tracing::info!("[mem] mi stats dump | --- full mimalloc stats table ---");
    // SAFETY: pure statistics output into our callback. The subprocess-wide
    // variant prints the aggregate *and* every heap's own table, including
    // the malloc size-bin breakdown that the plain mi_stats_print_out omits.
    unsafe {
        mi_subproc_heap_stats_print_out(mi_subproc_current(), dump_out, std::ptr::null_mut())
    };
    DUMP_LINE.with(|buf| {
        let rest = buf.borrow_mut().trim_end().to_string();
        if !rest.is_empty() {
            tracing::info!("[mem] mi stats dump | {}", rest);
        }
        buf.borrow_mut().clear();
    });
}

/// Trusted subset of the aggregated stats at one probe point.
pub struct MemStatsSnapshot {
    /// Committed heap bytes per the allocator's own accounting.
    pub committed_bytes: u64,
    pub abandoned_pages: u64,
    pub threads: u64,
    pub theaps: u64,
}

fn current(count: &mi_stat_count_t) -> u64 {
    count.current.max(0) as u64
}

/// The allocator's own committed-heap accounting, in MiB. `mi_process_info`
/// returns the OS commit charge (see the comment in the probe), so this is
/// the number to subtract from process-private bytes to get the driver/D3D
/// share. `None` when the stats call or the pinned mirror fails.
pub fn committed_mb() -> Option<u64> {
    read().map(|snapshot| snapshot.committed_bytes / (1024 * 1024))
}

/// Guards the one-time failure warn in `log_snapshot`.
static READ_FAILURE_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Reads the aggregated stats, or `None` when the call fails or the C
/// structure no longer matches the pinned mirror (dependency bump). Only
/// fields the linked build actually maintains are read out: the malloc
/// per-bin currents are aggregated under `MI_STAT > 1` only, so they are
/// always zero here and the live-bytes attribution comes from the dumps.
pub fn read() -> Option<MemStatsSnapshot> {
    // Zeroed heap buffer, not a stack value: the structure is ~4 KiB and the
    // callers' stacks include the audio thread's.
    let layout = Layout::new::<mi_stats_t>();
    let raw = unsafe { alloc_zeroed(layout) };
    if raw.is_null() {
        return None;
    }
    let stats = raw as *mut mi_stats_t;
    // v3 API contract: the caller fills the destination header first (size +
    // version, what mi_stats_init does on the C side); mi_stats_copy validates
    // it against the linked build and rejects mismatches before copying.
    unsafe {
        (*stats).size = std::mem::size_of::<mi_stats_t>();
        (*stats).version = MI_STAT_VERSION;
    }
    let ok = unsafe { mi_stats_get(stats) };
    let header_ok = ok
        && unsafe { (*stats).version } == MI_STAT_VERSION
        && unsafe { (*stats).size } == std::mem::size_of::<mi_stats_t>();
    let result = if !header_ok {
        None
    } else {
        let stats = unsafe { &*stats };
        Some(MemStatsSnapshot {
            committed_bytes: current(&stats.committed),
            abandoned_pages: current(&stats.pages_abandoned),
            threads: current(&stats.threads),
            theaps: current(&stats.theaps),
        })
    };
    // SAFETY: allocated above with the same layout, never freed elsewhere.
    unsafe { dealloc(raw, layout) };
    result
}

/// Logs one `[mem] mi stats` line. Meant to run right after the probe's
/// forced `mi_collect`, where stranded pages are at their minimum, so a
/// large `stranded_mb` at that point is fragmentation the collector could
/// not return and a large `live_mb` is real retained data. The full stats
/// table is dumped separately by the probe's step alerts (`dump_once`), not
/// here — the baseline idle state is known and the dump budget is reserved
/// for post-growth captures.
pub fn log_snapshot(collect_reclaim_mb: Option<u64>) {
    let Some(snapshot) = read() else {
        // Warn exactly once with the observed header so a mirror/layout drift
        // after a dependency bump is visible in the log instead of silently
        // dropping the whole `[mem] mi stats` series.
        if !READ_FAILURE_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                expected_size = std::mem::size_of::<mi_stats_t>(),
                expected_version = MI_STAT_VERSION,
                "[mem] mi stats unavailable: mi_stats_get rejected the mirror header \
                 (layout or version mismatch with the linked mimalloc)"
            );
        }
        return;
    };
    let mb = |bytes: u64| bytes / (1024 * 1024);
    // Only the aggregated `committed` / thread counters are trusted here: the
    // malloc per-bin currents are not maintained by the linked build
    // (MI_STAT==1 gates their aggregation), so live/stranded attribution
    // comes from the step-alert dumps instead.
    tracing::info!(
        committed_mb = mb(snapshot.committed_bytes),
        abandoned_pages = snapshot.abandoned_pages,
        threads = snapshot.threads,
        theaps = snapshot.theaps,
        collect_reclaim_mb = collect_reclaim_mb.unwrap_or(0),
        "[mem] mi stats"
    );
}
