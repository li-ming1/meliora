//! 系统托盘：Windows（Shell_NotifyIcon + message-only 窗口）与 macOS
//! （NSStatusItem + NSMenu）。Linux 刻意不做——GNOME 默认不显示托盘图标
//! （3.26 起移除系统托盘），该平台用户以 MPRIS 驱动播放
//! （`controllers::mpris` 已覆盖），且单人开发无法验证 KDE/GNOME/XFCE ×
//! Wayland/X11 的矩阵。
//!
//! # 设计
//!
//! - **平台壳与窗口显隐分离**：平台模块只拥有"图标 + 菜单 + tooltip"的壳
//!   资源；主窗口显隐由本模块经 gpui 窗口句柄取原生句柄完成
//!   （raw-window-handle 模式同 `ui::desktop_lyrics::disable_os_window_chrome`，
//!   **无需给 gpui 打补丁**）。
//! - **关闭拦截**：主窗口安装 gpui 的 `on_window_should_close`，回调永远返回
//!   false——关闭语义由本模块按"关闭到托盘"设置决定（隐藏或退出），✕ /
//!   Alt+F4 / 托盘菜单退出全部收敛到同一条决策路径。
//! - **命令编组**：壳回调（Win32 WndProc / NSMenu action）拿不到
//!   `&mut App`，统一经 std mpsc 推给 GPUI drain 任务（100ms 轮询，菜单
//!   点击无体感延迟；WndProc 与 menu action 本就运行在主线程，轮询只为
//!   拿到 cx）。
//! - **壳创建失败只降级不 panic**：release 是 panic=abort，不能为一个增强
//!   功能带走播放；失败时 `TRAY_AVAILABLE` 保持 false，关闭决策退化为退出。

use std::{
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Sender, channel},
    },
    time::Duration,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use cntp_i18n::tr;
use gpui::{App, Entity, Window};

use crate::{
    media::metadata::Metadata,
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    settings::SettingsGlobal,
    ui::models::{CurrentTrack, Models, PlaybackInfo},
};

/// 平台壳（图标/菜单/tooltip）事件 → 应用侧命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// 左键单击托盘图标 / 菜单的显隐项。
    ToggleWindow,
    /// 第二次启动实例发来的唤回请求（Windows）。
    ShowWindow,
    /// 打开设置窗口（托盘是窗口隐藏后的唯一入口，菜单里必须能到达设置）。
    OpenSettings,
    Quit,
    PlayPause,
    Next,
    Previous,
}

/// 两平台共用的菜单标签。集中在此保证 i18n 生成器的"单一定义点"约束
/// （同一 tr! 键只允许在一个源码位置定义）；平台专属标签
/// （TRAY_SHOW/TRAY_HIDE、TRAY_TOGGLE）留在各自平台模块。
pub(crate) fn play_pause_label() -> String {
    tr!("TRAY_PLAY_PAUSE", "Play/Pause").to_string()
}

pub(crate) fn previous_label() -> String {
    tr!("TRAY_PREVIOUS", "Previous").to_string()
}

pub(crate) fn next_label() -> String {
    tr!("TRAY_NEXT", "Next").to_string()
}

pub(crate) fn quit_label() -> String {
    tr!("TRAY_QUIT", "Quit").to_string()
}

pub(crate) fn settings_label() -> String {
    tr!("TRAY_SETTINGS", "Settings…").to_string()
}

pub(crate) fn app_name_label() -> String {
    tr!("APP_NAME").to_string()
}

/// 平台壳资源。`None` = 壳创建失败或平台不支持。
enum PlatformTray {
    #[cfg(windows)]
    Windows(windows::WindowsTray),
    #[cfg(target_os = "macos")]
    MacOs(macos::MacOsTray),
    None,
}

impl PlatformTray {
    fn set_tooltip(&self, text: &str) {
        match self {
            #[cfg(windows)]
            Self::Windows(tray) => tray.set_tooltip(text),
            #[cfg(target_os = "macos")]
            Self::MacOs(tray) => tray.set_tooltip(text),
            Self::None => {}
        }
    }

    /// 记录主窗口句柄（Windows 菜单标签的显隐判断用；macOS 无此需求）。
    fn note_window(&self, window: &Window) {
        match self {
            #[cfg(windows)]
            Self::Windows(_) => windows::note_main_window(window),
            #[cfg(target_os = "macos")]
            Self::MacOs(_) => {}
            Self::None => {}
        }
    }
}

