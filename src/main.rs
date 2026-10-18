// On Windows do NOT show a console window when opening the app
#![cfg_attr(
    all(not(test), not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use cntp_i18n::{I18N_MANAGER, tr_load};
use gpui::set_trace_enabled;
use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock, RwLock};

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
#[cfg(feature = "online_sources")]
mod online_sources;
mod paths;
mod playback;
mod power;
#[cfg(feature = "kugou")]
mod session_io;
mod settings;
mod stats;
#[cfg(test)]
mod test_support;
mod toasts;
// 托盘只在 Windows / macOS 存在；Linux 无托盘，调用点就地回退（见
// window_header.rs 的关闭分支与 app.rs 的 init 调用）。
#[cfg(any(windows, target_os = "macos"))]
mod tray;
pub mod ui;
#[cfg(feature = "online")]
mod updater;

const VERSION_STRING: &str = env!("MELIORA_VERSION_STRING");

/// Set on the replacement process spawned by [`spawn_replacement`]. The old
/// instance is still tearing down (gpui allows quit handlers 200ms, plus
/// destructors), so the child would otherwise see the single-instance mutex
/// held, activate the dying window and exit — "restart to apply" closing the
/// app. With this marker the child waits for the old instance to release it.
const RESTART_HANDOFF_ENV: &str = "MELIORA_RESTART_HANDOFF";

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
        // Every spawn_blocking shares this ceiling: the scan pipeline runs up
        // to 15 metadata reads (normal_worker_count = cores-1, one short
        // spawn_blocking per file) plus up to 8 artwork decodes/resizes
        // (cores/2, guarded by its own semaphore) — a peak of 23 concurrent
        // blocking items on this machine. At 16 the surplus submits land in the
        // pool's unbounded FIFO behind them, so interactive work that also
        // needs the pool (backdrop sharp-fill, cover decode, row metadata,
        // settings/session writes, QR) waits up to one artwork resize (~0.5s)
        // for the rest of the scan. 32 leaves headroom over the scan's own
        // self-limit without raising scan concurrency (the scan caps itself).
        // Idle blocking threads only reserve stack — no committed pages — so
        // this does not fight the memory caps.
        .max_blocking_threads(32)
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

/// 累计 kernel+user CPU 时间（100ns 单位）。与 `[mem]` 探针配套：跨采样
/// 差分除以墙钟即进程 CPU 占用，空闲进程应稳定 ~0%——与 `[frame]` 的
/// frames 字段联手检测"空闲却忙碌"（notify 泄漏、轮询循环、重绘风暴的
/// 非 CPU 形态）。探针是 `#[cfg(not(test))]`（测试用计数分配器替身），
/// 本 helper 跟随同一门。
#[cfg(all(target_os = "windows", not(test)))]
fn process_cpu_times_100ns() -> u64 {
    use core::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        // FILETIME 是两个 u32，按 u64 槽直收（小端下组合即 64 位值）。
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
    }

    // SAFETY: pseudo-handle always valid; every out-param is a plain
    // 8-byte slot written by the call.
    unsafe {
        let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
        let ok = GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        );
        if ok != 0 {
            kernel.saturating_add(user)
        } else {
            0
        }
    }
}

#[cfg(all(not(target_os = "windows"), not(test)))]
fn process_cpu_times_100ns() -> u64 {
    // Not implemented off Windows: the probe's `cpu_permille` stays 0 there.
    0
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
/// Gated to its two callers: the `not(test)` memory-probe valve and the
/// `online` updater (restart-to-apply). A default test build has neither,
/// so the fn is compiled out there instead of tripping dead_code.
#[cfg(any(feature = "online", not(test)))]
pub(crate) fn spawn_replacement() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env(RESTART_HANDOFF_ENV, "1")
                .spawn()
                .ok()
        })
        .is_some()
}

