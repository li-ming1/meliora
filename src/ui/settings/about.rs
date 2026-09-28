use cntp_i18n::tr;
use gpui::{
    Action, App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};

use crate::ui::{
    components::{
        button::{ButtonIntent, ButtonStyle, button},
        label::{Label, label},
        section_header::section_header,
    },
    global_actions::{About, Issues},
    troubleshooting::{CopyTroubleshootingInfo, OpenLog},
};

#[cfg(feature = "online")]
use crate::{
    settings::{SettingsGlobal, save_settings},
    ui::components::checkbox::checkbox,
    updater::{CheckSource, UpdateStatus, UpdaterGlobal},
};

pub struct AboutSettings {
    /// settings.json entity — source of the update toggles (`online` builds
    /// only; the page itself is stateless without the updater).
    #[cfg(feature = "online")]
    settings: Entity<crate::settings::Settings>,
}

impl AboutSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        Self::construct(cx)
    }

    #[cfg(feature = "online")]
    fn construct(cx: &mut App) -> Entity<Self> {
        let settings = cx.global::<SettingsGlobal>().model.clone();
        cx.new(|cx| {
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();
            if cx.has_global::<UpdaterGlobal>() {
                let updater = cx.global::<UpdaterGlobal>().state.clone();
                cx.observe(&updater, |_, _, cx| cx.notify()).detach();
            }
            Self { settings }
        })
    }

    #[cfg(not(feature = "online"))]
    fn construct(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {})
    }
}

/// One settings row: a titled label with explanation subtext and a
/// full-width secondary button that dispatches `action` (deferred, so it
/// runs outside of click handling) on click.
fn action_row<A: Action + Clone>(
    cx: &Context<AboutSettings>,
    label_id: &'static str,
    title: impl Into<SharedString>,
    subtext: impl Into<SharedString>,
    button_id: &'static str,
    button_label: impl Into<SharedString>,
    action: A,
) -> Label {
    label(label_id, title).subtext(subtext).w_full().child(
        button()
            .style(ButtonStyle::Regular)
            .intent(ButtonIntent::Secondary)
            .child(Into::<SharedString>::into(button_label))
            .id(button_id)
            .on_click(cx.listener(move |_, _, _, cx| {
                let action = action.clone();
                cx.defer(move |cx| cx.dispatch_action(&action));
            })),
    )
}

#[cfg(feature = "online")]
/// What the contextual button of the status row does right now; `None`
/// means the state machine is busy (checking / downloading) and shows no
/// button at all.
enum UpdateAction {
    None,
    Check,
    Download,
    OpenPage(String),
    Restart,
}

#[cfg(feature = "online")]
impl AboutSettings {
    /// Applies `update` to `settings.update`, then persists the settings
    /// file and notifies (interface-page pattern).
    fn update_update_settings(
        &self,
        cx: &mut App,
        update: impl FnOnce(&mut crate::settings::update::UpdateSettings),
    ) {
        self.settings.update(cx, move |settings, cx| {
            update(&mut settings.update);
            save_settings(cx, settings);
            cx.notify();
        });
    }

