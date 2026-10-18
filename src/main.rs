// Windows：打开应用时不显示控制台窗口
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
mod sync;
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

/// 由 [`spawn_replacement`] 启动的替换进程会带上这个标记。旧实例当时还在拆解
/// （gpui 给 quit handler 最多 200ms，外加析构函数），否则子进程会看到单实例互斥量
/// 仍被占用、激活那个垂死的窗口然后退出——即"重启以应用"反而把应用关掉。带此标记
/// 时，子进程会等待旧实例释放互斥量。
const RESTART_HANDOFF_ENV: &str = "MELIORA_RESTART_HANDOFF";

// 测试期统计分配次数：播放管线的分配行为测试需要它
#[cfg(test)]
#[global_allocator]
static ALLOC_GUARD: test_support::alloc_guard::CountingAllocator =
    test_support::alloc_guard::CountingAllocator;

// 非测试构建用 mimalloc：NT 堆从不把中等大小的空闲段归还给 OS，所以即使没有泄漏，
// 交互抖动也会把 private 字节棘轮式抬上去；mimalloc 会对空闲页去提交，活动停止后
// 已提交内存能回落。测试仍使用上面的计数分配器。
#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 把 mimalloc 的 purge delay 设为程序默认 0：释放的页立即去提交，而不是等出厂
/// 延迟，从而压低交互内存峰值（重度浏览点击会话实测 468 -> 407 MB）。该选项必须
/// 经 C API 设置，因为进程启动后无法再注入环境变量。`purge_delay` 在内置 v3
/// mimalloc 树（libmimalloc-sys 0.1.49）的枚举里位于下标 15；出厂默认 10 ms 兼作
/// 合理性校验，这样未来枚举重排时会安全失败，而不是误改无关选项。
#[cfg(not(test))]
fn tune_mimalloc_purge_delay() {
    use std::ffi::{c_int, c_long};

    unsafe extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
        fn mi_option_set(option: c_int, value: c_long);
    }

    const MI_OPTION_PURGE_DELAY: c_int = 15;
    // 头注释声称默认 10 ms，但 v3 的 options.c 表实际发的是 1000 ms。
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

/// purge-delay 覆盖是否生效；日志起来后由内存探针上报。
#[cfg(not(test))]
pub static MIMALLOC_PURGE_DELAY_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// page-reset 覆盖是否生效（见 [`tune_mimalloc_page_reset`]）。
#[cfg(not(test))]
pub static MIMALLOC_PAGE_RESET_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 让空闲页空间尽早去提交，而不是滞留在 arena 里：v3 的 `page_reset` 在每次释放时
/// 把页空间重置给 OS，`abandoned_page_purge` 在线程抛弃某页的瞬间就 purge 它。只有
/// `purge_delay = 0` 时，收集器仍只在 5 分钟一次的强制 collect 才 purge，于是两次
/// collect 之间会停着几十 MB「已提交但空闲」的 arena。内置的 v3 树不导出
/// `mi_option_get_name`，所以这两个选项只能按枚举下标寻址（v3 表里 page_reset = 10、
/// abandoned_page_purge = 11；由 `tune_mimalloc_purge_delay` 的锚点间接验证：
/// purge_delay 在下标 15、出厂默认 1000，据此钉死表布局）。故障安全：回读值不符就
/// 把旗标置 off，并由探针上报。
#[cfg(not(test))]
fn tune_mimalloc_page_reset() {
    use std::ffi::{c_int, c_long};

    unsafe extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
        fn mi_option_set(option: c_int, value: c_long);
    }

    const MI_OPTION_PAGE_RESET: c_int = 10;
    const MI_OPTION_ABANDONED_PAGE_PURGE: c_int = 11;
    // 两者出厂都是禁用；回读非零说明表布局与预期不符，改动它会误伤无关选项。
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
        // 所有 spawn_blocking 共用这个上限：扫描管线最多 15 个元数据读取
        // （normal_worker_count = cores-1，每个文件一次短 spawn_blocking）加最多
        // 8 个封面解码/缩放（cores/2，另有它自己的信号量守护）——本机峰值 23 个
        // 并发阻塞项。若设为 16，多出的提交会排在该池无界 FIFO 的队尾，于是同样
        // 需要这个池的交互工作（背景 sharp-fill、封面解码、行元数据、设置/会话
        // 写入、QR）要为剩余扫描等上最多一次封面缩放（~0.5s）。32 在扫描自身的
        // 并发上限之上留余量，又不抬高扫描并发（扫描自带封顶）。空闲阻塞线程只预留栈——
        // 不产生已提交页——所以这不会和内存上限互相拉扯。
        .max_blocking_threads(32)
        .build()
        .unwrap()
});