/// [frame] 探针的窗口标签注册表：gpui 的 `FrameTiming` 只带 `WindowId`，
/// 按窗口归因（主窗口 / 桌面歌词悬浮窗的帧节奏差一个数量级，混在一行里
/// 无法判读）需要应用在开窗时登记人类可读标签。反向映射（标签→当前
/// WindowId）：每标签一条，窗口反复开关只覆盖、不累积，已销毁窗口的
/// 旧 id 自然失配，无需反注册。探针跑在 tokio worker 上，30s 一次只读，
/// 注册是开窗级写——RwLock 而非 gpui Global（注册点在窗口创建路径上，
/// 拿不到 App 上下文）。
static FRAME_WINDOW_IDS: OnceLock<RwLock<HashMap<&'static str, gpui::WindowId>>> = OnceLock::new();

/// 开窗后登记该窗口在 `[frame]` 日志里的 `win=` 标签（同标签重开覆盖）。
pub fn register_frame_window_label(handle: gpui::AnyWindowHandle, label: &'static str) {
    let map = FRAME_WINDOW_IDS.get_or_init(Default::default);
    if let Ok(mut map) = map.write() {
        map.insert(label, handle.window_id());
    }
}

/// `[frame]` 渲染归属探针。gpui 侧只能看到窗口级 draw 聚合，`invalidations_max`
/// 不给归属；而 `mark_view_dirty` 会把一次 notify 的**整条祖先链**标脏
/// （gpui window.rs:1937），非 `.cached()` 的视图在脏祖先下逐层重跑 `render()`
/// （view.rs:387 的子树回放只对"自己不脏"的视图生效）。于是"切一首先歌到底让
/// 多少层重渲"这个问题在宿主侧完全不可见——本表把每个视图的 render 次数与
/// 耗时（µs 总和/峰值）按 30s 窗口差分出来，输出成 `[frame] render tally` 行。
///
/// 顺序必须与 [`RenderedView::LABELS`] 一致，编译期由下方 const assert 钉住。
#[derive(Clone, Copy)]
pub enum RenderedView {
    MainWindow,
    Library,
    Sidebar,
    Header,
    Controls,
    PlaybackSection,
    Scrubber,
    InfoSection,
    Lyrics,
    Queue,
    QueueRow,
    TrackRow,
    TableItem,
    GridItem,
    Immersive,
    ImmersiveProgress,
    DesktopLyrics,
    Search,
    Equalizer,
}

impl RenderedView {
    const LABELS: [&'static str; 19] = [
        "main",
        "library",
        "sidebar",
        "header",
        "controls",
        "playback",
        "scrubber",
        "info",
        "lyrics",
        "queue",
        "queue-row",
        "track-row",
        "table-item",
        "grid-item",
        "immersive",
        "imm-progress",
        "desk-lyrics",
        "search",
        "eq",
    ];
}

const _: () = assert!(RenderedView::Equalizer as usize + 1 == RenderedView::LABELS.len());

static RENDER_TALLY: [std::sync::atomic::AtomicU64; RenderedView::LABELS.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; RenderedView::LABELS.len()];
/// 每个视图 render 的 CPU 时间（µs）：窗口内总和 / 单次峰值。次数只说明
/// "被重渲了"，耗时才回答"值不值得缓存它"。
static RENDER_MICROS: [std::sync::atomic::AtomicU64; RenderedView::LABELS.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; RenderedView::LABELS.len()];
static RENDER_PEAK_MICROS: [std::sync::atomic::AtomicU64; RenderedView::LABELS.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; RenderedView::LABELS.len()];
/// 与 `[frame]` 探针同开关（`MELIORA_FRAME_TRACE`）：关闭时调用点只剩一次
/// relaxed load，不分配、不写。
static RENDER_TALLY_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 一次 `render()` 调用的测量作用域：进入时计数，Drop 时记账。绑定到函数
/// 作用域即可覆盖整个 render 体（元素构建的成本全在返回前发生）。
#[must_use = "bind it for the whole render body (`let _render = tally_render(..)`) or the timing is meaningless"]
pub struct RenderProbe {
    view: usize,
    started: Option<std::time::Instant>,
}

/// 视图 `render()` 入口调用一次。
pub fn tally_render(view: RenderedView) -> RenderProbe {
    use std::sync::atomic::Ordering::Relaxed;
    let enabled = RENDER_TALLY_ENABLED.load(Relaxed);
    if enabled {
        RENDER_TALLY[view as usize].fetch_add(1, Relaxed);
    }
    RenderProbe {
        view: view as usize,
        started: enabled.then(std::time::Instant::now),
    }
}

