use std::time::{Duration, Instant};

use crate::ui::design::{ICON_LG, ICON_SM};
use gpui::{
    Anchor, App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, Rgba, StatefulInteractiveElement, Styled, Window, anchored, deferred,
    div, point, prelude::FluentBuilder, px, relative,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    settings::SettingsGlobal,
    toasts::{Severity, Toast, ToastAction},
    ui::{
        components::{
            button::{ButtonIntent, ButtonSize, ButtonStyle, button},
            icons::{ALERT_CIRCLE, CHECK, CROSS, icon},
        },
        theme::Theme,
        util::reduced_motion_enabled,
    },
};

const MAX_VISIBLE: usize = 4;

/// 自动消失倒计时条的重绘节奏。`AnimationExt::with_animation` 会在动画的整个时长
/// 里每帧泵一次 `window.request_animation_frame()`，而每一次这样的请求都会通知
/// 渲染视图——这会把整条视图祖先链都置脏（`Window::mark_view_dirty` 会走
/// `view_path_reversed`），于是一条 5s 的 toast 独自就把 `MainWindow` 按 60Hz
/// 重画了约 300 次全屏绘制（按账本实测的 ~2.7ms/帧常数算约 0.8s 主线程 CPU），
/// 而用户什么都没做。这根条只是装饰、消失由计时器驱动，所以现在宽度由已经过的
/// 时间算出、按本 tick 刷新：10 次/秒仍能让倒计时读得清（360px 的条在 5s 内
/// 每 tick 走 7px），帧数只有原来的 1/6。滚动条淡出早就用同样的替换处理它
/// 淡出前的等待。
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// 动作按钮把 `(toast id, action index)` 打包进一个整数元素 id：索引住在低位，
/// toast id 在它上面。
const ACTION_INDEX_SHIFT: u32 = 16;

struct ActiveToast {
    id: u64,
    toast: Toast,
    /// toast 出现的时间，好让倒计时条的宽度由已经过的时间推出，
    /// 而不是由每帧的动画增量推出。
    shown_at: Instant,
}

impl ActiveToast {
    /// 自动消失倒计时还剩的比例，给进度条用。做钳位，因为某一帧可能正好落在
    /// 最后一次 tick 与消失之间。
    fn remaining(&self) -> f32 {
        let Some(duration) = self.toast.duration else {
            return 0.0;
        };
        let elapsed = Instant::now().saturating_duration_since(self.shown_at);
        (1.0 - elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
    }
}

pub struct ToastLayer {
    toasts: Vec<ActiveToast>,
    next_id: u64,
    /// 屏幕上会绘制倒计时条的那些 toast。`PROGRESS_TICK` 的一次泵送就覆盖全部
    /// （每根条的宽度各自来自它的 `shown_at`），所以 toast 叠加时重绘速率保持平稳。
    bar_count: usize,
    ticker_running: bool,
}

impl ToastLayer {
    pub fn new(cx: &mut App, mut receiver: UnboundedReceiver<Toast>) -> Entity<Self> {
        cx.new(|cx| {
            cx.spawn(async move |this, cx| {
                while let Some(toast) = receiver.recv().await {
                    this.update(cx, |layer: &mut Self, cx| layer.push(toast, cx))
                        .ok();
                }
            })
            .detach();

            Self {
                toasts: Vec::new(),
                next_id: 1,
                bar_count: 0,
                ticker_running: false,
            }
        })
    }

    fn push(&mut self, toast: Toast, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;

        let duration = toast.duration;
        let shown_at = Instant::now();
        self.toasts.push(ActiveToast {
            id,
            toast,
            shown_at,
        });

        if self.toasts.len() > MAX_VISIBLE {
            let evicted = self.toasts.remove(0);
            self.retire(&evicted);
        }

        if let Some(duration) = duration {
            self.bar_count += 1;
            self.start_ticker(cx);

            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(duration).await;
                this.update(cx, |layer, cx| layer.dismiss(id, cx)).ok();
            })
            .detach();
        }

        cx.notify();
    }

    /// 有一条 toast 离开了图层：释放它对倒计时泵占的额度。
    fn retire(&mut self, removed: &ActiveToast) {
        if removed.toast.duration.is_some() {
            self.bar_count = self.bar_count.saturating_sub(1);
        }
    }

    /// 一次 `PROGRESS_TICK` 泵送服务屏幕上所有条，因为每根条的宽度都在渲染时
    /// 取自它自己的 `shown_at`。逐 toast 泵送会叠加：四条在屏（批量下载那种场景，
    /// `MAX_VISIBLE`）就意味着每个 tick 四次重绘请求——40/s 而不是 60/s，收益微乎其微。
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker_running || self.bar_count == 0 {
            return;
        }
        self.ticker_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                if !this.update(cx, |layer, cx| layer.tick(cx)).unwrap_or(false) {
                    return;
                }
                cx.background_executor().timer(PROGRESS_TICK).await;
            }
        })
        .detach();
    }

    /// 一次泵送 tick：屏上有任意条时请求重绘，一条都不剩时收工。
    /// `reduced_motion` 是实时读取而不是 push 时读，所以在一条 toast 显示期间
    /// 改这个设置也不可能冻住一根正在画的条——设置打开时泵依然会醒，
    /// 但不再请求任何帧。
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.bar_count == 0 {
            self.ticker_running = false;
            return false;
        }
        if !cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion
        {
            cx.notify();
        }
        true
    }

    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(pos) = self.toasts.iter().position(|t| t.id == id) {
            let removed = self.toasts.remove(pos);
            self.retire(&removed);
            cx.notify();
        }
    }

    fn activate_action(&mut self, id: u64, idx: usize, cx: &mut Context<Self>) {
        let Some(pos) = self.toasts.iter().position(|t| t.id == id) else {
            return;
        };
        let mut removed = self.toasts.remove(pos);
        self.retire(&removed);
        cx.notify();

        if idx < removed.toast.actions.len() {
            let ToastAction { callback, .. } = removed.toast.actions.swap_remove(idx);
            callback(cx);
        }
    }
}

