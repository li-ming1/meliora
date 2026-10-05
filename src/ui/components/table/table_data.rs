use std::{fmt::Debug, hash::Hash, sync::Arc};

use futures::future::BoxFuture;
use gpui::{
    AnyElement, App, Div, ElementId, InteractiveElement, IntoElement, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder,
};
use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;

pub use crate::ui::components::context::ContextMenuBuilder;

use super::OnSelectHandler;
use crate::ui::components::{
    context::context,
    drag_drop::{AlbumDragData, DragPreview, TrackDragData},
    managed_image::ManagedImageKey,
};
use crate::ui::theme::Theme;

#[derive(Clone, Debug)]
pub enum TableDragData {
    Track(TrackDragData),
    Album(AlbumDragData),
}

/// Attaches the drag payload's source and drag-over highlight handlers to a
/// row container (table row or grid item; stateful, i.e. carrying an `.id`).
/// Both renderers wire identical handlers, so the logic lives here next to
/// [`TableDragData`].
pub fn with_drag_handlers<E>(container: E, drag_data: Option<TableDragData>) -> E
where
    E: InteractiveElement + StatefulInteractiveElement,
{
    match drag_data {
        Some(TableDragData::Track(track_data)) => {
            let display_name = track_data.display_name.clone();
            container
                .on_drag(track_data, move |_, _, _, cx| {
                    DragPreview::new(cx, display_name.clone())
                })
                .drag_over::<TrackDragData>(|style, _, _, _| {
                    style.bg(gpui::rgba(DRAG_OVER_HIGHLIGHT))
                })
        }
        Some(TableDragData::Album(album_data)) => {
            let display_name = album_data.display_name.clone();
            container
                .on_drag(album_data, move |_, _, _, cx| {
                    DragPreview::new(cx, display_name.clone())
                })
                .drag_over::<AlbumDragData>(|style, _, _, _| {
                    style.bg(gpui::rgba(DRAG_OVER_HIGHLIGHT))
                })
        }
        None => container,
    }
}

/// Wires the shared row-selection interaction onto a stateful row/grid
/// container (previously the verbatim-duplicated block of both
/// `GridItem::render` and `TableItem::render`): click-to-select gated on a
/// handler being present AND row availability, the pointer/hover/active
/// affordances, the dimmed style for unavailable rows, and middle-click
/// dispatch to [`TableData::handle_middle_mouse`]. `row` is `None` for
/// renderers that may be constructed without a resolved row (`TableItem`);
/// `GridItem` passes `Some`.
pub fn apply_row_selection<T, C>(
    container: Stateful<Div>,
    on_select: Option<OnSelectHandler<T, C>>,
    row: Option<Arc<T>>,
    is_available: bool,
    theme: &Theme,
) -> Stateful<Div>
where
    T: TableData<C> + 'static,
    C: Column + 'static,
{
    let selectable = on_select.is_some();
    container
        .when_some(on_select, {
            let row = row.clone();
            move |div, on_select| {
                if is_available {
                    div.on_click(move |_, _, cx| {
                        let Some(row) = row.as_ref() else {
                            return;
                        };
                        let id = row.get_table_id();
                        on_select(cx, &id)
                    })
                    .cursor_pointer()
                    .hover(|this| this.bg(theme.nav_button_hover))
                    .active(|this| this.bg(theme.nav_button_active))
                } else {
                    div.cursor_default().opacity(0.5)
                }
            }
        })
        .when(!selectable && !is_available, |this| this.opacity(0.5))
        .on_aux_click(move |ev, window, cx| {
            if let Some(row_data) = row.as_ref()
                && ev.is_middle_click()
            {
                row_data.handle_middle_mouse(window, cx, GridContext::Table);
            }
        })
}

/// Hosts a built context-menu tree in the standard elevated-background
/// container. Every `menu_on_open` wrapper in the app renders its menu
/// inside this same host div (table rows, grid items, track rows, file
/// rows), so the paint is identical everywhere.
pub fn elevated_menu_element(cx: &App, menu: impl IntoElement) -> AnyElement {
    div()
        .bg(cx.global::<Theme>().elevated_background)
        .child(menu)
        .into_any_element()
}

