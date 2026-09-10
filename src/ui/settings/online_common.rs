//! Shared plumbing for the online provider settings pages (`kugou`,
//! `netease`): the QR login flow (state machine, poll loop with a failure
//! cap, modal) and the download-folder field. Feature-gated together with
//! its users.

use std::{future::Future, sync::Arc, time::Duration};

use gpui::{
    App, Context, Entity, FontWeight, IntoElement, ParentElement, PathPromptOptions, RenderImage,
    SharedString, Styled, div, img, prelude::FluentBuilder, px,
};

use crate::{
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        components::{button::{ButtonIntent, button}, modal::modal, textbox::Textbox},
        theme::Theme,
    },
};

/// Consecutive transport failures tolerated by the poll loop before it gives
/// up like an expired code — a dead network must not spin the loop forever.
const QR_MAX_POLL_FAILURES: u32 = 10;

/// QR login progress shown inside the modal.
#[derive(Clone)]
pub(crate) struct QrLoginState {
    image: Option<Arc<RenderImage>>,
    phase: QrPhase,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum QrPhase {
    Generating,
    Waiting,
    Scanned,
    Expired,
}

/// Normalized login-status poll outcome; the provider persists its session
/// before returning `Success`.
pub(crate) enum QrPoll {
    Waiting,
    Scanned,
    Expired,
    Success,
}

/// Shared QR-login state: modal content plus a generation counter that
/// invalidates a running poll loop when the modal is closed or regenerated.
pub(crate) struct QrLogin {
    state: Option<QrLoginState>,
    generation: u64,
}

impl QrLogin {
    pub(crate) const fn new() -> Self {
        Self {
            state: None,
            generation: 0,
        }
    }

    /// Arms the modal for a fresh login; returns the new generation.
    fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.state = Some(QrLoginState {
            image: None,
            phase: QrPhase::Generating,
        });
        self.generation
    }

    /// Dismisses the modal and invalidates any running flow.
    pub(crate) fn close(&mut self) {
        self.generation += 1;
        self.state = None;
    }

    pub(crate) fn state(&self) -> Option<&QrLoginState> {
        self.state.as_ref()
    }

    /// True when `generation` no longer matches the running flow.
    fn stale(&self, generation: u64) -> bool {
        self.generation != generation
    }

    fn set_phase(&mut self, phase: QrPhase) {
        if let Some(state) = &mut self.state {
            state.phase = phase;
        }
    }
}

/// Provider hooks for the shared QR login flow, implemented by the online
/// settings pages. The network calls run on the Tokio runtime; errors are
/// flattened to strings because the runner only ever logs or toasts them.
pub(crate) trait QrLoginHost: 'static + Sized {
    /// Delay between login-status polls.
    const POLL_INTERVAL: Duration;

    /// Request a fresh login key.
    fn create_key() -> impl Future<Output = Result<String, String>> + Send;
    /// Render the key as a QR image (runs on the blocking pool).
    fn build_qr(key: &str) -> Result<Arc<RenderImage>, String>;
    /// Poll the login status for `key`.
    fn poll(key: String) -> impl Future<Output = Result<QrPoll, String>> + Send;
    /// Toast shown when a request fails.
    fn failed_toast(err: String) -> Toast;
    /// Copy for the current QR phase.
    fn phase_label(phase: QrPhase) -> SharedString;
    /// UI follow-ups once the login succeeded; the session is already
    /// persisted and the runner closes the modal itself.
    fn on_login(&mut self, cx: &mut Context<Self>);

    /// The QR state owned by the page.
    fn qr_login(&mut self) -> &mut QrLogin;

    /// Entry point: open the modal and start the flow.
    fn start_login(&mut self, cx: &mut Context<Self>) {
        spawn_login::<Self>(cx);
    }

    /// Dismiss the modal.
    fn close_qr(&mut self, cx: &mut Context<Self>) {
        self.qr_login().close();
        cx.notify();
    }
}

