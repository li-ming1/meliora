use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, NaiveDate, Utc};
use cntp_i18n::{Date, I18N_MANAGER, StringModifier, tr};
use futures::future::BoxFuture;
use gpui::{App, SharedString};
use indexmap::IndexMap;
use rustc_hash::{FxBuildHasher, FxHashMap};

use super::{
    Album, ArtistWithCounts, DATE_PRECISION_FULL_DATE, DATE_PRECISION_YEAR,
    DATE_PRECISION_YEAR_MONTH, DBString, Track,
};
use crate::{
    library::db::{self, AlbumSortMethod, ArtistSortMethod, LibraryAccess, TrackSortMethod},
    ui::{
        availability::is_online_path,
        components::{
            drag_drop::{AlbumDragData, TrackDragData},
            managed_image::ManagedImageKey,
            table::table_data::{
                Column, ContextMenuBuilder, GridContext, TableData, TableDragData, TableSort,
            },
        },
        library::context_menus::{
            AlbumContextMenuContext, TrackContextMenuContext, album_menu_for_table_shared,
            play_album_next, play_track_next, track_menu_for_table_shared,
        },
        models::{Models, cached_album},
        util::format_duration,
    },
};

fn parse_album_release_date(release_date: &DBString) -> Option<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(release_date.0.as_ref(), "%Y-%m-%d").ok()?;
    Some(DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

fn album_release_date_format(precision: i32) -> Option<(&'static str, &'static str)> {
    match precision {
        DATE_PRECISION_YEAR => Some(("Y", "medium")),
        DATE_PRECISION_YEAR_MONTH => Some(("YM", "medium")),
        DATE_PRECISION_FULL_DATE => Some(("YMD", "medium")),
        _ => None,
    }
}

fn format_album_release_date_with(
    release_date: Option<&DBString>,
    format: &'static str,
    length: &'static str,
) -> Option<SharedString> {
    let release_date = parse_album_release_date(release_date?)?;
    let format_var = (None, format);
    let length_var = (Some("length"), length);
    let variables = [&format_var, &length_var];
    let locale = &I18N_MANAGER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .locale;
    Some(Date.transform(locale, &release_date, &variables).into())
}

fn format_album_release_date(
    release_date: Option<&DBString>,
    date_precision: Option<i32>,
) -> Option<SharedString> {
    let (format, length) = album_release_date_format(date_precision?)?;
    format_album_release_date_with(release_date, format, length)
}

/// Upper bound of a row prefetch cache (simple FIFO, no LRU dependency): two
/// full ±256-row prefetch windows plus slack.
const ROW_CACHE_CAPACITY: usize = 1024;

/// Row prefetch cache shared by every table type: past the keep-around band
/// each newly built row resolves itself through `TableData::get_row`, which is
/// one UI-thread `RUNTIME.block_on` DB hit per row (`cx.get_track_by_id` and
/// friends). The table component batch-prefetches the visible window ± 256
/// rows on the async runtime into the type's cache whenever the window moves;
/// `get_row` reads it first and only falls back to the blocking path on a
/// miss.
///
/// `generation` invalidates in-flight prefetch tasks: `clear_row_cache` runs
/// on every table reload (sort change, scan completion) and bumps it, so a
/// task started before the reload can never write pre-rescan rows back.
struct RowCache<Id, Row> {
    generation: u64,
    order: VecDeque<Id>,
    rows: FxHashMap<Id, Arc<Row>>,
}

impl<Id: Eq + std::hash::Hash, Row> RowCache<Id, Row> {
    fn empty() -> Self {
        Self {
            generation: 0,
            order: VecDeque::new(),
            rows: FxHashMap::default(),
        }
    }
}

/// Cached row lookup; a short `Mutex` critical section, per doctrine §18.
fn cached_row<Id: Eq + std::hash::Hash, Row>(
    cache: &Mutex<RowCache<Id, Row>>,
    id: Id,
) -> Option<Arc<Row>> {
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rows
        .get(&id)
        .cloned()
}

/// FIFO-bounded insert; returns false when the cache was cleared mid-prefetch
/// (stale generation), which tells the prefetch task to stop early.
fn insert_cached_row<Id: Eq + std::hash::Hash + Clone, Row>(
    cache: &Mutex<RowCache<Id, Row>>,
    id: Id,
    row: Arc<Row>,
    generation: u64,
) -> bool {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.generation != generation {
        return false;
    }

    if cache.rows.contains_key(&id) {
        return true;
    }

    if cache.order.len() >= ROW_CACHE_CAPACITY
        && let Some(oldest) = cache.order.pop_front()
    {
        cache.rows.remove(&oldest);
    }
    cache.order.push_back(id.clone());
    cache.rows.insert(id, row);
    true
}

/// Drops a row cache's contents and invalidates in-flight prefetch tasks.
fn clear_row_cache<Id, Row>(cache: &Mutex<RowCache<Id, Row>>) {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.rows.clear();
    cache.order.clear();
    cache.generation = cache.generation.wrapping_add(1);
}

/// Generation snapshot read once before a prefetch loop, so inserts can
/// detect a mid-flight clear.
fn row_cache_generation<Id, Row>(cache: &Mutex<RowCache<Id, Row>>) -> u64 {
    cache.lock().unwrap_or_else(|e| e.into_inner()).generation
}

fn track_row_cache() -> &'static Mutex<RowCache<i64, Track>> {
    static CACHE: OnceLock<Mutex<RowCache<i64, Track>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RowCache::empty()))
}

