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
use crate::sync::{RecoveredMutex, RecoveredRwLock};
use crate::{
    library::db::{self, AlbumSortMethod, ArtistSortMethod, TrackSortMethod},
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
        library_access::LibraryAccess,
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
    let locale = &I18N_MANAGER.read_recovered().locale;
    Some(Date.transform(locale, &release_date, &variables).into())
}

fn format_album_release_date(
    release_date: Option<&DBString>,
    date_precision: Option<i32>,
) -> Option<SharedString> {
    let (format, length) = album_release_date_format(date_precision?)?;
    format_album_release_date_with(release_date, format, length)
}

/// 单行预取缓存的容量上界（简单 FIFO，不引 LRU 依赖）：两个完整的 ±256 行预取
/// 窗口再留些余量。
const ROW_CACHE_CAPACITY: usize = 1024;

/// 所有表格类型共用的行预取缓存：超出 keep-around 区带之后，每个新构建的行都靠
/// `TableData::get_row` 自我解析，而那是每行一次 UI 线程 `RUNTIME.block_on` 打库
/// （`cx.get_track_by_id` 一类）。表格组件在窗口移动时于异步运行时上批量预取
/// 可见窗口 ± 256 行进该类型的缓存；`get_row` 先读缓存，只有未命中才回落到阻塞路径。
///
/// `generation` 用来让已在途的预取任务失效：`clear_row_cache` 在每次表格重载
/// （排序变更、扫描完成）时都会跑并递增它，因此重载之前启动的任务永远无法把扫描前的
/// 旧行写回。
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

/// 命中缓存的行查询；`Mutex` 临界区极短，符合教条 §18。
fn cached_row<Id: Eq + std::hash::Hash, Row>(
    cache: &Mutex<RowCache<Id, Row>>,
    id: Id,
) -> Option<Arc<Row>> {
    cache.lock_recovered().rows.get(&id).cloned()
}

