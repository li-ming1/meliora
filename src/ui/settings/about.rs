use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

use crate::ui::{
    components::{
        button::{ButtonIntent, ButtonStyle, button},
        label::label,
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

impl Render for AboutSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section_header(tr!("ABOUT_SECTION")))
            .child(
                label("about-open-about", tr!("ABOUT"))
                    .subtext(tr!(
                        "ABOUT_SECTION_ABOUT_SUBTEXT",
                        "Show version, source and license information."
                    ))
                    .w_full()
                    .child(
                        button()
                            .style(ButtonStyle::Regular)
                            .intent(ButtonIntent::Secondary)
                            .child(tr!("ABOUT_OPEN", "Open About"))
                            .id("open-about-button")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.defer(move |cx| cx.dispatch_action(&About));
                            })),
                    ),
            )
            .child(
                label(
                    "about-report-issue",
                    tr!("CODEBERG_ISSUES", "Report an Issue"),
                )
                .subtext(tr!(
                    "ABOUT_SECTION_ISSUE_SUBTEXT",
                    "Report a bug or share feedback on the project's issue tracker."
                ))
                .w_full()
                .child(
                    button()
                        .style(ButtonStyle::Regular)
                        .intent(ButtonIntent::Secondary)
                        .child(tr!("CODEBERG_ISSUES_OPEN", "Open Issue Tracker"))
                        .id("report-issue-button")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.defer(move |cx| cx.dispatch_action(&Issues));
                        })),
                ),
            )
            .child(
                label(
                    "about-copy-troubleshooting",
                    tr!("ACTION_COPY_TROUBLESHOOTING_INFO"),
                )
                .subtext(tr!(
                    "ABOUT_SECTION_COPY_SUBTEXT",
                    "Copy system and version details to the clipboard for bug reports."
                ))
                .w_full()
                .child(
                    button()
                        .style(ButtonStyle::Regular)
                        .intent(ButtonIntent::Secondary)
                        .child(tr!("ACTION_COPY", "Copy"))
                        .id("copy-troubleshooting-button")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.defer(move |cx| cx.dispatch_action(&CopyTroubleshootingInfo));
                        })),
                ),
            )
            .child(
                label("about-open-log", tr!("ACTION_OPEN_LOG"))
                    .subtext(tr!(
                        "ABOUT_SECTION_LOG_SUBTEXT",
                        "Open the application's log file to troubleshoot problems."
                    ))
                    .w_full()
                    .child(
                        button()
                            .style(ButtonStyle::Regular)
                            .intent(ButtonIntent::Secondary)
                            .child(tr!("OPEN_LOG_OPEN", "Open Log File"))
                            .id("open-log-button")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.defer(move |cx| cx.dispatch_action(&OpenLog));
                            })),
                    ),
            )
    }
}