fn album_row_cache() -> &'static Mutex<RowCache<i64, Album>> {
    static CACHE: OnceLock<Mutex<RowCache<i64, Album>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RowCache::empty()))
}

fn artist_row_cache() -> &'static Mutex<RowCache<i64, ArtistWithCounts>> {
    static CACHE: OnceLock<Mutex<RowCache<i64, ArtistWithCounts>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RowCache::empty()))
}

/// 行预取分块的绑定变量数上限：留在 SQLite 变量上限（旧版构建 999）之下，
/// 同 db.rs 的 `PLAYLIST_IN_CHUNK` 先例。
const ROW_PREFETCH_IN_CHUNK: usize = 900;

/// 分块预取的行契约：SELECT 前缀与主键读取由各类型自带，与 queries/ 下
/// 对应单行查询互引防漂移。
trait PrefetchRow: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> {
    /// `SELECT … FROM <table>`（不含 WHERE），列集逐列对齐 queries/ 下对应
    /// 单行查询的 SELECT，单侧改动即漂移。
    const CHUNK_SQL_PREFIX: &'static str;
    /// WHERE 子句里的 id 列（带表别名时含限定）。
    const ID_COLUMN: &'static str;
    /// 分块查询失败日志用的表标签。
    const LOG_LABEL: &'static str;

    /// IN 查询的返回顺序与请求顺序无关，主键必须取自行本身。
    fn row_id(&self) -> i64;
}

impl PrefetchRow for Album {
    // 列集逐列对齐 queries/library/find_album_metadata_by_id.sql 的 SELECT
    const CHUNK_SQL_PREFIX: &'static str = "SELECT id, title, title_sortable, \
        NULLIF(artist_display_override, '') AS artist_display_override, release_date, \
        date_precision, created_at, label, catalog_number, isrc, vinyl_numbering FROM album";
    const ID_COLUMN: &'static str = "id";
    const LOG_LABEL: &'static str = "album";

    fn row_id(&self) -> i64 {
        self.id
    }
}

impl PrefetchRow for Track {
    // 列集逐列对齐 queries/library/find_track_by_id.sql 的 SELECT
    const CHUNK_SQL_PREFIX: &'static str = "SELECT id, title, album_id, track_number, \
        disc_number, duration, location, artist_names, disc_subtitle FROM track";
    const ID_COLUMN: &'static str = "id";
    const LOG_LABEL: &'static str = "track";

    fn row_id(&self) -> i64 {
        self.id
    }
}

impl PrefetchRow for ArtistWithCounts {
    // 列集逐列对齐 queries/library/find_artist_with_counts_by_id.sql 的 SELECT
    const CHUNK_SQL_PREFIX: &'static str = "SELECT a.id, a.name, \
        (SELECT COUNT(*) FROM album_artist aa WHERE aa.artist_id = a.id) AS album_count, \
        (SELECT COUNT(*) FROM track t \
        JOIN album_artist aa ON t.album_id = aa.album_id \
        WHERE aa.artist_id = a.id) \
        + (SELECT COUNT(*) FROM track_artist ta WHERE ta.artist_id = a.id) AS track_count \
        FROM artist a";
    const ID_COLUMN: &'static str = "a.id";
    const LOG_LABEL: &'static str = "artist";

