use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gpui::{prelude::FluentBuilder, *};

use crate::{
    library::types::{Track, table::TrackColumn},
    playback::{interface::PlaybackInterface, queue::QueueItemData},
    ui::{
        components::table::{Table, table_data::TABLE_MAX_WIDTH},
        library::{
            context_menus::{TrackContextMenuContext, play_from_track},
            table_view_header::TableViewHeader,
        },
        models::Models,
    },
};
#[derive(Clone)]
pub struct TrackView {
    table_view_header: Entity<TableViewHeader<Track, TrackColumn>>,
    table: Entity<Table<Track, TrackColumn>>,
}

impl TrackView {
    pub(super) fn new(cx: &mut App, initial_scroll_offset: Option<f32>) -> Entity<Self> {
        cx.new(|cx| {
            let state = cx.global::<Models>().scan_state.clone();

            let table_settings = cx.global::<Models>().table_settings.clone();
            let initial_settings = table_settings
                .read(cx)
                .get(Table::<Track, TrackColumn>::get_table_name().as_str())
                .cloned();

            let table_ref = Rc::new(RefCell::new(None::<Entity<Table<Track, TrackColumn>>>));
            let table_ref_clone = table_ref.clone();

            let handler = Rc::new(
                move |cx: &mut App, id: &(i64, String, Option<i64>, String)| {
                    if let Some(table) = table_ref_clone.borrow().as_ref() {
                        let items = table.read(cx).get_items();
                        if let Some(items) = items {
                            // no per-track `Path::exists` probe here: filtering
                            // the whole table stat'd one file per track (10k
                            // syscalls per double-click on a large library)
                            // before anything could start. Missing files are
                            // skipped by the playback engine instead, and row
                            // availability is already greyed from the row data.
                            let queue_items: Vec<QueueItemData> = items
                                .iter()
                                .map(|(id, _, album_id, path)| {
                                    QueueItemData::new(
                                        cx,
                                        PathBuf::from(path),
                                        Some(*id),
                                        *album_id,
                                    )
                                })
                                .collect();

                            if queue_items.is_empty() {
                                return;
                            }

                            let index = queue_items
                                .iter()
                                .position(|item| item.get_db_id() == Some(id.0))
                                .unwrap_or(0);

                            let playback = cx.global::<PlaybackInterface>();
                            playback.replace_queue_with_index(queue_items, index);
                            playback.play();
                        }
                    }
                },
            );

            let context_menu_context = TrackContextMenuContext {
                show_go_to_album: true,
                show_go_to_artist: true,
                play_from_here: Some(Rc::new({
                    let table_ref = table_ref.clone();
                    move |cx, track| {
                        let table_ref_read = table_ref.borrow();
                        let Some(table) = table_ref_read.as_ref() else {
                            return;
                        };
                        let Some(items) = table.read(cx).get_items() else {
                            return;
                        };

                        // no per-track exists() probe (same reason as the
                        // double-click handler above); playback skips files
                        // that went missing
                        let queue_items = items
                            .iter()
                            .map(|(id, _, album_id, path)| {
                                QueueItemData::new(cx, PathBuf::from(path), Some(*id), *album_id)
                            })
                            .collect::<Vec<_>>();

                        play_from_track(cx, track, queue_items);
                    }
                })),
            };

            let table = Table::new(
                cx,
                Some(handler),
                context_menu_context,
                initial_scroll_offset,
                initial_settings.as_ref(),
            );
            *table_ref.borrow_mut() = Some(table.clone());

            super::observe_scan_for_table(cx, &state, table.clone());

            TrackView {
                table_view_header: TableViewHeader::new(cx, table.clone()),
                table,
            }
        })
    }

    pub fn get_scroll_offset(&self, cx: &App) -> f32 {
        self.table.read(cx).get_scroll_offset(cx)
    }
}

impl Render for TrackView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let full_width = settings.interface.effective_full_width();

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .when(!full_width, |this: Div| this.max_w(px(TABLE_MAX_WIDTH)))
            .child(
                div()
                    .pb(px(0.0))
                    .flex()
                    .flex_col()
                    .w_full()
                    .h_full()
                    .child(self.table_view_header.clone())
                    .child(self.table.clone()),
            )
    }
}
