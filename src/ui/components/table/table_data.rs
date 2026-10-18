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

/// 给行容器（表格行或网格项；有状态的，即带 `.id`）接上拖拽载荷来源与 drag-over 高亮
/// 处理器。两个渲染器接的处理器完全相同，所以逻辑就放在 [`TableDragData`] 旁边。
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

/// 把共享的行选择交互装到有状态的行/网格容器上（此前是 `GridItem::render` 与
/// `TableItem::render` 里逐字重复的那一段）：点击选择以"处理器存在"AND"行可用"为门，
/// 指针/悬停/按下三态样式，不可用行的淡化样式，以及中键派发到
/// [`TableData::handle_middle_mouse`]。可能未解析出行就构造的渲染器（`TableItem`）传
/// `row = None`；`GridItem` 传 `Some`。
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

/// 在标准elevated-background容器里承载已构建好的右键菜单树。全应用的 `menu_on_open`
/// 包装都把菜单渲染进这同一个宿主 div（表格行、网格项、曲目行、文件行），因此各处绘制
/// 完全一致。
pub fn elevated_menu_element(cx: &App, menu: impl IntoElement) -> AnyElement {
    div()
        .bg(cx.global::<Theme>().elevated_background)
        .child(menu)
        .into_any_element()
}

/// 把已构建好的行/网格内容元素包进共享的右键菜单脚手架（此前是 `GridItem::render` 与
/// `TableItem::render` 尾部近乎逐字重复的部分）：`context(id).with(content)`，其菜单树
/// 只在菜单真正打开时才构建（[`TableData::get_context_menu`] 的契约），外加一个可选 overlay
/// 作为 context 元素的兄弟节点渲染，以免嵌进菜单的 `deferred` 弹窗里。没有菜单时原样返回
/// 内容。
///
/// `fill_container` 区分两个调用点变体：网格项把 context 元素拉伸到整个瓦片、overlay 包在
/// `size_full` div 里；表格行保持自身宽度、overlay 包在 `w_full` 里。
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
        // 菜单树只在菜单打开时构建
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

/// 列头拖拽重排用的拖拽载荷。
#[derive(Clone, Debug)]
pub struct ColumnReorderDrag {
    pub source_index: usize,
}

// 表格布局常量
pub const TABLE_MAX_WIDTH: f32 = 1000.0;
/// 版式规则：非全宽模式下把表格宽度钉在 `TABLE_MAX_WIDTH`。此前六个库视图各自
/// 内联同一条闭包（还分裂成 `|this|` 与 `|this: Div|` 两种拼写），规则从此只此一处。
pub fn capped_table_width<D: gpui::Styled>(this: D) -> D {
    this.max_w(gpui::px(TABLE_MAX_WIDTH))
}
pub const TABLE_IMAGE_COLUMN_WIDTH: f32 = 47.0;
pub const TABLE_HEADER_HEIGHT: f32 = 36.0;

/// 面板变窄时列按比例压缩，但绝不低于此值；越过各列下限之和后表格改为水平滚动。
pub const MIN_COLUMN_WIDTH: f32 = 48.0;

/// 既没有保存宽度、也不在表格默认列映射里的列所使用的宽度。
pub const DEFAULT_COLUMN_WIDTH: f32 = 100.0;

/// 兼容的拖拽悬停在行/网格单元上时绘制的半透明高亮（见 [`with_drag_handlers`]）。
pub const DRAG_OVER_HIGHLIGHT: u32 = 0x88888822;

// 列宽调整常量
pub const COLUMN_MIN_WIDTH: f32 = 50.0;
pub const COLUMN_RESIZE_HANDLE_WIDTH: f32 = 6.0;
pub const TABLE_HEADER_GROUP: &str = "table-header-group";

pub trait Column: Clone + Copy + Debug + Hash + PartialEq + Eq + Send {
    /// 取列的友好显示名称文本。
    fn get_column_name(&self) -> SharedString;

    /// 该列是否允许用户调整宽度。默认为 true。
    fn is_resizable(&self) -> bool {
        true
    }

    /// 该列是否允许用户隐藏。像"Title"这样的必需列要返回 `false`。默认为 true。
    fn is_hideable(&self) -> bool {
        true
    }

    /// 返回该类型所有可能的列变体。构建列可见性菜单时必需。
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

/// 网格项当前所处的展示上下文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridContext {
    /// 在 Table 组件内部
    Table,
    /// 独立使用 / 表格之外
    Standalone,
}

