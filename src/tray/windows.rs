//! Windows 托盘壳：`Shell_NotifyIconW` + message-only 窗口。
//!
//! 为什么是 message-only 窗口（`HWND_MESSAGE` 父）：托盘回调消息需要一个
//! WndProc 接收端，但它不属于任何可见窗口、不进任务栏、不参与 gpui 的
//! 窗口管理。主线程创建，gpui 的 GetMessage 泵顺带派发它的消息
//! （`DispatchMessage` 对 message-only 窗口与普通窗口一视同仁），因此
//! **不需要给 gpui 打任何补丁**。它同时充当第二次启动的激活消息靶点
//! （`FindWindowW` 按类名定位，隐藏窗口也可查找）。
//!
//! 图标丢失的自愈**不**依赖 `TaskbarCreated`：message-only 窗口收不到广播
//! 消息（MSDN「Window Features → Message-Only Windows」原文
//! *does not receive broadcast messages*），explorer 重启发的那条广播到不了
//! 本进程。检测改为自己发现，见 `ShellRecovery`。

use crate::sync::RecoveredMutex;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
};
use std::time::{Duration, Instant};

use windows::{
    Win32::{
        Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW,
                DefWindowProcW, DestroyMenu, DestroyWindow, FindWindowW, GetCursorPos,
                GetSystemMetrics, GetWindowThreadProcessId, HICON, HMENU, HWND_MESSAGE, IMAGE_ICON,
                IsWindowVisible, LR_SHARED, LoadImageW, MF_SEPARATOR, MF_STRING, PostMessageW,
                RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SW_HIDE,
                SW_RESTORE, SetForegroundWindow, ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                TrackPopupMenu, WINDOW_EX_STYLE, WM_DESTROY, WM_LBUTTONUP, WM_MBUTTONUP, WM_NULL,
                WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
            },
        },
    },
    core::PCWSTR,
    core::w,
};

use super::TrayCommand;
use cntp_i18n::tr;

/// 壳 → 应用命令的发送端（唯一托盘实例，进程级静态）。
static TRAY_TX: OnceLock<Sender<TrayCommand>> = OnceLock::new();

/// 主窗口 HWND（供菜单标签判断显隐）。显隐/轮询命令经 gpui 路径刷新。
static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);

/// explorer 重启时广播的 `TaskbarCreated` 消息 id。message-only 窗口收不到广播，
/// 登记它只是窗口类型若变更时的保险。
static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

/// 最近一次 tooltip 文本。壳侧图标丢失期间 UI 侧不会再有成功的 notify，而
/// 重注册的 `NIM_ADD` 与心跳探针都要发文本——若回落应用名，tooltip 就会一直
/// 错到下一次切歌；这里让两者都带上最新内容。
static LAST_TOOLTIP: Mutex<Option<String>> = Mutex::new(None);

/// 图标丢失后的自愈状态。**不**依赖 `TaskbarCreated`：message-only 窗口收不到
/// 广播消息（模块文档引了 MSDN 原文），`tray_wndproc` 里保留的那个分支只是
/// 「窗口类型若改成顶层隐藏窗」时的保险。
///
/// 实测事故（2026-10-19 从轮转日志取证）：一次 `NIM_MODIFY` 在跨进程 IPC 里阻塞
/// 7.06s 后返回 FALSE / `E_FAIL`，此后 107 次刷新全在 0.3ms 内被拒（注册已不在
/// 壳的表里），从 3474s 一路到 20110s（4.6h）直到进程结束，图标再无出现。形态与
/// explorer／通知区重建一致，但日志里没有那条重启事件本身——所以自愈不能指望
/// 知道原因，只能认「这次 IPC 没被接受」。
#[derive(Debug)]
struct ShellRecovery {
    /// `Some` = 注册已丢，值为下一次 `NIM_ADD` 的时刻；`None` = 注册在位。
    retry_at: Option<Instant>,
    attempts: u32,
    /// 上一次心跳探针的时刻；`None` = 尚未探过（首次服务即探一次）。
    last_probe: Option<Instant>,
}

impl ShellRecovery {
    const INIT: Self = Self {
        retry_at: None,
        attempts: 0,
        last_probe: None,
    };

    /// 回到空闲：不再重注册，但心跳探针继续盯着（下一次 `NIM_MODIFY` 被拒会
    /// 重新挂起）。`now` 记为最近一次探针时刻——刚发过一次成功的 IPC。
    fn idle_at(now: Instant) -> Self {
        Self {
            last_probe: Some(now),
            ..Self::INIT
        }
    }
}

