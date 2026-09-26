//! KuGou settings section: QR-code login, profile display and logout.
//! Gated behind the `kugou` cargo feature.

use std::{future::Future, sync::Arc, time::Duration};

use crate::ui::design::ICON_SM;
use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, Render, RenderImage,
    SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::{
    kugou::{self, api::QrStatus},
    settings::{Settings, SettingsGlobal, playback::OnlineQuality},
    toasts::Toast,
    ui::{
        components::{
            button::{ButtonIntent, button},
            checkbox::checkbox,
            icons::{FOLDER, icon},
            label::label,
            section_header::section_header,
            tooltip::build_tooltip,
        },
        settings::online_common::{
            DownloadDirField, QrLogin, QrLoginHost, QrPhase, QrPoll, render_qr_modal,
        },
        theme::Theme,
    },
};

pub struct KugouSettings {
    settings: Entity<Settings>,
    logged_in: bool,
    nickname: Option<SharedString>,
    /// Human-readable VIP status line ("VIP · till 2026-08-24" / "No VIP").
    vip_line: Option<SharedString>,
    profile_failed: bool,
    qr: QrLogin,
    download_dir: DownloadDirField,
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
                qr: QrLogin::new(),
                download_dir: DownloadDirField::new(),
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
            let vip_refreshed = crate::RUNTIME
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
                // VIP line can now be rendered from it - unless the fetch
                // itself failed, which must not read as "No VIP".
                this.vip_line = Some(match vip_refreshed {
                    Ok(Ok(())) => crate::ui::kugou::vip_status_line(),
                    _ => tr!("KUGOU_VIP_STATUS_FETCH_FAILED", "Could not fetch VIP status")
                        .into(),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn logout(&mut self, cx: &mut Context<Self>) {
        kugou::shared_client().logout();
        self.logged_in = false;
        self.nickname = None;
        self.profile_failed = false;
        // logout() drops the cached VIP detail; reflect that immediately
        // instead of leaving the previous account's status line up.
        self.vip_line = Some(crate::ui::kugou::vip_status_line());
        self.qr.close();
        cx.notify();
    }
}

/// Localized "select download folder" label, defined once so the i18n
/// generator never sees a duplicate `SELECT_DOWNLOAD_DIR` key.
fn select_download_dir_label() -> cntp_i18n::I18nString {
    tr!("SELECT_DOWNLOAD_DIR", "Select the download folder...")
}

impl QrLoginHost for KugouSettings {
    const POLL_INTERVAL: Duration = Duration::from_secs(3);

    fn create_key() -> impl Future<Output = Result<String, String>> + Send {
        async move {
            kugou::shared_client()
                .qr_create_key()
                .await
                .map_err(|err| err.to_string())
        }
    }

    fn build_qr(key: &str) -> Result<Arc<RenderImage>, String> {
        crate::ui::kugou::build_login_qr(key).map_err(|err| err.to_string())
    }

    fn poll(key: String) -> impl Future<Output = Result<QrPoll, String>> + Send {
        async move {
            let status = kugou::shared_client()
                .qr_check(&key)
                .await
                .map_err(|err| err.to_string())?;
            Ok(match status {
                QrStatus::Waiting => QrPoll::Waiting,
                QrStatus::Scanned => QrPoll::Scanned,
                QrStatus::Expired => QrPoll::Expired,
                QrStatus::Success { token, userid } => {
                    kugou::shared_client().store_login(
                        token,
                        userid,
                        kugou::LoginExtras::default(),
                    );
                    QrPoll::Success
                }
            })
        }
    }

    fn failed_toast(err: String) -> Toast {
        Toast::error(tr!(
            "KUGOU_QR_FAILED",
            "Failed to generate QR code: {{err}}",
            err = err
        ))
    }

    fn phase_label(phase: QrPhase) -> SharedString {
        match phase {
            QrPhase::Generating => tr!("KUGOU_QR_LOADING", "Generating QR code...").into(),
            QrPhase::Waiting => tr!("KUGOU_QR_WAITING", "Waiting for scan...").into(),
            QrPhase::Scanned => tr!("KUGOU_QR_SCANNED", "Scanned. Confirm on your phone...").into(),
            QrPhase::Expired => tr!("KUGOU_QR_EXPIRED", "QR code expired.").into(),
        }
    }

    fn on_login(&mut self, cx: &mut Context<Self>) {
        self.logged_in = true;
        self.nickname = None;
        self.profile_failed = false;
        crate::ui::kugou::claim_daily_vip_async();
        self.fetch_profile(cx);
    }

    fn qr_login(&mut self) -> &mut QrLogin {
        &mut self.qr
    }
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
                            this.child(div().text_sm().text_color(theme.text_secondary).child(vip))
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
                    .child(div().flex_1().child(self.download_dir.input(cx)))
                    .child(
                        button()
                            .id("kugou-download-dir-browse")
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
                tr!("KUGOU_QR_TITLE", "KuGou Login").into(),
                Some(tr!("KUGOU_QR_HINT", "Scan with the KuGou mobile app").into()),
                tr!("KUGOU_QR_REGENERATE", "New QR Code").into(),
                tr!("KUGOU_QR_CANCEL", "Cancel").into(),
                &theme,
                cx,
            ));
        }
        root
    }
}