/// Wraps a fully-built row/grid content element in the shared context-menu
/// scaffolding (previously the near-verbatim tail of both `GridItem::render`
/// and `TableItem::render`): `context(id).with(content)` whose menu tree is
/// built only when the menu actually opens (the
/// [`TableData::get_context_menu`] contract), plus the optional overlay
/// rendered as a sibling of the context element so it is not nested inside
/// the menu's `deferred` popup. With no menu, the content is returned as-is.
///
/// `fill_container` picks the two call-site variants: grid items stretch the
/// context element to the whole tile and wrap the overlay in a `size_full`
/// div; table rows keep their own width and wrap in `w_full`.
pub fn wrap_with_context_menu(
    id: impl Into<ElementId>,
    content: impl IntoElement,
    context_menu: Option<(ContextMenuBuilder, Option<AnyElement>)>,
    fill_container: bool,
) -> AnyElement {
    let Some((menu_builder, overlay)) = context_menu else {
        return content.into_any_element();
    };

    let ctx = context(id);
    let ctx = if fill_container {
        ctx.w_full().h_full()
    } else {
        ctx
    };
    let ctx = ctx
        .with(content)
        // menu tree is built only when the menu opens
        .menu_on_open(move |window, cx| {
            let menu = menu_builder(window, cx);
            elevated_menu_element(cx, menu)
        });

    match overlay {
        Some(overlay) => {
            if fill_container {
                div()
                    .size_full()
                    .child(ctx)
                    .child(overlay)
                    .into_any_element()
            } else {
                div().w_full().child(ctx).child(overlay).into_any_element()
            }
        }
        None => ctx.into_any_element(),
    }
}

/// Drag payload for column header reordering.
#[derive(Clone, Debug)]
pub struct ColumnReorderDrag {
    pub source_index: usize,
}

// table layout constants
pub const TABLE_MAX_WIDTH: f32 = 1000.0;
pub const TABLE_IMAGE_COLUMN_WIDTH: f32 = 47.0;
pub const TABLE_HEADER_HEIGHT: f32 = 36.0;

/// Columns compress proportionally when the pane narrows, but never below
/// this; past the combined floor the table scrolls horizontally instead.
pub const MIN_COLUMN_WIDTH: f32 = 48.0;

/// Width used for a column that has neither a saved width nor an entry in
/// the table's default column map.
pub const DEFAULT_COLUMN_WIDTH: f32 = 100.0;

/// Translucent highlight painted on a row/grid cell while a compatible drag
/// hovers over it (see [`with_drag_handlers`]).
pub const DRAG_OVER_HIGHLIGHT: u32 = 0x88888822;

// column resize constants
pub const COLUMN_MIN_WIDTH: f32 = 50.0;
pub const COLUMN_RESIZE_HANDLE_WIDTH: f32 = 6.0;
pub const TABLE_HEADER_GROUP: &str = "table-header-group";

pub trait Column: Clone + Copy + Debug + Hash + PartialEq + Eq + Send {
    /// Retrieves the friendly name text of the column.
    fn get_column_name(&self) -> SharedString;

    /// Returns whether this column can be resized by the user.
    /// Defaults to true.
    fn is_resizable(&self) -> bool {
        true
    }

    /// Returns whether this column can be hidden by the user.
    /// Return `false` for essential columns like "Title".
    /// Defaults to true.
    fn is_hideable(&self) -> bool {
        true
    }

    /// Returns all possible column variants for this type.
    /// Required for building the column visibility menu.
    fn all_columns() -> &'static [Self];
}

#[derive(Copy, Clone)]
pub struct TableSort<C>
where
    C: Column,
{
    pub column: C,
    pub ascending: bool,
}

/// Context in which a grid item is being displayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridContext {
    /// Inside a Table component
    Table,
    /// Standalone / outside table
    Standalone,
}