static RECOVERY: Mutex<ShellRecovery> = Mutex::new(ShellRecovery::INIT);

/// 心跳探针间隔：丢失后最迟 30s 发现，覆盖「丢失后一直没人切歌」的情形。
/// 一次 `NIM_MODIFY` 的开销与每次切歌的刷新同量级，30s 一档可忽略。
const SHELL_PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// 首次重注册延迟。**不是实测值**——我们没量过壳多久才开始接受 `NIM_ADD`，这一
/// 档只是「别在同一瞬间连环重发」的下限。实测的是那次事故里「一次都不重试」的
/// 代价（4.6h 无图标），所以这里的纪律是：一定要重试，但退避有界（见
/// `REREGISTER_MAX_ATTEMPTS`）。
const REREGISTER_FIRST_DELAY: Duration = Duration::from_millis(200);
const REREGISTER_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// 放弃本轮自愈的尝试次数。200ms 起步、倍增、10s 封顶，60 次累计实测 562s
/// （约 9.4 分钟）——必须明显长于 explorer 重启的实际耗时，否则自愈会在壳回来
/// 之前就停手。放弃是安全的：心跳探针仍在跑，下一次 `NIM_MODIFY` 被拒会重新挂起。
const REREGISTER_MAX_ATTEMPTS: u32 = 60;

/// 第 `attempts` 次失败后再等待多久重试。纯函数，便于单测。
fn reregister_delay(attempts: u32) -> Duration {
    (REREGISTER_FIRST_DELAY * (1u32 << attempts.min(6))).min(REREGISTER_BACKOFF_MAX)
}

fn tooltip_cache_text() -> String {
    LAST_TOOLTIP
        .lock_recovered()
        .clone()
        .unwrap_or_else(super::app_name_label)
}

fn recovery_pending() -> bool {
    RECOVERY.lock_recovered().retry_at.is_some()
}

/// 把图标标记为「已丢、待重注册」。幂等：已在挂起中就不动退避时刻，否则每次
/// 刷新都会把首次延迟往后推。
fn arm_reregister() {
    let mut state = RECOVERY.lock_recovered();
    if state.retry_at.is_none() {
        state.attempts = 0;
        state.retry_at = Some(Instant::now() + REREGISTER_FIRST_DELAY);
        tracing::warn!("tray shell: icon registration lost, re-registering");
    }
}

const TRAY_CLASS: windows::core::PCWSTR = w!("MelioraTrayHost");
const TRAY_ID: u32 = 1;
/// 托盘回调消息（`NOTIFYICONDATAW::uCallbackMessage`）与二次启动激活消息。
const WM_APP_CALLBACK: u32 = 0x8000 + 1;
const WM_APP_ACTIVATE: u32 = 0x8000 + 2;

const IDM_PLAY_PAUSE: usize = 1001;
const IDM_PREVIOUS: usize = 1002;
const IDM_NEXT: usize = 1003;
const IDM_TOGGLE: usize = 1004;
const IDM_QUIT: usize = 1005;
const IDM_SETTINGS: usize = 1006;

/// 托盘壳句柄。Drop 时摘除图标并销毁 message-only 窗口。
pub(super) struct WindowsTray {
    hwnd: HWND,
    /// ADD 时的图标句柄（`LR_SHARED`，无需销毁）。NIM_MODIFY 重发完整
    /// flags 时需要它——只发 NIF_TIP 在部分 shell 版本上不可靠。
    icon: Option<HICON>,
}

impl WindowsTray {
    /// 与 ADD 同一套完整 flags（参考实现的 GetData() 模式）：只发 NIF_TIP 时
    /// 部分 shell 版本不会更新 infotip。
    fn modify(&self, text: &str) -> bool {
        unsafe {
            notify(
                "modify",
                self.hwnd,
                NIM_MODIFY,
                NIF_ICON | NIF_MESSAGE | NIF_TIP,
                Some(text),
                self.icon,
            )
        }
    }

    pub(super) fn set_tooltip(&self, text: &str) {
        // 先记文本再走 IPC：挂起期间这次 MODIFY 必然被拒，缓存下来由重注册的
        // `NIM_ADD` 代发（也免掉被拒日志刷屏）。
        *LAST_TOOLTIP.lock_recovered() = Some(text.to_owned());
        if recovery_pending() {
            return;
        }
        if !self.modify(text) {
            arm_reregister();
        }
    }

