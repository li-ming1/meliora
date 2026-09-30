//! NetEase settings section: QR login, online quality and download directory. Gated behind the `netease` cargo feature (via the parent module).

use std::{future::Future, sync::Arc, time::Duration};

use crate::ui::design::ICON_SM;
use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, RenderImage,
    SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::{
    netease::{self, api::QrStatus, client::UserProfile},
    settings::{Settings, SettingsGlobal, playback::NeteaseQuality},
    toasts::Toast,
    ui::{
        components::{
            button::{ButtonIntent, button},
            checkbox::checkbox,
            icons::{FOLDER, icon},
            label::label,
            managed_image::{ManagedImageKey, managed_image},
            section_header::section_header,
            tooltip::build_tooltip,
        },
        settings::online_common::{
            DownloadDirField, QrLogin, QrLoginHost, QrPhase, QrPoll, render_qr_modal,
        },
        theme::Theme,
    },
};

pub struct NeteaseSettings {
    settings: Entity<Settings>,
    logged_in: bool,
    nickname: Option<SharedString>,
    avatar_url: Option<SharedString>,
    qr: QrLogin,
    download_dir: DownloadDirField,
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
                qr: QrLogin::new(),
                download_dir: DownloadDirField::new(),
            };

            if logged_in {
                // Seed from the persisted session so the nickname/avatar show
                // instantly, then refresh over the network.
                if let Some(profile) = client.cached_user_profile() {
                    this.apply_profile(profile);
                }
                this.fetch_profile(cx);
            }

            this
        })
    }

    /// Mirrors a fetched profile into the page state; an empty avatar URL
    /// means the account has no avatar.
    fn apply_profile(&mut self, profile: UserProfile) {
        self.nickname = Some(profile.nickname.into());
        self.avatar_url =
            (!profile.avatar_url.is_empty()).then(|| SharedString::from(profile.avatar_url));
    }

    fn fetch_profile(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let client = netease::shared_client();
            let request = crate::RUNTIME
                .spawn(async move { client.refresh_user_profile().await })
                .await;

            this.update(cx, |this, cx| {
                if let Ok(Ok(Some(profile))) = request {
                    this.apply_profile(profile);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn logout(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let client = netease::shared_client();
            // The server call runs on the Tokio runtime; the client clears the
            // local session itself regardless of the outcome.
            let _ = crate::RUNTIME
                .spawn(async move { client.logout().await })
                .await;

            this.update(cx, |this, cx| {
                this.logged_in = false;
                this.nickname = None;
                this.avatar_url = None;
                this.qr.close();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Localized "select download folder" label, defined once so the i18n
/// generator never sees a duplicate `NETEASE_SELECT_DOWNLOAD_DIR` key.
fn select_download_dir_label() -> cntp_i18n::I18nString {
    tr!(
        "NETEASE_SELECT_DOWNLOAD_DIR",
        "Select the download folder..."
    )
}

impl QrLoginHost for NeteaseSettings {
    const POLL_INTERVAL: Duration = Duration::from_secs(2);

    // Explicit `impl Future + Send`, not `async fn`: the trait contract
    // requires Send futures (callers spawn them on the runtime), and only the
    // RPITIT form expresses that bound.
    #[allow(clippy::manual_async_fn)]
    fn create_key() -> impl Future<Output = Result<String, String>> + Send {
        async move {
            netease::shared_client()
                .qr_create_key()
                .await
                .map_err(|err| err.to_string())
        }
    }

    fn build_qr(key: &str) -> Result<Arc<RenderImage>, String> {
        crate::ui::netease::build_login_qr(key).map_err(|err| err.to_string())
    }

    #[allow(clippy::manual_async_fn)] // Send bound required by the trait contract, see create_key
    fn poll(key: String) -> impl Future<Output = Result<QrPoll, String>> + Send {
        async move {
            let status = netease::shared_client()
                .qr_check(&key)
                .await
                .map_err(|err| err.to_string())?;
            Ok(match status {
                QrStatus::Waiting => QrPoll::Waiting,
                QrStatus::Scanned => QrPoll::Scanned,
                QrStatus::Expired => QrPoll::Expired,
                // qr_check captured the login cookies into the session.
                QrStatus::Success => QrPoll::Success,
            })
        }
    }

    fn failed_toast(err: String) -> Toast {
        Toast::error(tr!(
            "NETEASE_QR_FAILED",
            "Could not load QR code: {{err}}",
            err = err
        ))
    }

    fn phase_label(phase: QrPhase) -> SharedString {
        match phase {
            QrPhase::Generating => tr!("NETEASE_QR_LOADING", "Generating QR code...").into(),
            QrPhase::Waiting => tr!(
                "NETEASE_QR_WAITING",
                "Scan with the NetEase Cloud Music app"
            )
            .into(),
            QrPhase::Scanned => tr!("NETEASE_QR_SCANNED", "Scanned — confirm on your phone").into(),
            QrPhase::Expired => tr!("NETEASE_QR_EXPIRED", "QR code expired").into(),
        }
    }

    fn on_login(&mut self, cx: &mut Context<Self>) {
        self.logged_in = true;
        self.nickname = None;
        self.avatar_url = None;
        crate::ui::netease::prime_liked_cache();
        self.fetch_profile(cx);
    }

    fn qr_login(&mut self) -> &mut QrLogin {
        &mut self.qr
    }
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
                                        .thumb()
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
                    .child(div().flex_1().child(self.download_dir.input(cx)))
                    .child(
                        button()
                            .id("netease-download-dir-browse")
                            .child(icon(FOLDER).size(ICON_SM))
                            .tooltip(build_tooltip(select_download_dir_label()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.download_dir.browse(select_download_dir_label().into(), cx);
                            })),
                    ),
            );

        let mut root = div().child(content);
        if let Some(qr) = self.qr.state().cloned() {
            root = root.child(render_qr_modal(
                &qr,
                tr!("NETEASE_QR_TITLE", "NetEase QR Login").into(),
                None,
                tr!("NETEASE_QR_REGENERATE", "Regenerate").into(),
                tr!("NETEASE_QR_CANCEL", "Cancel").into(),
                &theme,
                cx,
            ));
        }
        root
    }
}
