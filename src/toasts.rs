use std::{sync::OnceLock, time::Duration};

use cntp_i18n::I18nString;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

static TOAST_SENDER: OnceLock<UnboundedSender<Toast>> = OnceLock::new();

const DEFAULT_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

pub struct ToastAction {
    pub label: I18nString,
    pub callback: Box<dyn FnOnce(&mut gpui::App) + Send + 'static>,
}

/// 一条排队等待浮层渲染的 toast 通知。
pub struct Toast {
    pub severity: Severity,
    pub message: I18nString,
    /// 默认 5s，`Toast::with_duration`（online 门控）可覆盖（如更新提示 12s）。
    pub duration: Option<Duration>,
    pub actions: Vec<ToastAction>,
}

impl Toast {
    pub fn new(severity: Severity, message: I18nString) -> Self {
        Self {
            severity,
            message,
            duration: Some(DEFAULT_DURATION),
            actions: Vec::new(),
        }
    }

    pub fn info(message: I18nString) -> Self {
        Self::new(Severity::Info, message)
    }

    pub fn success(message: I18nString) -> Self {
        Self::new(Severity::Success, message)
    }

    pub fn warning(message: I18nString) -> Self {
        Self::new(Severity::Warning, message)
    }

    pub fn error(message: I18nString) -> Self {
        Self::new(Severity::Error, message)
    }

    pub fn with_action(
        mut self,
        label: I18nString,
        callback: impl FnOnce(&mut gpui::App) + Send + 'static,
    ) -> Self {
        self.actions.push(ToastAction {
            label,
            callback: Box::new(callback),
        });
        self
    }

    /// 覆盖默认的 5s 存活时间——设 `duration` 字段是给那些容易被错过、
    /// 但多留一会儿代价很低的提示（如更新器的"重启以应用"）。调用方只存在于
    /// online 门控的更新器里，故该 builder 沿用同一门控。
    #[cfg(feature = "online")]
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }
}

/// 将 toast 排入待显示队列。sender 尚未安装时静默丢弃
/// （例如极早期启动阶段 `init` 之前）。
pub fn emit_toast(toast: Toast) {
    if let Some(sender) = TOAST_SENDER.get() {
        let _ = sender.send(toast);
    }
}

pub fn init() -> UnboundedReceiver<Toast> {
    let (tx, rx) = unbounded_channel();
    TOAST_SENDER
        .set(tx)
        .expect("toasts::init called more than once");
    rx
}