/// 当前进程内存，单位 MiB：`(private_committed, working_set)`。
/// `private_committed` 是*提交电荷*（任务管理器显示的那个 / OS 实际保留的量），
/// 不是常驻集；常驻页波动大得多，以往日志里的长期曲线会被它低估。
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

        // `cb` 必须在调用前置好；每个计数器都是纯输出参数，初值为零
        let mut counters = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            ..Default::default()
        };
        // SAFETY: 缓冲区大小与类型都正确；伪句柄在查询当前进程时恒有效。
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

    // SAFETY: 伪句柄恒有效；每个输出参数都是一个普通 8 字节槽位，由调用写入。
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
    // 非 Windows 未实现：探针的 `cpu_permille` 在那里恒为 0。
    0
}

// mimalloc 自己的记账，单位 MiB：`(committed, rss)`。与 `[mem]` 探针同一处采样，
// 用来把「每首歌的提交量增长」拆成两种情形："堆内持有"（mi_commit 与 private_mb
// 同步攀升——堆内部的驻留/碎片）还是"堆外在长"（private_mb 攀升而 mi_commit 持平
// —— D3D/驱动的地盘）。
// 只有 `#[cfg(not(test))]` 的内存探针调用这些函数；测试构建换成上面的计数分配器，
// 所以在测试里直接把声明关掉。
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
    // SAFETY: 所有指针都是有效输出参数；该调用是纯粹的统计读取。
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

/// 强制做一次完整 purge，并报告它换回多少 MiB 已提交堆。大量回收说明每首歌的棘轮
/// 式增长只是分配器尚未去提交的空闲页碎片（无害——这些页会被复用）；提交量在涨而
/// 回收量约 0，说明实时分配正在累积、泄漏是真的。
#[cfg(not(test))]
fn mimalloc_force_collect_reclaim_mb(before: u64) -> u64 {
    // SAFETY: 统计相邻的维护调用；按 mimalloc 文档是安全的。
    unsafe { mi_collect(true) };
    let (after, _) = mimalloc_memory_mb();
    before.saturating_sub(after)
}

/// 在一个具名的低频 UI 事件上采样并记录进程内存，好让后续 `[mem]` 的步进能归因到
/// 触发它的用户动作：单靠 30 秒探针分不清浏览和播放，而 2026-09 的日志显示浏览期
/// 出现无法归因的 +35..155 MB 步进。
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

