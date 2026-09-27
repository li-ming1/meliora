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

pub struct AboutSettings;

impl AboutSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self)
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

impl Render for AboutSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
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
            ))
    }
}