/// 托盘壳是否可用（init 成功置位）。关闭决策以此为前置：壳不在时
/// "关闭到托盘"必须退化为退出，否则用户将失去唤回窗口的入口。
static TRAY_AVAILABLE: AtomicBool = AtomicBool::new(false);

fn tray_available() -> bool {
    TRAY_AVAILABLE.load(Ordering::Relaxed)
}

fn close_to_tray(cx: &App) -> bool {
    cx.global::<SettingsGlobal>()
        .model
        .read(cx)
        .interface
        .close_to_tray
}

/// 创建托盘并接线。调用点：`ui::app::run` 主窗口建起之后（需要 `Models` /
/// `PlaybackInterface` 全局就位）。`start_minimized` 设置开启时直接以隐藏
/// 状态启动（创建后即藏，不产生可见闪烁帧之外的中间态成本）。
pub fn init(cx: &mut App) {
    let (tx, rx) = channel();
    let platform = Rc::new(platform_init(tx));
    if matches!(platform.as_ref(), PlatformTray::None) {
        tracing::warn!("tray: shell unavailable, running without tray icon");
        return;
    }
    TRAY_AVAILABLE.store(true, Ordering::Relaxed);

    // 登记主窗口句柄（Windows 菜单标签的显隐判断用）。
    if let Some(handle) = crate::ui::app::find_main_window(cx) {
        let platform = Rc::clone(&platform);
        let _ = handle.update(cx, |_, window, _| platform.note_window(window));
    }

    // 启动即入托盘。
    if close_to_tray(cx)
        && cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .start_minimized
    {
        hide_main_window(cx);
    }

    // tooltip：曲目或元数据变化时刷新为「曲名 — 艺术家」，无曲目时回落应用
    // 名。两个实体都观察——播放桥先推 current_track 再推 metadata，只观察
    // 一个会漏中间态（切歌瞬间显示上一首）。
    let metadata = cx.global::<Models>().metadata.clone();
    let current_track = cx.global::<PlaybackInfo>().current_track.clone();
    // 观察目标用独立 clone：refresh 闭包会移动原实体。
    let metadata_observer = metadata.clone();
    let current_track_observer = current_track.clone();
    let refresh = {
        let platform = Rc::clone(&platform);
        // 内容守卫：metadata 与 current_track 两个观察者会在同一次切歌里连着
        // 触发，播放桥的 MetadataUpdate 也没有相等守卫，实测 86 分钟会话里
        // 133 行日志（占全日志 16%）+ 等量 NIM_MODIFY IPC 中有相当部分是
        // 同文本重放。文本没变就既不刷壳也不记日志。
        let last_text = std::cell::RefCell::new(String::new());
        Rc::new(move |cx: &mut App| {
            let text = tray_tooltip(cx, &metadata, &current_track);
            if text == *last_text.borrow() {
                return;
            }
            *last_text.borrow_mut() = text.clone();
            // 诊断：tooltip 不显示时区分"文本就是空"与"API 调用失败"
            // （后者在 tray/windows.rs 的 notify() 里有 GetLastError 日志）。
            tracing::info!(target: "tray", text_len = text.len(), "tray tooltip refresh");
            platform.set_tooltip(&text);
        })
    };
    {
        let refresh = Rc::clone(&refresh);
        cx.observe(&metadata_observer, move |_, cx| refresh(cx))
            .detach();
    }
    {
        let refresh = Rc::clone(&refresh);
        cx.observe(&current_track_observer, move |_, cx| refresh(cx))
            .detach();
    }
    refresh(cx);

    // drain：壳事件 → 需要 cx 的处理。100ms 轮询；不能阻塞 recv——主线程
    // 卡住就是掉帧。
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            while let Ok(command) = rx.try_recv() {
                cx.update(|cx| handle_command(command, cx));
            }
        }
    })
    .detach();
}

/// 关闭请求的统一决策（✕ 按钮与 OS 关闭都走这里）。返回 `false` = 吞掉
/// 本次关闭（`on_window_should_close` 语义：阻止默认处理；显隐或 quit 都
/// 由我们自己做）。
///
/// `window` 是触发关闭的那个窗口，**必须直接用它显隐**：关闭流程进行中
/// 再去 `find_main_window` + `entity.update` 不可靠（窗口可能已被平台移出
/// `cx.windows()`，查找返回 None → 隐藏静默失效，表现为"点关闭没反应"）。
pub fn handle_close_request(window: &mut Window, cx: &mut App) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    if tray_available() && close_to_tray(cx) {
        platform_hide_window(window);
        note_platform_window(window);
        return false;
    }
    cx.quit();
    false
}

