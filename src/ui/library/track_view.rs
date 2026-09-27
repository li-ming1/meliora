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
/// Builds the play queue from table rows. Shared by the double-click handler
/// and the context menu's "play from here" action below.
fn queue_items_from_rows(
    cx: &mut App,
    items: &[(i64, String, Option<i64>, String)],
) -> Vec<QueueItemData> {
    items
        .iter()
        .map(|(id, _, album_id, path)| {
            QueueItemData::new(cx, PathBuf::from(path), Some(*id), *album_id)
        })
        .collect()
}

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

            // Weak on purpose: Table 持有的行回调会捕获这个 cell，强引用
            // Entity 会成环——把整棵表钉在导航历史里无法释放。
            let table_ref = Rc::new(RefCell::new(None::<WeakEntity<Table<Track, TrackColumn>>>));
            let table_ref_clone = table_ref.clone();

            let handler = Rc::new(
                move |cx: &mut App, id: &(i64, String, Option<i64>, String)| {
                    let Some(table) = table_ref_clone.borrow().as_ref().and_then(|w| w.upgrade())
                    else {
                        return;
                    };
                    let Some(items) = table.read(cx).get_items() else {
                        return;
                    };

                    // no per-track `Path::exists` probe here: filtering
                    // the whole table stat'd one file per track (10k
                    // syscalls per double-click on a large library)
                    // before anything could start. Missing files are
                    // skipped by the playback engine instead, and row
                    // availability is already greyed from the row data.
                    if items.is_empty() {
                        return;
                    }

                    // The clicked row's index falls out of the item scan
                    // directly — no second pass over the built queue.
                    let index = items
                        .iter()
                        .position(|(row_id, _, _, _)| *row_id == id.0)
                        .unwrap_or(0);

                    let queue_items = queue_items_from_rows(cx, &items);

                    let playback = cx.global::<PlaybackInterface>();
                    playback.replace_queue_with_index(queue_items, index);
                    playback.play();
                },
            );

            let context_menu_context = TrackContextMenuContext {
                show_go_to_album: true,
                show_go_to_artist: true,
                play_from_here: Some(Rc::new({
                    let table_ref = table_ref.clone();
                    move |cx, track| {
                        let Some(table) = table_ref.borrow().as_ref().and_then(|w| w.upgrade())
                        else {
                            return;
                        };
                        let Some(items) = table.read(cx).get_items() else {
                            return;
                        };

                        // no per-track exists() probe (same reason as the
                        // double-click handler above); playback skips files
                        // that went missing
                        let queue_items = queue_items_from_rows(cx, &items);

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
            *table_ref.borrow_mut() = Some(table.downgrade());

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