/// 后台内存探针：每 30 秒采样一次进程内存，让长时间播放会话在日志里留下一条曲线
/// （已提交 private 字节 vs 工作集），据此区分真实泄漏与缓存/缓存容量增长。"step"
/// （滚动窗口上的净增长）会显式记录，让长期内存里每一次活动驱动的抬升都可归因，
/// 而不是静默发生。图集可观测性（与 gpui_windows 的 directx_atlas.rs 里 MELIORA
/// PERMANENT OBSERVABILITY PATCH 一节配套）：按纹理类型 [polychrome, monochrome,
/// subpixel] 各 5 个计数器——页 created、页 dropped（清空并归还给驱动）、页 live、
/// free_listed、tiles_by_key 长度。在 [mem] periodic 探针里读取，好让 glyph 页累积
/// （2026-09-27 泄漏类）在每份长会话日志中都可见。
///
/// 由 `atlas-probe` cargo feature 门控：该符号只由*本地打过补丁的* gpui-ce
/// checkout 导出。上游（因而也包括拉取原始 rev 的 CI）没有 `meliora_atlas_probe`，
/// 且 gpui_windows 在非 Windows 目标上根本不参与编译——这里的 extern 不加门控会让
/// 本机之外的所有链接失败。
#[cfg(all(target_os = "windows", feature = "atlas-probe", not(test)))]
fn atlas_probe_snapshot() -> [u64; 15] {
    // 该符号就在链接图里已有的 gpui_windows rlib 中；不加 #[link] 属性——
    // MSVC 会要求对应的 .lib 导入库。
    unsafe extern "C" {
        fn meliora_atlas_probe(out: *mut u64, len: usize) -> usize;
    }
    let mut out = [0u64; 15];
    unsafe {
        meliora_atlas_probe(out.as_mut_ptr(), out.len());
    }
    out
}