/// FIFO 有界插入；缓存若在预取途中被清空（代际过期）返回 false，
/// 据此告诉预取任务提前停止。
fn insert_cached_row<Id: Eq + std::hash::Hash + Clone, Row>(
    cache: &Mutex<RowCache<Id, Row>>,
    id: Id,
    row: Arc<Row>,
    generation: u64,
) -> bool {
    let mut cache = cache.lock_recovered();
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

/// 清空行缓存内容并让已在途的预取任务失效。
fn clear_row_cache<Id, Row>(cache: &Mutex<RowCache<Id, Row>>) {
    let mut cache = cache.lock_recovered();
    cache.rows.clear();
    cache.order.clear();
    cache.generation = cache.generation.wrapping_add(1);
}

/// 代际快照，在预取循环开始前读一次，好让每次插入能察觉途中的清空。
fn row_cache_generation<Id, Row>(cache: &Mutex<RowCache<Id, Row>>) -> u64 {
    cache.lock_recovered().generation
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
        let cache = cache.lock_recovered();
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

/// 按显示顺序给出的各列默认宽度（逻辑像素），供下面的 `default_columns`
/// 实现共用。
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
        // 先查预取缓存：命中就绕开下面那次 UI 线程 `block_on`
        // （滚出 keep-around 区带后，每个新建行都要付一次）
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
            // 这个宽度看着奇怪，是因为图片列占 47.0
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
        // 行以引用计数的形式进入 builder；深度克隆只在菜单真正打开时才发生
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
        // 缓存的可用性快照（启动与扫描完成时各自重载一次）：先前每行一次
        // `block_on` 查询加每曲目一次 stat，会让每次滚动的行构建都卡住
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
    type Identifier = (i64, Option<i64>, String);
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
        // 先查预取缓存：命中就绕开下面那次 UI 线程 `block_on`
        // （滚出 keep-around 区带后，每个新建行都要付一次）
        if let Some(track) = cached_row(track_row_cache(), id.0) {
            return Ok(Some(track));
        }

        Ok(cx.get_track_by_id(id.0).ok())
    }

    fn get_column(&self, cx: &mut App, column: TrackColumn) -> Option<SharedString> {
        match column {
            TrackColumn::TrackNumber => {
                // 缓存的专辑元数据：重建一行过去会为每个渲染列发一次 `get_album_by_id` block_on
                let vinyl_numbering = self
                    .album_id
                    .and_then(|id| cached_album(cx, id))
                    .map(|album| album.vinyl_numbering)
                    .unwrap_or(false);

                match (self.disc_number, self.track_number) {
                    (Some(disc), Some(track)) => {
                        if vinyl_numbering && (1..=26).contains(&disc) {
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
        // 每首曲目都有自己的封面关联（与专辑相同时即共享同一张）
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
        // HTTP(S) 流没有磁盘文件，而可用性快照只认本地路径：在线曲目必须直接
        // 短路为可用，与 is_track_path_available 的做法完全一致
        if is_online_path(&self.location) {
            return true;
        }

        // 缓存的可用性快照（启动与扫描完成时重载，与专辑/歌手集合同一趟）：先前
        // 每行一次 `path.exists()` stat 会让每次滚动的行构建都卡住。不过在首次
        // 重载落地之前，仍要回落到加快照之前的确切行为——`None` 绝不能被读成
        // 「不可用」，因为在那个启动窗口里构建的行只捕获一次判定，此后一直是
        // 置灰且点不动，直到整表重载。
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
        // `is_available` 在行构建时解析一次并被 builder 捕获：菜单树本身只在打开
        // 时才构建，因此每次重绘都重新 stat 文件既没必要也是错的。
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
        // 行以引用计数的形式进入 builder；深度克隆只在菜单真正打开时才发生
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
        // 先查预取缓存：命中就绕开下面那次 UI 线程 `block_on`
        // （滚出 keep-around 区带后，每个新建行都要付一次）
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
        // 缓存的可用性快照（启动与扫描完成时各自重载一次）：先前每行一次
        // `list_all_tracks_by_artist` 查询加每曲目一次 stat，会让每次滚动的行构建都卡住
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

/// 表格读取路径的耗时探针——库加载、排序以及滚动时行预取的 §37 回归防护层。
/// 配套探针：`row_build_bench`（逐行查询）、`test_support::scale_probe`
/// （100k 次热读取，含搜索索引查询）、`session_storage` 的大会话 bench。
///
/// ```text
/// cargo test --release table::bench -- --ignored --nocapture
/// ```
#[cfg(test)]
mod bench {
    use super::{clear_row_cache, prefetch_rows_chunked, track_row_cache};
    use crate::{
        library::db::{self, TrackSortMethod},
        test_support::{create_test_pool, seed_library},
    };

    const ALBUMS: usize = 200;
    const TRACKS_PER_ALBUM: usize = 50;
    const ARTISTS: usize = ALBUMS;

    /// 这个规模下整表加载要几十 ms，因此只跑寥寥几轮，而不用共享的 400 次
    /// 微查询助手。
    fn summarize_table(name: &str, runs: usize, mut f: impl FnMut()) {
        f(); // 预热：首轮会把页缓存填满
        let mut samples = Vec::with_capacity(runs);
        for _ in 0..runs {
            let start = std::time::Instant::now();
            f();
            samples.push(start.elapsed().as_secs_f64() * 1_000.0);
        }
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let mean = samples.iter().sum::<f64>() / runs as f64;
        println!("{name:<28} min={min:>8.1}ms  mean={mean:>8.1}ms  ({runs} runs)");
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn report_table_read_costs() {
        // 启动期数据库那一半：建池 + 迁移（[startup] 日志里的
        // `database pool ready elapsed_ms` 在现场量的是同一件事）。
        let pool_start = std::time::Instant::now();
        let (_dir, pool) = crate::RUNTIME.block_on(async {
            let (dir, pool) = create_test_pool("table-bench").await;
            seed_library(&pool, ALBUMS, TRACKS_PER_ALBUM, ARTISTS).await;
            (dir, pool)
        });
        println!(
            "create_test_pool (pool + migrate): {:?}",
            pool_start.elapsed()
        );
        let total_tracks = ALBUMS * TRACKS_PER_ALBUM;
        println!("library: {ALBUMS} albums, {total_tracks} tracks, {ARTISTS} artists");

        // ---- 库加载 + 排序：全部表格排序方式 ----
        // 默认排序（`None` → ArtistAsc）就是 find_tracks_artist_asc.sql 里记录的那笔
        // 已接受技术债——整表 JOIN + COALESCE 排序；这些数字是它的回归基线。
        let sorts: Vec<(&str, TrackSortMethod)> = vec![
            (
                "list_tracks ArtistAsc (default)",
                TrackSortMethod::ArtistAsc,
            ),
            ("list_tracks ArtistDesc", TrackSortMethod::ArtistDesc),
            ("list_tracks TitleAsc", TrackSortMethod::TitleAsc),
            ("list_tracks AlbumAsc", TrackSortMethod::AlbumAsc),
            ("list_tracks DurationAsc", TrackSortMethod::DurationAsc),
            (
                "list_tracks TrackNumberAsc",
                TrackSortMethod::TrackNumberAsc,
            ),
        ];
        for (label, sort) in sorts {
            summarize_table(label, 5, || {
                let _ = crate::RUNTIME
                    .block_on(db::list_tracks(&pool, sort))
                    .unwrap();
            });
        }

        // ---- 滚动预取：对冷 id 的分块 IN 查询 ----
        // 预取会滤掉已在缓存里的 id，所以每次发射都拿一段全新的冷 id 并先清空缓存
        // （属准备工作，不计入耗时）；3 次中的最好值代表每块的成本。
        let all_ids: Vec<i64> = (0..total_tracks as i64).collect();
        for chunk_count in [1usize, 10] {
            let mut best_ms = f64::INFINITY;
            for shot in 0..3 {
                let slice: Vec<i64> = all_ids.iter().skip(shot * 900).take(900).copied().collect();
                if slice.len() < 900 {
                    break;
                }
                clear_row_cache(track_row_cache());
                let start = std::time::Instant::now();
                crate::RUNTIME.block_on(prefetch_rows_chunked(
                    track_row_cache(),
                    pool.clone(),
                    slice,
                ));
                best_ms = best_ms.min(start.elapsed().as_secs_f64() * 1_000.0);
            }
            println!(
                "prefetch chunk x{chunk_count:<2} (900 ids each)   {:>8.1}ms per chunk  \
                 {:>7.1}µs/id  (best of 3)",
                best_ms,
                best_ms * 1_000.0 / 900.0,
            );
        }
        clear_row_cache(track_row_cache());
    }
}