    /// 壳侧自愈，由 gpui drain 任务每 100ms 调一次。与 `set_tooltip` 同为主
    /// 线程，`RECOVERY` 只是跨调用点的状态盒，没有竞争。
    ///
    /// 两条职责：挂起中按退避阶梯重发 `NIM_ADD`；注册在位时每
    /// `SHELL_PROBE_INTERVAL` 发一次内容不变的 `NIM_MODIFY` 作心跳探针——它唯一
    /// 的用途是拿回「壳还认不认这个注册」这个答复。
    pub(super) fn service_shell_recovery(&self) {
        let now = Instant::now();
        let mut state = RECOVERY.lock_recovered();
        if let Some(at) = state.retry_at {
            if now < at {
                return;
            }
            state.attempts += 1;
            let attempts = state.attempts;
            // 图标句柄也重取：旧 explorer 的共享句柄不再可信。
            let added = unsafe {
                notify(
                    "add",
                    self.hwnd,
                    NIM_ADD,
                    NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    Some(&tooltip_cache_text()),
                    load_icon(),
                )
            };
            if added {
                tracing::warn!(attempts, "tray shell: icon re-registered");
                *state = ShellRecovery::idle_at(now);
            } else if attempts >= REREGISTER_MAX_ATTEMPTS {
                tracing::error!(
                    attempts,
                    "tray shell: re-registration gave up, heartbeat keeps watching"
                );
                *state = ShellRecovery::idle_at(now);
            } else {
                state.retry_at = Some(now + reregister_delay(attempts));
            }
            return;
        }
        if state
            .last_probe
            .is_some_and(|at| now - at < SHELL_PROBE_INTERVAL)
        {
            return;
        }
        state.last_probe = Some(now);
        // 探针失败要 arm_reregister，必须先放锁——std Mutex 不可重入。
        drop(state);
        if !self.modify(&tooltip_cache_text()) {
            arm_reregister();
        }
    }
}

impl Drop for WindowsTray {
    fn drop(&mut self) {
        unsafe {
            notify(
                "delete",
                self.hwnd,
                NIM_DELETE,
                NOTIFY_ICON_DATA_FLAGS(0),
                None,
                None,
            );
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

pub(super) fn init(tx: Sender<TrayCommand>) -> Option<WindowsTray> {
    let _ = TRAY_TX.set(tx);
    let hinstance = HINSTANCE(unsafe { GetModuleHandleW(PCWSTR::null()).ok()?.0 });

    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: hinstance,
        lpszClassName: TRAY_CLASS,
        ..Default::default()
    };
    // atom == 0 也可能是类已注册（重复 init 的防御），不视为失败——
    // CreateWindowExW 对已注册类照样可用。
    unsafe { RegisterClassW(&wnd_class) };

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            TRAY_CLASS,
            PCWSTR::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
    }
    .ok()?;
    // 0 = 注册失败。必须挡掉：`tray_wndproc` 用「消息号 == 存储值」匹配，而
    // `open_menu` 会自发 `WM_NULL`(=0)，两者一旦相等每次右键菜单收尾都会误触发
    // 一次重注册。
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_created != 0 {
        let _ = TASKBAR_CREATED_MSG.set(taskbar_created);
    }

    let icon = unsafe { load_icon() };
    let added = unsafe {
        notify(
            "add",
            hwnd,
            NIM_ADD,
            NIF_ICON | NIF_MESSAGE | NIF_TIP,
            Some(&super::app_name_label()),
            icon,
        )
    };
    if !added {
        // 壳拒绝（explorer 未就绪等）：销毁窗口，返回 None 降级。
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return None;
    }
    // 不调 NIM_SETVERSION：v0（经典行为）的 tooltip 路径最可靠，
    // 而我们用不到 v4 的焦点语义。
    Some(WindowsTray { hwnd, icon })
}

/// 记录主窗口句柄（菜单标签的显隐判断用；不改变任何行为）。
pub(super) fn note_main_window(window: &gpui::Window) {
    if let Some(hwnd) = window_hwnd(window) {
        MAIN_HWND.store(hwnd.0 as usize, Ordering::Relaxed);
    }
}

pub(super) fn show_window(window: &mut gpui::Window) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    MAIN_HWND.store(hwnd.0 as usize, Ordering::Relaxed);
    unsafe {
        // SW_RESTORE 同时覆盖"隐藏中"与"最小化中"两种状态。
        let _ = ShowWindow(hwnd, SW_RESTORE);
        // 托盘点击时本进程就是前台进程，SetForegroundWindow 不受前台锁约束。
        let _ = SetForegroundWindow(hwnd);
    }
}

