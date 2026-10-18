// 改写自 https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs

use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, GlobalElementId, Hsla,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollHandle, ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
    actions, div, fill, hsla, point, prelude::*, px, relative, size,
};
use tracing::error;
use unicode_segmentation::*;

use crate::ui::{global_actions::PlayPause, theme::Theme};

/// `TextElement::paint` 每帧每行都会跑一次，而 `error!` 写入是同步刷盘，于是一次失败的绘制
/// 否则会以 60-120 次/秒打到日志上。绘制失败是持续性的（字体/图集后端状态），所以只记录第一次；
/// 绘制恢复后输入框照常渲染，那不需要日志行。
static PAINT_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectWordLeft,
        SelectWordRight,
        WordLeft,
        WordRight,
        SelectAll,
        SelectToStart,
        SelectToEnd,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Next,
        Previous,
        Accept,
        DeleteWordBackward,
        DeleteWordForward,
        DeleteToBeginningOfLine,
        DeleteToEndOfLine
    ]
);

fn next_word_boundary(content: &str, offset: usize) -> usize {
    for (start, segment) in content.split_word_bound_indices() {
        if segment.chars().all(|c| c.is_whitespace()) {
            continue;
        }
        let end = start + segment.len();
        if end > offset {
            return end;
        }
    }
    content.len()
}

fn previous_word_boundary(content: &str, offset: usize) -> usize {
    let mut last_start = 0;
    for (start, segment) in content.split_word_bound_indices() {
        if segment.chars().all(|c| c.is_whitespace()) {
            continue;
        }
        if start >= offset {
            return last_start;
        }
        last_start = start;
    }
    last_start
}

fn word_range_for_offset(content: &str, offset: usize) -> Range<usize> {
    if content.is_empty() {
        return 0..0;
    }

    let mut last_before: Option<Range<usize>> = None;

    for (start, segment) in content.split_word_bound_indices() {
        if segment.chars().all(|c| c.is_whitespace()) {
            continue;
        }
        let end = start + segment.len();
        if start <= offset && offset < end {
            return start..end;
        }
        if end <= offset {
            last_before = Some(start..end);
        } else {
            return last_before.unwrap_or(start..end);
        }
    }

    last_before.unwrap_or(0..0)
}

#[derive(Copy, Clone, Default, PartialEq)]
enum SelectionDragMode {
    #[default]
    Char,
    Word,
    All,
}

/// 转发给传给 [`TextInput::new`] 的可选增强输入处理器的 action。
#[derive(Copy, Clone)]
pub enum EnrichedInputAction {
    Next,
    Previous,
    Accept,
}

type EnrichedInputHandler = Box<dyn Fn(EnrichedInputAction, &mut Window, &mut App)>;

pub struct TextInput {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    pub content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    shaped_line: Option<ShapedLineCache>,
    is_selecting: bool,
    selection_drag_mode: SelectionDragMode,
    word_drag_anchor: usize,
    word_drag_anchor_range: Range<usize>,
    enriched_input_handler: Option<EnrichedInputHandler>,
}

// 变更事件携带的是 SharedString：克隆它只是一次 Arc 引用计数自增，所以逐按键的事件发射零分配
// （对比每次按键新建一个 String）。
impl EventEmitter<SharedString> for TextInput {}

