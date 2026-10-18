use cntp_i18n::tr;
use gpui::{App, AppContext, Context, Entity, IntoElement, Render, Window, div};

use crate::ui::{
    components::{
        action_dialog::{ActionDialog, ActionDialogAction, Severity},
        button::ButtonIntent,
        icons::{CROSS, FOLDER_SEARCH},
    },
    models::{Models, SettingsHealth},
    util::reveal_path_for_file_manager,
};

pub struct CorruptSettingsDialog;

impl CorruptSettingsDialog {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self)
    }
}

impl Render for CorruptSettingsDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 由根视图（app.rs）无条件挂载；用借用匹配，这样健康路径（绝大多数情形）
        // 什么都不克隆。
        let path = match cx.global::<Models>().settings_health.read(cx) {
            SettingsHealth::Corrupt { path } => path.clone(),
            SettingsHealth::Ok => return div().into_any_element(),
        };
        let path_display = path.display().to_string();

        ActionDialog::new(
            tr!(
                "SETTINGS_CORRUPT_DIALOG_TITLE",
                "Settings couldn't be loaded"
            ),
            tr!(
                "SETTINGS_CORRUPT_DIALOG_BODY",
                "Your settings file exists but couldn't be read. The scanner is paused to protect \
                your library. Edit the file to fix it - the app will reload it \
                automatically - or quit to investigate."
            ),
        )
        .severity(Severity::Danger)
        .paths([path_display])
        .action(
            ActionDialogAction::new(
                "settings-corrupt-show",
                FOLDER_SEARCH,
                tr!("SETTINGS_CORRUPT_DIALOG_SHOW", "Show settings file"),
                ButtonIntent::Secondary,
                move |_, _, cx| reveal_path_for_file_manager(path.as_path(), cx),
            )
            .subtitle(tr!(
                "SETTINGS_CORRUPT_DIALOG_SHOW_SUBTITLE",
                "Open the settings folder so you can edit the file. The app will reload \
                automatically once it's valid."
            )),
        )
        .action(
            ActionDialogAction::new(
                "settings-corrupt-quit",
                CROSS,
                tr!("SETTINGS_CORRUPT_DIALOG_QUIT", "Quit"),
                ButtonIntent::Danger,
                |_, _, cx| cx.quit(),
            )
            .subtitle(tr!(
                "SETTINGS_CORRUPT_DIALOG_QUIT_SUBTITLE",
                "Close the app without changing anything. Your settings file is preserved."
            )),
        )
        .into_any_element()
    }
}
