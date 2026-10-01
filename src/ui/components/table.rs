mod column_resize_handle;
pub mod grid_item;
pub mod table_data;

mod table_item;

use std::{cell::Cell, rc::Rc, sync::Arc};

use crate::ui::design::ICON_SM;
use crate::{
    settings::{
        SettingsGlobal,
        interface::clamp_grid_min_item_width,
        storage::{TableSettings, TableViewModeSetting},
    },
    ui::{
        app::Pool,
        caching::meliora_cache,
        components::{
            context::context,
            drag_drop::DragPreview,
            icons::{CHEVRON_DOWN, CHEVRON_UP, icon},
            menu::{menu, menu_check_item},
            scrollbar::{ScrollbarAxis, floating_scrollbar},
            table::table_data::TABLE_HEADER_HEIGHT,
            uniform_grid::uniform_grid,
        },
        models::Models,
        theme::Theme,
        util::{create_or_retrieve_view, prune_views},
    },
};
use column_resize_handle::column_resize_handle;
use gpui::{prelude::FluentBuilder, *};
use indexmap::IndexMap;
use rustc_hash::{FxBuildHasher, FxHashMap};
use table_data::{
    Column, ColumnReorderDrag, DEFAULT_COLUMN_WIDTH, GridContext, MIN_COLUMN_WIDTH,
    TABLE_HEADER_GROUP, TABLE_IMAGE_COLUMN_WIDTH, TableData, TableSort,
};
use table_item::TableItem;

type RowMap<T, C> = FxHashMap<usize, Entity<TableItem<T, C>>>;

#[allow(type_alias_bounds)]
pub type OnSelectHandler<T, C>
where
    C: Column,
    T: TableData<C>,
= Rc<dyn Fn(&mut App, &T::Identifier) + 'static>;

/// Prefetch band: when the visible-row center moves more than half this many
/// rows, the table batch-prefetches full rows for center ± `ROW_PREFETCH_PAD`
/// on the async runtime into the `TableData` row cache, so newly built rows
/// hit the cache instead of one UI-thread `block_on` DB hit per row.
const ROW_PREFETCH_PAD: usize = 256;

