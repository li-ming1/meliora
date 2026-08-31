//! KuGou settings section: QR-code login, profile display and logout.
//! Gated behind the `kugou` cargo feature.

use std::{sync::Arc, time::Duration};

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, PathPromptOptions,
    Render, RenderImage, SharedString, Styled, Window, div, img, prelude::FluentBuilder, px,
};

use crate::{
    kugou::{self, api::QrStatus},
    settings::{Settings, SettingsGlobal, playback::OnlineQuality, save_settings},
    toasts::{Toast, emit_toast},
    ui::{
        components::{
            button::{ButtonIntent, button},
            checkbox::checkbox,
            icons::{FOLDER, icon},
            label::label,
            modal::modal,
            section_header::section_header,
            textbox::Textbox,
            tooltip::build_tooltip,
        },
        theme::Theme,
    },
};

/// QR login progress shown inside the modal.
#[derive(Clone)]
struct QrLoginState {
    image: Option<Arc<RenderImage>>,
    phase: QrPhase,
}

#[derive(Clone, Copy, PartialEq)]
enum QrPhase {
    Generating,
    Waiting,
    Scanned,
    Expired,
}

impl QrPhase {
    fn label(self) -> SharedString {
        match self {
            Self::Generating => tr!("KUGOU_QR_LOADING", "Generating QR code...").into(),
            Self::Waiting => tr!("KUGOU_QR_WAITING", "Waiting for scan...").into(),
            Self::Scanned => tr!("KUGOU_QR_SCANNED", "Scanned. Confirm on your phone...").into(),
            Self::Expired => tr!("KUGOU_QR_EXPIRED", "QR code expired.").into(),
        }
    }
}

pub struct KugouSettings {
    settings: Entity<Settings>,
    logged_in: bool,
    nickname: Option<SharedString>,
    /// Human-readable VIP status line ("VIP · till 2026-08-24" / "No VIP").
    vip_line: Option<SharedString>,
    profile_failed: bool,
    qr: Option<QrLoginState>,
    qr_generation: u64,
    download_dir_input: Option<Entity<Textbox>>,
}