impl TextInput {
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = previous_word_boundary(&self.content, self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = next_word_boundary(&self.content, self.cursor_offset());
        self.select_to(target, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = previous_word_boundary(&self.content, self.cursor_offset());
        self.move_to(target, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = next_word_boundary(&self.content, self.cursor_offset());
        self.move_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    /// 全选内容。
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn delete_word_backward(
        &mut self,
        _: &DeleteWordBackward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to(
            previous_word_boundary(&self.content, self.cursor_offset()),
            window,
            cx,
        );
    }

    fn delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to(
            next_word_boundary(&self.content, self.cursor_offset()),
            window,
            cx,
        );
    }

    fn delete_to_beginning_of_line(
        &mut self,
        _: &DeleteToBeginningOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to(0, window, cx);
    }

    fn delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_to(self.content.len(), window, cx);
    }

    /// 删除当前选区；选区为空时先把光标移到 `target`，以便恰好删掉那一段。
    fn delete_to(&mut self, target: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(target, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(self.previous_boundary(self.cursor_offset()), window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(self.next_boundary(self.cursor_offset()), window, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.index_for_mouse_position(event.position);
        self.is_selecting = true;

        match event.click_count {
            2 => {
                let word_range = self.word_range_for_offset(offset);
                if event.modifiers.shift {
                    let anchor = if self.selection_reversed {
                        self.selected_range.end
                    } else {
                        self.selected_range.start
                    };
                    let cursor_target = if word_range.end <= anchor {
                        word_range.start
                    } else if word_range.start >= anchor || offset >= anchor {
                        word_range.end
                    } else {
                        word_range.start
                    };
                    self.select_to(cursor_target, cx);
                    self.word_drag_anchor = anchor;
                    self.word_drag_anchor_range = self.word_range_for_offset(anchor);
                } else {
                    self.move_to(word_range.start, cx);
                    self.select_to(word_range.end, cx);
                    self.word_drag_anchor = offset;
                    self.word_drag_anchor_range = word_range;
                }
                self.selection_drag_mode = SelectionDragMode::Word;
            }
            3 => {
                self.move_to(0, cx);
                self.select_to(self.content.len(), cx);
                self.selection_drag_mode = SelectionDragMode::All;
            }
            _ => {
                self.selection_drag_mode = SelectionDragMode::Char;
                if event.modifiers.shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx);
                }
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
        self.selection_drag_mode = SelectionDragMode::Char;
        self.word_drag_anchor_range = 0..0;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }

        let offset = self.index_for_mouse_position(event.position);

        match self.selection_drag_mode {
            SelectionDragMode::Char => self.select_to(offset, cx),
            SelectionDragMode::Word => {
                let current_word = self.word_range_for_offset(offset);
                let new_range = if offset >= self.word_drag_anchor {
                    self.word_drag_anchor_range.start..current_word.end
                } else {
                    current_word.start..self.word_drag_anchor_range.end
                };
                let new_reversed = offset < self.word_drag_anchor;

                if self.selected_range != new_range || self.selection_reversed != new_reversed {
                    self.selected_range = new_range;
                    self.selection_reversed = new_reversed;
                    cx.notify();
                }
            }
            SelectionDragMode::All => {}
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace("\n", " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        // 拖框选/点击时每个鼠标 report 都会打进来（125-1000Hz），纵向移动与
        // 同一字形内的水平移动算出的 offset 不变；状态没变时渲染结果逐字节
        // 相同，跳过 notify（§5 事件合并 / §33 丢弃已被覆盖的工作）。
        // `selection_reversed` 不在这里改写，而折叠区间下 `cursor_offset()`
        // 会读它——所以只需比 range。
        if self.selected_range == (offset..offset) {
            return;
        }
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let before = (self.selected_range.clone(), self.selection_reversed);
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        // 同 `move_to`：鼠标没跨过字形边界时区间与反向位都没变。
        if (self.selected_range.clone(), self.selection_reversed) != before {
            cx.notify()
        }
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        byte_offset_from_utf16(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// 解析一次编辑要作用的区间：给定则用该 UTF-16 区间，否则用 IME 标记区间，再否则用当前选区。
    fn resolve_edit_range(&self, range_utf16: Option<&Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone())
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn word_range_for_offset(&self, offset: usize) -> Range<usize> {
        word_range_for_offset(&self.content, offset)
    }

    /// 清空内容、选区、IME、拖拽与布局状态。
    pub fn reset(&mut self) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        self.is_selecting = false;
        self.selection_drag_mode = SelectionDragMode::Char;
        self.word_drag_anchor = 0;
        self.word_drag_anchor_range = 0..0;
    }

    /// 全局 "space" 键绑定会触发 `PlayPause`（keybinds.json）；在获得焦点的输入框里它必须
    /// 输入一个真空格字符，而不是切换播放。
    fn space(&mut self, _: &PlayPause, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, " ", window, cx)
    }

    /// 把 `action` 转发给传给 [`TextInput::new`] 的增强输入处理器（若装了的话）。
    fn dispatch_enriched_action(
        &mut self,
        action: EnrichedInputAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handler) = self.enriched_input_handler.as_mut() {
            handler(action, window, cx);
        }
    }

    pub fn next(&mut self, _: &Next, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_enriched_action(EnrichedInputAction::Next, window, cx);
    }

    pub fn previous(&mut self, _: &Previous, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_enriched_action(EnrichedInputAction::Previous, window, cx);
    }

    pub fn accept(&mut self, _: &Accept, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_enriched_action(EnrichedInputAction::Accept, window, cx);
    }

    /// 替换内容并把光标移到新文本末尾。
    pub fn set_value(&mut self, cx: &mut Context<Self>, value: SharedString) {
        self.content = value;
        // 旧的标记区间指的是上一版文本：留着它会让 prepaint 用新内容长度去减一个更大的 end
        // （下溢），还可能让 IME 组合态跨到新文本上存活。光标移动不能改用 `move_to`，
        // 因为它的"区间未变"守卫会在光标本来已在末尾时跳过重绘。
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }
}

/// 把 IME 的选区（**`new_text` 内部**的 UTF-16 偏移）映射进文档：先按替换串换算，再在编辑起点
/// 重设基准。用编辑区间的 end 当作选区 end 会越过插入文本，并可能让 `selected_range.end`
/// 超出 `content.len()`。
fn rebase_selection(
    range: Range<usize>,
    new_text: &str,
    new_selected_range_utf16: Option<&Range<usize>>,
) -> Range<usize> {
    match new_selected_range_utf16 {
        Some(selected) => {
            let start = byte_offset_from_utf16(new_text, selected.start);
            let end = byte_offset_from_utf16(new_text, selected.end);
            range.start + start..range.start + end
        }
        None => {
            let end = range.start + new_text.len();
            end..end
        }
    }
}

/// UTF-16 代码单元偏移 -> `text` 中的字节偏移（钳位到其末尾）。
fn byte_offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;

    for ch in text.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }

    utf8_offset
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.resolve_edit_range(range_utf16.as_ref());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();

        cx.emit(self.content.clone());
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.resolve_edit_range(range_utf16.as_ref());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = rebase_selection(range, new_text, new_selected_range_utf16.as_ref());

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        assert_eq!(last_layout.text, self.content);
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

/// [`TextElement`] 的 `shape_line` 结果缓存，键覆盖排版及其装饰段消费的一切：未变的输入因此
/// 在每次重渲染时都跳过重排（以及每帧的 run 向量）。键必须保持完整，否则会画出过期的一行
/// （字形、下划线或颜色不对）。
struct ShapedLineCache {
    display_text: SharedString,
    font: Font,
    font_size: Pixels,
    color: Hsla,
    marked_range: Option<Range<usize>>,
    scale_factor: f32,
    line: ShapedLine,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let marked_range = input.marked_range.clone();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), hsla(0., 0., 0., 0.2))
        } else {
            (content, style.color)
        };

        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let scale_factor = window.scale_factor();