/// Schedules a background row prefetch when the visible center has moved half
/// a prefetch band since the last one. `state` tracks (rows generation,
/// center) so a stationary table costs one `Cell` read per frame; on schedule
/// the full rows for `center ± ROW_PREFETCH_PAD` are fetched on the async
/// runtime into the `TableData` row cache (cleared on every reload), turning
/// the per-row `get_row` UI-thread `block_on` into a cache hit for rows past
/// the keep-around band.
fn schedule_row_prefetch<T, C>(
    state: &Rc<Cell<(u64, usize)>>,
    generation: u64,
    center: usize,
    items: &[T::Identifier],
    cx: &mut App,
) where
    T: TableData<C>,
    C: Column,
{
    let (scheduled_generation, scheduled_center) = state.get();
    if scheduled_generation == generation
        && scheduled_center.abs_diff(center) <= ROW_PREFETCH_PAD / 2
    {
        return;
    }
    state.set((generation, center));

    let start = center.saturating_sub(ROW_PREFETCH_PAD);
    let end = (center + ROW_PREFETCH_PAD + 1).min(items.len());
    if start >= end {
        return;
    }

    let pool = cx.global::<Pool>().0.clone();
    if let Some(prefetch) = T::prefetch_rows(pool, &items[start..end]) {
        // dropping the JoinHandle detaches the task
        #[allow(clippy::let_underscore_future)]
        let _ = crate::RUNTIME.spawn(prefetch);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableViewMode {
    List,
    Grid,
}

#[derive(Clone)]
pub struct Table<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    context_menu_context: T::ContextMenuContext,
    default_columns: IndexMap<C, f32, FxBuildHasher>,
    columns: Entity<Arc<IndexMap<C, f32, FxBuildHasher>>>,
    /// 上次构建行视图时的列集（键序列）快照：columns notify 时与之比较，
    /// 区分仅宽度变化（跳过 clear_row_views）与结构变化（hide/show/reorder）
    last_column_set: Arc<IndexMap<C, f32, FxBuildHasher>>,
    /// 列宽拖拽结束（MouseUp / 双击复位）后由句柄回调的持久化入口：
    /// 拖拽期间宽度 notify 以指针上报率到达，不能每次都写设置
    persist_on_resize_end: Rc<dyn Fn(&mut App)>,
    // preserves hidden column widths, even if not shown
    hidden_column_widths: Entity<FxHashMap<C, f32>>,
    views: Entity<RowMap<T, C>>,
    render_counter: Entity<usize>,

    grid_views: Entity<FxHashMap<usize, Entity<grid_item::GridItem<T, C>>>>,
    grid_render_counter: Entity<usize>,
    view_mode: Entity<TableViewMode>,
    grid_scroll_handle: UniformListScrollHandle,

    items: Option<Arc<Vec<T::Identifier>>>,
    /// Bumped on every reload so a slow load that finishes after a newer one
    /// cannot overwrite the newer rows.
    rows_generation: u64,
    /// Last scheduled row prefetch: (rows generation, visible center). See
    /// `schedule_row_prefetch`.
    prefetch_state: Rc<Cell<(u64, usize)>>,
    sort_method: Entity<Option<TableSort<C>>>,
    on_select: Option<OnSelectHandler<T, C>>,
    list_vertical_scroll_handle: UniformListScrollHandle,
    list_horizontal_scroll_handle: ScrollHandle,
    /// Precomputed once instead of `format!`-ing the element id every frame.
    horizontal_scroll_id: SharedString,
}

pub enum TableEvent {
    NewRows,
}

impl<T, C> EventEmitter<TableEvent> for Table<T, C>
where
    T: TableData<C>,
    C: Column + 'static,
{
}

impl<T, C> Table<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    /// Reloads the row identifiers on a background task, so opening a view,
    /// changing the sort or completing a scan never runs the (full-library)
    /// query on the UI thread. The table keeps showing its previous rows until
    /// the new ones land, then rebuilds the row views. A generation guard drops
    /// a result a newer reload has already superseded, and the load time is
    /// logged so the cost stays measurable.
    fn reload_rows(&mut self, cx: &mut Context<Self>) {
        let pool = cx.global::<Pool>().0.clone();
        let sort = *self.sort_method.read(cx);
        self.rows_generation = self.rows_generation.wrapping_add(1);
        let generation = self.rows_generation;

        // prefetched rows must never outlive a rescan; the generation bump
        // inside also invalidates in-flight prefetch tasks
        T::clear_row_cache();

        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let rows = crate::RUNTIME
                .spawn(async move { T::get_rows(pool, sort).await })
                .await;
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

            let _ = this.update(cx, |this, cx| {
                // A newer reload landed first: this result is stale.
                if this.rows_generation != generation {
                    return;
                }

                match rows {
                    Ok(Ok(items)) => {
                        tracing::info!(
                            table = %T::get_table_name(),
                            rows = items.len(),
                            elapsed_ms,
                            "table rows loaded"
                        );
                        this.items = Some(Arc::new(items));
                        this.clear_row_views(cx);
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(
                            table = %T::get_table_name(),
                            error = %err,
                            "table rows query failed"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            table = %T::get_table_name(),
                            error = %err,
                            "table rows task failed"
                        );
                    }
                }

                cx.notify();
            });
        })
        .detach();
    }

    pub fn new(
        cx: &mut App,
        on_select: Option<OnSelectHandler<T, C>>,
        context_menu_context: T::ContextMenuContext,
        initial_scroll_offset: Option<f32>,
        initial_settings: Option<&TableSettings>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let (initial_columns, initial_hidden) =
                Self::build_columns_from_settings(initial_settings);

            let default_columns = T::default_columns();
            let initial_columns = Arc::new(initial_columns);
            let columns = cx.new(|_| initial_columns.clone());
            let hidden_column_widths = cx.new(|_| initial_hidden);
            let views = cx.new(|_| FxHashMap::default());
            let render_counter = cx.new(|_| 0);

            let grid_views = cx.new(|_| FxHashMap::default());
            let grid_render_counter = cx.new(|_| 0);
            let initial_view_mode = match initial_settings.map(|s| s.view_mode) {
                Some(TableViewModeSetting::Grid) => TableViewMode::Grid,
                _ => TableViewMode::List,
            };
            let view_mode = cx.new(|_| initial_view_mode);
            let grid_scroll_handle = UniformListScrollHandle::new();

            let sort_method = cx.new(|_| None);
            let list_vertical_scroll_handle = UniformListScrollHandle::new();
            let list_horizontal_scroll_handle = ScrollHandle::new();
            let prefetch_state = Rc::new(Cell::new((0_u64, 0_usize)));
            let horizontal_scroll_id =
                SharedString::from(format!("{}-horizontal-scroll", T::get_table_name()));

            let restore_scroll_offset = |handle: &UniformListScrollHandle, offset: f32| {
                handle.0.borrow().base_handle.set_offset(gpui::Point {
                    x: px(0.0),
                    y: px(-offset),
                });
            };
            if let Some(offset) = initial_scroll_offset {
                restore_scroll_offset(&list_vertical_scroll_handle, offset);
                restore_scroll_offset(&grid_scroll_handle, offset);
            }

            cx.observe(&sort_method, |this: &mut Table<T, C>, _, cx| {
                this.reload_rows(cx);
            })
            .detach();

            // 列宽拖拽结束点回调 Table 持久化，见 columns 观察者；弱引用避免
            // Table 结构体自持有 Entity<Self> 形成释放循环
            let table_entity = cx.entity().downgrade();
            let persist_on_resize_end: Rc<dyn Fn(&mut App)> = Rc::new(move |cx| {
                let _ = table_entity.update(cx, |table, cx| table.persist_settings(cx));
            });

            cx.observe(&columns, |this: &mut Table<T, C>, columns, cx| {
                // 列宽拖拽以指针上报率 notify：仅宽度变化时跳过整体重建，
                // 行实体靠自身观察者更新快照与宽度，宽度持久化挂在拖拽结束点；
                // 仅结构变化（hide/show/reorder）才清行视图并即时持久化
                let current = columns.read(cx).clone();
                let structural = current.len() != this.last_column_set.len()
                    || current
                        .keys()
                        .zip(this.last_column_set.keys())
                        .any(|(a, b)| a != b);

                if structural {
                    this.last_column_set = current;
                    this.clear_row_views(cx);
                    this.persist_settings(cx);
                }

                cx.notify();
            })
            .detach();

            cx.observe(&view_mode, |this: &mut Table<T, C>, _, cx| {
                this.persist_settings(cx);

                cx.notify();
            })
            .detach();

            cx.subscribe(&cx.entity(), |this, _, event, cx| match event {
                TableEvent::NewRows => this.reload_rows(cx),
            })
            .detach();

            let mut this = Self {
                context_menu_context,
                default_columns,
                columns,
                last_column_set: initial_columns,
                persist_on_resize_end,
                hidden_column_widths,
                views,
                render_counter,
                grid_views,
                grid_render_counter,
                view_mode,
                grid_scroll_handle,
                items: None,
                rows_generation: 0,
                prefetch_state,
                sort_method,
                on_select,
                list_vertical_scroll_handle,
                list_horizontal_scroll_handle,
                horizontal_scroll_id,
            };

            this.reload_rows(cx);
            this
        })
    }

    pub fn get_scroll_offset(&self, cx: &App) -> f32 {
        let offset = match *self.view_mode.read(cx) {
            TableViewMode::List => self
                .list_vertical_scroll_handle
                .0
                .borrow()
                .base_handle
                .offset(),
            TableViewMode::Grid => self.grid_scroll_handle.0.borrow().base_handle.offset(),
        };
        (-offset.y).into()
    }

    pub fn get_view_mode(&self, cx: &App) -> TableViewMode {
        *self.view_mode.read(cx)
    }

    pub fn set_view_mode(&mut self, view_mode: TableViewMode, cx: &mut App) {
        self.view_mode.update(cx, |mode, cx| {
            *mode = view_mode;
            cx.notify();
        });
    }

    pub fn get_items(&self) -> Option<Arc<Vec<T::Identifier>>> {
        self.items.clone()
    }

    /// 丢弃全部缓存行视图（列表+网格）。仅在行重载与列集结构变化
    /// （hide/show/reorder）时调用；仅宽度变化不清行，行实体靠自身
    /// 观察者更新快照与宽度。
    fn clear_row_views(&mut self, cx: &mut Context<Self>) {
        self.views = cx.new(|_| FxHashMap::default());
        self.render_counter = cx.new(|_| 0);
        self.grid_views = cx.new(|_| FxHashMap::default());
        self.grid_render_counter = cx.new(|_| 0);
    }

    /// Writes the current column set/widths and view mode into the shared
    /// table-settings model, keyed by table name.
    fn persist_settings(&self, cx: &mut Context<Self>) {
        let settings = self.get_settings(cx);
        let table_settings_model = cx.global::<Models>().table_settings.clone();
        table_settings_model.update(cx, |map, _| {
            map.insert(T::get_table_name().to_string(), settings);
        });
    }

    /// Copy-on-write update of the visible column map: clones the shared map,
    /// lets `apply` mutate the copy, then publishes it and notifies.
    fn update_columns(
        &self,
        cx: &mut App,
        apply: impl FnOnce(&mut IndexMap<C, f32, FxBuildHasher>),
    ) {
        self.columns.update(cx, |cols, cx| {
            let mut new_cols = (**cols).clone();
            apply(&mut new_cols);
            *cols = Arc::new(new_cols);
            cx.notify();
        });
    }

    pub fn toggle_column(&mut self, column: C, cx: &mut App) {
        if self.columns.read(cx).contains_key(&column) {
            self.hide_column(column, cx);
        } else {
            self.show_column(column, cx);
        }
    }

    pub fn hide_column(&mut self, column: C, cx: &mut App) {
        if !column.is_hideable() {
            return;
        }

        let width = self.columns.read(cx).get(&column).copied();
        if let Some(w) = width {
            self.hidden_column_widths.update(cx, |map, _| {
                map.insert(column, w);
            });
        }

        self.update_columns(cx, |cols| {
            cols.shift_remove(&column);
        });
    }

    pub fn show_column(&mut self, column: C, cx: &mut App) {
        // use the previous col widths if available
        let default_columns = T::default_columns();
        let width = self
            .hidden_column_widths
            .read(cx)
            .get(&column)
            .copied()
            .or_else(|| default_columns.get(&column).copied())
            .unwrap_or(DEFAULT_COLUMN_WIDTH);

        // insert based on default column positions
        let default_order: Vec<C> = default_columns.keys().copied().collect();
        let target_idx = default_order.iter().position(|c| *c == column).unwrap_or(0);

        self.update_columns(cx, |cols| {
            let mut insert_idx = 0;
            for (idx, key) in cols.keys().enumerate() {
                if let Some(pos) = default_order.iter().position(|c| c == key)
                    && pos < target_idx
                {
                    insert_idx = idx + 1;
                }
            }

            cols.shift_insert(insert_idx, column, width);
        });

        self.hidden_column_widths.update(cx, |map, _| {
            map.remove(&column);
        });
    }

    fn build_columns_from_settings(
        settings: Option<&TableSettings>,
    ) -> (IndexMap<C, f32, FxBuildHasher>, FxHashMap<C, f32>) {
        let default_columns = T::default_columns();

        let Some(settings) = settings else {
            return (default_columns, FxHashMap::default());
        };

        let legacy_order: Vec<String>;

        // load column order or derive from hidden_columns (legacy format)
        let column_order: &[String] = if !settings.column_order.is_empty() {
            &settings.column_order
        } else if !settings.hidden_columns.is_empty() {
            legacy_order = default_columns
                .keys()
                .filter(|c| {
                    !settings
                        .hidden_columns
                        .contains(&c.get_column_name().to_string())
                })
                .map(|c| c.get_column_name().to_string())
                .collect();
            &legacy_order
        } else {
            return (default_columns, FxHashMap::default());
        };

        let mut visible_columns = IndexMap::with_hasher(FxBuildHasher);
        let mut hidden_widths = FxHashMap::default();

        for name in column_order {
            if let Some((&col, &default_width)) = default_columns
                .iter()
                .find(|(c, _)| c.get_column_name() == name.as_str())
            {
                let width = settings
                    .column_widths
                    .get(name.as_str())
                    .copied()
                    .unwrap_or(default_width);
                visible_columns.insert(col, width);
            }
        }

        for (&col, &default_width) in &default_columns {
            if visible_columns.contains_key(&col) {
                continue;
            }
            let width = settings
                .column_widths
                .get(col.get_column_name().as_ref())
                .copied()
                .unwrap_or(default_width);
            if col.is_hideable() {
                hidden_widths.insert(col, width);
            } else {
                // Non-hideable columns always shown; append after ordered ones.
                visible_columns.insert(col, width);
            }
        }

        (visible_columns, hidden_widths)
    }

    pub fn get_settings(&self, cx: &App) -> TableSettings {
        let columns = self.columns.read(cx);
        let hidden = self.hidden_column_widths.read(cx);

        let mut column_widths = std::collections::HashMap::new();

        for (col, width) in columns.iter() {
            column_widths.insert(col.get_column_name().to_string(), *width);
        }

        for (col, width) in hidden.iter() {
            column_widths.insert(col.get_column_name().to_string(), *width);
        }

        let column_order = columns
            .iter()
            .map(|(col, _)| col.get_column_name().to_string())
            .collect();

        TableSettings {
            column_widths,
            column_order,
            view_mode: match *self.view_mode.read(cx) {
                TableViewMode::List => TableViewModeSetting::List,
                TableViewMode::Grid => TableViewModeSetting::Grid,
            },
            ..Default::default()
        }
    }

    fn reorder_column(&mut self, from: usize, to: usize, cx: &mut App) {
        self.columns.update(cx, |cols, cx| {
            let mut new_cols = (**cols).clone();
            if from == to || from >= new_cols.len() || to >= new_cols.len() {
                return;
            }
            if let Some((key, val)) = new_cols.shift_remove_index(from) {
                new_cols.shift_insert(to, key, val);
            }
            *cols = Arc::new(new_cols);
            cx.notify();
        });
    }

    pub fn get_table_name() -> SharedString {
        T::get_table_name()
    }
}