    fn row_id(&self) -> i64 {
        self.id
    }
}

/// 三类表格共用的行预取主体：剔除缓存已持有的 id（重叠窗口保持廉价）后，
/// 剩余 id 按 ≤`ROW_PREFETCH_IN_CHUNK` 个绑定变量分块，每块一条
/// `… WHERE id IN (…)` 整行取回——替代逐行串行 await（pool 仅 3 连接，
/// 快速滚动时排空慢，未命中行还会退化成 UI 线程逐行 `block_on`）。
/// 代际不符（预取途中 `clear_row_cache`）即中止，过期行绝不回写。
// `Send + Unpin`：fetch_all 对查询输出类型的固有要求，BoxFuture<'static, ()>
// 调用点的 future Send 也依赖它（三个行类型均为纯数据结构，自动满足）。
async fn prefetch_rows_chunked<Row: PrefetchRow + Send + Unpin>(
    cache: &'static Mutex<RowCache<i64, Row>>,
    pool: sqlx::SqlitePool,
    ids: Vec<i64>,
) {
    let mut pending = ids;
    // 一次临界区同时完成"过滤已缓存 id + 快照代际"：逐 id 调 cached_row 是
    // O(n) 次锁获取（快速滚动时 n 达数百）。std Mutex 不可重入，guard 内
    // 不能调用 cached_row / row_cache_generation，判定就地内联。
    let generation = {
        let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|id| !cache.rows.contains_key(id));
        cache.generation
    };

    for chunk in pending.chunks(ROW_PREFETCH_IN_CHUNK) {
        // 每块之间校验代际：缓存已被清空（重载）则不再发过期查询
        if row_cache_generation(cache) != generation {
            return;
        }

        // 内联动态 SQL 先例：db.rs playlist_contains_all_tracks
        let sql = format!(
            "{} WHERE {} IN ({})",
            Row::CHUNK_SQL_PREFIX,
            Row::ID_COLUMN,
            db::in_placeholders(chunk.len())
        );
        let mut query = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql));
        for &id in chunk {
            query = query.bind(id);
        }

        match query.fetch_all(&pool).await {
            Ok(rows) => {
                for row in rows {
                    if !insert_cached_row(cache, row.row_id(), Arc::new(row), generation) {
                        // 查询期间缓存被清空（重载）：停止回写过期行
                        return;
                    }
                }
            }
            Err(err) => {
                tracing::debug!(
                    table = Row::LOG_LABEL,
                    count = chunk.len(),
                    error = %err,
                    "row prefetch chunk failed"
                );
            }
        }
    }
}

/// Default per-column widths (logical pixels) in display order, shared by the
/// `default_columns` impls below.
fn default_column_widths<C>(
    widths: impl IntoIterator<Item = (C, f32)>,
) -> IndexMap<C, f32, FxBuildHasher>
where
    C: std::hash::Hash + Eq,
{
    let mut columns: IndexMap<C, f32, FxBuildHasher> = IndexMap::with_hasher(FxBuildHasher);
    columns.extend(widths);
    columns
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum AlbumColumn {
    Title,
    Artist,
    Date,
    Label,
    CatalogNumber,
}

impl Column for AlbumColumn {
    fn get_column_name(&self) -> SharedString {
        match self {
            AlbumColumn::Title => tr!("COLUMN_TITLE", "Title").into(),
            AlbumColumn::Artist => tr!("COLUMN_ARTIST", "Artist").into(),
            AlbumColumn::Date => tr!("COLUMN_DATE", "Date").into(),
            AlbumColumn::Label => tr!("COLUMN_LABEL", "Label").into(),
            AlbumColumn::CatalogNumber => tr!("COLUMN_CATALOG_NUMBER", "Catalog Number").into(),
        }
    }

    fn is_hideable(&self) -> bool {
        !matches!(self, AlbumColumn::Title)
    }

    fn all_columns() -> &'static [Self] {
        &[
            AlbumColumn::Title,
            AlbumColumn::Artist,
            AlbumColumn::Date,
            AlbumColumn::Label,
            AlbumColumn::CatalogNumber,
        ]
    }
}

