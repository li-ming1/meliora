use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gpui::{prelude::FluentBuilder, *};

use crate::{
    library::types::{Track, table::TrackColumn},
    playback::{interface::PlaybackInterface, queue::QueueItemData},
    ui::{
        components::table::{Table, table_data::TABLE_MAX_WIDTH},
        library::{
            context_menus::{TrackContextMenuContext, play_from_track},
            table_shell::TableShell,
        },
    },
};

/// 从表格行构造播放队列。双击处理器与下面右键菜单的「从这里播放」共用它。
fn queue_items_from_rows(items: &[(i64, Option<i64>, String)]) -> Vec<QueueItemData> {
    items
        .iter()
        .map(|(id, album_id, path)| QueueItemData::new(PathBuf::from(path), Some(*id), *album_id))
        .collect()
}

#[derive(Clone)]
pub struct TrackView {
    shell: Entity<TableShell<Track, TrackColumn>>,
}

impl TrackView {
    pub(super) fn new(cx: &mut App, initial_scroll_offset: Option<f32>) -> Entity<Self> {
        // Weak on purpose: Table 持有的行回调会捕获这个 cell，强引用
        // Entity 会成环——把整棵表钉在导航历史里无法释放。
        let table_ref = Rc::new(RefCell::new(None::<WeakEntity<Table<Track, TrackColumn>>>));
        let table_ref_clone = table_ref.clone();

        let handler = Rc::new(move |cx: &mut App, id: &(i64, Option<i64>, String)| {
            let Some(table) = table_ref_clone.borrow().as_ref().and_then(|w| w.upgrade()) else {
                return;
            };
            let Some(items) = table.read(cx).get_items() else {
                return;
            };

            // 这里不做逐曲目的 `Path::exists` 探测：过滤整张表就是每首曲目 stat 一个
            // 文件（大库上双击一次就是 10k 次系统调用），在此之前什么都播不了。
            // 丢失的文件改由播放引擎跳过，而行可用性已经据行数据置灰了。
            if items.is_empty() {
                return;
            }

            // 被点击行的下标直接从这次 items 扫描里得出——不再对建好的队列走第二遍。
            let index = items
                .iter()
                .position(|(row_id, _, _)| *row_id == id.0)
                .unwrap_or(0);

            let queue_items = queue_items_from_rows(&items);

            let playback = cx.global::<PlaybackInterface>();
            playback.replace_queue_with_index(queue_items, index);
            playback.play();
        });

        let context_menu_context = TrackContextMenuContext {
            show_go_to_album: true,
            show_go_to_artist: true,
            play_from_here: Some(Rc::new({
                let table_ref = table_ref.clone();
                move |cx, track| {
                    let Some(table) = table_ref.borrow().as_ref().and_then(|w| w.upgrade()) else {
                        return;
                    };
                    let Some(items) = table.read(cx).get_items() else {
                        return;
                    };

                    // 不做逐曲目的 exists() 探测（理由与上面的双击处理器相同）；
                    // 播放时跳过丢失的文件
                    let queue_items = queue_items_from_rows(&items);

                    play_from_track(cx, track, queue_items);
                }
            })),
        };

        let shell: Entity<TableShell<Track, TrackColumn>> =
            TableShell::new(cx, handler, context_menu_context, initial_scroll_offset);
        *table_ref.borrow_mut() = Some(shell.read(cx).table().downgrade());

        cx.new(|_| TrackView { shell })
    }

    pub fn get_scroll_offset(&self, cx: &App) -> f32 {
        self.shell.read(cx).get_scroll_offset(cx)
    }
}

impl Render for TrackView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let full_width = crate::ui::util::full_width_enabled(cx);

        let (header, table) = {
            let shell = self.shell.read(cx);
            (shell.header().clone(), shell.table().clone())
        };

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
                    .child(header)
                    .child(table),
            )
    }
}