        let cached = input
            .shaped_line
            .as_ref()
            .filter(|cache| {
                cache.display_text == display_text
                    && cache.font == font
                    && cache.font_size == font_size
                    && cache.color == text_color
                    && cache.marked_range == marked_range
                    && cache.scale_factor == scale_factor
            })
            .map(|cache| cache.line.clone());
        let line = if let Some(line) = cached {
            line
        } else {
            let run = TextRun {
                len: display_text.len(),
                font: font.clone(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
                letter_spacing: None,
            };
            let runs = if let Some(marked_range) = marked_range.as_ref() {
                vec![
                    TextRun {
                        len: marked_range.start,
                        ..run.clone()
                    },
                    TextRun {
                        len: marked_range.end - marked_range.start,
                        underline: Some(UnderlineStyle {
                            color: Some(run.color),
                            thickness: px(1.0),
                            wavy: false,
                        }),
                        ..run.clone()
                    },
                    TextRun {
                        len: display_text.len() - marked_range.end,
                        ..run
                    },
                ]
                .into_iter()
                .filter(|run| run.len > 0)
                .collect()
            } else {
                vec![run]
            };

            let line =
                window
                    .text_system()
                    .shape_line(display_text.clone(), font_size, &runs, None);
            self.input.update(cx, |input, _cx| {
                input.shaped_line = Some(ShapedLineCache {
                    display_text,
                    font,
                    font_size,
                    color: text_color,
                    marked_range,
                    scale_factor,
                    line: line.clone(),
                });
            });
            line
        };

        let theme = cx.global::<Theme>();

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(1.), bounds.bottom() - bounds.top()),
                    ),
                    theme.caret_color,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    theme.text_highlight_background,
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        if let Err(e) = line.paint(
            bounds.origin,
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        ) && !PAINT_FAILURE_LOGGED.swap(true, Ordering::Relaxed)
        {
            error!("Failed to paint text input line: {e:?}");
        }

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl TextInput {
    pub fn new(
        cx: &mut App,
        focus_handle: FocusHandle,
        content: Option<SharedString>,
        placeholder: Option<SharedString>,
        enriched_input_handler: Option<EnrichedInputHandler>,
    ) -> Entity<TextInput> {
        cx.new(|_| TextInput {
            focus_handle,
            content: content.unwrap_or_else(|| "".into()),
            placeholder: placeholder.unwrap_or_else(|| "".into()),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            shaped_line: None,
            is_selecting: false,
            selection_drag_mode: SelectionDragMode::Char,
            word_drag_anchor: 0,
            word_drag_anchor_range: 0..0,
            scroll_handle: ScrollHandle::new(),
            enriched_input_handler,
        })
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            // `.role()` 只在带元素 id 的（stateful）元素上可用；本组件可有多个实例，
            // 故 id 带 entity 身份，避免同路径下两个输入框串用状态。
            .id(("text-input", cx.entity_id()))
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .role(gpui::Role::TextInput)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::delete_word_backward))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(Self::delete_to_beginning_of_line))
            .on_action(cx.listener(Self::delete_to_end_of_line))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::space))
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::previous))
            .on_action(cx.listener(Self::accept))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(
                div()
                    .id(("textinput", cx.entity_id()))
                    .overflow_x_scroll()
                    .track_scroll(&self.scroll_handle)
                    .w_full()
                    .pr(px(2.0))
                    .pb(px(2.0))
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ime_selection_rebases_within_replacement_text() {
        // 用 "pin" 替换一段 3 字节的标记区间，光标落在其末尾。
        assert_eq!(rebase_selection(2..5, "pin", Some(&(3..3))), 5..5);
        // 选区是 new_text 内的 UTF-16，多字节字符需要换算
        // to their byte width ("你" = 3 bytes, one UTF-16 unit).
        assert_eq!(rebase_selection(0..0, "你a", Some(&(1..1))), 3..3);
        assert_eq!(rebase_selection(4..7, "你a", Some(&(0..2))), 4..8);
        // 没有 IME 选区：光标落在插入文本之后。
        assert_eq!(rebase_selection(4..7, "ab", None), 6..6);
        // 旧的编辑区间 end 绝不能漏进新的 end。
        assert_eq!(rebase_selection(0..6, "pin", Some(&(3..3))), 3..3);
    }

    #[test]
    fn word_range_empty_content() {
        assert_eq!(word_range_for_offset("", 0), 0..0);
    }

    #[test]
    fn word_range_single_word_at_start() {
        assert_eq!(word_range_for_offset("hello", 0), 0..5);
    }

    #[test]
    fn word_range_single_word_at_end() {
        assert_eq!(word_range_for_offset("hello", 4), 0..5);
    }

    #[test]
    fn word_range_single_word_at_boundary() {
        assert_eq!(word_range_for_offset("hello", 5), 0..5);
    }

    #[test]
    fn word_range_two_words_inside_first() {
        assert_eq!(word_range_for_offset("hello world", 2), 0..5);
    }

    #[test]
    fn word_range_two_words_in_space_prefers_preceding() {
        assert_eq!(word_range_for_offset("hello world", 5), 0..5);
    }

    #[test]
    fn word_range_two_words_inside_second() {
        assert_eq!(word_range_for_offset("hello world", 8), 6..11);
    }

    #[test]
    fn word_range_two_words_at_second_start() {
        assert_eq!(word_range_for_offset("hello world", 6), 6..11);
    }

    #[test]
    fn word_range_multiple_spaces_prefers_preceding() {
        assert_eq!(word_range_for_offset("hello   world", 7), 0..5);
    }

    #[test]
    fn word_range_offset_before_first_word() {
        assert_eq!(word_range_for_offset("  hello", 0), 2..7);
    }

    #[test]
    fn word_range_only_whitespace() {
        assert_eq!(word_range_for_offset("   ", 1), 0..0);
    }

    #[test]
    fn word_range_only_punctuation() {
        // 按 UAX#29 词边界，每个 "!" 都是独立的一段
        assert_eq!(word_range_for_offset("!!!", 1), 1..2);
    }

    #[test]
    fn word_range_punctuation_between_words() {
        assert_eq!(word_range_for_offset("hello, world", 5), 5..6);
    }

    #[test]
    fn word_range_punctuation_before_word() {
        assert_eq!(word_range_for_offset("hello, world", 6), 5..6);
    }

    #[test]
    fn word_range_unicode_emoji() {
        let content = "hello 🌍 world";
        let emoji_start = "hello ".len();
        let emoji_end = emoji_start + "🌍".len();
        let world_start = emoji_end + " ".len();
        let world_end = world_start + "world".len();

        assert_eq!(word_range_for_offset(content, 0), 0..5);
        assert_eq!(
            word_range_for_offset(content, emoji_start),
            emoji_start..emoji_end
        );
        assert_eq!(
            word_range_for_offset(content, world_start),
            world_start..world_end
        );
    }

    #[test]
    fn next_word_boundary_empty() {
        assert_eq!(next_word_boundary("", 0), 0);
    }

    #[test]
    fn next_word_boundary_at_start() {
        assert_eq!(next_word_boundary("hello", 0), 5);
    }

    #[test]
    fn next_word_boundary_mid_word() {
        assert_eq!(next_word_boundary("hello", 2), 5);
    }

    #[test]
    fn next_word_boundary_at_word_end() {
        assert_eq!(next_word_boundary("hello", 5), 5);
    }

    #[test]
    fn next_word_boundary_between_words() {
        assert_eq!(next_word_boundary("hello  world", 5), 12);
    }

    #[test]
    fn next_word_boundary_past_last() {
        assert_eq!(next_word_boundary("hello", 5), 5);
    }

    #[test]
    fn next_word_boundary_unicode() {
        let content = "hello 🌍 world";
        let emoji_start = "hello ".len();
        let emoji_end = emoji_start + "🌍".len();
        let world_end = emoji_end + " world".len();

        assert_eq!(next_word_boundary(content, 0), 5);
        assert_eq!(next_word_boundary(content, emoji_start), emoji_end);
        assert_eq!(next_word_boundary(content, emoji_end), world_end);
    }

    #[test]
    fn previous_word_boundary_empty() {
        assert_eq!(previous_word_boundary("", 0), 0);
    }

    #[test]
    fn previous_word_boundary_at_start() {
        assert_eq!(previous_word_boundary("hello", 0), 0);
    }

    #[test]
    fn previous_word_boundary_mid_word() {
        assert_eq!(previous_word_boundary("hello", 3), 0);
    }

    #[test]
    fn previous_word_boundary_at_word_end() {
        assert_eq!(previous_word_boundary("hello", 5), 0);
    }

    #[test]
    fn previous_word_boundary_between_words() {
        assert_eq!(previous_word_boundary("hello  world", 7), 0);
    }

    #[test]
    fn previous_word_boundary_past_last() {
        assert_eq!(previous_word_boundary("hello", 5), 0);
    }

    #[test]
    fn previous_word_boundary_unicode() {
        let content = "hello 🌍 world";
        let emoji_start = "hello ".len();
        let emoji_end = emoji_start + "🌍".len();
        let world_start = emoji_end + " ".len();

        assert_eq!(previous_word_boundary(content, 5), 0);
        assert_eq!(previous_word_boundary(content, emoji_end), emoji_start);
        assert_eq!(previous_word_boundary(content, world_start), emoji_start);
    }
}