impl TableData<AlbumColumn> for Album {
    type Identifier = (u32, String);
    type ContextMenuContext = AlbumContextMenuContext;

    fn get_table_name() -> SharedString {
        tr!("TABLE_ALBUMS", "Albums").into()
    }

    fn get_rows(
        pool: sqlx::SqlitePool,
        sort: Option<TableSort<AlbumColumn>>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<Self::Identifier>>> {
        let sort_method = match sort {
            Some(TableSort { column, ascending }) => match (column, ascending) {
                (AlbumColumn::Title, true) => AlbumSortMethod::TitleAsc,
                (AlbumColumn::Title, false) => AlbumSortMethod::TitleDesc,
                (AlbumColumn::Artist, true) => AlbumSortMethod::ArtistAsc,
                (AlbumColumn::Artist, false) => AlbumSortMethod::ArtistDesc,
                (AlbumColumn::Date, true) => AlbumSortMethod::ReleaseAsc,
                (AlbumColumn::Date, false) => AlbumSortMethod::ReleaseDesc,
                (AlbumColumn::Label, true) => AlbumSortMethod::LabelAsc,
                (AlbumColumn::Label, false) => AlbumSortMethod::LabelDesc,
                (AlbumColumn::CatalogNumber, true) => AlbumSortMethod::CatalogAsc,
                (AlbumColumn::CatalogNumber, false) => AlbumSortMethod::CatalogDesc,
            },
            None => AlbumSortMethod::ArtistAsc,
        };

        Box::pin(async move { Ok(db::list_albums(&pool, sort_method).await?) })
    }

    fn get_row(cx: &mut gpui::App, id: Self::Identifier) -> anyhow::Result<Option<Arc<Self>>> {
        // prefetch cache first: a hit avoids the UI-thread `block_on` below
        // (one per newly built row after scrolling past the keep-around band)
        if let Some(album) = cached_row(album_row_cache(), id.0 as i64) {
            return Ok(Some(album));
        }

        Ok(cx.get_album_by_id(id.0 as i64).ok())
    }

    fn prefetch_rows(
        pool: sqlx::SqlitePool,
        ids: &[Self::Identifier],
    ) -> Option<BoxFuture<'static, ()>> {
        let ids: Vec<i64> = ids.iter().map(|id| id.0 as i64).collect();
        if ids.is_empty() {
            return None;
        }

        Some(Box::pin(prefetch_rows_chunked(
            album_row_cache(),
            pool,
            ids,
        )))
    }

    fn clear_row_cache() {
        clear_row_cache(album_row_cache());
    }

    fn get_column(&self, _cx: &mut App, column: AlbumColumn) -> Option<SharedString> {
        match column {
            AlbumColumn::Title => Some(self.title.0.clone()),
            AlbumColumn::Artist => self.artist_display_override.as_ref().map(|v| v.0.clone()),
            AlbumColumn::Date => {
                format_album_release_date(self.release_date.as_ref(), self.date_precision)
            }
            AlbumColumn::Label => self.label.as_ref().map(|v| v.0.clone()),
            AlbumColumn::CatalogNumber => self.catalog_number.as_ref().map(|v| v.0.clone()),
        }
    }

    fn get_image_path(&self) -> Option<SharedString> {
        Some(format!("!db://album/{}/thumb", self.id).into())
    }

    fn get_full_image_key(&self) -> Option<ManagedImageKey> {
        Some(ManagedImageKey::Album(self.id))
    }

    fn has_images() -> bool {
        true
    }

    fn column_monospace(_column: AlbumColumn) -> bool {
        false
    }

    fn get_element_id(&self) -> impl Into<gpui::ElementId> {
        ("album", self.id as u32)
    }

    fn get_table_id(&self) -> Self::Identifier {
        (self.id as u32, self.title.0.clone().into())
    }

    fn default_columns() -> IndexMap<AlbumColumn, f32, FxBuildHasher> {
        default_column_widths([
            (AlbumColumn::Title, 300.0),
            (AlbumColumn::Artist, 200.0),
            (AlbumColumn::Date, 125.0),
            (AlbumColumn::Label, 150.0),
            // length is weird because the image column is 47.0
            (AlbumColumn::CatalogNumber, 178.0),
        ])
    }