/// TableData trait 定义表格数据获取、排序与列举的接口。实现它即可让表格以结构化方式展示数据。
pub trait TableData<C>: Sized
where
    C: Column,
{
    type Identifier: Clone + Debug + Send + 'static;
    type ContextMenuContext: Clone;

    /// 取表格名称。
    fn get_table_name() -> SharedString;

    /// 取表格的行。返回值是标识符向量，可用它取回完整行数据；sort 参数用于指定行的排序方向。
    ///
    /// 这个调用运行在 UI 线程之外：实现必须走异步 `db` 辅助函数（由调用方在 Tokio runtime 上
    /// 驱动），而不是同步的 `LibraryAccess` 包装——后者的 `block_on` 会在每次打开视图、改排序
    /// 或扫描完成时把 UI 线程停在一个全库查询上（准则 §2.3 / §14）。
    fn get_rows(
        pool: sqlx::SqlitePool,
        sort: Option<TableSort<C>>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<Self::Identifier>>>;

    /// 取表格的某一特定行。返回指向表格数据的 Arc，可用它把列取成 SharedString。id 参数用于
    /// 定位要取回的行。
    fn get_row(cx: &mut App, id: Self::Identifier) -> anyhow::Result<Option<Arc<Self>>>;

    /// 内部扩展：为给定标识符批量预取完整行到实现内部缓存，这样逐行 `get_row`（超出保留带之后
    /// 每个新建行一次 UI 线程 `block_on` DB 访问）在滚动时命中缓存而不是现查。表格组件在可视窗口
    /// 移动时调用它，并把返回的 future 派到异步 runtime 上——绝不在 UI 线程 await。类型不支持
    /// 预取时（默认）返回 `None`。
    fn prefetch_rows(
        _pool: sqlx::SqlitePool,
        _ids: &[Self::Identifier],
    ) -> Option<BoxFuture<'static, ()>> {
        None
    }

    /// 内部扩展：丢弃行预取缓存。表格在每次重载（改排序、扫描完成）时调用，因此缓存行绝不可能
    /// 比一次重扫描活得更久；实现还必须让已在途的预取 future 失效（代际守卫）。
    fn clear_row_cache() {}

    /// 取行上某一列的值。
    fn get_column(&self, cx: &mut App, column: C) -> Option<SharedString>;

    /// 返回行是否可能含图像。布局阶段用它决定是否显示占位封面与表头区块。
    fn has_images() -> bool;

    /// 取行对应的图像路径。
    fn get_image_path(&self) -> Option<SharedString>;

    /// 取行的全质量封面键，供 `managed_image` 使用。
    fn get_full_image_key(&self) -> Option<ManagedImageKey>;

    /// 取表格的默认列宽映射。
    fn default_columns() -> IndexMap<C, f32, FxBuildHasher>;

    /// 返回给定列是否应使用等宽字体绘制。
    ///
    /// 主要由数字构成的列（日期、时长）应为 true。
    fn column_monospace(column: C) -> bool;

    /// 取行的唯一元素 id。它与行 id 不同，用于在 GPUI 里标识这一行。
    fn get_element_id(&self) -> impl Into<ElementId>;

    /// 取行的表格 ID。
    fn get_table_id(&self) -> Self::Identifier;

    /// 返回该行当前是否可交互。
    fn is_available(&self, _cx: &mut App) -> bool {
        true
    }

    /// 返回该行的拖拽数据（若支持拖拽）。返回 None 表示不支持拖拽。默认实现返回 None。
    fn get_drag_data(&self) -> Option<TableDragData> {
        None
    }

    /// 返回该行右键菜单的构建器，外加一个可选 overlay（例如模态框），它渲染在 context 弹窗
    /// 之外，以免嵌进 `deferred`。
    ///
    /// 构建器在菜单打开期间的每次渲染都会运行，所以其函数体只能读缓存与快照——绝不发起阻塞查询
    /// 或系统调用。`is_available` 是行视图在构造时已经解析过一次的可用性；实现必须捕获它，
    /// 而不是重新调用 `is_available`（那会 stat 文件系统或打数据库）。
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

    /// 内部扩展：契约与 [`TableData::get_context_menu`] 相同，但把行的共享 `Arc` 句柄交给实现，
    /// 让返回的构建器只需持有一份引用计数，而不是每帧深拷贝整行。默认委托给
    /// `get_context_menu`。
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

    /// 该行的可选中键（鼠标中键）处理器。
    fn handle_middle_mouse(&self, _window: &mut Window, _cx: &mut App, _grid_context: GridContext) {
    }

    /// 返回该表是否支持以网格视图渲染。
    fn supports_grid_view() -> bool {
        false
    }

    /// 相对表格数据取网格项内容。返回 `(主文本, 可选次文本)` 二元组。
    fn get_grid_content(&self, _cx: &mut App) -> Option<(SharedString, Option<SharedString>)> {
        None
    }

    /// 在给定上下文下取网格项内容。返回 `(主文本, 可选次文本)` 二元组。
    /// 默认委托给 `get_grid_content`。
    fn get_grid_content_for(
        &self,
        cx: &mut App,
        _context: GridContext,
    ) -> Option<(SharedString, Option<SharedString>)> {
        self.get_grid_content(cx)
    }
}
