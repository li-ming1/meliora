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
    },
};

const MAX_VISIBLE: usize = 4;

/// Repaint cadence of the auto-dismiss countdown bar. `AnimationExt::with_animation`
/// pumps `window.request_animation_frame()` once per frame for the animation's whole
/// duration, and each such request notifies the rendering view — which dirties the
/// complete view-ancestor chain (`Window::mark_view_dirty` walks `view_path_reversed`),
/// so a 5s toast alone re-drew `MainWindow` at 60Hz (~300 full-window draws, ~0.8s of
/// main-thread CPU at the ledger's measured ~2.7ms/frame constant) while the user did
/// nothing at all. The bar is decorative and dismissal is timer-driven, so the width is
/// now computed from elapsed time and refreshed on this tick instead: 10x/s keeps the
/// countdown legible (a 360px bar over 5s moves 7px per tick) at 1/6 the frames.
/// Same substitution the scrollbar fade already uses for its pre-fade wait.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// Action buttons pack `(toast id, action index)` into one integer element
/// id: the index lives in the low bits, the toast id above it.
const ACTION_INDEX_SHIFT: u32 = 16;

struct ActiveToast {
    id: u64,
    toast: Toast,
    /// When the toast appeared, so the countdown bar can derive its width
    /// from elapsed time rather than from a per-frame animation delta.
    shown_at: Instant,
}

impl ActiveToast {
    /// Fraction of the auto-dismiss countdown still left, for the progress bar.
    /// Clamped because a frame can land between the last tick and the dismissal.
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
    /// Toasts on screen that draw a countdown bar. One pump at `PROGRESS_TICK`
    /// covers all of them (each bar's width comes from its own `shown_at`), so
    /// the repaint rate stays flat as toasts stack up.
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

    /// A toast left the layer: release its claim on the countdown pump.
    fn retire(&mut self, removed: &ActiveToast) {
        if removed.toast.duration.is_some() {
            self.bar_count = self.bar_count.saturating_sub(1);
        }
    }

    /// One `PROGRESS_TICK` pump serves every bar on screen, since each bar's
    /// width comes from its own `shown_at` at render time. Per-toast pumps would
    /// stack: four visible toasts (the batch-download case, `MAX_VISIBLE`) meant
    /// four repaint requests per tick — 40/s instead of 60/s, barely a win.
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

    /// One pump tick: request a repaint while any bar is on screen, stand down
    /// when none is. `reduced_motion` is read live rather than at push time, so
    /// flipping the setting mid-toast can't freeze a bar that is being drawn —
    /// with the setting on, the pump still wakes, but asks for no frames.
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
        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;
        // Read once for the whole layer: render_toast runs per toast per
        // frame (the progress animation re-renders every frame), and cloning
        // the whole Theme there just to read one radius is pure waste.
        let radius_md = px(cx.global::<Theme>().radius_md);

        let mut column = div().flex().flex_col().gap(px(8.0)).w(px(360.0));
        for toast in &self.toasts {
            // Per-toast theme lookup: the borrowed &Theme must not outlive the
            // statement, since render_toast below re-borrows cx mutably.
            let colors = toast_colors(cx.global::<Theme>(), toast.toast.severity);
            column = column.child(render_toast(toast, colors, reduced_motion, radius_md, cx));
        }

        anchored()
            // 38 (height of window_header w/ border) + 16 = 54
            .position(point(viewport.width - px(16.0), px(54.0)))
            .anchor(Anchor::TopRight)
            .child(deferred(column))
            .into_any_element()
    }
}

/// The four theme colors a toast paints with, resolved per severity.
struct ToastColors {
    background: Rgba,
    border: Rgba,
    text: Rgba,
    /// Color of the auto-dismiss progress bar.
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
                    // Success toasts with actions (updater restart) render as
                    // Primary; there is no dedicated green intent button.
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