    fn get_drag_data(&self) -> Option<TableDragData> {
        Some(TableDragData::Album(AlbumDragData::new(
            self.id,
            self.title.0.clone(),
        )))
    }

    fn get_context_menu(
        &self,
        window: &mut gpui::Window,
        cx: &mut App,
        context: &Self::ContextMenuContext,
        _grid_context: GridContext,
        _is_available: bool,
    ) -> Option<(ContextMenuBuilder, Option<gpui::AnyElement>)> {
        Some(album_menu_for_table_shared(
            Arc::new(self.clone()),
            context,
            window,
            cx,
        ))
    }

    fn get_context_menu_shared(
        row: &Arc<Self>,
        window: &mut gpui::Window,
        cx: &mut App,
        context: &Self::ContextMenuContext,
        _grid_context: GridContext,
        _is_available: bool,
    ) -> Option<(ContextMenuBuilder, Option<gpui::AnyElement>)> {
        // the row enters the builder as a refcount; the deep clone happens
        // only when the menu actually opens
        Some(album_menu_for_table_shared(
            row.clone(),
            context,
            window,
            cx,
        ))
    }

    fn handle_middle_mouse(
        &self,
        _window: &mut gpui::Window,
        cx: &mut App,
        _grid_context: GridContext,
    ) {
        play_album_next(cx, self);
    }

    fn supports_grid_view() -> bool {
        true
    }

    fn get_grid_content(&self, _cx: &mut App) -> Option<(SharedString, Option<SharedString>)> {
        let title = self.title.0.clone();
        let artist = self.artist_display_override.as_ref().map(|v| v.0.clone());
        Some((title, artist))
    }

    fn get_grid_content_for(
        &self,
        _cx: &mut App,
        context: GridContext,
    ) -> Option<(SharedString, Option<SharedString>)> {
        let title = self.title.0.clone();

        let artist_part: Option<String> = match context {
            GridContext::Table => self
                .artist_display_override
                .as_ref()
                .map(|v| v.0.to_string()),
            GridContext::Standalone => None,
        };

        let secondary = match artist_part {
            Some(artist) => {
                format_album_release_date_with(self.release_date.as_ref(), "Y", "medium")
                    .map(|year| format!("{artist} • {year}").into())
                    .or(Some(SharedString::from(artist)))
            }
            None => format_album_release_date(self.release_date.as_ref(), self.date_precision),
        };

        Some((title, secondary))
    }

    fn is_available(&self, cx: &mut App) -> bool {
        // cached availability snapshot (reloaded at startup and on scan
        // completion): the per-row `block_on` query plus a stat per track
        // stalled row construction on every scroll
        cx.global::<Models>()
            .available_albums
            .read(cx)
            .as_ref()
            .is_some_and(|set| set.contains(&self.id))
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum TrackColumn {
    TrackNumber,
    Title,
    Album,
    Artist,
    Length,
}

impl Column for TrackColumn {
    fn get_column_name(&self) -> SharedString {
        match self {
            TrackColumn::TrackNumber => tr!("TRACK_NUMBER", "#").into(),
            TrackColumn::Title => tr!("COLUMN_TITLE").into(),
            TrackColumn::Album => tr!("COLUMN_ALBUM", "Album").into(),
            TrackColumn::Artist => tr!("COLUMN_ARTIST").into(),
            TrackColumn::Length => tr!("COLUMN_LENGTH", "Length").into(),
        }
    }

    fn is_hideable(&self) -> bool {
        !matches!(self, TrackColumn::Title)
    }

    fn all_columns() -> &'static [Self] {
        &[
            TrackColumn::TrackNumber,
            TrackColumn::Title,
            TrackColumn::Album,
            TrackColumn::Artist,
            TrackColumn::Length,
        ]
    }
}

impl TableData<TrackColumn> for Track {
    type Identifier = (i64, String, Option<i64>, String);
    type ContextMenuContext = TrackContextMenuContext;

    fn get_table_name() -> SharedString {
        tr!("TABLE_TRACKS", "Tracks").into()
    }

