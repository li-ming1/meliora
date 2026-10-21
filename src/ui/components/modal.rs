use std::rc::Rc;
use std::sync::Mutex;

use gpui::*;
use prelude::FluentBuilder;
use smallvec::SmallVec;

use crate::sync::RecoveredMutex;
use crate::ui::{constants::APP_SHADOW_SIZE, theme::Theme};

use super::window_chrome::round_untiled_corners;

/// 哪些窗口当前渲染了模态框。必须按窗口记账：设置窗的扫码/确认框过去会
/// 通过一个进程级 bool 让主窗的 Ctrl+K/Ctrl+F 静默失效。
pub struct ModalActive(pub Mutex<SmallVec<[WindowId; 4]>>);

impl Default for ModalActive {
    fn default() -> Self {
        Self(Mutex::new(SmallVec::new()))
    }
}

impl ModalActive {
    fn mark(&self, window: WindowId) {
        let mut active = self.0.lock_recovered();
        if !active.contains(&window) {
            active.push(window);
        }
    }

    /// 清掉指定窗口的标记；主窗在每帧 render 开头调用，modal 子树随后重新标记。
    pub fn clear(&self, window: WindowId) {
        self.0.lock_recovered().retain(|active| *active != window);
    }

    pub fn active_in(&self, window: WindowId) -> bool {
        self.0.lock_recovered().contains(&window)
    }
}

impl Global for ModalActive {}

pub type OnExitHandler = dyn Fn(&mut Window, &mut App);

#[derive(IntoElement)]
pub struct Modal {
    div: Stateful<Div>,
    on_exit: Option<Rc<OnExitHandler>>,
}

impl Modal {
    fn new() -> Modal {
        Modal {
            div: div().id("modal-fg"),
            on_exit: None,
        }
    }

    pub fn on_exit(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_exit = Some(Rc::new(handler));
        self
    }
}

actions!(modal, [CloseModal]);

impl ParentElement for Modal {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.div.extend(elements);
    }
}

impl RenderOnce for Modal {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        cx.global::<ModalActive>()
            .mark(window.window_handle().window_id());

        let decorations = window.window_decorations();
        let theme = cx.global::<Theme>();
        let mut size = window.viewport_size();

        let rounding = px(theme.radius_md);
        let shadow_size = APP_SHADOW_SIZE;

        match decorations {
            Decorations::Server => (),
            Decorations::Client { tiling } => {
                if !tiling.top {
                    size.height -= shadow_size;
                }
                if !tiling.bottom {
                    size.height -= shadow_size;
                }
                if !tiling.left {
                    size.width -= shadow_size;
                }
                if !tiling.right {
                    size.width -= shadow_size;
                }
            }
        }

        anchored().position(point(px(0.0), px(0.0))).child(deferred(
            div()
                .occlude()
                .flex()
                .w(size.width)
                .h(size.height)
                .bg(theme.modal_overlay_bg)
                .id("modal-bg")
                .map(|div| match decorations {
                    Decorations::Server => div,
                    Decorations::Client { tiling } => round_untiled_corners(div, tiling, rounding)
                        .when(!tiling.top, |div| div.mt(shadow_size))
                        .when(!tiling.bottom, |div| div.mb(shadow_size))
                        .when(!tiling.left, |div| div.ml(shadow_size))
                        .when(!tiling.right, |div| div.mr(shadow_size)),
                })
                .when_some(self.on_exit, |this, on_exit| {
                    let on_exit_clone = Rc::clone(&on_exit);
                    this.on_any_mouse_down(move |_, window, cx| {
                        on_exit_clone(window, cx);
                    })
                    .on_action(move |_: &CloseModal, window, cx| {
                        on_exit(window, cx);
                    })
                })
                .child(
                    self.div
                        .occlude()
                        .m_auto()
                        .border_color(theme.elevated_border_color)
                        .border_1()
                        .bg(theme.elevated_background)
                        .rounded(px(theme.radius_lg))
                        .flex_col()
                        .on_any_mouse_down(|_, _, cx| {
                            cx.stop_propagation();
                        }),
                ),
        ))
    }
}

pub fn modal() -> Modal {
    Modal::new()
}