impl<T, C> Render for Table<T, C>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let sort_method = self.sort_method.read(cx);
        let items = self.items.clone();
        let view_mode = *self.view_mode.read(cx);
        let rows_generation = self.rows_generation;

        let columns_read = self.columns.read(cx);
        let column_count = columns_read.len();
        let default_columns = &self.default_columns;

        let mut header = div()
            .w_full()
            .flex()
            .id("table-header-inner")
            .group(SharedString::from(TABLE_HEADER_GROUP));

        if T::has_images() {
            header = header.child(
                div()
                    .w(px(TABLE_IMAGE_COLUMN_WIDTH))
                    .h(px(TABLE_HEADER_HEIGHT))
                    .pl(px(18.0))
                    .pr(px(10.0))
                    .py(px(2.0))
                    .text_sm()
                    .flex_shrink_0()
                    .text_ellipsis()
                    .border_b_1()
                    .border_color(theme.border_color),
            );
        }

        let drop_background_color = theme.background_tertiary;

        for (i, column) in columns_read.iter().enumerate() {
            let is_last = i == column_count - 1;
            let base_width = *column.1;
            let column_id = *column.0;
            let default_width = default_columns
                .get(&column_id)
                .copied()
                .unwrap_or(base_width);

            header = header.child(
                div()
                    .overflow_hidden()
                    .flex()
                    // fluid column: the saved width is a grow weight, so the
                    // table fills the pane exactly at any width (compresses
                    // when narrow, spreads when wide)
                    .flex_grow(base_width.max(1.0))
                    .flex_basis(px(0.0))
                    .min_w(px(MIN_COLUMN_WIDTH))
                    .h(px(TABLE_HEADER_HEIGHT))
                    .px(px(12.0))
                    .py(px(6.0))
                    .when(!T::has_images() && i == 0, |div| div.pl(px(18.0)))
                    .text_sm()
                    .border_b_1()
                    .border_color(theme.border_color)
                    .font_weight(FontWeight::BOLD)
                    .child(
                        div()
                            .flex_shrink(1.0)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(column_id.get_column_name()),
                    )
                    .when_some(sort_method.as_ref(), |this, method| {
                        this.when(method.column == column_id, |this| {
                            this.child(
                                icon(if method.ascending {
                                    CHEVRON_UP
                                } else {
                                    CHEVRON_DOWN
                                })
                                .size(ICON_SM)
                                .ml(px(4.0))
                                .flex_shrink_0()
                                .my_auto(),
                            )
                        })
                    })
                    .id(i)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sort_method.update(cx, move |this, cx| {
                            if let Some(method) = this.as_mut() {
                                if method.column == column_id {
                                    method.ascending = !method.ascending;
                                } else {
                                    *this = Some(TableSort {
                                        column: column_id,
                                        ascending: true,
                                    });
                                }
                            } else {
                                *this = Some(TableSort {
                                    column: column_id,
                                    ascending: true,
                                });
                            }

                            cx.notify();
                        })
                    }))
                    .on_drag(ColumnReorderDrag { source_index: i }, move |_, _, _, cx| {
                        DragPreview::new(cx, column_id.get_column_name())
                    })
                    .drag_over::<ColumnReorderDrag>(move |style, _, _, _| {
                        style.bg(drop_background_color)
                    })
                    .on_drop(cx.listener(move |this, drag: &ColumnReorderDrag, _, cx| {
                        this.reorder_column(drag.source_index, i, cx);
                    })),
            );

            if column_id.is_resizable() && !is_last {
                header = header.child(column_resize_handle(
                    i,
                    self.columns.clone(),
                    default_width,
                    self.persist_on_resize_end.clone(),
                ));
            }
        }

        let all_columns = C::all_columns();
        let mut column_menu = menu();
        for col in all_columns {
            let is_visible = columns_read.contains_key(col);
            let is_hideable = col.is_hideable();
            let column_copy = *col;

            column_menu = column_menu.item(
                menu_check_item(
                    col.get_column_name(),
                    is_visible,
                    col.get_column_name(),
                    cx.listener(move |this, _, _, cx| {
                        this.toggle_column(column_copy, cx);
                    }),
                )
                .disabled(!is_hideable),
            );
        }

        let header_with_context = context("table-header-context")
            .with(header)
            .child(div().bg(theme.elevated_background).child(column_menu));

        div()
            .id(T::get_table_name())
            .overflow_hidden()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .child(match view_mode {
                TableViewMode::List => {
                    let views_model = self.views.clone();
                    let render_counter = self.render_counter.clone();
                    let columns = self.columns.clone();
                    let list_context_menu_context = self.context_menu_context.clone();
                    let list_handler = self.on_select.clone();
                    let list_vertical_scroll_handle = self.list_vertical_scroll_handle.clone();
                    let list_horizontal_scroll_handle = self.list_horizontal_scroll_handle.clone();
                    let prefetch_state = self.prefetch_state.clone();

                    let mut horizontal_viewport = div()
                        .id(self.horizontal_scroll_id.clone())
                        .overflow_x_scroll()
                        .overflow_y_hidden()
                        .track_scroll(&list_horizontal_scroll_handle)
                        .flex()
                        .flex_col()
                        .flex_grow(1.0)
                        // a scroll container must be allowed to shrink below its
                        // content: without this the canvas's column-sum min-width
                        // pins the viewport to the un-shrunk table width and the
                        // resizable panel covers the overflowing part
                        .min_w(px(0.0))
                        .min_h(px(0.0));

                    // GPUI otherwise maps a vertical wheel delta onto this viewport's only
                    // scrollable axis (X). Keep Y scrolling on the inner uniform list.
                    horizontal_viewport.style().restrict_scroll_to_axis = Some(true);

                    let list_canvas = div()
                        .image_cache(meliora_cache((T::get_table_name(), 0_usize), 200))
                        .relative()
                        // no min-width here: columns are flex items and compress
                        // proportionally when the pane narrows; a min-width equal to the
                        // column sum pinned the canvas to the un-shrunk width so the
                        // resizable panel covered the overflow
                        .w_full()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(header_with_context)
                        .when_some(items, |this, items| {
                            let items_len = items.len();
                            this.child(
                                div()
                                    .relative()
                                    .w_full()
                                    .h_full()
                                    .flex_grow(1.0)
                                    .min_h(px(0.0))
                                    .child({
                                        let mut list = uniform_list(
                                            "table-list",
                                            items_len,
                                            move |range, _, cx| {
                                                let start = range.start;
                                                let is_templ_render =
                                                    range.start == 0 && range.end == 1;

                                                if !is_templ_render {
                                                    let center = (range.start + range.end) / 2;

                                                    // keep the row prefetch one band ahead
                                                    // of the visible window so new rows hit
                                                    // the row cache
                                                    schedule_row_prefetch::<T, C>(
                                                        &prefetch_state,
                                                        rows_generation,
                                                        center,
                                                        items.as_slice(),
                                                        cx,
                                                    );

                                                    // 每次窗口重建只做一次 prune（原为逐行
                                                    // 调用，O(可见行数×缓存键数) 键迭代）
                                                    prune_views(
                                                        &views_model,
                                                        &render_counter,
                                                        center,
                                                        cx,
                                                    );
                                                }

                                                items[range]
                                                    .iter()
                                                    .enumerate()
                                                    .map(|(idx, item)| {
                                                        let idx = idx + start;

                                                        div()
                                                            .w_full()
                                                            .child(create_or_retrieve_view(
                                                                &views_model,
                                                                idx,
                                                                |cx| {
                                                                    TableItem::new(
                                                                        cx,
                                                                        item.clone(),
                                                                        &columns,
                                                                        list_handler.clone(),
                                                                        list_context_menu_context
                                                                            .clone(),
                                                                    )
                                                                },
                                                                cx,
                                                            ))
                                                            .into_any_element()
                                                    })
                                                    .collect()
                                            },
                                        )
                                        .track_scroll(&list_vertical_scroll_handle)
                                        .w_full()
                                        .h_full();
                                        // GPUI otherwise maps a horizontal gesture onto this
                                        // list's only scrollable axis (Y). Keep X scrolling on
                                        // the outer viewport.
                                        list.style().restrict_scroll_to_axis = Some(true);
                                        list
                                    }),
                            )
                        });

                    horizontal_viewport.child(list_canvas).into_any_element()
                }
                TableViewMode::Grid => {
                    let grid_views_model = self.grid_views.clone();
                    let grid_render_counter = self.grid_render_counter.clone();
                    let grid_prefetch_state = self.prefetch_state.clone();
                    let grid_scroll_handle = self.grid_scroll_handle.clone();
                    let grid_context_menu_context = self.context_menu_context.clone();
                    let grid_handler = self.on_select.clone();
                    let grid_min_item_width = {
                        let settings = cx.global::<SettingsGlobal>().model.read(cx);
                        clamp_grid_min_item_width(settings.interface.grid_min_item_width)
                    };
                    let grid_padding = 10.0;

                    // 每次窗口重建只 prune 一次（首格触发；闭包随渲染每帧重建，
                    // Cell 天然逐帧复位），render_counter 仍逐格推进，band 演化
                    // 与原逐格调用一致，删除集合是其子集
                    let prune_gate = Cell::new(false);

                    div()
                        .relative()
                        .w_full()
                        .flex()
                        .h_full()
                        .px(px(grid_padding))
                        .overflow_y_hidden()
                        .when_some(items, |this, items| {
                            let items_len = items.len();
                            this.child(
                                uniform_grid(
                                    "grid-list",
                                    items_len,
                                    grid_scroll_handle.clone(),
                                    move |idx, _, cx| {
                                        if prune_gate.replace(true) {
                                            grid_render_counter.update(cx, |m, _| *m = idx);
                                        } else {
                                            prune_views(
                                                &grid_views_model,
                                                &grid_render_counter,
                                                idx,
                                                cx,
                                            );
                                        }

                                        // keep the row prefetch one band ahead of the
                                        // visible window so new rows hit the row cache
                                        schedule_row_prefetch::<T, C>(
                                            &grid_prefetch_state,
                                            rows_generation,
                                            idx,
                                            items.as_slice(),
                                            cx,
                                        );

                                        // Fallible equivalent of `create_or_retrieve_view`: a row can
                                        // vanish between get_rows and this frame (a rescan deleted it,
                                        // or its query failed), and the old `.expect` here panicked the
                                        // whole app off a stale items snapshot. The lookup result is
                                        // bound before matching so the entity's read guard is gone
                                        // before the `update` below re-borrows it.
                                        let mut view = grid_views_model.read(cx).get(&idx).cloned();
                                        if view.is_none() {
                                            view = grid_item::GridItem::new(
                                                cx,
                                                items[idx].clone(),
                                                grid_handler.clone(),
                                                grid_context_menu_context.clone(),
                                                GridContext::Table,
                                            );
                                            if let Some(built) = view.clone() {
                                                grid_views_model.update(cx, |m, _| {
                                                    m.insert(idx, built);
                                                });
                                            }
                                        }

                                        // A vanished row renders as a blank cell for this frame; the
                                        // reload already in flight replaces the stale snapshot. No
                                        // per-item image_cache here either: GridItem draws its artwork
                                        // through managed_image, which never touches the gpui image
                                        // cache the wrapper would feed.
                                        match view {
                                            Some(view) => {
                                                div().size_full().child(view).into_any_element()
                                            }
                                            None => div().into_any_element(),
                                        }
                                    },
                                )
                                .min_item_width(px(grid_min_item_width))
                                .gap(px(0.0))
                                .py(px(grid_padding)),
                            )
                            .child(
                                floating_scrollbar("grid-scrollbar", grid_scroll_handle)
                                    .right(px(4.0)),
                            )
                        })
                        .into_any_element()
                }
            })
            .when(view_mode == TableViewMode::List, |this| {
                this.child(
                    floating_scrollbar(
                        "list-vertical-scrollbar",
                        self.list_vertical_scroll_handle.clone(),
                    )
                    .top(px(TABLE_HEADER_HEIGHT))
                    .right(px(4.0))
                    .bottom(px(14.0)),
                )
                .child(
                    floating_scrollbar(
                        "list-horizontal-scrollbar",
                        self.list_horizontal_scroll_handle.clone(),
                    )
                    .axis(ScrollbarAxis::Horizontal)
                    .left(px(4.0))
                    .right(px(14.0)),
                )
            })
    }
}