/// The TableData trait defines the interface for retrieving, sorting, and listing data for a table.
/// Implementing this trait allows a table to display data in a structured manner.
pub trait TableData<C>: Sized
where
    C: Column,
{
    type Identifier: Clone + Debug + Send + 'static;
    type ContextMenuContext: Clone;

    /// Retrieves the name of the table.
    fn get_table_name() -> SharedString;

    /// Retrieves the rows of the table. The rows are returned as a vector of identifiers, which
    /// can be used to retrieve the full row data. The sort parameter can be used to specify the
    /// sorting order of the rows.
    ///
    /// This runs off the UI thread: implementations must go through the async
    /// `db` helpers (which the caller drives on the Tokio runtime) rather than
    /// the synchronous `LibraryAccess` wrappers — their `block_on` parked the
    /// UI thread on a full-library query every time a view opened, the sort
    /// changed, or a scan completed (doctrine §2.3 / §14).
    fn get_rows(
        pool: sqlx::SqlitePool,
        sort: Option<TableSort<C>>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<Self::Identifier>>>;

    /// Retrieves a specific row of the table. The row is returned as an Arc to the table data,
    /// which can be used to retrieve the row data as SharedStrings. The id parameter is used to
    /// identify the row to retrieve.
    fn get_row(cx: &mut App, id: Self::Identifier) -> anyhow::Result<Option<Arc<Self>>>;

    /// Internal extension: batch-prefetch full rows for the given identifiers
    /// into an implementation-internal cache, so per-row `get_row` calls (one
    /// UI-thread `block_on` DB hit per newly built row past the keep-around
    /// band) hit the cache while scrolling instead. The table component calls
    /// this when the visible window moves and spawns the returned future on
    /// the async runtime — never await it on the UI thread. Returns `None`
    /// when the type has no prefetch support (default).
    fn prefetch_rows(
        _pool: sqlx::SqlitePool,
        _ids: &[Self::Identifier],
    ) -> Option<BoxFuture<'static, ()>> {
        None
    }

    /// Internal extension: drops the row-prefetch cache. The table calls this
    /// on every reload (sort change, scan completion) so cached rows can never
    /// outlive a rescan; implementations must also invalidate in-flight
    /// prefetch futures (generation guard).
    fn clear_row_cache() {}

    /// Retrieves a column from the row.
    fn get_column(&self, cx: &mut App, column: C) -> Option<SharedString>;

    /// Returns true if the rows may contain images. This is used during the layout phase to
    /// determine if placeholder covers and the header section should be displayed.
    fn has_images() -> bool;

    /// Retrieves the associated image for the row.
    fn get_image_path(&self) -> Option<SharedString>;

    /// Retrieves the full-quality key for the row, for use with `managed_image`.
    fn get_full_image_key(&self) -> Option<ManagedImageKey>;

    /// Retrieves the default column widths for the table.
    fn default_columns() -> IndexMap<C, f32, FxBuildHasher>;

    /// Returns a boolean indicating whether or not a given column should be displayed using a
    /// monospaced font.
    ///
    /// This should be true for columns that contain mostly numbers, like a date or time.
    fn column_monospace(column: C) -> bool;

    /// Retrieves a unique element id for the row. This is different from the row id, as it is
    /// used to identify the row in GPUI.
    fn get_element_id(&self) -> impl Into<ElementId>;

    /// Retrieves the table ID for the row.
    fn get_table_id(&self) -> Self::Identifier;

    /// Returns whether the row is currently available for interaction.
    fn is_available(&self, _cx: &mut App) -> bool {
        true
    }

    /// Returns drag data for this row, if dragging is supported. If None is returned, dragging is
    /// not supported. Default implementation returns None.
    fn get_drag_data(&self) -> Option<TableDragData> {
        None
    }

    /// Returns a builder for this row's context menu, plus an optional
    /// overlay (e.g. a modal) rendered outside the context popup so it is not
    /// nested inside `deferred`.
    ///
    /// The builder runs on every render while the menu is open, so its body
    /// may only read caches and snapshots — never issue blocking queries or
    /// system calls. `is_available` is the availability the row view already
    /// resolved once at construction; impls must capture it instead of
    /// re-running `is_available` (which would stat the filesystem or hit the
    /// database).
    fn get_context_menu(
        &self,
        _window: &mut Window,
        _cx: &mut App,
        _context: &Self::ContextMenuContext,
        _grid_context: GridContext,
        _is_available: bool,
    ) -> Option<(ContextMenuBuilder, Option<AnyElement>)> {
        None
    }

    /// Internal extension: same contract as [`TableData::get_context_menu`],
    /// but hands the row's shared `Arc` handle to the implementation so the
    /// returned builder can hold a refcount instead of deep-cloning the whole
    /// row on every frame. Default delegates to `get_context_menu`.
    fn get_context_menu_shared(
        row: &Arc<Self>,
        window: &mut Window,
        cx: &mut App,
        context: &Self::ContextMenuContext,
        grid_context: GridContext,
        is_available: bool,
    ) -> Option<(ContextMenuBuilder, Option<AnyElement>)> {
        row.get_context_menu(window, cx, context, grid_context, is_available)
    }

    /// Optional middle mouse button handler for this row.
    fn handle_middle_mouse(&self, _window: &mut Window, _cx: &mut App, _grid_context: GridContext) {
    }

    /// Returns true if the table supports rendering as a grid view.
    fn supports_grid_view() -> bool {
        false
    }

    /// Retrieves the content for the grid item relative to the table data.
    /// Returns a tuple of (Primary string, Optional Secondary string).
    fn get_grid_content(&self, _cx: &mut App) -> Option<(SharedString, Option<SharedString>)> {
        None
    }

    /// Retrieves the content for the grid item in a given context.
    /// Returns a tuple of (Primary string, Optional Secondary string).
    /// By default, delegates to `get_grid_content`.
    fn get_grid_content_for(
        &self,
        cx: &mut App,
        _context: GridContext,
    ) -> Option<(SharedString, Option<SharedString>)> {
        self.get_grid_content(cx)
    }
}
