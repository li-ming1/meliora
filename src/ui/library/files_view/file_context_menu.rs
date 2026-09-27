use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use cntp_i18n::tr;
use gpui::{
    App, ClickEvent, Context, Entity, IntoElement, RenderOnce, Window, prelude::FluentBuilder,
};

use crate::{
    playback::queue::QueueItemData,
    ui::{
        components::{
            icons::{PLAY, REFRESH},
            menu::{menu, menu_item, menu_separator},
        },
        library::context_menus::{
            play_next, play_now, queue_item, track_show_in_file_manager_label,
        },
        util::reveal_path_for_file_manager,
    },
};

use super::FilesView;

/// Menu action for a file entry: queues the file as an id-less queue item
/// (path only, no DB row behind it) and runs `op` on it — the shared body of
/// the play / play-next / add-to-queue items.
fn file_menu_action(
    path: Arc<Path>,
    op: fn(&mut App, QueueItemData),
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_, _, cx| {
        let data = QueueItemData::new(cx, path.to_path_buf(), None, None);
        op(cx, data);
    }
}

/// Menu action for a folder entry: runs a `FilesView` folder operation on
/// `path`.
fn folder_menu_action(
    path: Arc<Path>,
    files_view: Entity<FilesView>,
    op: fn(&mut FilesView, PathBuf, &mut Context<FilesView>),
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_, _, cx| files_view.update(cx, |view, cx| op(view, path.to_path_buf(), cx))
}

#[derive(IntoElement)]
pub struct FileContextMenu {
    // Arc-shared with the row that spawned this menu: cloning the PathBuf the
    // menu was built from cost an allocation on every render of every row.
    path: Arc<Path>,
    is_dir: bool,
    is_audio: bool,
    is_available: bool,
    files_view: Entity<FilesView>,
}

impl FileContextMenu {
    pub fn new(
        path: Arc<Path>,
        is_dir: bool,
        is_audio: bool,
        is_available: bool,
        files_view: Entity<FilesView>,
    ) -> Self {
        Self {
            path,
            is_dir,
            is_audio,
            is_available,
            files_view,
        }
    }
}

impl RenderOnce for FileContextMenu {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let path = self.path;
        let is_dir = self.is_dir;
        let is_audio = self.is_audio;
        let is_available = self.is_available;
        let files_view = self.files_view;

        let reveal_label = track_show_in_file_manager_label();

        menu()
            .when(is_audio, |m| {
                m.item(
                    menu_item(
                        "file_play",
                        Some(PLAY),
                        tr!("PLAY"),
                        file_menu_action(path.clone(), play_now),
                    )
                    .disabled(!is_available),
                )
                .item(
                    menu_item(
                        "file_play_next",
                        None::<&'static str>,
                        tr!("PLAY_NEXT"),
                        file_menu_action(path.clone(), play_next),
                    )
                    .disabled(!is_available),
                )
                .item(
                    menu_item(
                        "file_queue",
                        None::<&'static str>,
                        tr!("ADD_TO_QUEUE"),
                        file_menu_action(path.clone(), queue_item),
                    )
                    .disabled(!is_available),
                )
                .item(menu_separator())
            })
            .when(is_dir, |m| {
                m.item(menu_item(
                    "folder_play",
                    Some(PLAY),
                    tr!("PLAY_FOLDER", "Play folder"),
                    folder_menu_action(
                        path.clone(),
                        files_view.clone(),
                        FilesView::play_folder_recursive,
                    ),
                ))
                .item(menu_item(
                    "folder_queue",
                    None::<&'static str>,
                    tr!("ADD_FOLDER_TO_QUEUE", "Add folder to queue"),
                    folder_menu_action(
                        path.clone(),
                        files_view.clone(),
                        FilesView::queue_folder_recursive,
                    ),
                ))
                .item(menu_separator())
                .item(menu_item(
                    "folder_refresh",
                    Some(REFRESH),
                    tr!("REFRESH_FOLDER", "Refresh"),
                    folder_menu_action(path.clone(), files_view.clone(), FilesView::refresh_dir),
                ))
                .item(menu_separator())
            })
            .item(menu_item(
                "file_reveal",
                None::<&'static str>,
                reveal_label,
                move |_, _, cx| reveal_path_for_file_manager(&path, cx),
            ))
    }
}
