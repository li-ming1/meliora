//! Windows 托盘壳：`Shell_NotifyIconW` + message-only 窗口。
//!
//! 为什么是 message-only 窗口（`HWND_MESSAGE` 父）：托盘回调消息需要一个
//! WndProc 接收端，但它不属于任何可见窗口、不进任务栏、不参与 gpui 的
//! 窗口管理。主线程创建，gpui 的 GetMessage 泵顺带派发它的消息
//! （`DispatchMessage` 对 message-only 窗口与普通窗口一视同仁），因此
//! **不需要给 gpui 打任何补丁**。它同时充当第二次启动的激活消息靶点
//! （`FindWindowW` 按类名定位，隐藏窗口也可查找）。
//!
//! explorer 崩溃重启后托盘图标会消失：注册 `TaskbarCreated` 消息，
//! 收到时重新 NIM_ADD。

use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
};

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

/// explorer 重启后发来的 `TaskbarCreated` 注册消息 id。
static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

/// 最近一次 tooltip 文本。explorer 重启时 UI 侧不会再有 notify，重注册若
/// 只会回落应用名，tooltip 就会一直错到下一次切歌；这里让 NIM_ADD 带上
/// 重启前的内容。
static LAST_TOOLTIP: Mutex<Option<String>> = Mutex::new(None);

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
    pub(super) fn set_tooltip(&self, text: &str) {
        // 先记下文本再走 IPC：失败无所谓（explorer 重启中），
        // TaskbarCreated 重注册会用这个缓存补上。
        *LAST_TOOLTIP.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_owned());
        unsafe {
            notify(
                self.hwnd,
                NIM_MODIFY,
                // 与 ADD 同一套完整 flags（参考实现的 GetData() 模式）：
                // 只发 NIF_TIP 时部分 shell 版本不会更新 infotip。
                NIF_ICON | NIF_MESSAGE | NIF_TIP,
                Some(text),
                self.icon,
            );
        }
    }
}

impl Drop for WindowsTray {
    fn drop(&mut self) {
        unsafe {
            notify(self.hwnd, NIM_DELETE, NOTIFY_ICON_DATA_FLAGS(0), None, None);
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
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    let _ = TASKBAR_CREATED_MSG.set(taskbar_created);

    let icon = unsafe { load_icon() };
    let added = unsafe {
        notify(
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
unsafe fn notify(
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
    // 诊断：tooltip 不显示的排查靠这几行（message/flags/tip 长度 + 结果）。
    // meliora.log 里 grep "tray shell"。
    if !ok {
        let err = unsafe { GetLastError() };
        tracing::info!(
            target: "tray",
            ?err,
            tip_len = tip.map(str::len),
            "tray shell: Shell_NotifyIconW failed"
        );
    } else {
        tracing::debug!(
            target: "tray",
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
            // explorer 崩溃重启：重新注册图标。
            m if Some(m) == TASKBAR_CREATED_MSG.get().copied() => {
                let icon = load_icon();
                let tip = LAST_TOOLTIP
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(super::app_name_label);
                notify(
                    hwnd,
                    NIM_ADD,
                    NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    Some(tip.as_str()),
                    icon,
                );
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
