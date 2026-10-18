//! 直接读出 mimalloc v3 的按尺寸分档聚合统计，让 `[mem]` 探针不必依赖外部 profiler
//! 就能把实时分配字节与分配器的页驻留分开：强制 purge 之后的
//! `committed - live` 就是搁浅页，而分档明细指出*是哪些分配尺寸*持有这些驻留内存。
//!
//! 下面的 `mi_stats_t` 镜像按内置 v3 树的布局钉死（libmimalloc-sys 0.1.49，
//! `MI_STAT_VERSION` 5）；该结构会自报大小与版本，所以依赖升级改动布局时 `read()`
//! 返回 `None`，而不是打出垃圾数据。

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
#[allow(dead_code)] // 全布局镜像：多数字段只为布局存在，真正读取的只有几个
struct mi_stats_t {
    size: usize,
    version: usize,
    // MI_STAT_FIELDS()，顺序与 include/mimalloc-stats.h 完全一致。
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

/// `mi_subproc_id_t`：一个抽象的、指针大小的句柄。
#[repr(C)]
#[derive(Clone, Copy)]
struct mi_subproc_id_t {
    _id: *mut c_void,
}

/// `mi_output_fun`：mimalloc 通过这个回调把统计表喂出来，碎片大小任意。
type mi_output_fun = extern "C" fn(msg: *const c_char, arg: *mut c_void);

thread_local! {
    static DUMP_LINE: RefCell<String> = const { RefCell::new(String::new()) };
}

extern "C" fn dump_out(msg: *const c_char, _arg: *mut c_void) {
    if msg.is_null() {
        return;
    }
    // SAFETY: mimalloc 传入以 NUL 结尾的字符串，仅在本次调用期间有效。
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

/// 完整统计表 dump 的每进程预算，防止浏览会话刷屏（预算耗尽后 `dump_once` 变成
/// 空操作）。
const MAX_DUMPS: u32 = 3;

/// 记录 mimalloc 的完整统计表（按堆：各类别的 peak / total / current，外加页计数）。
/// 由探针的 `mem step` 告警调用，以便在可疑增长之后立刻抓取分配器状态。每进程三次
/// dump 的预算能防止浏览会话刷屏；之后的调用是空操作。
pub fn dump_once() {
    static DUMP_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if DUMP_COUNT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < MAX_DUMPS).then_some(n + 1)
        })
        .is_err()
    {
        return;
    }
    tracing::info!("[mem] mi stats dump | --- full mimalloc stats table ---");
    // SAFETY: 纯粹的统计输出，写进我们自己的回调。subprocess 版本会打印聚合值
    // *以及*每个堆各自的表，其中含普通 mi_stats_print_out 省略的 malloc 分档明细。
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

/// 某个探针时刻上，聚合统计中可信的子集。
pub struct MemStatsSnapshot {
    /// 按分配器自身记账的已提交堆字节。
    pub committed_bytes: u64,
    pub abandoned_pages: u64,
    pub threads: u64,
    pub theaps: u64,
}

fn current(count: &mi_stat_count_t) -> u64 {
    count.current.max(0) as u64
}

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// 分配器自己的已提交堆记账，单位 MiB。`mi_process_info` 返回的是 OS 提交电荷
/// （见探针里那条注释），所以要用进程 private 字节减去它才能得到驱动/D3D 的份额。
/// stats 调用或钉死布局的镜像失败时返回 `None`。
pub fn committed_mb() -> Option<u64> {
    read().map(|snapshot| snapshot.committed_bytes / BYTES_PER_MIB)
}

/// 守护 `log_snapshot` 里那条只告警一次的失败提示。
static READ_FAILURE_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 读取聚合统计；调用失败，或 C 结构已与钉死的镜像不再匹配（依赖升级）时返回
/// `None`。只读出链接进来的构建确实维护的字段：malloc 的逐档 current 仅在
/// `MI_STAT > 1` 下才聚合，所以这里恒为零，live 字节的归因要靠 dump。
pub fn read() -> Option<MemStatsSnapshot> {
    // 用归零的堆缓冲区而不是栈上值：这个结构约 4 KiB，而调用方的栈里含音频线程的。
    let layout = Layout::new::<mi_stats_t>();
    let raw = unsafe { alloc_zeroed(layout) };
    if raw.is_null() {
        return None;
    }
    let stats = raw as *mut mi_stats_t;
    // v3 API 约定：调用方先填好目标头部（size + version，即 C 侧 mi_stats_init 做的
    // 事）；mi_stats_copy 会拿它与链接进来的构建校验，不匹配就先拒绝再拷贝。
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
    // SAFETY: 上面用同一布局分配，且别处从不释放。
    unsafe { dealloc(raw, layout) };
    result
}

/// 记录一行 `[mem] mi stats`，只给聚合口径：`committed_mb` / `abandoned_pages` /
/// `threads` / `theaps` / `collect_reclaim_mb`。刻意设计成紧跟探针的强制
/// `mi_collect` 之后运行——那时搁浅页处于最少，此刻 `abandoned_pages` 仍高才说明
/// 有线程抛弃的页没被收走。逐档 current 给不出，所以本行**不**含 live/stranded
/// 的归因；那份数据在探针 step 告警的完整 dump（`dump_once`）里。
pub fn log_snapshot(collect_reclaim_mb: Option<u64>) {
    let Some(snapshot) = read() else {
        // 只告警一次并带上实测到的头部值，这样依赖升级导致的镜像/布局漂移会在日志
        // 里可见，而不是静默丢掉整条 `[mem] mi stats` 序列。
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
    let mb = |bytes: u64| bytes / BYTES_PER_MIB;
    // 这里只信聚合的 `committed` / 线程计数：malloc 的逐档 current 不由链接进来的
    // 构建维护（MI_STAT==1 会关掉它们的聚合），所以 live/stranded 的归因改由
    // step 告警的 dump 提供。
    tracing::info!(
        committed_mb = mb(snapshot.committed_bytes),
        abandoned_pages = snapshot.abandoned_pages,
        threads = snapshot.threads,
        theaps = snapshot.theaps,
        collect_reclaim_mb = collect_reclaim_mb.unwrap_or(0),
        "[mem] mi stats"
    );
}