impl Render for ToastLayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.toasts.is_empty() {
            return div().into_any_element();
        }

        let viewport = window.viewport_size();
        let reduced_motion = reduced_motion_enabled(cx);
        // 整个图层只读一次：render_toast 是每条 toast 每帧都跑的（进度动画每帧
        // 重渲染），在那里仅为读一个 radius 就克隆整个 Theme 纯属浪费。
        let radius_md = px(cx.global::<Theme>().radius_md);

        let mut column = div().flex().flex_col().gap(px(8.0)).w(px(360.0));
        for toast in &self.toasts {
            // 逐 toast 查主题：借来的 &Theme 不能活得比这条语句更久，
            // 因为下面的 render_toast 会重新可变借用 cx。
            let colors = toast_colors(cx.global::<Theme>(), toast.toast.severity);
            column = column.child(render_toast(toast, colors, reduced_motion, radius_md, cx));
        }

        anchored()
            // 38（window_header 含边框的高度）+ 16 = 54
            .position(point(viewport.width - px(16.0), px(54.0)))
            .anchor(Anchor::TopRight)
            .child(deferred(column))
            .into_any_element()
    }
}

/// 一条 toast 会用的四个主题色，按严重级别解析。
struct ToastColors {
    background: Rgba,
    border: Rgba,
    text: Rgba,
    /// 自动消失进度条的颜色。
    track: Rgba,
}

fn toast_colors(theme: &Theme, severity: Severity) -> ToastColors {
    match severity {
        Severity::Info => ToastColors {
            background: theme.toast_info_background,
            border: theme.toast_info_border,
            text: theme.toast_info_text,
            track: theme.toast_info_track,
        },
        Severity::Success => ToastColors {
            background: theme.toast_success_background,
            border: theme.toast_success_border,
            text: theme.toast_success_text,
            track: theme.toast_success_track,
        },
        Severity::Warning => ToastColors {
            background: theme.toast_warning_background,
            border: theme.toast_warning_border,
            text: theme.toast_warning_text,
            track: theme.toast_warning_track,
        },
        Severity::Error => ToastColors {
            background: theme.toast_error_background,
            border: theme.toast_error_border,
            text: theme.toast_error_text,
            track: theme.toast_error_track,
        },
    }
}

fn render_toast(
    active: &ActiveToast,
    colors: ToastColors,
    reduced_motion: bool,
    radius_md: gpui::Pixels,
    cx: &mut Context<ToastLayer>,
) -> impl IntoElement {
    let id = active.id;
    let toast = &active.toast;
    let icon_path = match toast.severity {
        Severity::Success => CHECK,
        Severity::Info | Severity::Warning | Severity::Error => ALERT_CIRCLE,
    };

    let mut actions_row = div().flex().gap(px(6.0)).pt(px(2.0));
    for (idx, action) in toast.actions.iter().enumerate() {
        actions_row = actions_row.child(
            button()
                .id(ElementId::from((
                    "toast-action",
                    (id << ACTION_INDEX_SHIFT) | idx as u64,
                )))
                .style(ButtonStyle::Regular)
                .size(ButtonSize::Regular)
                .intent(match toast.severity {
                    Severity::Info => ButtonIntent::Secondary,
                    // 带动作的成功 toast（更新器重启）按 Primary 渲染；
                    // 没有专门的绿色意图按钮。
                    Severity::Success => ButtonIntent::Primary,
                    Severity::Warning => ButtonIntent::Warning,
                    Severity::Error => ButtonIntent::Danger,
                })
                .child(action.label.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.activate_action(id, idx, cx);
                })),
        );
    }

    let main_row = div()
        .flex()
        .child(
            div()
                .flex()
                .items_center()
                .border_r_1()
                .p(px(10.0))
                .border_color(colors.border)
                .child(icon(icon_path).size(ICON_LG)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .w_full()
                .flex_shrink(1.0)
                .pt(px(8.0))
                .pl(px(12.0))
                .pb(px(9.0))
                .pr(px(8.0))
                .gap(px(6.0))
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink(1.0)
                        .text_sm()
                        .overflow_hidden()
                        .child(toast.message.clone()),
                )
                .when(!toast.actions.is_empty(), |this| this.child(actions_row)),
        )
        .child(
            div()
                .id(ElementId::from(("toast-close", id)))
                .flex()
                .p(px(6.0))
                .items_start()
                .cursor_pointer()
                .child(icon(CROSS).size(ICON_SM))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.dismiss(id, cx);
                })),
        );

    let progress_bar = toast.duration.filter(|_| !reduced_motion).map(|_| {
        div()
            .h(px(2.0))
            .w(relative(active.remaining()))
            .bg(colors.track)
    });

    div()
        .occlude()
        .rounded(radius_md)
        .bg(colors.background)
        .border_1()
        .border_color(colors.border)
        .text_color(colors.text)
        .overflow_hidden()
        .shadow_md()
        .flex()
        .flex_col()
        .child(main_row)
        .when_some(progress_bar, |this, bar| this.child(bar))
}