    fn get_rows(
        pool: sqlx::SqlitePool,
        sort: Option<TableSort<TrackColumn>>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<Self::Identifier>>> {
        let sort_method = match sort {
            Some(TableSort { column, ascending }) => match (column, ascending) {
                (TrackColumn::Title, true) => TrackSortMethod::TitleAsc,
                (TrackColumn::Title, false) => TrackSortMethod::TitleDesc,
                (TrackColumn::Artist, true) => TrackSortMethod::ArtistAsc,
                (TrackColumn::Artist, false) => TrackSortMethod::ArtistDesc,
                (TrackColumn::Album, true) => TrackSortMethod::AlbumAsc,
                (TrackColumn::Album, false) => TrackSortMethod::AlbumDesc,
                (TrackColumn::Length, true) => TrackSortMethod::DurationAsc,
                (TrackColumn::Length, false) => TrackSortMethod::DurationDesc,
                (TrackColumn::TrackNumber, true) => TrackSortMethod::TrackNumberAsc,
                (TrackColumn::TrackNumber, false) => TrackSortMethod::TrackNumberDesc,
            },
            None => TrackSortMethod::ArtistAsc,
        };

        Box::pin(async move { Ok(db::list_tracks(&pool, sort_method).await?) })
    }

    fn get_row(cx: &mut gpui::App, id: Self::Identifier) -> anyhow::Result<Option<Arc<Self>>> {
        // prefetch cache first: a hit avoids the UI-thread `block_on` below
        // (one per newly built row after scrolling past the keep-around band)
        if let Some(track) = cached_row(track_row_cache(), id.0) {
            return Ok(Some(track));
        }

        Ok(cx.get_track_by_id(id.0).ok())
    }

    fn get_column(&self, cx: &mut App, column: TrackColumn) -> Option<SharedString> {
        match column {
            TrackColumn::TrackNumber => {
                // cached album metadata: rebuilding a row used to issue one
                // `get_album_by_id` block_on per rendered column
                let vinyl_numbering = self
                    .album_id
                    .and_then(|id| cached_album(cx, id))
                    .map(|album| album.vinyl_numbering)
                    .unwrap_or(false);

                match (self.disc_number, self.track_number) {
                    (Some(disc), Some(track)) => {
                        if vinyl_numbering {
                            let side = (b'A' + (disc - 1) as u8) as char;
                            Some(format!("{}{}", side, track).into())
                        } else {
                            Some(format!("{}-{}", disc, track).into())
                        }
                    }
                    (None, Some(track)) => Some(track.to_string().into()),
                    _ => None,
                }
            }
            TrackColumn::Title => Some(self.title.0.clone()),
            TrackColumn::Album => {
                if let Some(album_id) = self.album_id {
                    cached_album(cx, album_id).map(|v| v.title.0.clone())
                } else {
                    None
                }
            }
            TrackColumn::Artist => {
                if let Some(artist) = &self.artist_names {
                    Some(artist.0.clone())
                } else if let Some(album_id) = self.album_id {
                    cached_album(cx, album_id).and_then(|album| {
                        album.artist_display_override.as_ref().map(|v| v.0.clone())
                    })
                } else {
                    None
                }
            }
            TrackColumn::Length => Some(format_duration(self.duration, true).into()),
        }
    }

    fn get_image_path(&self) -> Option<SharedString> {
        // every track has its own artwork association (shared with the album when identical)
        Some(format!("!db://track/{}/thumb", self.id).into())
    }

    fn get_full_image_key(&self) -> Option<ManagedImageKey> {
        Some(ManagedImageKey::Track(self.id))
    }

    fn has_images() -> bool {
        true
    }

    fn column_monospace(_column: TrackColumn) -> bool {
        false
    }

    fn get_element_id(&self) -> impl Into<gpui::ElementId> {
        ("track", self.id as u32)
    }

    fn get_table_id(&self) -> Self::Identifier {
        (
            self.id,
            self.title.0.clone().into(),
            self.album_id,
            self.location.to_string_lossy().to_string(),
        )
    }

    fn default_columns() -> IndexMap<TrackColumn, f32, FxBuildHasher> {
        default_column_widths([
            (TrackColumn::TrackNumber, 75.0),
            (TrackColumn::Title, 350.0),
            (TrackColumn::Album, 250.0),
            (TrackColumn::Artist, 225.0),
            (TrackColumn::Length, 100.0),
        ])
    }

    fn get_drag_data(&self) -> Option<TableDragData> {
        Some(TableDragData::Track(TrackDragData::from_track(
            self.id,
            self.album_id,
            self.location.clone(),
            self.title.0.clone(),
        )))
    }

    fn is_available(&self, cx: &mut App) -> bool {
        // HTTP(S) streams have no file on disk and the availability snapshot
        // only knows local paths: online tracks must short-circuit to
        // available exactly like is_track_path_available does
        if is_online_path(&self.location) {
            return true;
        }

        // cached availability snapshot (reloaded at startup and on scan
        // completion, same pass as the album/artist sets): the per-row
        // `path.exists()` stat stalled row construction on every scroll.
        // Until the first reload lands, though, fall back to the exact
        // pre-snapshot behavior — `None` must not read as "unavailable",
        // because rows built in that startup window capture the verdict
        // once and stay greyed-out/unclickable until a full table reload.
        match cx.global::<Models>().available_tracks.read(cx).as_ref() {
            Some(set) => set.contains(&self.id),
            None => self.location.exists(),
        }
    }

    fn get_context_menu(
        &self,
        window: &mut gpui::Window,
        cx: &mut App,
        context: &Self::ContextMenuContext,
        _grid_context: GridContext,
        is_available: bool,
    ) -> Option<(ContextMenuBuilder, Option<gpui::AnyElement>)> {
        // `is_available` is resolved once at row construction and captured by
        // the builder: the menu tree itself is only built when the menu opens,
        // so re-statting the file per repaint is both unnecessary and wrong.
        Some(track_menu_for_table_shared(
            Arc::new(self.clone()),
            is_available,
            context,
            window,
            cx,
        ))
    }

    fn get_context_menu_shared(
        row: &Arc<Self>,
        window: &mut gpui::Window,
        cx: &mut App,
        context: &Self::ContextMenuContext,
        _grid_context: GridContext,
        is_available: bool,
    ) -> Option<(ContextMenuBuilder, Option<gpui::AnyElement>)> {
        // the row enters the builder as a refcount; the deep clone happens
        // only when the menu actually opens
        Some(track_menu_for_table_shared(
            row.clone(),
            is_available,
            context,
            window,
            cx,
        ))
    }

    fn prefetch_rows(
        pool: sqlx::SqlitePool,
        ids: &[Self::Identifier],
    ) -> Option<BoxFuture<'static, ()>> {
        let ids: Vec<i64> = ids.iter().map(|id| id.0).collect();
        if ids.is_empty() {
            return None;
        }

        Some(Box::pin(prefetch_rows_chunked(
            track_row_cache(),
            pool,
            ids,
        )))
    }

