//! NetEase settings section: QR login, online quality and download directory. Gated behind the `netease` cargo feature (via the parent module).

use std::{sync::Arc, time::Duration};

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, PathPromptOptions,
    Render, RenderImage, SharedString, Styled, Window, div, img, prelude::FluentBuilder, px,
};

use crate::{
    netease::{self, api::QrStatus},
    settings::{Settings, SettingsGlobal, playback::NeteaseQuality},
    toasts::{Toast, emit_toast},
    ui::{
        components::{
            button::{ButtonIntent, button},
            checkbox::checkbox,
            icons::{FOLDER, icon},
            label::label,
            managed_image::{ManagedImageKey, managed_image},
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
            Self::Generating => tr!("NETEASE_QR_LOADING", "Generating QR code...").into(),
            Self::Waiting => {
                tr!("NETEASE_QR_WAITING", "Scan with the NetEase Cloud Music app").into()
            }
            Self::Scanned => tr!("NETEASE_QR_SCANNED", "Scanned — confirm on your phone").into(),
            Self::Expired => tr!("NETEASE_QR_EXPIRED", "QR code expired").into(),
        }
    }
}

pub struct NeteaseSettings {
    settings: Entity<Settings>,
    logged_in: bool,
    nickname: Option<SharedString>,
    avatar_url: Option<SharedString>,
    qr: Option<QrLoginState>,
    qr_generation: u64,
    download_dir_input: Option<Entity<Textbox>>,
}