/// Drives one QR login: request a key, render the image, then poll until
/// success, expiry or the failure cap. Provider specifics come from `H`.
fn spawn_login<H: QrLoginHost>(cx: &mut Context<H>) {
    cx.spawn(async move |this, cx| {
        let generation = match this.update(cx, |this, _| this.qr_login().begin()) {
            Ok(generation) => generation,
            Err(_) => return, // page released while starting
        };

        // step 1: request a fresh key and render it as a QR image
        let key = match crate::RUNTIME.spawn(async move { H::create_key().await }).await {
            Ok(Ok(key)) => Ok(key),
            Ok(Err(err)) => Err(err),
            Err(err) => Err(err.to_string()),
        };
        let key = match key {
            Ok(key) => key,
            Err(err) => {
                emit_toast(H::failed_toast(err));
                this.update(cx, |this, cx| {
                    if !this.qr_login().stale(generation) {
                        this.qr_login().close();
                        cx.notify();
                    }
                })
                .ok();
                return;
            }
        };

        let key_for_qr = key.clone();
        let image = match crate::RUNTIME.spawn_blocking(move || H::build_qr(&key_for_qr)).await {
            Ok(Ok(image)) => Ok(image),
            Ok(Err(err)) => Err(err),
            Err(err) => Err(err.to_string()),
        };
        let start_polling = this
            .update(cx, |this, cx| {
                if this.qr_login().stale(generation) {
                    return false;
                }
                match image {
                    Ok(image) => {
                        if let Some(state) = &mut this.qr_login().state {
                            state.image = Some(image);
                            state.phase = QrPhase::Waiting;
                        }
                        cx.notify();
                        true
                    }
                    Err(err) => {
                        tracing::error!("failed to render login qr: {err}");
                        emit_toast(H::failed_toast(err));
                        this.qr_login().close();
                        cx.notify();
                        false
                    }
                }
            })
            .unwrap_or(false);
        if !start_polling {
            return;
        }

        // step 2: poll until the mobile app confirms (or the code expires);
        // repeated transport failures give up like an expired code so a dead
        // network cannot spin this loop forever.
        let mut failures = 0u32;
        loop {
            cx.background_executor().timer(H::POLL_INTERVAL).await;

            let poll_key = key.clone();
            let check = crate::RUNTIME
                .spawn(async move { H::poll(poll_key).await })
                .await;
            let check = match check {
                Ok(Ok(poll)) => Ok(poll),
                Ok(Err(err)) => Err(err),
                Err(err) => Err(err.to_string()),
            };

            let keep_going = this
                .update(cx, |this, cx| {
                    if this.qr_login().stale(generation) || this.qr_login().state.is_none() {
                        return false;
                    }

                    match check {
                        Ok(QrPoll::Waiting) => {
                            failures = 0;
                            this.qr_login().set_phase(QrPhase::Waiting);
                        }
                        Ok(QrPoll::Scanned) => {
                            failures = 0;
                            this.qr_login().set_phase(QrPhase::Scanned);
                        }
                        Ok(QrPoll::Expired) => {
                            this.qr_login().set_phase(QrPhase::Expired);
                            cx.notify();
                            return false;
                        }
                        Ok(QrPoll::Success) => {
                            this.qr_login().close();
                            this.on_login(cx);
                            cx.notify();
                            return false;
                        }
                        Err(err) => {
                            tracing::warn!("qr login poll failed: {err}");
                            failures += 1;
                        }
                    }

                    if failures >= QR_MAX_POLL_FAILURES {
                        tracing::warn!(
                            "qr login poll gave up after {failures} consecutive failures"
                        );
                        this.qr_login().set_phase(QrPhase::Expired);
                        cx.notify();
                        return false;
                    }

                    cx.notify();
                    true
                })
                .unwrap_or(false);

            if !keep_going {
                break;
            }
        }
    })
    .detach();
}