/// 用原始 CLI 参数启动一份当前可执行文件的新副本。spawn 失败时返回 `false`
/// （调用方降级继续跑）。由内存硬上限阀门（立即以哨兵退出码退出）和更新器的
/// "重启以应用"（走 gpui 退出，让 `on_app_quit` 的落盘仍然执行）共用；会话态存在
/// `playback_session.json`，由存储 worker 持续持久化，因此重启间隙约 2 s。
/// 门控范围就是它的两个调用方：`not(test)` 的内存探针阀门与 `online` 更新器
/// （restart-to-apply）。默认测试构建两者都没有，所以在该配置下把这个函数编掉，
/// 而不是触发 dead_code。
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
    use crate::sync::RecoveredMutex;
    static LAST_SILENT: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if let Some(labels) = labels.as_ref() {
        let mut silent: Vec<&'static str> = labels
            .iter()
            .filter(|(_, id)| !drawn.contains(id))
            .map(|(label, _)| *label)
            .collect();
        silent.sort_unstable();
        let mut last = LAST_SILENT.lock_recovered();
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
    // 最近 10 分钟内的净提交增长被视为一次 "step"。
    const STEP_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
    const STEP_MIN_MB: i64 = 20;
    const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    /// 看门狗的软 private-commit 预算（MiB）。按 2026-09-27 浸泡实测调校
    /// （110+ 次切歌、封面浏览、歌词+队列打开）：持续平台期 ~370-385 MB，突发峰值
    /// ~440 MB 且数分钟内回落。越过 420 MB 后的 purge 已无法用实时活动解释——那
    /// 意味着棘轮式增长（或新泄漏），所以每次采样强制 purge，并每第 4 次采样告警。
    const MEMORY_SOFT_CAP_MB: u64 = 420;
    /// 硬 private-commit 上限（MiB）。泄漏修复给每条已知增长路径都封了界，但
    /// "无论运行多久都不可能超过"需要一套对*未知*未来 bug 强制生效的机制，而且
    /// 进程内没有任何操作能回收 glyph 图集页（逐出会触发 2026-09-08 的悬空场景
    /// 崩溃类）。所以最终阀门是重启进程：连续两个 30 s 采样越线（两次采样之间
    /// 提交量至少持续 30 s 高于上限）即为持续越线——spawn 替换 exe（会话态存在
    /// playback_session.json，设置随改随存）并退出。修复后的采样间增速是个位数 MB，
    /// 因此无论何种 bug，这个界都能守住到 ~10 MB 以内。带最小存活时间守卫，防止
    /// 启动瞬态造成循环重启；两个值都可用环境变量覆盖以便测试。
    const MEMORY_HARD_CAP_MB: u64 = 500;
    const HARD_CAP_CONSECUTIVE_SAMPLES: u32 = 2;
    const HARD_CAP_MIN_UPTIME: std::time::Duration = std::time::Duration::from_secs(300);
    /// 压力旗标清除前相对软顶的滞回余量：shed 之后缓存会按需回填，只有降到
    /// cap − 30 MB 才清除，旗标才不会在边界反复抖动。
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
        // 压力 shed 的记账：连续越线采样数，以及上升沿（上一个采样还在线内）。
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
        // 首个 [frame] 行最早出现在启动后 ~90s（60s 延迟 + 30s 首
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
            // mi_process_info 的 commit 只是镜像 OS 电荷（它由
            // GetProcessMemoryInfo 回填），所以真正的 mimalloc 记账来自 stats API：
            // heap = 分配器拥有的量，non-heap = 驱动/D3D/图集/栈的提交。
            let heap_committed_mb = crate::mimalloc_stats::committed_mb().unwrap_or(mi_commit_mb);
            // 每第 10 次采样（~5 分钟）强制一次完整 purge，并记录它换回多少：
            // 大量回收 = 空闲页碎片，约 0 = 增长是实时数据、泄漏是真的。
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
            // 内存上限看门狗：越过软 private-commit 预算后，改为每次采样都 purge，
            // 不再等 5 分钟节奏，并在日志里说明。该预算高于实测稳态（歌词+队列+网格
            // 全开时 ~300 MB）、低于已知抖动漂移，所以越线意味着激进的 page-reset
            // 选项没能压住的碎片（或新泄漏）。
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
            // 压力 shed：软顶是硬阀门计数所依据的预警线（间隔 30 s 的连续两次采样）。
            // 上升沿时让出通用图像缓存——render LRU 收到窗口的一半、backdrop LRU
            // 只留当前曲目——并置起压力旗标，让沉浸页停止预取下一首（省掉一整条
            // 4096 管线）。持续越线时每第 4 次采样（~2 分钟）再 shed 一次，以覆盖
            // shed 之后的缓存回填。逐出走回收漏斗的正常年龄门（封面 60s / 大图 3s），
            // 因此驱动侧的提交缓解在分钟量级落地——正是硬阀门要求的那种持续形态：
            // 2026-10-02 那次重启的爬坡（沉浸切歌 3 分钟内 341→521 MB）会在首次越线
            // 时 shed 约 60 MB，比真正破线早两个采样。旗标只在 cap − margin 以下
            // 才清除（滞回）。
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
            // 与强制 collect 同频：在完整 purge 之后立刻读 mimalloc 自己的按尺寸
            // 分档统计，此时搁浅页处于最少。那里的较大搁浅提交是收集器无法归还的
            // 碎片；较大的 live 总量则是真正驻留的数据，靠 top bins 看是哪些尺寸。
            if tick_count.is_multiple_of(10) {
                crate::mimalloc_stats::log_snapshot(mi_collect_reclaim_mb);
            }

            let step_alert = match baseline {
                Some((at, from_mb)) if at.elapsed() >= STEP_WINDOW => {
                    let delta = private as i64 - from_mb as i64;
                    // 把窗口前移到当前采样，让持续爬坡始终以同一个参考点度量，
                    // 而不是归零重来。
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
                // 可疑增长之后立刻抓取分配器状态
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

            // 硬上限阀门：提交量持续越过硬上限就重启进程（见 MEMORY_HARD_CAP_MB）。
            // 单次采样可能是突发峰值；连续两次才说明它不会自行回落。
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
                // 只在 atlas-probe feature 打开时才会解析（本地打过补丁的 gpui
                // checkout）；其余场合用 None，既保住日志字段形状，又不碰那个缺失
                // 的 extern 符号。
                #[cfg(all(target_os = "windows", feature = "atlas-probe"))]
                let atlas_field = Some(atlas_probe_snapshot());
                #[cfg(not(all(target_os = "windows", feature = "atlas-probe")))]
                let atlas_field: Option<[u64; 15]> = None;
                // UI 线程被数据库查询停住的会话累计量。放在这里是因为它是"看不见的
                // 等待"的分母：没有它，`[db] slow` 只是零散事件，读不出总占比。
                let (ui_query_calls, ui_query_total_ms, ui_query_peak_us) =
                    crate::ui::library_access::ui_query_stats();
                tracing::info!(
                    private_mb = private,
                    working_mb = working,
                    heap_commit_mb = heap_committed_mb,
                    non_heap_mb = private.saturating_sub(heap_committed_mb),
                    cpu_permille = cpu_permille,
                    ui_query_calls,
                    ui_query_total_ms,
                    ui_query_peak_us,
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

/// 磁盘在线封面缓存的总字节数，单位 MiB。统计本会话触碰过多少张不同封面，与进程
/// 内存无关。采样节奏由调用方决定：`[mem]` 探针在低频档刷新它（每第 10 次采样，
/// 刷新点见 `spawn_memory_probe`），所以读日志的人要把它当作阶梯值，而不是逐采样
/// 的读数。
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

/// 单实例守卫：一个具名内核互斥量，在整个进程生命周期内持有。第二次启动会与运行中
/// 的实例争抢 settings.json、playback_session.json、音乐库数据库和音频设备——这些
/// 都不是为两个写入者设计的。进程崩溃时内核会回收该互斥量，所以残留锁永远不会挡住
/// 下次启动。
#[cfg(target_os = "windows")]
fn enforce_single_instance() -> anyhow::Result<()> {
    use windows::{
        Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
        Win32::System::Threading::CreateMutexW,
        Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW},
        core::w,
    };

    // restart-to-apply 的子进程会与父进程的拆解相撞：gpui 给 quit handler 最多
    // 200ms，析构函数还要再花时间，所以子进程走到这里时互斥量可能仍被持有。
    // 有界等待一会儿，而不是去激活那个垂死的父实例然后退出。
    let handoff = std::env::var_os(RESTART_HANDOFF_ENV).is_some();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mutex = unsafe { CreateMutexW(None, false, w!("Local\\MelioraSingleInstance"))? };
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            // `HANDLE` 没有 Drop 实现：丢弃这个绑定会让内核句柄保持打开，于是
            // 互斥量在整个进程生命周期内一直被持有。内核会在退出时回收它
            // （崩溃也不会留下残留锁）。
            let _ = mutex;
            return Ok(());
        }
        let _ = unsafe { CloseHandle(mutex) };
        if !handoff || std::time::Instant::now() >= deadline {
            // 已有实例：优先唤回它的窗口（托盘宿主窗口接收激活消息；窗口隐藏
            // 时这是用户唯一的"再点一次桌面图标"恢复路径）。唤回失败（目标实例
            // 可能还在启动中、托盘壳未建）才退回提示对话框。
            if !crate::tray::activate_existing_instance() {
                // 这里跑在 logging/i18n 初始化之前，所以提示用普通对话框配固定
                // 双语硬编码文本，而不是 tr! 查表。
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

    // RegisterApplicationRestart 在受策略限制/较旧的构建上可能失败；这只是一个
    // 锦上添花的 OS 集成，绝不是拒绝启动的理由。logger 还没起来，所以先把错误
    // 带在身上，等它起来再记。
    #[cfg(target_os = "windows")]
    let restart_registration_error = init_windows_restart().err();

    // 立刻关掉 GPUI mini profiler，避免不必要的分配
    set_trace_enabled(false);

    #[cfg(not(test))]
    tune_mimalloc_purge_delay();
    #[cfg(not(test))]
    tune_mimalloc_page_reset();

    // 把遗留在旧名 `li-ming1/meliora` 与 `mailliw/hummingbird` 下的数据/日志目录
    // 迁移过来，让登录态和缓存在改名后仍然存活。必须跑在 logging::init 之前，
    // 因为后续每次初始化都读这些路径；失败项先收集在这里，logger 起来后立刻记录。
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

    // 即使本次会话还没写入过任何封面，也要把在线图片缓存约束到它的 30 天年龄窗口。
    #[cfg(feature = "online_sources")]
    crate::media::http_source::prune_image_cache_background();

    crate::ui::app::run()
}