    /// One checkbox row toggling one `bool` field of `UpdateSettings`.
    fn toggle_row(
        cx: &Context<Self>,
        label_id: &'static str,
        check_id: &'static str,
        title: impl Into<SharedString>,
        subtext: impl Into<SharedString>,
        checked: bool,
        toggle: fn(&mut crate::settings::update::UpdateSettings),
    ) -> Label {
        label(label_id, title)
            .subtext(subtext)
            .cursor_pointer()
            .w_full()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_update_settings(cx, toggle);
            }))
            .child(checkbox(check_id, checked))
    }

    /// `(status subtext, contextual button action)` for the updater's
    /// current state.
    fn update_status(cx: &App) -> (SharedString, UpdateAction) {
        let status = if cx.has_global::<UpdaterGlobal>() {
            cx.global::<UpdaterGlobal>().state.read(cx).status.clone()
        } else {
            UpdateStatus::Idle
        };
        match status {
            UpdateStatus::Idle => (
                tr!("UPDATE_STATUS_IDLE", "Never checked").into(),
                UpdateAction::Check,
            ),
            UpdateStatus::Checking => (
                tr!("UPDATE_STATUS_CHECKING", "Checking for updates…").into(),
                UpdateAction::None,
            ),
            UpdateStatus::UpToDate => (
                tr!("UPDATE_STATUS_UP_TO_DATE", "You're on the latest version").into(),
                UpdateAction::Check,
            ),
            UpdateStatus::Failed => (
                tr!(
                    "UPDATE_STATUS_FAILED",
                    "Update check failed — check your network and try again"
                )
                .into(),
                UpdateAction::Check,
            ),
            UpdateStatus::Available {
                version,
                url,
                binary_url,
                ..
            } => (
                tr!(
                    "UPDATE_STATUS_AVAILABLE",
                    "Version {{version}} is available",
                    version = version
                )
                .into(),
                match binary_url {
                    Some(_) => UpdateAction::Download,
                    None => UpdateAction::OpenPage(url),
                },
            ),
            UpdateStatus::Downloading {
                received, total, ..
            } => {
                let percent = if total > 0 {
                    (received * 100 / total) as u32
                } else {
                    0
                };
                (
                    tr!(
                        "UPDATE_STATUS_DOWNLOADING",
                        "Downloading update… {{percent}}%",
                        percent = percent
                    )
                    .into(),
                    UpdateAction::None,
                )
            }
            UpdateStatus::ReadyToRestart { version } => (
                tr!(
                    "UPDATE_STATUS_READY",
                    "Version {{version}} downloaded — restart to apply",
                    version = version
                )
                .into(),
                UpdateAction::Restart,
            ),
        }
    }

    /// The status row: current version, live updater state, and the one
    /// contextual action for that state.
    fn update_status_row(&self, cx: &mut Context<Self>) -> Label {
        let (status_text, action) = Self::update_status(cx);
        let row = label(
            "update-status",
            tr!(
                "UPDATE_CURRENT",
                "Current version: {{version}}",
                version = crate::VERSION_STRING
            ),
        )
        .subtext(status_text)
        .w_full();

        let action_button = match action {
            UpdateAction::None => None,
            UpdateAction::Check => Some(
                button()
                    .style(ButtonStyle::Regular)
                    .intent(ButtonIntent::Secondary)
                    .id("update-check-button")
                    .child(tr!("UPDATE_CHECK_NOW", "Check Now"))
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::updater::check_for_updates(cx, CheckSource::Manual);
                    })),
            ),
            UpdateAction::Download => Some(
                button()
                    .style(ButtonStyle::Regular)
                    .intent(ButtonIntent::Secondary)
                    .id("update-download-button")
                    .child(tr!("UPDATE_DOWNLOAD_NOW", "Download and Install"))
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::updater::start_download(cx);
                    })),
            ),
            UpdateAction::OpenPage(url) => Some(
                button()
                    .style(ButtonStyle::Regular)
                    .intent(ButtonIntent::Secondary)
                    .id("update-open-page-button")
                    .child(tr!("UPDATE_OPEN_RELEASE", "View Release Page"))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            ),
            UpdateAction::Restart => Some(
                button()
                    .style(ButtonStyle::Regular)
                    .intent(ButtonIntent::Secondary)
                    .id("update-restart-button")
                    .child(tr!("UPDATE_RESTART_NOW", "Restart Now"))
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::updater::restart_to_apply(cx);
                    })),
            ),
        };
        row.children(action_button)
    }
}

impl Render for AboutSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section_header(tr!("ABOUT_SECTION")))
            .child(action_row(
                cx,
                "about-open-about",
                tr!("ABOUT"),
                tr!(
                    "ABOUT_SECTION_ABOUT_SUBTEXT",
                    "Show version, source and license information."
                ),
                "open-about-button",
                tr!("ABOUT_OPEN", "Open About"),
                About,
            ))
            .child(action_row(
                cx,
                "about-report-issue",
                tr!("CODEBERG_ISSUES", "Report an Issue"),
                tr!(
                    "ABOUT_SECTION_ISSUE_SUBTEXT",
                    "Report a bug or share feedback on the project's issue tracker."
                ),
                "report-issue-button",
                tr!("CODEBERG_ISSUES_OPEN", "Open Issue Tracker"),
                Issues,
            ))
            .child(action_row(
                cx,
                "about-copy-troubleshooting",
                tr!("ACTION_COPY_TROUBLESHOOTING_INFO"),
                tr!(
                    "ABOUT_SECTION_COPY_SUBTEXT",
                    "Copy system and version details to the clipboard for bug reports."
                ),
                "copy-troubleshooting-button",
                tr!("ACTION_COPY", "Copy"),
                CopyTroubleshootingInfo,
            ))
            .child(action_row(
                cx,
                "about-open-log",
                tr!("ACTION_OPEN_LOG"),
                tr!(
                    "ABOUT_SECTION_LOG_SUBTEXT",
                    "Open the application's log file to troubleshoot problems."
                ),
                "open-log-button",
                tr!("OPEN_LOG_OPEN", "Open Log File"),
                OpenLog,
            ));

        #[cfg(feature = "online")]
        {
            page = page
                .child(section_header(tr!("UPDATE_SECTION", "Updates")))
                .child(self.update_status_row(cx))
                .child(Self::toggle_row(
                    cx,
                    "update-auto-check-label",
                    "update-auto-check",
                    tr!("UPDATE_AUTO_CHECK", "Check for updates on startup"),
                    tr!(
                        "UPDATE_AUTO_CHECK_SUBTEXT",
                        "Check GitHub for a new release a few seconds after the app starts."
                    ),
                    self.settings.read(cx).update.auto_check,
                    |update| update.auto_check = !update.auto_check,
                ))
                .child(Self::toggle_row(
                    cx,
                    "update-auto-download-label",
                    "update-auto-download",
                    tr!("UPDATE_AUTO_DOWNLOAD", "Download updates automatically"),
                    tr!(
                        "UPDATE_AUTO_DOWNLOAD_SUBTEXT",
                        "Download a new release in the background as soon as it is found; \
                         installing still waits for an explicit restart."
                    ),
                    self.settings.read(cx).update.auto_download,
                    |update| update.auto_download = !update.auto_download,
                ));
        }

        page
    }
}