/// Renders the QR login modal. `title`, `hint` and the button labels come
/// from the page so each keeps its own i18n keys.
pub(crate) fn render_qr_modal<H: QrLoginHost>(
    qr: &QrLoginState,
    title: SharedString,
    hint: Option<SharedString>,
    regenerate_label: SharedString,
    cancel_label: SharedString,
    theme: &Theme,
    cx: &mut Context<H>,
) -> impl IntoElement {
    let weak = cx.weak_entity();

    modal()
        .on_exit(move |_, cx| {
            weak.update(cx, |this, cx| this.close_qr(cx)).ok();
        })
        .child(
            div()
                .w(px(380.0))
                .max_w_full()
                .p(px(24.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .text_size(px(18.0))
                        .font_weight(FontWeight::BOLD)
                        .child(title),
                )
                .children(hint.map(|hint| {
                    div().text_sm().text_color(theme.text_secondary).child(hint)
                }))
                .child(match &qr.image {
                    Some(image) => div()
                        .rounded(px(theme.radius_md))
                        .overflow_hidden()
                        .child(
                            img(image.clone())
                                .w(px(260.0))
                                .h(px(260.0))
                                .flex_shrink(0.0),
                        ),
                    None => div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(260.0))
                        .h(px(260.0))
                        .text_sm()
                        .text_color(theme.text_secondary)
                        // Same copy as the Generating phase label.
                        .child(H::phase_label(QrPhase::Generating)),
                })
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.text_secondary)
                        .child(H::phase_label(qr.phase)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .when(qr.phase == QrPhase::Expired, |this| {
                            this.child(
                                button()
                                    .id("online-qr-regenerate")
                                    .intent(ButtonIntent::Primary)
                                    .child(regenerate_label)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.start_login(cx);
                                    })),
                            )
                        })
                        .child(
                            button()
                                .id("online-qr-cancel")
                                .child(cancel_label)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_qr(cx);
                                })),
                        ),
                ),
        )
}

/// Shared download-folder field: a lazily-created textbox plus the native
/// folder picker. Persisted through `update_playback_settings`, so both
/// providers write the same `playback.download_dir` setting.
pub(crate) struct DownloadDirField {
    input: Option<Entity<Textbox>>,
}

impl DownloadDirField {
    pub(crate) const fn new() -> Self {
        Self { input: None }
    }

    /// Lazily-created download directory textbox. Submitting an empty value
    /// resets to the platform default downloads folder.
    pub(crate) fn input(&mut self, cx: &mut App) -> Entity<Textbox> {
        if self.input.is_none() {
            let current = cx
                .global::<SettingsGlobal>()
                .model
                .read(cx)
                .playback
                .effective_download_dir()
                .display()
                .to_string();
            let input = Textbox::new_with_value_submit(cx, Default::default(), |value, cx| {
                let trimmed = value.trim().to_string();
                set_download_dir(cx, (!trimmed.is_empty()).then_some(trimmed));
            });
            input.update(cx, |textbox, cx| {
                textbox.set_value(cx, SharedString::from(current));
            });
            self.input = Some(input);
        }
        self.input.clone().unwrap()
    }

    /// Opens the native folder picker; the chosen folder is written into the
    /// textbox and saved immediately.
    pub(crate) fn browse(&self, prompt: SharedString, cx: &mut App) {
        let path_future = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt),
        });
        let input = self.input.clone();
        cx.spawn(async move |cx| {
            let Ok(Ok(Some(paths))) = path_future.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let dir = dir.display().to_string();
            if let Some(input) = input {
                input.update(cx, |textbox, cx| {
                    textbox.set_value(cx, SharedString::from(dir.clone()));
                });
            }
            cx.update(|cx| set_download_dir(cx, Some(dir)));
        })
        .detach();
    }
}

/// Persists the download directory setting (empty/None = platform default).
fn set_download_dir(cx: &mut App, dir: Option<String>) {
    let settings = cx.global::<SettingsGlobal>().model.clone();
    super::update_playback_settings(&settings, cx, move |playback| {
        playback.download_dir = dir;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_login_generation_invalidates_stale_flows() {
        let mut qr = QrLogin::new();
        assert!(qr.state().is_none());

        let g1 = qr.begin();
        assert!(!qr.stale(g1));
        qr.set_phase(QrPhase::Scanned);
        assert_eq!(qr.state().unwrap().phase, QrPhase::Scanned);

        // Regenerating arms a new generation and invalidates the old one.
        let g2 = qr.begin();
        assert_ne!(g1, g2);
        assert!(qr.stale(g1));

        // Closing (cancel/logout) invalidates everything and clears the modal.
        qr.close();
        assert!(qr.state().is_none());
        assert!(qr.stale(g2));
    }
}
