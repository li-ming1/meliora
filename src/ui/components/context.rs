use std::rc::Rc;

use gpui::*;

use crate::ui::theme::Theme;

actions!(context, [CloseContextMenu]);

type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;
type MenuBuilder = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// 共享的右键菜单构建器契约：构建器在菜单打开期间的每次渲染都会运行（包括 hover 驱动的重绘），
/// 所以其函数体只能读缓存与快照——绝不发起阻塞查询或系统调用。
pub type ContextMenuBuilder = MenuBuilder;

/// 先通知可选的关闭处理器，再通过清空已保存的打开位置来隐藏菜单。三条收起路径共用：点击菜单、
/// 点击菜单外、以及 `CloseContextMenu` action。
fn dismiss_menu(
    on_close: &Option<CloseHandler>,
    state: &Entity<Option<Point<Pixels>>>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(on_close) = on_close {
        on_close(window, cx);
    }
    state.update(cx, |pos, cx| {
        *pos = None;
        cx.notify();
    });
}

/// 挂在 `div` 上的右键菜单。菜单体在打开期间的每次渲染都重建（见 [`ContextMenuBuilder`]），
/// 以锚定 overlay 显示，直到点击菜单、点击菜单外或 `CloseContextMenu` action 把它收起。
#[derive(IntoElement)]
pub struct ContextMenu {
    pub(self) id: ElementId,
    pub(self) div: Div,
    pub(self) element: Option<AnyElement>,
    pub(self) menu: Option<Div>,
    pub(self) menu_fn: Option<MenuBuilder>,
    pub(self) on_close: Option<CloseHandler>,
}

impl ContextMenu {
    pub fn with(mut self, element: impl IntoElement) -> Self {
        self.element = Some(element.into_any_element());
        self
    }

    pub fn menu_on_open(
        mut self,
        builder: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.menu_fn = Some(Rc::new(builder));
        self
    }

    pub fn on_close(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }
}

impl Styled for ContextMenu {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl ParentElement for ContextMenu {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.menu.as_mut().unwrap().extend(elements);
    }
}

impl RenderOnce for ContextMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| None::<Point<Pixels>>);
        let focus_handle = window
            .use_keyed_state((self.id.clone(), "focus"), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();

        let position = *state.read(cx);

        let state_open = state.clone();
        let state_click = state.clone();
        let state_out = state.clone();
        let state_esc = state;
        let on_click_close = self.on_close.clone();
        let on_out_close = self.on_close.clone();
        let on_esc_close = self.on_close.clone();
        let focus_open = focus_handle.clone();

        // 在借用 theme 之前构建（可能惰性的）菜单：构建器需要 `&mut cx`，而下面的样式只是从
        // 这个借用上读取 Copy 的颜色值。
        let menu = match self.menu_fn {
            Some(build) if position.is_some() => Some(div().child(build(window, cx))),
            Some(_) => None,
            None => self.menu,
        };

        let theme = cx.global::<Theme>();

        let overlay = if let (Some(pos), Some(menu)) = (position, menu) {
            Some(
                anchored().position(pos).child(deferred(
                    menu.occlude()
                        .border_1()
                        .shadow_sm()
                        .rounded(px(theme.radius_md))
                        .border_color(theme.elevated_border_color)
                        .bg(theme.elevated_background)
                        .id("menu")
                        .track_focus(&focus_handle)
                        .role(gpui::Role::Menu)
                        .on_click(move |_, window, cx| {
                            dismiss_menu(&on_click_close, &state_click, window, cx);
                        })
                        .on_mouse_down_out(move |_, window, cx| {
                            dismiss_menu(&on_out_close, &state_out, window, cx);
                        })
                        .on_action(move |_: &CloseContextMenu, window, cx| {
                            dismiss_menu(&on_esc_close, &state_esc, window, cx);
                        }),
                )),
            )
        } else {
            None
        };

        self.div
            .id(self.id)
            .on_aux_click(move |ev, window, cx| {
                if ev.is_right_click() {
                    state_open.update(cx, |pos, cx| {
                        *pos = Some(ev.position());
                        cx.notify();
                    });
                    focus_open.focus(window, cx);
                }
            })
            .children(self.element)
            .children(overlay)
    }
}

pub fn context(id: impl Into<ElementId>) -> ContextMenu {
    ContextMenu {
        id: id.into(),
        div: div(),
        element: None,
        menu: Some(div()),
        menu_fn: None,
        on_close: None,
    }
}