    fn clear_row_cache() {
        clear_row_cache(track_row_cache());
    }

    fn handle_middle_mouse(
        &self,
        _window: &mut gpui::Window,
        cx: &mut App,
        _grid_context: GridContext,
    ) {
        play_track_next(cx, self);
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum ArtistColumn {
    Name,
    Albums,
    Tracks,
}

impl Column for ArtistColumn {
    fn get_column_name(&self) -> SharedString {
        match self {
            ArtistColumn::Name => tr!("COLUMN_NAME", "Name").into(),
            ArtistColumn::Albums => tr!("COLUMN_ALBUMS", "# of Albums").into(),
            ArtistColumn::Tracks => tr!("COLUMN_TRACKS", "# of Tracks").into(),
        }
    }

    fn is_hideable(&self) -> bool {
        !matches!(self, ArtistColumn::Name)
    }

    fn all_columns() -> &'static [Self] {
        &[
            ArtistColumn::Name,
            ArtistColumn::Albums,
            ArtistColumn::Tracks,
        ]
    }
}

impl TableData<ArtistColumn> for ArtistWithCounts {
    type Identifier = i64;
    type ContextMenuContext = ();

    fn get_table_name() -> SharedString {
        tr!("TABLE_ARTISTS", "Artists").into()
    }