impl Drop for RenderProbe {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(started) = self.started else { return };
        let micros = started.elapsed().as_micros() as u64;
        RENDER_MICROS[self.view].fetch_add(micros, Relaxed);
        RENDER_PEAK_MICROS[self.view].fetch_max(micros, Relaxed);
    }
}

/// 取窗口内增量并清零，拼成一行 `label=次数/µs总和/µs峰值`；无重渲的视图
/// 不出现（空闲就该是空串）。每个 30s 窗口一次分配，随日志行一起释放。
/// 探针本体是 `#[cfg(not(test))]`，本函数只被它调用，同门下编译。
#[cfg(not(test))]
fn take_render_tally() -> String {
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering::Relaxed;
    let mut out = String::new();
    let rows = RenderedView::LABELS
        .iter()
        .zip(&RENDER_TALLY)
        .zip(RENDER_MICROS.iter().zip(&RENDER_PEAK_MICROS));
    for ((label, count), (micros, peak)) in rows {
        let n = count.swap(0, Relaxed);
        let total_us = micros.swap(0, Relaxed);
        let peak_us = peak.swap(0, Relaxed);
        if n > 0 {
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "{label}={n}/{total_us}/{peak_us}");
        }
    }
    out
}

/// 聚合并记录自上次调用以来的帧计时（`[frame] periodic`）。draw 时长是
/// gpui `Window::draw` 的 CPU 段（场景构建 + 元素绘制）；dirty→draw 含
/// 调度与合并延迟（上限为一个帧间隔属正常，持续超一个帧间隔才是问题）。
/// gpui 的帧计时环形缓冲是全窗口共享的：按 `window_id` 分组，每个有帧的
/// 窗口各输出一行（`win=` 字段）。登记过但本窗口零帧的标签也各输出一行
/// `frames=0`——隐藏/遮挡期间 gpui 不产生帧回调（省电，正确行为），但
/// "探针活着而这个窗没画"与"探针压根没数据"必须在日志里长得不一样，
/// 否则无法据此验证"扫描期根视图重绘归零"这类结论。
#[cfg(not(test))]
/// `[frame]` 延迟归因的下限：低于这个值的等待不做遮挡判断（正常一帧的重绘
/// 区间内本来就落不下别的帧，全部会被误报成遮挡）。见 log_frame_aggregate。
const SLOW_DRAW_GAP_US: u64 = 500_000;