impl NeteaseSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let client = netease::shared_client();
        let logged_in = client.logged_in();
        let settings = cx.global::<SettingsGlobal>().model.clone();

        cx.new(|cx| {
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            let mut this = Self {
                settings,
                logged_in,
                nickname: None,
                avatar_url: None,
                qr: None,
                qr_generation: 0,
                download_dir_input: None,
            };

            if logged_in {
                // Seed from the persisted session so the nickname/avatar show
                // instantly, then refresh over the network.
                if let Some(profile) = client.cached_user_profile() {
                    this.nickname = Some(profile.nickname.into());
                    this.avatar_url = (!profile.avatar_url.is_empty())
                        .then(|| SharedString::from(profile.avatar_url));
                }
                this.fetch_profile(cx);
            }

            this
        })
    }

    fn fetch_profile(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let client = netease::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.refresh_user_profile().await })
                .await;

            this.update(cx, |this, cx| {
                if let Ok(Ok(Some(profile))) = request {
                    this.nickname = Some(profile.nickname.into());
                    this.avatar_url = (!profile.avatar_url.is_empty())
                        .then(|| SharedString::from(profile.avatar_url));
                }
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
            let client = netease::shared_client();
            let key = crate::RUNTIME
                .spawn(async move { client.qr_create_key().await })
                .await;

            let key = match key {
                Ok(Ok(key)) => key,
                Ok(Err(err)) => {
                    emit_toast(Toast::error(tr!(
                        "NETEASE_QR_FAILED",
                        "Could not load QR code: {{err}}",
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
                    emit_toast(Toast::error(tr!("NETEASE_QR_FAILED", err = err.to_string())));
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
                .spawn_blocking(move || crate::ui::netease::build_login_qr(&key_for_qr))
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
                            tracing::error!("failed to render netease login qr: {err}");
                            emit_toast(Toast::error(tr!(
                                "NETEASE_QR_FAILED",
                                err = err.to_string()
                            )));
                            this.qr = None;
                            cx.notify();
                            false
                        }
                        Err(err) => {
                            tracing::error!("failed to render netease login qr: {err}");
                            emit_toast(Toast::error(tr!(
                                "NETEASE_QR_FAILED",
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
                    .timer(Duration::from_secs(2))
                    .await;

                let client = netease::shared_client();
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
                            Ok(Ok(QrStatus::Success)) => {
                                // qr_check captured the login cookies into the
                                // session; pull the profile and liked list now.
                                this.logged_in = true;
                                this.nickname = None;
                                this.avatar_url = None;
                                this.qr = None;
                                crate::ui::netease::prime_liked_cache();
                                this.fetch_profile(cx);
                                cx.notify();
                                return false;
                            }
                            Ok(Err(err)) => {
                                tracing::warn!("netease qr poll failed: {err}");
                            }
                            Err(err) => {
                                tracing::warn!("netease qr poll failed: {err}");
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
        cx.spawn(async move |this, cx| {
            let client = netease::shared_client();
            // The server call runs on the Tokio runtime; the client clears the
            // local session itself regardless of the outcome.
            let _ = crate::RUNTIME.spawn(async move { client.logout().await }).await;

            this.update(cx, |this, cx| {
                this.logged_in = false;
                this.nickname = None;
                this.avatar_url = None;
                this.qr_generation += 1;
                this.qr = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            let settings = self.settings.clone();
            let input = Textbox::new_with_value_submit(cx, Default::default(), {
                move |value, cx| {
                    let trimmed = value.trim().to_string();
                    super::update_playback_settings(&settings, cx, move |playback| {
                        playback.download_dir = (!trimmed.is_empty()).then_some(trimmed);
                    });
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
        let settings = self.settings.clone();
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
            cx.update(|cx| {
                super::update_playback_settings(&settings, cx, move |playback| {
                    playback.download_dir = Some(dir);
                });
            });
        })
        .detach();
    }
}

/// Localized "select download folder" label, defined once so the i18n
/// generator never sees a duplicate `NETEASE_SELECT_DOWNLOAD_DIR` key.
fn select_download_dir_label() -> cntp_i18n::I18nString {
    tr!("NETEASE_SELECT_DOWNLOAD_DIR", "Select the download folder...")
}

impl Render for NeteaseSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();

        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(section_header(tr!("NETEASE_SECTION")));

        if self.logged_in {
            content = content
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .when_some(self.avatar_url.clone(), |this, url| {
                            this.child(
                                div()
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .rounded_full()
                                    .overflow_hidden()
                                    .bg(theme.album_art_background)
                                    .child(
                                        managed_image(
                                            "netease-settings-avatar",
                                            ManagedImageKey::HttpCover(url),
                                        )
                                        .size_full(),
                                    ),
                            )
                        })
                        .child(
                            div().text_sm().text_color(theme.text_secondary).child(
                                match &self.nickname {
                                    Some(name) => name.clone(),
                                    None => tr!(
                                        "NETEASE_NICKNAME_UNAVAILABLE",
                                        "Logged in (profile unavailable)"
                                    )
                                    .into(),
                                },
                            ),
                        ),
                )
                .child(
                    button()
                        .id("netease-logout")
                        .intent(ButtonIntent::Danger)
                        .child(tr!("NETEASE_LOGOUT", "Log Out"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.logout(cx);
                        })),
                );
        } else {
            content = content
                .child(
                    div().text_sm().text_color(theme.text_secondary).child(tr!(
                        "NETEASE_SETTINGS_INTRO",
                        "Scan the QR code with the NetEase Cloud Music app to play songs online and access your playlists."
                    )),
                )
                .child(
                    button()
                        .id("netease-login")
                        .intent(ButtonIntent::Primary)
                        .child(tr!("NETEASE_LOGIN", "Log In with QR Code"))
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
                        "netease-online-quality",
                        tr!("PLAYBACK_NETEASE_QUALITY", "NetEase online sound quality"),
                    )
                    .subtext(tr!(
                        "PLAYBACK_NETEASE_QUALITY_SUBTEXT",
                        "Only affects online NetEase playback; higher tiers usually require a NetEase VIP."
                    )),
                ),
            )
            .children(
                [
                    (
                        "standard",
                        tr!("NETEASE_QUALITY_STANDARD", "Standard (128 kbps)"),
                        NeteaseQuality::Standard,
                    ),
                    (
                        "higher",
                        tr!("NETEASE_QUALITY_HIGHER", "Higher (192 kbps)"),
                        NeteaseQuality::Higher,
                    ),
                    (
                        "exhigh",
                        tr!("NETEASE_QUALITY_EXHIGH", "Exhigh (320 kbps)"),
                        NeteaseQuality::Exhigh,
                    ),
                    (
                        "lossless",
                        tr!("NETEASE_QUALITY_LOSSLESS", "Lossless (FLAC)"),
                        NeteaseQuality::Lossless,
                    ),
                    (
                        "hires",
                        tr!("NETEASE_QUALITY_HIRES", "Hi-Res"),
                        NeteaseQuality::HiRes,
                    ),
                ]
                .into_iter()
                .map(|(id, name, quality)| {
                    label(
                        SharedString::from(format!("netease-online-quality-{id}")),
                        name,
                    )
                    .cursor_pointer()
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        super::update_playback_settings(&this.settings, cx, move |playback| {
                            playback.netease_quality = quality;
                        });
                    }))
                    .child(checkbox(
                        SharedString::from(format!("netease-online-quality-{id}-check")),
                        playback.netease_quality == quality,
                    ))
                }),
            )
            .child(
                div().pt(px(4.0)).child(
                    label(
                        "netease-download-dir",
                        tr!("NETEASE_DOWNLOAD_DIR", "NetEase download folder"),
                    )
                    .subtext(tr!(
                        "NETEASE_DOWNLOAD_DIR_SUBTEXT",
                        "Where downloaded NetEase tracks are saved. Leave empty for the system Downloads folder."
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
                            .id("netease-download-dir-browse")
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
                                        .child(tr!("NETEASE_QR_TITLE", "NetEase QR Login")),
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
                                        .child(tr!("NETEASE_QR_LOADING")),
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
                                                    .id("netease-qr-regenerate")
                                                    .intent(ButtonIntent::Primary)
                                                    .child(tr!(
                                                        "NETEASE_QR_REGENERATE",
                                                        "Regenerate"
                                                    ))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_login(cx);
                                                    })),
                                            )
                                        })
                                        .child(
                                            button()
                                                .id("netease-qr-cancel")
                                                .child(tr!("NETEASE_QR_CANCEL", "Cancel"))
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