    fn get_rows(
        pool: sqlx::SqlitePool,
        sort: Option<TableSort<ArtistColumn>>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<Self::Identifier>>> {
        let sort_method = match sort {
            Some(TableSort { column, ascending }) => match (column, ascending) {
                (ArtistColumn::Name, true) => ArtistSortMethod::NameAsc,
                (ArtistColumn::Name, false) => ArtistSortMethod::NameDesc,
                (ArtistColumn::Albums, true) => ArtistSortMethod::AlbumsAsc,
                (ArtistColumn::Albums, false) => ArtistSortMethod::AlbumsDesc,
                (ArtistColumn::Tracks, true) => ArtistSortMethod::TracksAsc,
                (ArtistColumn::Tracks, false) => ArtistSortMethod::TracksDesc,
            },
            None => ArtistSortMethod::NameAsc,
        };

        Box::pin(async move { Ok(db::list_artists(&pool, sort_method).await?) })
    }

    fn get_row(cx: &mut gpui::App, id: Self::Identifier) -> anyhow::Result<Option<Arc<Self>>> {
        // prefetch cache first: a hit avoids the UI-thread `block_on` below
        // (one per newly built row after scrolling past the keep-around band)
        if let Some(artist) = cached_row(artist_row_cache(), id) {
            return Ok(Some(artist));
        }

        Ok(cx.get_artist_with_counts(id).ok())
    }

    fn prefetch_rows(
        pool: sqlx::SqlitePool,
        ids: &[Self::Identifier],
    ) -> Option<BoxFuture<'static, ()>> {
        let ids: Vec<i64> = ids.to_vec();
        if ids.is_empty() {
            return None;
        }

        Some(Box::pin(prefetch_rows_chunked(
            artist_row_cache(),
            pool,
            ids,
        )))
    }

    fn clear_row_cache() {
        clear_row_cache(artist_row_cache());
    }

    fn get_column(&self, _cx: &mut App, column: ArtistColumn) -> Option<SharedString> {
        match column {
            ArtistColumn::Name => self.name.as_ref().map(|v| v.0.clone()),
            ArtistColumn::Albums => Some(self.album_count.to_string().into()),
            ArtistColumn::Tracks => Some(self.track_count.to_string().into()),
        }
    }

    fn get_image_path(&self) -> Option<SharedString> {
        None
    }

    fn get_full_image_key(&self) -> Option<ManagedImageKey> {
        None
    }

    fn has_images() -> bool {
        false
    }

    fn column_monospace(_column: ArtistColumn) -> bool {
        false
    }

    fn get_element_id(&self) -> impl Into<gpui::ElementId> {
        ("artist", self.id as u32)
    }

    fn get_table_id(&self) -> Self::Identifier {
        self.id
    }

    fn is_available(&self, cx: &mut App) -> bool {
        // cached availability snapshot (reloaded at startup and on scan
        // completion): the per-row `get_all_tracks_by_artist` query plus a
        // stat per track stalled row construction on every scroll
        cx.global::<Models>()
            .available_artists
            .read(cx)
            .as_ref()
            .is_some_and(|set| set.contains(&self.id))
    }

    fn default_columns() -> IndexMap<ArtistColumn, f32, FxBuildHasher> {
        default_column_widths([
            (ArtistColumn::Name, 400.0),
            (ArtistColumn::Albums, 150.0),
            (ArtistColumn::Tracks, 150.0),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::{album_release_date_format, parse_album_release_date};
    use crate::library::types::{
        DATE_PRECISION_FULL_DATE, DATE_PRECISION_YEAR, DATE_PRECISION_YEAR_MONTH, DBString,
    };
    use chrono::{TimeZone, Utc};

    #[test]
    fn selects_release_date_formats_for_each_precision() {
        assert_eq!(
            album_release_date_format(DATE_PRECISION_YEAR),
            Some(("Y", "medium"))
        );
        assert_eq!(
            album_release_date_format(DATE_PRECISION_YEAR_MONTH),
            Some(("YM", "medium"))
        );
        assert_eq!(
            album_release_date_format(DATE_PRECISION_FULL_DATE),
            Some(("YMD", "medium"))
        );
    }

    #[test]
    fn parses_stored_release_dates_at_utc_midnight() {
        assert_eq!(
            parse_album_release_date(&DBString::from("1995-06-01")),
            Some(Utc.with_ymd_and_hms(1995, 6, 1, 0, 0, 0).single().unwrap())
        );
    }
}