#[cfg(not(test))]
fn log_frame_aggregate(collector: &mut gpui::profiler::FrameTimingCollector) {
    let frames = collector.collect_unseen();
    let mut by_window: HashMap<gpui::WindowId, Vec<&gpui::profiler::FrameTiming>> = HashMap::new();
    for frame in &frames {
        by_window.entry(frame.window_id).or_default().push(frame);
    }
    let drawn: Vec<gpui::WindowId> = by_window.keys().copied().collect();
    let mut windows: Vec<(gpui::WindowId, Vec<&gpui::profiler::FrameTiming>)> =
        by_window.into_iter().collect();
    windows.sort_by_key(|(id, _)| id.as_u64());
    let labels = FRAME_WINDOW_IDS.get_or_init(Default::default);
    let labels = labels.read().ok();
    for (window_id, frames) in windows {
        let win = labels
            .as_ref()
            .and_then(|labels| {
                labels
                    .iter()
                    .find(|(_, id)| **id == window_id)
                    .map(|(label, _)| *label)
            })
            .map_or_else(
                || format!("win-{}", window_id.as_u64()),
                |label| label.to_string(),
            );
        let mut draws_us: Vec<u64> = frames
            .iter()
            .map(|frame| frame.draw_duration().as_micros() as u64)
            .collect();
        draws_us.sort_unstable();
        let n = draws_us.len();
        let over = |budget_us: u64| draws_us.iter().filter(|d| **d > budget_us).count();
        let dirty_to_draw_max_us = frames
            .iter()
            .filter_map(|frame| frame.dirty_to_draw_duration())
            .map(|d| d.as_micros() as u64)
            .max()
            .unwrap_or(0);
        let invalidations_max = frames
            .iter()
            .map(|frame| frame.invalidations)
            .max()
            .unwrap_or(0);
        // 延迟归因用直接证据，不用"dirty→draw 超过 60s 就算遮挡"的事后阈值。
        // 在最大延迟那一帧的 (dirty_at, draw_end) 区间里数本窗口还落地过几帧：
        //   0 帧   ⇒ 这段时间根本没产生帧回调（隐藏/最小化/遮挡的省电正确行为，
        //            或主线程被长时间占死）——到底是哪种，看同一时间戳的
        //            [db] slow ui-thread query / [mem] 行来分辨；
        //   ≥1 帧  ⇒ 帧循环是活的，这一帧却被压住了 = 真实的调度/卡顿问题。
        // 阈值版的实证错误就在日志里：真遮挡的 9 个样本恰好全 >60s，而 47.97s
        // 与 54.67s 两个样本被报成"非遮挡"，落在盲区里既不能证实也不能排除。
        // 下限 SLOW_DRAW_GAP_US 以下的延迟不做归因：一次正常重绘的区间里
        // 本来就落不下别的帧，会把每一帧都误报成"遮挡"。
        let late = frames
            .iter()
            .filter_map(|frame| frame.dirty_to_draw_duration().map(|d| (d, frame)))
            .max_by_key(|(d, _)| *d);
        let frames_in_gap = late
            .and_then(|(_, late)| {
                late.dirty_at.map(|dirty_at| {
                    frames
                        .iter()
                        .filter(|frame| {
                            !std::ptr::eq(*frame, late)
                                && frame.draw_end > dirty_at
                                && frame.draw_end < late.draw_end
                        })
                        .count()
                })
            })
            .unwrap_or(0);
        let occluded_gap = dirty_to_draw_max_us > SLOW_DRAW_GAP_US && frames_in_gap == 0;
        tracing::info!(
            win = %win,
            frames = n,
            draw_p50_us = draws_us[n / 2],
            draw_p99_us = draws_us[((n * 99) / 100).min(n - 1)],
            draw_max_us = draws_us[n - 1],
            over_8ms = over(8_000),
            over_16ms = over(16_000),
            over_33ms = over(33_000),
            dirty_to_draw_max_us,
            frames_in_gap,
            invalidations_max,
            occluded_gap,
            "[frame] periodic"
        );
    }

    // 登记过、本窗口内零帧的标签：显式 frames=0，按标签排序保持日志可读顺序。
    // 只在"静默集合变化"时记：主窗常驻托盘/设置窗关闭后集合稳定，逐窗口重放
    // 会变成每分钟数行、每天约 1MB 的纯噪声；而进入静默的那一次仍会留下证据。
    static LAST_SILENT: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if let Some(labels) = labels.as_ref() {
        let mut silent: Vec<&'static str> = labels
            .iter()
            .filter(|(_, id)| !drawn.contains(id))
            .map(|(label, _)| *label)
            .collect();
        silent.sort_unstable();
        let mut last = LAST_SILENT.lock().unwrap_or_else(|e| e.into_inner());
        if silent != *last {
            for label in &silent {
                tracing::info!(win = %label, frames = 0, "[frame] periodic");
            }
            *last = silent;
        }
    }

    // 渲染归属：这一窗口内每个视图重渲了几次。切歌/悬停/滚动三类场景的判读
    // 全靠这行——见 `RenderedView` 文档注释。
    if RENDER_TALLY_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        let tally = take_render_tally();
        if !tally.is_empty() {
            tracing::info!(tally = %tally, "[frame] render tally");
        }
    }
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
    /// process: two consecutive 30 s samples (the commit stays above the cap
    /// for at least the 30 s between them) mean it is sustained past the
    /// line — spawn a replacement exe (session
    /// state lives in playback_session.json, settings save on change) and
    /// exit. Growth between samples is single-digit MB at post-fix rates,
    /// so the bound holds to within ~10 MB regardless of the bug. Guarded by
    /// a minimum-uptime so a startup transient cannot loop-restart; both
    /// values are env-overridable for testing.
    const MEMORY_HARD_CAP_MB: u64 = 500;
    const HARD_CAP_CONSECUTIVE_SAMPLES: u32 = 2;
    const HARD_CAP_MIN_UPTIME: std::time::Duration = std::time::Duration::from_secs(300);
    /// Hysteresis margin below the soft cap before the pressure flag clears:
    /// after a shed the caches refill on demand, and clearing only at
    /// cap − 30 MB keeps the flag from flapping around the boundary.
    const PRESSURE_CLEAR_MARGIN_MB: u64 = 30;

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
        // covers_mb 阶梯值：只在低频档刷新（见循环内刷新点），两次刷新
        // 之间保持上次数值——字段语义是"低频刷新的磁盘封面缓存用量"。
        let mut covers_mb: u64 = 0;
        // Pressure-shed bookkeeping: consecutive over-cap samples and the
        // rising edge (previous sample was under the cap).
        let mut over_cap_streak: u32 = 0;
        let mut was_over_cap = false;
        // 启动分级：interval 首个 tick 立即完成，t=0 全量枚举 image-cache 目录
        // （数千文件的 metadata）会与首帧窗口争盘，先推迟 60s 再进入采样循环
        // （间隔 30s，对内存观测无实质损失；看门狗晚 60s 相对 300s 最小存活
        // 守卫可忽略）。
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        // [frame] 探针：gpui 内建的帧计时（profiler::FrameTiming，运行时
        // 开关、关闭时零开销）——每 30s 聚合本窗口的帧耗时分布（draw 时长
        // p50/p99/max、超 8/16/33ms 计数、合并的 invalidation 数、dirty→
        // draw 延迟），作为渲染优化的测量基线。空闲 UI 不产生帧：frames
        // 长期偏高 = notify 泄漏/无谓重绘，本探针兼任其检测器。环形缓冲
        // 有界（16MiB），无驻留风险；MELIORA_FRAME_TRACE=0 关闭。
        // 注意：首个 [frame] 行最早出现在启动后 ~90s（60s 延迟 + 30s 首
        // 个聚合窗口），更早检查日志看到零行不是探针失效。
        let frame_trace = env_override("MELIORA_FRAME_TRACE", 1) != 0;
        if frame_trace {
            gpui::profiler::set_frame_trace_enabled(true);
            // 渲染归属计数与帧计时同开关（关掉时调用点仅一次 relaxed load）。
            RENDER_TALLY_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let mut frame_collector = gpui::profiler::FrameTimingCollector::new();
        // CPU 占用差分状态：上次采样的累计 kernel+user 时间（100ns）。
        let mut last_cpu: Option<(u64, std::time::Instant)> = None;
        loop {
            tick.tick().await;
            if frame_trace {
                log_frame_aggregate(&mut frame_collector);
            }
            let (private, working) = process_memory_mb();
            // 全机逻辑核归一的进程 CPU 占用（千分比）。用千分比而不是百分比：
            // 16 核 × 30 s 归一化下 1% = 480 ms 核心时间，而本应用的全部工作
            // 区间都在 1% 以下（一条 4096 背景管线 570 ms 单线程 ≈ 0.2%），
            // 整数百分比把它们一律显示成 0——实测一整轮会话 172 个采样全为
            // 0，探针因此无法履行"检测空闲却忙碌"的本职。与 [frame] frames
            // 联手定位"空闲却忙碌"。
            let cpu_permille = {
                let now = std::time::Instant::now();
                let total = process_cpu_times_100ns();
                let permille = last_cpu
                    .and_then(|(prev, prev_at)| {
                        let wall_us = prev_at.elapsed().as_micros() as u64;
                        (wall_us > 0).then(|| {
                            let cores = std::thread::available_parallelism()
                                .map(|n| n.get() as u64)
                                .unwrap_or(1);
                            let cpu_us = total.saturating_sub(prev) / 10;
                            (cpu_us * 1000) / wall_us.saturating_mul(cores)
                        })
                    })
                    .unwrap_or(0);
                last_cpu = Some((total, now));
                permille
            };
            let render_cache = crate::ui::components::managed_image::render_cache_mb();
            let render_cache_entries = crate::ui::components::managed_image::render_cache_entries();
            let (img_entries, img_mb) = crate::ui::caching::image_cache_stats();
            let funnel = crate::ui::components::managed_image::tile_drop_stats();
            let (funnel_pending_mb, funnel_pending_large, funnel_pushed_large) =
                crate::ui::components::managed_image::funnel_pending_detail();
            let (backdrop_cache_entries, backdrop_cache_mb) =
                crate::ui::components::managed_image::backdrop_cache_stats();
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
            // covers_mb 低频档：磁盘封面缓存目录可达数千文件，全量
            // read_dir + metadata 枚举不为一个观测字段每 30s 付一次——与
            // mimalloc_stats 快照同频（每 10 个采样 ≈5 分钟，同款
            // is_multiple_of(10) 写法），tick 1 补一次让首个 periodic 日志
            // 就有真值（60s 启动延迟的争盘理由见循环前注释，两者共用）。
            // 两次刷新之间 covers_mb 保持上次数值（阶梯更新）。该字段只
            // 服务于 [mem] periodic / mem step 的日志观测：mem step 告警
            // 的判定只吃 private commit 增量，covers_mb 是伴随字段，阶梯
            // 值（至多 ~5 分钟陈旧）可接受；进程内 img_cache_mb 仍逐采样
            // 刷新，告警路径不受影响。
            if tick_count == 1 || tick_count.is_multiple_of(10) {
                // 封面缓存目录可达数千文件，同步枚举走阻塞池，不占 async worker。
                covers_mb = crate::RUNTIME
                    .spawn_blocking(disk_cover_cache_mb)
                    .await
                    .unwrap_or(0);
            }
            let scheduled_collect = tick_count.is_multiple_of(10);
            // Memory-cap watchdog: past the soft private-commit budget, purge
            // on every sample instead of waiting for the 5-minute cadence, and
            // say so in the log. The budget sits above the measured steady
            // state (~300 MB with lyrics+queue+grid open) and below the known
            // churn drift, so a breach means fragmentation the eager
            // page-reset options failed to hold down (or a fresh leak).
            let watchdog_purge = private >= MEMORY_SOFT_CAP_MB;
            let mi_collect_reclaim_mb = if scheduled_collect || watchdog_purge {
                // mi_collect(true) 是整堆扫描（10-06 实测一次回收 206MB 的就是它）。
                // 探针跑在只有 2 个 worker 的 RUNTIME 上，同步执行等于长期占住一半
                // 异步产能数十至上百 ms，而软顶看门狗触发时正是最需要响应的时候——
                // 交阻塞池，与本探针内 covers_mb 的既有做法一致。
                Some(
                    crate::RUNTIME
                        .spawn_blocking(move || mimalloc_force_collect_reclaim_mb(mi_commit_mb))
                        .await
                        .unwrap_or(0),
                )
            } else {
                None
            };
            if watchdog_purge && tick_count.is_multiple_of(4) {
                tracing::warn!(
                    private_mb = private,
                    soft_cap_mb = MEMORY_SOFT_CAP_MB,
                    reclaimed_mb = mi_collect_reclaim_mb,
                    "mem watchdog: private commit over soft cap, forced mimalloc purge"
                );
            }
            // Pressure shed: the soft cap is the early-warning line the hard
            // valve counts from (two consecutive samples 30 s apart). On the
            // rising edge, yield the general-purpose image caches — render
            // LRU to half its window, backdrop LRU to the current track only
            // — and raise the pressure flag so the immersive view stops
            // prefetching the next track (a whole 4096 pipeline saved).
            // Sustained over-cap re-sheds every 4th sample (~2 min) to catch
            // post-shed cache refill. Evictions ride the funnel's normal age
            // gates (60s covers / 3s large), so driver-side commit relief
            // lands on minute scale — precisely the sustained regime the hard
            // valve requires: the 2026-10-02 restart ramp (341→521 MB over
            // 3 min of immersive track switching) would have shed ~60 MB at
            // the first crossing, two samples before the breach. The flag
            // clears only below cap − margin (hysteresis).
            if watchdog_purge {
                over_cap_streak += 1;
                let rising = !was_over_cap;
                if rising || over_cap_streak.is_multiple_of(4) {
                    crate::ui::components::managed_image::render_cache_shrink(
                        crate::ui::components::managed_image::RENDER_CACHE_PRESSURE_KEEP,
                    );
                    crate::ui::components::managed_image::backdrop_cache_shrink(1);
                }
                if rising {
                    let (funnel_pending_mb, _, _) =
                        crate::ui::components::managed_image::funnel_pending_detail();
                    tracing::warn!(
                        private_mb = private,
                        soft_cap_mb = MEMORY_SOFT_CAP_MB,
                        render_cache_entries =
                            crate::ui::components::managed_image::render_cache_entries(),
                        funnel_pending_mb,
                        "mem pressure: shedding image caches (render LRU → 12, \
                         backdrop LRU → 1) and suspending next-track prefetch"
                    );
                }
                crate::ui::components::managed_image::set_memory_pressure(true);
            } else {
                over_cap_streak = 0;
                if private + PRESSURE_CLEAR_MARGIN_MB < MEMORY_SOFT_CAP_MB
                    && crate::ui::components::managed_image::memory_pressure()
                {
                    crate::ui::components::managed_image::set_memory_pressure(false);
                    tracing::info!(
                        private_mb = private,
                        "mem pressure cleared; caches refill on demand"
                    );
                }
            }
            was_over_cap = watchdog_purge;
            // Same cadence as the forced collect: mimalloc's own size-bin
            // statistics read right after a full purge, where stranded pages
            // are at their minimum. A large stranded commit there is
            // fragmentation the collector cannot return; a large live total
            // is real retained data, and the top bins say which sizes.
            if tick_count.is_multiple_of(10) {
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
                    // 阶梯值：低频档刷新（见上方 covers_mb 刷新点）
                    covers_mb,
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
                    // 阀门重启依赖会话/设置随改随存：防抖窗口内的最后一笔
                    // 同步补写后再退出，替换进程才能读到完整状态。
                    crate::settings::flush_pending_settings();
                    std::process::exit(70);
                }
                tracing::error!("memory hard cap: replacement spawn failed; continuing degraded");
                hard_cap_breaches = 0;
            } else {
                let purge0 =
                    MIMALLOC_PURGE_DELAY_APPLIED.load(std::sync::atomic::Ordering::Relaxed);
                let page_reset0 =
                    MIMALLOC_PAGE_RESET_APPLIED.load(std::sync::atomic::Ordering::Relaxed);
                // Only resolved when the atlas-probe feature is on (locally
                // patched gpui checkout); None elsewhere keeps the field in
                // the log shape without touching the missing extern symbol.
                #[cfg(all(target_os = "windows", feature = "atlas-probe"))]
                let atlas_field = Some(atlas_probe_snapshot());
                #[cfg(not(all(target_os = "windows", feature = "atlas-probe")))]
                let atlas_field: Option<[u64; 15]> = None;
                tracing::info!(
                    private_mb = private,
                    working_mb = working,
                    heap_commit_mb = heap_committed_mb,
                    non_heap_mb = private.saturating_sub(heap_committed_mb),
                    cpu_permille = cpu_permille,
                    mi_rss_mb = mi_rss_mb,
                    mi_collect_reclaim_mb = mi_collect_reclaim_mb,
                    atlas = ?atlas_field,
                    // 阶梯值：低频档刷新（每 10 个采样 ≈5 分钟，tick 1 补一次）
                    covers_mb,
                    render_cache_mb = render_cache,
                    render_cache_entries = render_cache_entries,
                    backdrop_cache_mb,
                    backdrop_cache_entries,
                    img_cache_mb = img_mb,
                    img_cache_entries = img_entries,
                    funnel_pending = funnel.0,
                    funnel_pushed = funnel.1,
                    funnel_pending_mb,
                    funnel_pending_large,
                    funnel_pushed_large,
                    tiles_reclaimed = funnel.2,
                    tiles_kept_by_cache = funnel.3,
                    tiles_kept_holders = funnel.4,
                    tiles_leaked = funnel.5,
                    purge_delay0 = purge0,
                    page_reset0 = page_reset0,
                    "[mem] periodic"
                );
            }
        }
    });
}

