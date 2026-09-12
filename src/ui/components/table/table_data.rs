use std::{fmt::Debug, hash::Hash, sync::Arc};

use futures::future::BoxFuture;
use gpui::{AnyElement, App, ElementId, SharedString, Window};
use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;

pub use crate::ui::components::context::ContextMenuBuilder;

use crate::ui::components::{
    drag_drop::{AlbumDragData, TrackDragData},
    managed_image::ManagedImageKey,
};

#[derive(Clone, Debug)]
pub enum TableDragData {
    Track(TrackDragData),
    Album(AlbumDragData),
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

    /// Returns a lazy builder for this row's context menu, plus an optional
    /// overlay (e.g. a modal) rendered outside the context popup so it is not
    /// nested inside `deferred`.
    ///
    /// The builder is invoked only when the user actually opens the menu, so
    /// the menu tree (and anything its render touches — DB queries, filesystem
    /// stats, translations) stays off the per-row repaint path. `is_available`
    /// is the availability the row view already resolved once at construction;
    /// impls must capture it instead of re-running `is_available` (which would
    /// stat the filesystem or hit the database).
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
