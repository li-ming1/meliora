//! macOS 托盘壳：`NSStatusItem` + `NSMenu`。
//!
//! 与 Windows 侧同构：平台壳只拥有"图标 + 菜单 + tooltip"，窗口显隐由
//! `tray` 门面经 `NSView::window()` 取原生窗口完成（无需 gpui 补丁）。
//! 菜单动作经 `define_class!` 定义的 target 对象桥接——target 由菜单项以
//! unsafe-unretained 语义持有，故 `MacOsTray` 必须同时持有 target 的强
//! 引用，否则菜单项动作会打野指针。

use std::sync::{OnceLock, mpsc::Sender};

use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol},
    sel,
};
use objc2_app_kit::{
    NSBezierPath, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSView, NSWindow,
};
use objc2_foundation::NSString;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::TrayCommand;
use cntp_i18n::tr;

/// 壳 → 应用命令的发送端（与 windows 侧同模式的进程级静态）。
static TRAY_TX: OnceLock<Sender<TrayCommand>> = OnceLock::new();

/// 托盘壳句柄。Drop 时移除状态项。
pub(super) struct MacOsTray {
    status_item: Retained<NSStatusItem>,
    /// 菜单项的 target 是 unsafe-unretained 引用，必须比菜单项活得久。
    _target: Retained<MelioraTrayTarget>,
    mtm: MainThreadMarker,
}

impl MacOsTray {
    pub(super) fn set_tooltip(&self, text: &str) {
        if let Some(button) = self.status_item.button(self.mtm) {
            button.setToolTip(Some(&NSString::from_str(text)));
        }
    }
}

impl Drop for MacOsTray {
    fn drop(&mut self) {
        self.status_item.setVisible(false);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.status_item);
    }
}

/// 菜单动作的接收 target。无 ivars——命令统一走 `TRAY_TX` 静态（与
/// Windows 的 WndProc 同一模式）。
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct MelioraTrayTarget;

    unsafe impl NSObjectProtocol for MelioraTrayTarget {}

    impl MelioraTrayTarget {
        #[unsafe(method(trayPlayPause:))]
        fn play_pause(&self, _sender: Option<&AnyObject>) {
            send(TrayCommand::PlayPause);
        }

        #[unsafe(method(trayPrevious:))]
        fn previous(&self, _sender: Option<&AnyObject>) {
            send(TrayCommand::Previous);
        }

        #[unsafe(method(trayNext:))]
        fn next(&self, _sender: Option<&AnyObject>) {
            send(TrayCommand::Next);
        }

        #[unsafe(method(trayToggleWindow:))]
        fn toggle_window(&self, _sender: Option<&AnyObject>) {
            send(TrayCommand::ToggleWindow);
        }

        #[unsafe(method(trayOpenSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            send(TrayCommand::OpenSettings);
        }

        #[unsafe(method(trayQuit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            send(TrayCommand::Quit);
        }
    }
);

pub(super) fn init(tx: Sender<TrayCommand>) -> Option<MacOsTray> {
    let _ = TRAY_TX.set(tx);
    // 托盘只能在主线程创建；AppKit 全部 API 亦同。
    let mtm = MainThreadMarker::new()?;

    let target = {
        let this = MelioraTrayTarget::alloc(mtm);
        unsafe { msg_send![super(this), init] }
    };
    let menu = build_menu(mtm, &target);

    let status_bar = NSStatusBar::systemStatusBar();
    let status_item = status_bar.statusItemWithLength(status_bar.thickness());
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&tray_icon()));
        button.setToolTip(Some(&NSString::from_str(&super::app_name_label())));
    }
    status_item.setMenu(Some(&menu));

    Some(MacOsTray {
        status_item,
        _target: target,
        mtm,
    })
}

fn build_menu(mtm: MainThreadMarker, target: &MelioraTrayTarget) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    // 自定义 target 不实现 NSMenuValidation，禁用自动启用判定，否则条目
    // 会被系统一律置灰。
    menu.setAutoenablesItems(false);
    add_item(
        &menu,
        mtm,
        target,
        &super::play_pause_label(),
        sel!(trayPlayPause:),
    );
    add_item(
        &menu,
        mtm,
        target,
        &super::previous_label(),
        sel!(trayPrevious:),
    );
    add_item(&menu, mtm, target, &super::next_label(), sel!(trayNext:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(
        &menu,
        mtm,
        target,
        &tr!("TRAY_TOGGLE", "Show/Hide Window"),
        sel!(trayToggleWindow:),
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(
        &menu,
        mtm,
        target,
        &super::settings_label(),
        sel!(trayOpenSettings:),
    );
    add_item(&menu, mtm, target, &super::quit_label(), sel!(trayQuit:));
    menu
}

fn add_item(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    target: &MelioraTrayTarget,
    title: &str,
    action: objc2::runtime::Sel,
) {
    let item = NSMenuItem::initWithTitle_action_keyEquivalent(
        NSMenuItem::alloc(mtm),
        &NSString::from_str(title),
        Some(action),
        &NSString::from_str(""),
    );
    // `setTarget` 收 `Option<&AnyObject>`；AnyObject 是 ZST 类型擦除引用，
    // 指针转换与 objc2 的 `ProtocolObject::from_ref` 内部同一手法。
    let target_ref: &AnyObject = unsafe { &*(std::ptr::from_ref(target).cast::<AnyObject>()) };
    unsafe { item.setTarget(Some(target_ref)) };
    menu.addItem(&item);
}

/// 程序化绘制 18pt 音符图标：`setTemplate(true)` 后由系统按菜单栏明暗
/// 着色，零资产依赖、无 SF Symbols 版本门槛。
fn tray_icon() -> Retained<NSImage> {
    let image = NSImage::initWithSize(NSImage::alloc(), objc2_foundation::NSSize::new(18.0, 18.0));
    image.lockFocus();
    unsafe {
        // 符头（椭圆）
        NSBezierPath::bezierPathWithOvalInRect(objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::new(3.5, 2.0),
            objc2_foundation::NSSize::new(6.5, 5.0),
        ))
        .fill();
        // 符杆
        NSBezierPath::fillRect(objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::new(9.3, 4.0),
            objc2_foundation::NSSize::new(1.7, 12.0),
        ));
        // 符尾（三角）
        let flag = NSBezierPath::bezierPath();
        flag.moveToPoint(objc2_foundation::NSPoint::new(11.0, 15.6));
        flag.lineToPoint(objc2_foundation::NSPoint::new(15.2, 13.6));
        flag.lineToPoint(objc2_foundation::NSPoint::new(11.0, 11.6));
        flag.closePath();
        flag.fill();
    }
    image.unlockFocus();
    image.setTemplate(true);
    image
}

pub(super) fn show_window(window: &mut gpui::Window) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    unsafe {
        ns_window.makeKeyAndOrderFront(None);
        ns_window.orderFrontRegardless();
    }
}

pub(super) fn hide_window(window: &mut gpui::Window) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    unsafe {
        ns_window.orderOut(None);
    }
}

pub(super) fn window_visible(window: &mut gpui::Window) -> bool {
    let Some(ns_window) = ns_window(window) else {
        return false;
    };
    unsafe { ns_window.isVisible() }
}

/// raw-window-handle 0.6 的 AppKit 句柄只带 `ns_view`（无 ns_window 字段），
/// 窗口经 `-[NSView window]` 取。
fn ns_window(window: &gpui::Window) -> Option<Retained<NSWindow>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    let ns_view: &NSView = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
    ns_view.window()
}

fn send(command: TrayCommand) {
    if let Some(tx) = TRAY_TX.get() {
        let _ = tx.send(command);
    }
}