/// Total bytes of the on-disk online-cover cache, in MiB. Tracks how many
/// distinct covers the session has touched, independent of process memory.
/// Sampling cadence is owned by the caller: the `[mem]` probe refreshes it
/// on the low-frequency tier (every 10th sample, see the refresh site in
/// `spawn_memory_probe`), so readers treat the logged value as a staircase,
/// not a per-sample reading.
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

/// Single-instance guard: a named kernel mutex held for the whole process
/// lifetime. A second launch would race the running instance for
/// settings.json, playback_session.json, the library database and the audio
/// device — none of that is designed for two writers. The kernel reclaims the
/// mutex on crash, so a stale lock can never block the next launch.
#[cfg(target_os = "windows")]
fn enforce_single_instance() -> anyhow::Result<()> {
    use windows::{
        Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
        Win32::System::Threading::CreateMutexW,
        Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW},
        core::w,
    };

    // A restart-to-apply child races the parent's teardown: gpui gives quit
    // handlers up to 200ms and destructors add more, so the mutex can still be
    // held when the child reaches this point. Wait a bounded while instead of
    // activating the dying parent and exiting.
    let handoff = std::env::var_os(RESTART_HANDOFF_ENV).is_some();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mutex = unsafe { CreateMutexW(None, false, w!("Local\\MelioraSingleInstance"))? };
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            // `HANDLE` has no Drop impl: discarding the binding leaves the
            // kernel handle open, so the mutex stays held for the process
            // lifetime. The kernel reclaims it at exit (a crash cannot leave
            // a stale lock behind).
            let _ = mutex;
            return Ok(());
        }
        let _ = unsafe { CloseHandle(mutex) };
        if !handoff || std::time::Instant::now() >= deadline {
            // 已有实例：优先唤回它的窗口（托盘宿主窗口接收激活消息；窗口隐藏
            // 时这是用户唯一的"再点一次桌面图标"恢复路径）。唤回失败（目标实例
            // 可能还在启动中、托盘壳未建）才退回提示对话框。
            if !crate::tray::activate_existing_instance() {
                // Runs before logging/i18n init, so the notice is a plain dialog with
                // a fixed bilingual text rather than a tr! lookup.
                unsafe {
                    MessageBoxW(
                        None,
                        w!(
                            "Meliora is already running.\nMeliora \u{5df2}\u{7ecf}\u{5728}\u{8fd0}\u{884c}\u{3002}"
                        ),
                        w!("Meliora"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
            }
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    enforce_single_instance()?;

    // RegisterApplicationRestart can fail on policies/older builds; that is a
    // nice-to-have OS integration, never a reason to refuse to start. The
    // logger is not up yet, so carry the error until it is.
    #[cfg(target_os = "windows")]
    let restart_registration_error = init_windows_restart().err();

    // disable the GPUI mini profiler immediately to avoid unnecessary allocations
    set_trace_enabled(false);

    #[cfg(not(test))]
    tune_mimalloc_purge_delay();
    #[cfg(not(test))]
    tune_mimalloc_page_reset();

    // move any data/log dirs left under the legacy `li-ming1/meliora` and
    // `mailliw/hummingbird` names so logins and caches survive the renames.
    // Runs before logging::init because every later init reads these paths;
    // failures are collected here and logged right after the logger is up.
    let mut migration_failures = crate::paths::migrate_legacy_li_ming1_dirs();
    migration_failures.extend(crate::paths::migrate_legacy_dirs());

    I18N_MANAGER.load_source(tr_load!());
    crate::logging::init()?;

    for err in migration_failures {
        tracing::warn!(%err, "failed to migrate legacy data dir");
    }

    #[cfg(target_os = "windows")]
    if let Some(error) = restart_registration_error {
        tracing::warn!(%error, "failed to register application restart; continuing");
    }

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