pub(super) fn hide_window(window: &mut gpui::Window) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    MAIN_HWND.store(hwnd.0 as usize, Ordering::Relaxed);
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub(super) fn window_visible(window: &mut gpui::Window) -> bool {
    let Some(hwnd) = window_hwnd(window) else {
        return false;
    };
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

/// 第二次启动：定位运行中实例的 message-only 窗口并投递激活消息。
/// 前台锁处理：本进程（用户刚启动、是前台）授权目标进程设置前台。
pub(super) fn activate_existing_instance() -> bool {
    unsafe {
        let Ok(hwnd) = FindWindowW(TRAY_CLASS, PCWSTR::null()) else {
            return false;
        };
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        PostMessageW(Some(hwnd), WM_APP_ACTIVATE, WPARAM(0), LPARAM(0)).is_ok()
    }
}

fn window_hwnd(window: &gpui::Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // UFCS 显式选 trait 方法：gpui::Window 另有同名 inherent 方法
    // （返回 AnyWindowHandle），直接点调用会解析到它。
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    Some(HWND(win32.hwnd.get() as *mut _))
}

unsafe fn load_icon() -> Option<HICON> {
    let hinstance = HINSTANCE(unsafe { GetModuleHandleW(PCWSTR::null()).ok()?.0 });
    let cx = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let cy = unsafe { GetSystemMetrics(SM_CYSMICON) };
    // 资源 id 1 = assets/icon.rc 的 `1 ICON "app.ico"`（embed-resource 已
    // 把它链进 exe）。LR_SHARED：共享图标无需 DestroyIcon。
    //
    // `PCWSTR(1 as *const u16)` 是 MAKEINTRESOURCE(1)：指针位置承载的是
    // 整数资源 id 而非地址，windows-rsc文档里的标准写法。
    // clippy::manual_dangling_ptr 在此为误报（建议的
    // `std::ptr::dangling::<u16>()` 语义是"对齐地址 2"，会传错 id）。
    #[allow(clippy::manual_dangling_ptr)]
    let icon = unsafe {
        LoadImageW(
            Some(hinstance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            cx,
            cy,
            LR_SHARED,
        )
    }
    .ok()?;
    Some(HICON(icon.0))
}

/// `Shell_NotifyIconW` 薄封装：统一填公共字段，按需写 tip / icon。
///
/// `kind` 只进日志。之前没有它，事故取证时无法区分「`NIM_ADD` 被拒」与
/// 「`NIM_MODIFY` 被拒」，只能靠 `tip_len` 与相邻日志反推（见 `ShellRecovery`）。
unsafe fn notify(
    kind: &'static str,
    hwnd: HWND,
    message: NOTIFY_ICON_MESSAGE,
    flags: NOTIFY_ICON_DATA_FLAGS,
    tip: Option<&str>,
    icon: Option<HICON>,
) -> bool {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: flags,
        uCallbackMessage: WM_APP_CALLBACK,
        ..Default::default()
    };
    if let Some(icon) = icon {
        nid.hIcon = icon;
    }
    if let Some(tip) = tip {
        write_tip(&mut nid.szTip, tip);
    }
    let ok = unsafe { Shell_NotifyIconW(message, &nid).as_bool() };
    // 诊断：tooltip 不显示的排查靠这几行（kind/flags/tip 长度 + 结果）。
    // meliora.log 里 grep "tray shell"。失败意味着这次 IPC 没被壳接受（最常见
    // 是注册已不在），必须到 warn 才不会被淹没在每次切歌的刷新日志中。
    if !ok {
        let err = unsafe { GetLastError() };
        tracing::warn!(
            target: "tray",
            ?err,
            kind,
            flags = flags.0,
            tip_len = tip.map(str::len),
            "tray shell: Shell_NotifyIconW failed"
        );
    } else {
        tracing::debug!(
            target: "tray",
            kind,
            tip_len = tip.map(str::len),
            "tray shell: Shell_NotifyIconW ok"
        );
    }
    ok
}

fn write_tip(buf: &mut [u16; 128], text: &str) {
    let mut count = 0;
    for unit in text.encode_utf16().take(127) {
        buf[count] = unit;
        count += 1;
    }
    buf[count] = 0;
}

fn send(command: TrayCommand) {
    if let Some(tx) = TRAY_TX.get() {
        let _ = tx.send(command);
    }
}

/// message-only 窗口的 WndProc（主线程）。
unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_CALLBACK => {
                // lparam 的低位字是鼠标事件。
                match lparam.0 as u32 & 0xFFFF {
                    x if x == WM_LBUTTONUP => send(TrayCommand::ToggleWindow),
                    x if x == WM_MBUTTONUP => send(TrayCommand::PlayPause),
                    x if x == WM_RBUTTONUP => open_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            // explorer 重启的广播。message-only 窗口收不到它（模块文档引了 MSDN
            // 原文），所以这条只是保险：窗口类型若改成顶层隐藏窗它会立即生效。
            // 真正的检测在 `service_shell_recovery` 的心跳探针里。
            m if Some(m) == TASKBAR_CREATED_MSG.get().copied() => {
                arm_reregister();
                LRESULT(0)
            }
            // 第二次启动的唤回请求。
            WM_APP_ACTIVATE => {
                send(TrayCommand::ShowWindow);
                LRESULT(0)
            }
            WM_DESTROY => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 右键菜单：`TPM_RETURNCMD` 同步返回选中项 id（免去 WM_COMMAND 与消息
/// 循环时序问题），配合前台舞蹈保证菜单点击窗外能正确消失。
unsafe fn open_menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    append_item(menu, &super::play_pause_label(), IDM_PLAY_PAUSE);
    append_item(menu, &super::previous_label(), IDM_PREVIOUS);
    append_item(menu, &super::next_label(), IDM_NEXT);
    let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) };
    let toggle_label = if main_window_visible() {
        tr!("TRAY_HIDE", "Hide Window").to_string()
    } else {
        tr!("TRAY_SHOW", "Show Window").to_string()
    };
    append_item(menu, &toggle_label, IDM_TOGGLE);
    let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) };
    append_item(menu, &super::settings_label(), IDM_SETTINGS);
    append_item(menu, &super::quit_label(), IDM_QUIT);

    let mut point = POINT::default();
    let _ = unsafe { GetCursorPos(&mut point) };
    // TrackPopupMenu 要求 owner 窗口暂时成为前台，否则菜单不消失/不吃点击；
    // 收尾的 WM_NULL 是配套的标准动作。
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
    };
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
    let _ = unsafe { DestroyMenu(menu) };

    match chosen.0 as usize {
        IDM_PLAY_PAUSE => send(TrayCommand::PlayPause),
        IDM_PREVIOUS => send(TrayCommand::Previous),
        IDM_NEXT => send(TrayCommand::Next),
        IDM_TOGGLE => send(TrayCommand::ToggleWindow),
        IDM_SETTINGS => send(TrayCommand::OpenSettings),
        IDM_QUIT => send(TrayCommand::Quit),
        _ => {}
    }
}