/// ✕ 按钮路径（不经过 OS WM_CLOSE）：与 [`handle_close_request`] 同一决策。
pub fn request_close(window: &mut Window, cx: &mut App) {
    handle_close_request(window, cx);
}

/// 第二次启动时唤回已在运行的实例。返回 false = 没找到/发送失败（调用方
/// 保留原提示）。仅 Windows 实现（单实例守护本身也是 Windows-only）。
pub fn activate_existing_instance() -> bool {
    #[cfg(windows)]
    {
        windows::activate_existing_instance()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn handle_command(command: TrayCommand, cx: &mut App) {
    match command {
        #[cfg(any(windows, target_os = "macos"))]
        TrayCommand::ToggleWindow => toggle_main_window(cx),
        #[cfg(any(windows, target_os = "macos"))]
        TrayCommand::ShowWindow => show_main_window(cx),
        TrayCommand::Quit => cx.quit(),
        TrayCommand::OpenSettings => crate::ui::settings::open_settings_window(cx),
        TrayCommand::PlayPause => {
            let interface = cx.global::<PlaybackInterface>();
            match cx.global::<PlaybackInfo>().playback_state.read(cx) {
                PlaybackState::Playing => interface.pause(),
                _ => interface.play(),
            }
        }
        TrayCommand::Next => cx.global::<PlaybackInterface>().next(),
        TrayCommand::Previous => cx.global::<PlaybackInterface>().previous(),
    }
}

fn tray_tooltip(
    cx: &App,
    metadata: &Entity<Metadata>,
    current_track: &Entity<Option<CurrentTrack>>,
) -> String {
    let app_name = tr!("APP_NAME").to_string();
    if current_track.read(cx).is_none() {
        return app_name;
    }
    let metadata = metadata.read(cx);
    let title = metadata.name.as_deref().unwrap_or_default();
    let artist = metadata.artist.as_deref().unwrap_or_default();
    match (title.is_empty(), artist.is_empty()) {
        (false, false) => format!("{title} — {artist}"),
        (false, true) => title.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => app_name,
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn toggle_main_window(cx: &mut App) {
    let visible = crate::ui::app::find_main_window(cx).is_some_and(|handle| {
        handle
            .update(cx, |_, window, _| platform_window_visible(window))
            .unwrap_or(false)
    });
    if visible {
        hide_main_window(cx);
    } else {
        show_main_window(cx);
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn show_main_window(cx: &mut App) {
    let Some(handle) = crate::ui::app::find_main_window(cx) else {
        // 极端路径：窗口已销毁（正常托盘流程只显隐不销毁）。经 host 全局重建。
        crate::ui::app::restore_main_window(cx);
        return;
    };
    let _ = handle.update(cx, |_, window, _| {
        platform_show_window(window);
        window.activate_window();
    });
}

#[cfg(any(windows, target_os = "macos"))]
fn hide_main_window(cx: &mut App) {
    if let Some(handle) = crate::ui::app::find_main_window(cx) {
        let _ = handle.update(cx, |_, window, _| platform_hide_window(window));
    }
}

#[cfg(windows)]
fn platform_show_window(window: &mut Window) {
    windows::show_window(window)
}

/// 把触发关闭的窗口登记进平台壳（Windows 菜单标签的显隐判断依赖
/// `MAIN_HWND`；关闭路径不走 `find_main_window`，故在此补登记）。
#[cfg(windows)]
fn note_platform_window(window: &Window) {
    windows::note_main_window(window);
}

#[cfg(target_os = "macos")]
fn note_platform_window(_window: &Window) {}

#[cfg(target_os = "macos")]
fn platform_show_window(window: &mut Window) {
    macos::show_window(window)
}

#[cfg(windows)]
fn platform_hide_window(window: &mut Window) {
    windows::hide_window(window)
}

#[cfg(target_os = "macos")]
fn platform_hide_window(window: &mut Window) {
    macos::hide_window(window)
}

#[cfg(windows)]
fn platform_window_visible(window: &mut Window) -> bool {
    windows::window_visible(window)
}

#[cfg(target_os = "macos")]
fn platform_window_visible(window: &mut Window) -> bool {
    macos::window_visible(window)
}

fn platform_init(tx: Sender<TrayCommand>) -> PlatformTray {
    #[cfg(windows)]
    return match windows::init(tx) {
        Some(tray) => PlatformTray::Windows(tray),
        None => PlatformTray::None,
    };
    #[cfg(target_os = "macos")]
    return match macos::init(tx) {
        Some(tray) => PlatformTray::MacOs(tray),
        None => PlatformTray::None,
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        drop(tx);
        PlatformTray::None
    }
}
