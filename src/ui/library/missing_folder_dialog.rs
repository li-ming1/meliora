//! 扫描发现库文件夹已不存在时弹出的模态框：用户选择保留还是删除其中的条目，
//! 并可选地把这次选择记为设置里的默认策略。

use cntp_i18n::tr;
use gpui::{App, AppContext, Context, Entity, IntoElement, Render, Window, div};

use crate::{
    library::scan::{MissingFolderDecision, ScanEvent, ScanInterface},
    settings::{SettingsGlobal, save_settings, scan::MissingFolderPolicy},
    ui::{
        components::{
            action_dialog::{ActionDialog, ActionDialogAction, CheckboxFooter},
            button::ButtonIntent,
            icons::{FOLDER_CHECK, TRASH},
        },
        models::Models,
        models::SettingsHealth,
    },
};

pub struct MissingFolderDialog {
    remember_choice: bool,
}

impl MissingFolderDialog {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            remember_choice: false,
        })
    }

    /// 用户勾选「不再询问」时把 `policy` 持久化到设置；无论是否勾选都重置复选框，
    /// 所以下一次对话框从「未勾选」开始。
    fn maybe_persist_policy(&mut self, policy: MissingFolderPolicy, cx: &mut Context<Self>) {
        if self.remember_choice {
            let settings = cx.global::<SettingsGlobal>().model.clone();
            settings.update(cx, |settings, cx| {
                settings.scanning.missing_folder_policy = policy;
                save_settings(cx, settings);
                cx.notify();
            });
        }

        self.remember_choice = false;
    }

    fn resolve_action(&mut self, decision: MissingFolderDecision, cx: &mut Context<Self>) {
        let policy = match decision {
            MissingFolderDecision::KeepInLibrary => MissingFolderPolicy::KeepInLibrary,
            MissingFolderDecision::DeleteFromLibrary => MissingFolderPolicy::DeleteFromLibrary,
        };
        self.maybe_persist_policy(policy, cx);
        cx.global::<ScanInterface>()
            .resolve_missing_folders(decision);
        cx.notify();
    }
}

impl Render for MissingFolderDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 由根视图（app.rs）无条件挂载；隐藏状态渲染一个空 div，契约与
        // `CorruptSettingsDialog` 相同。若改为在根视图里读 `scan_state`，整个窗口
        // 装饰就会按扫描进度频率（250 ms）重渲染。损坏设置对话框的优先级镜像了
        // 根视图原来的 `!show_corrupt_settings_dialog &&` 门。
        let models = cx.global::<Models>();
        if matches!(
            *models.settings_health.read(cx),
            SettingsHealth::Corrupt { .. }
        ) {
            return div().into_any_element();
        }
        let paths = match models.scan_state.read(cx) {
            ScanEvent::WaitingForMissingFolderDecision { paths } => paths.clone(),
            _ => return div().into_any_element(),
        };

        let remember_choice = self.remember_choice;
        let mut footer = CheckboxFooter::new(
            "missing-folder-dont-ask-again",
            remember_choice,
            tr!("SCANNING_MISSING_DIALOG_DONT_ASK_AGAIN", "Don't ask again"),
            cx.listener(|this, _, _, cx| {
                this.remember_choice = !this.remember_choice;
                cx.notify();
            }),
        );
        if remember_choice {
            footer = footer.hint(tr!(
                "SCANNING_MISSING_DIALOG_DONT_ASK_HINT",
                "You can change this later in Settings > Library."
            ));
        }

        ActionDialog::new(
            tr!("SCANNING_MISSING_DIALOG_TITLE", "Missing library folders"),
            tr!(
                "SCANNING_MISSING_DIALOG_BODY",
                "One or more folders in your library are missing. What would you like to do with \
                the items in those folders?"
            ),
        )
        .paths(paths.iter().map(|p| p.to_string()))
        .action(
            ActionDialogAction::new(
                "missing-folder-keep",
                FOLDER_CHECK,
                tr!("SCANNING_MISSING_DIALOG_KEEP", "Keep in Library"),
                ButtonIntent::Secondary,
                cx.listener(|this, _, _, cx| {
                    this.resolve_action(MissingFolderDecision::KeepInLibrary, cx);
                }),
            )
            .subtitle(tr!(
                "SCANNING_MISSING_DIALOG_KEEP_SUBTITLE",
                "Keep the missing albums and tracks. You won't be able to listen to them until \
                the folder is returned or the device is reconnected, but they'll remain in your \
                library and playlists."
            )),
        )
        .action(
            ActionDialogAction::new(
                "missing-folder-delete",
                TRASH,
                tr!("SCANNING_MISSING_DIALOG_DELETE", "Delete items"),
                ButtonIntent::Danger,
                cx.listener(|this, _, _, cx| {
                    this.resolve_action(MissingFolderDecision::DeleteFromLibrary, cx);
                }),
            )
            .subtitle(tr!(
                "SCANNING_MISSING_DIALOG_DELETE_SUBTITLE",
                "Remove the tracks and albums from the missing folder now. They will be removed \
                from your library and playlists."
            )),
        )
        .footer(footer)
        .into_any_element()
    }
}