fn append_item(menu: HMENU, text: &str, id: usize) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = unsafe { AppendMenuW(menu, MF_STRING, id, PCWSTR(wide.as_ptr())) };
}

fn main_window_visible() -> bool {
    let raw = MAIN_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }
    unsafe { IsWindowVisible(HWND(raw as *mut _)).as_bool() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避阶梯：200ms 起步、倍增、10s 封顶；放弃前累计 562.4s（约 9.4 分钟），
    /// 必须明显长于 explorer 重启的实际耗时，否则自愈会在壳回来之前就停手。
    #[test]
    fn reregister_backoff_ladder_is_bounded() {
        assert_eq!(reregister_delay(0), Duration::from_millis(200));
        assert_eq!(reregister_delay(1), Duration::from_millis(400));
        assert_eq!(reregister_delay(5), Duration::from_millis(6400));
        assert_eq!(reregister_delay(6), REREGISTER_BACKOFF_MAX);
        assert_eq!(reregister_delay(u32::MAX), REREGISTER_BACKOFF_MAX);

        let total: Duration = (1..=REREGISTER_MAX_ATTEMPTS)
            .map(reregister_delay)
            .fold(Duration::ZERO, |acc, delay| acc + delay);
        assert_eq!(total.as_millis(), 562_400);
    }
}