impl KugouSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let logged_in = kugou::shared_client().logged_in();
        let settings = cx.global::<SettingsGlobal>().model.clone();

        cx.new(|cx| {
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            let mut this = Self {
                settings,
                logged_in,
                nickname: None,
                vip_line: Some(crate::ui::kugou::vip_status_line()),
                profile_failed: false,
                qr: None,
                qr_generation: 0,
                download_dir_input: None,
            };

            if logged_in {
                this.fetch_profile(cx);
            }

            this
        })
    }

    fn fetch_profile(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let client = kugou::shared_client();
            let detail_client = client.clone();
            let request = crate::RUNTIME
                .spawn(async move { client.user_profile().await })
                .await;
            let _ = crate::RUNTIME
                .spawn(async move { detail_client.refresh_vip_detail().await })
                .await;

            this.update(cx, |this, cx| {
                match request {
                    Ok(Ok(profile)) => {
                        this.nickname = Some(profile.nickname.into());
                        this.profile_failed = false;
                    }
                    _ => this.profile_failed = true,
                }
                // refresh_vip_detail cached the response on the session, so the
                // VIP line can now be rendered from it.
                this.vip_line = Some(crate::ui::kugou::vip_status_line());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_login(&mut self, cx: &mut Context<Self>) {
        self.qr_generation += 1;
        let generation = self.qr_generation;
        self.qr = Some(QrLoginState {
            image: None,
            phase: QrPhase::Generating,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            // step 1: request a fresh key and render it as a QR image
            let client = kugou::shared_client();
            let key = crate::RUNTIME.spawn(async move { client.qr_create_key().await }).await;

            let key = match key {
                Ok(Ok(key)) => key,
                Ok(Err(err)) => {
                    emit_toast(Toast::error(tr!(
                        "KUGOU_QR_FAILED",
                        "Failed to generate QR code: {{err}}",
                        err = err.to_string()
                    )));
                    this.update(cx, |this, cx| {
                        if this.qr_generation == generation {
                            this.qr = None;
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
                Err(err) => {
                    emit_toast(Toast::error(tr!(
                        "KUGOU_QR_TASK_FAILED",
                        "Login request failed: {{err}}",
                        err = err.to_string()
                    )));
                    this.update(cx, |this, cx| {
                        if this.qr_generation == generation {
                            this.qr = None;
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
            };

            let key_for_qr = key.clone();
            let image = crate::RUNTIME
                .spawn_blocking(move || crate::ui::kugou::build_login_qr(&key_for_qr))
                .await;

            let start_polling = this
                .update(cx, |this, cx| {
                    if this.qr_generation != generation {
                        return false;
                    }

                    match image {
                        Ok(Ok(image)) => {
                            if let Some(qr) = this.qr.as_mut() {
                                qr.image = Some(image);
                                qr.phase = QrPhase::Waiting;
                            }
                            cx.notify();
                            true
                        }
                        Ok(Err(err)) => {
                            tracing::error!("failed to render kugou login qr: {err}");
                            emit_toast(Toast::error(tr!(
                                "KUGOU_QR_FAILED",
                                err = err.to_string()
                            )));
                            this.qr = None;
                            cx.notify();
                            false
                        }
                        Err(err) => {
                            tracing::error!("failed to render kugou login qr: {err}");
                            emit_toast(Toast::error(tr!(
                                "KUGOU_QR_FAILED",
                                err = err.to_string()
                            )));
                            this.qr = None;
                            cx.notify();
                            false
                        }
                    }
                })
                .unwrap_or(false);

            if !start_polling {
                return;
            }

            // step 2: poll until the mobile app confirms (or the code expires)
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(3))
                    .await;

                let client = kugou::shared_client();
                let poll_key = key.clone();
                let check = crate::RUNTIME
                    .spawn(async move { client.qr_check(&poll_key).await })
                    .await;

                let keep_going = this
                    .update(cx, |this, cx| {
                        let Some(qr) = this.qr.as_mut() else {
                            return false;
                        };
                        if this.qr_generation != generation {
                            return false;
                        }

                        match check {
                            Ok(Ok(QrStatus::Waiting)) => qr.phase = QrPhase::Waiting,
                            Ok(Ok(QrStatus::Scanned)) => qr.phase = QrPhase::Scanned,
                            Ok(Ok(QrStatus::Expired)) => {
                                qr.phase = QrPhase::Expired;
                                cx.notify();
                                return false;
                            }
                            Ok(Ok(QrStatus::Success { token, userid })) => {
                                kugou::shared_client().store_login(
                                    token,
                                    userid,
                                    kugou::LoginExtras::default(),
                                );
                                this.logged_in = true;
                                this.nickname = None;
                                this.profile_failed = false;
                                this.qr = None;
                                crate::ui::kugou::claim_daily_vip_async();
                                this.fetch_profile(cx);
                                cx.notify();
                                return false;
                            }
                            Ok(Err(err)) => {
                                tracing::warn!("kugou qr poll failed: {err}");
                            }
                            Err(err) => {
                                tracing::warn!("kugou qr poll failed: {err}");
                            }
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

    fn logout(&mut self, cx: &mut Context<Self>) {
        kugou::shared_client().logout();
        self.logged_in = false;
        self.nickname = None;
        self.profile_failed = false;
        self.qr_generation += 1;
        self.qr = None;
        cx.notify();
    }

    fn close_qr(&mut self, cx: &mut Context<Self>) {
        self.qr_generation += 1;
        self.qr = None;
        cx.notify();
    }

    /// Lazily-created download directory textbox. Submitting an empty value
    /// resets to the platform default downloads folder.
    fn download_dir_input(&mut self, cx: &mut Context<Self>) -> Entity<Textbox> {
        if self.download_dir_input.is_none() {
            let current = cx
                .global::<SettingsGlobal>()
                .model
                .read(cx)
                .playback
                .effective_download_dir()
                .display()
                .to_string();
            let input = Textbox::new_with_value_submit(cx, Default::default(), {
                move |value, cx| {
                    let trimmed = value.trim().to_string();
                    set_download_dir(cx, (!trimmed.is_empty()).then_some(trimmed));
                }
            });
            input.update(cx, |textbox, cx| {
                textbox.set_value(cx, SharedString::from(current));
            });
            self.download_dir_input = Some(input);
        }
        self.download_dir_input.clone().unwrap()
    }

    /// Opens the native folder picker; the chosen folder is written into the
    /// textbox and saved immediately.
    fn browse_download_dir(&self, cx: &mut App) {
        let path_future = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(select_download_dir_label().into()),
        });
        let input = self.download_dir_input.clone();
        cx.spawn(async move |cx| {
            let Ok(Ok(Some(paths))) = path_future.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let dir = dir.display().to_string();
            if let Some(input) = input.clone() {
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
    settings.update(cx, |settings, cx| {
        settings.playback.download_dir = dir;
        save_settings(cx, settings);
        cx.notify();
    });
}

/// Localized "select download folder" label, defined once so the i18n
/// generator never sees a duplicate `SELECT_DOWNLOAD_DIR` key.
fn select_download_dir_label() -> cntp_i18n::I18nString {
    tr!("SELECT_DOWNLOAD_DIR", "Select the download folder...")
}

impl Render for KugouSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();

        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(section_header(tr!("KUGOU_SECTION")));

        if self.logged_in {
            content = content
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tr!("KUGOU_LOGGED_IN", "Logged in")),
                        )
                        .child(
                            div().text_sm().text_color(theme.text_secondary).child(
                                match (&self.nickname, self.profile_failed) {
                                    (Some(name), _) => name.clone(),
                                    (None, true) => tr!(
                                        "KUGOU_NICKNAME_UNAVAILABLE",
                                        "Logged in (profile unavailable)"
                                    )
                                    .into(),
                                    (None, false) => tr!("KUGOU_LOADING", "Loading...").into(),
                                },
                            ),
                        )
                        .when_some(self.vip_line.clone(), |this, vip| {
                            this.child(
                                div().text_sm().text_color(theme.text_secondary).child(vip),
                            )
                        }),
                )
                .child(
                    button()
                        .id("kugou-logout")
                        .intent(ButtonIntent::Danger)
                        .child(tr!("KUGOU_LOGOUT", "Log Out"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.logout(cx);
                        })),
                );
        } else {
            content = content
                .child(
                    div().text_sm().text_color(theme.text_secondary).child(tr!(
                        "KUGOU_SETTINGS_INTRO",
                        "Log in with the KuGou mobile app to play tracks online and access your cloud playlists."
                    )),
                )
                .child(
                    button()
                        .id("kugou-login")
                        .intent(ButtonIntent::Primary)
                        .child(tr!("KUGOU_LOGIN", "Log In with QR Code"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.start_login(cx);
                        })),
                );
        }

        let playback = self.settings.read(cx).playback.clone();

        content = content
            .child(
                div().pt(px(4.0)).child(
                    label(
                        "kugou-online-quality",
                        tr!("PLAYBACK_ONLINE_QUALITY", "KuGou online sound quality"),
                    )
                    .subtext(tr!(
                        "PLAYBACK_ONLINE_QUALITY_SUBTEXT",
                        "Only affects online KuGou playback; higher tiers usually require KuGou VIP."
                    )),
                ),
            )
            .children(
                [
                    (
                        "128",
                        tr!("ONLINE_QUALITY_128", "128 kbps"),
                        OnlineQuality::Standard,
                    ),
                    (
                        "320",
                        tr!("ONLINE_QUALITY_320", "320 kbps"),
                        OnlineQuality::High,
                    ),
                    (
                        "flac",
                        tr!("ONLINE_QUALITY_FLAC", "FLAC (lossless)"),
                        OnlineQuality::Lossless,
                    ),
                    (
                        "hires",
                        tr!("ONLINE_QUALITY_HIRES", "Hi-Res"),
                        OnlineQuality::HiRes,
                    ),
                ]
                .into_iter()
                .map(|(id, name, quality)| {
                    label(
                        SharedString::from(format!("kugou-online-quality-{id}")),
                        name,
                    )
                    .cursor_pointer()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        super::update_playback_settings(&this.settings, cx, move |playback| {
                            playback.online_quality = quality;
                        });
                    }))
                    .child(checkbox(
                        SharedString::from(format!("kugou-online-quality-{id}-check")),
                        playback.online_quality == quality,
                    ))
                }),
            )
            .child(
                div().pt(px(4.0)).child(
                    label(
                        "kugou-download-dir",
                        tr!("PLAYBACK_DOWNLOAD_DIR", "KuGou download folder"),
                    )
                    .subtext(tr!(
                        "PLAYBACK_DOWNLOAD_DIR_SUBTEXT",
                        "Where downloaded KuGou tracks are saved. Leave empty for the system Downloads folder."
                    )),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .w_full()
                    .child(div().flex_1().child(self.download_dir_input(cx)))
                    .child(
                        button()
                            .id("kugou-download-dir-browse")
                            .child(icon(FOLDER).size(px(14.0)))
                            .tooltip(build_tooltip(select_download_dir_label()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.browse_download_dir(cx);
                            })),
                    ),
            );

        let qr = self.qr.clone();
        let weak = cx.weak_entity();

        div()
            .child(content)
            .when_some(qr, |this, qr| {
                this.child(
                    modal()
                        .on_exit(move |_, cx| {
                            weak.update(cx, |this, cx| {
                                this.close_qr(cx);
                            })
                            .ok();
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
                                        .child(tr!("KUGOU_QR_TITLE", "KuGou Login")),
                                )
                                .child(
                                    div().text_sm().text_color(theme.text_secondary).child(tr!(
                                        "KUGOU_QR_HINT",
                                        "Scan with the KuGou mobile app"
                                    )),
                                )
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
                                        .child(tr!("KUGOU_QR_LOADING")),
                                })
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.text_secondary)
                                        .child(qr.phase.label()),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(8.0))
                                        .when(qr.phase == QrPhase::Expired, |this| {
                                            this.child(
                                                button()
                                                    .id("kugou-qr-regenerate")
                                                    .intent(ButtonIntent::Primary)
                                                    .child(tr!(
                                                        "KUGOU_QR_REGENERATE",
                                                        "New QR Code"
                                                    ))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_login(cx);
                                                    })),
                                            )
                                        })
                                        .child(
                                            button()
                                                .id("kugou-qr-cancel")
                                                .child(tr!("KUGOU_QR_CANCEL", "Cancel"))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.close_qr(cx);
                                                })),
                                        ),
                                ),
                        ),
                )
            })
    }
}
