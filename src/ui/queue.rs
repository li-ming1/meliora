use crate::sync::RecoveredRwLock;
use crate::ui::design::ICON_SM;
use crate::{
    playback::{interface::PlaybackInterface, queue::QueueItemData, thread::PlaybackState},
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        availability::is_track_path_available,
        components::{
            context::context,
            drag_drop::{
                AlbumDragData, DragDropItemState, DragDropListConfig, DragDropListManager,
                DragPreview, DropIndicator, DropPosition, TrackDragData, calculate_move_target,
                check_drag_cancelled, handle_external_drag_move, handle_track_drag_move,
                handle_track_drop_multi, request_edge_scroll,
            },
            icons::{
                CROSS, DISC, FOLDER_SEARCH, PLAYLIST_ADD, STAR, STAR_FILLED, TRASH, USERS, icon,
            },
            managed_image::{ManagedImageKey, managed_image},
            menu::{menu, menu_item, menu_separator},
            nav_button::nav_button,
            scrollbar::{ScrollableHandle, floating_scrollbar},
            tooltip::build_tooltip,
        },
        library::{
            ViewSwitchMessage, add_to_playlist::AddToPlaylist, context_menus::navigate_to_artists,
        },
        library_access::LibraryAccess,
        queue_metadata,
    },
};
use cntp_i18n::{tr, trn};
use gpui::*;
use prelude::FluentBuilder;
use rustc_hash::{FxHashMap, FxHashSet};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tracing::{debug, error};

use super::{
    components::button::{ButtonSize, ButtonStyle, button},
    models::{
        HasLikedState, Models, PlaybackInfo, is_song_liked, subscribe_liked_updates,
        toggle_album_like, toggle_like,
    },
    scroll_follow::SmoothScrollFollow,
    theme::Theme,
    util::{VIEW_KEEP_AROUND, create_or_retrieve_view, reduced_motion_enabled},
};

/// 队列拖放操作的列表标识符
const QUEUE_LIST_ID: &str = "queue";
/// 每个队列条目的像素高度
const QUEUE_ITEM_HEIGHT: f32 = 60.0;
/// 队列自动跟随动画的时长。
const QUEUE_FOLLOW_ANIMATION_DURATION: Duration = Duration::from_millis(180);

/// 队列共享的选中状态。
pub struct QueueSelection {
    selected: FxHashSet<usize>,
    /// `selected` 的有序快照，每次变更都维护，渲染路径可直接借用，
    /// 不必每帧每行再分配并排序。
    sorted: Vec<usize>,
    /// `resync_sorted` 每次变更都自增。让渲染路径的消费者（每行拖拽
    /// 同伴缓存）用选中版本号作缓存键，而不必每帧重新推导；某次变更
    /// 即使集合不变也只导致一次无害的缓存重建。
    epoch: u64,
    anchor: Option<usize>,
}

impl QueueSelection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            selected: FxHashSet::default(),
            sorted: Vec::new(),
            epoch: 0,
            anchor: None,
        })
    }

    pub fn contains(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    pub fn is_multi(&self) -> bool {
        self.selected.len() > 1
    }

    /// 选中版本：任何变更器运行都会变化（它们全部汇入 `resync_sorted`），
    /// 即便结果集合完全相同。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn indices(&self) -> Vec<usize> {
        self.sorted.clone()
    }

    /// 借用式有序快照；渲染路径的消费者用它彻底跳过每帧分配。
    pub fn sorted(&self) -> &[usize] {
        &self.sorted
    }

    /// 变更后重建有序快照。选中变化是用户操作频率，这里的开销无关紧要。
    /// 所有变更器都经过本方法，使其成为每行拖拽缓存所依赖 epoch 的唯一
    /// 自增点。
    fn resync_sorted(&mut self) {
        self.sorted.clear();
        self.sorted.extend(self.selected.iter().copied());
        self.sorted.sort_unstable();
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        self.resync_sorted();
        self.anchor = None;
        cx.notify();
    }

    /// 普通点击：清空选中，只选中这一项。
    pub fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected.clear();
        self.selected.insert(index);
        self.resync_sorted();
        self.anchor = Some(index);
        cx.notify();
    }

    /// Ctrl/Cmd+点击：在选中集里切换这一项。
    pub fn ctrl_toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected.contains(&index) {
            self.selected.remove(&index);
            if self.anchor == Some(index) {
                self.anchor = self.selected.iter().copied().next();
            }
        } else {
            self.selected.insert(index);
            self.anchor = Some(index);
        }
        self.resync_sorted();
        cx.notify();
    }

    /// Shift+点击：选中从锚点到这一项的区间。
    /// 替换此前的任何区间选择；没有锚点时用 `current_position`
    /// （正在播放的曲目）作为锚点。
    pub fn shift_range(
        &mut self,
        index: usize,
        current_position: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let anchor = self.anchor.or(current_position).unwrap_or(index);
        self.anchor = Some(anchor);
        self.selected.clear();
        let start = anchor.min(index);
        let end = anchor.max(index);
        self.selected.extend(start..=end);
        self.resync_sorted();
        cx.notify();
    }
}

/// [`QueueItem`] 的多选拖拽同伴缓存：有序选中集减去本行，键为（选中 epoch，
/// 本行索引）。
///
/// `QueueSelection::sorted()` 已是每次变更都维护的快照，因此残留的每帧重算
/// 只剩喂给拖拽负载的那次 filter+collect `Vec`；负载的 `additional_indices`
/// 是 `Arc<[usize]>`，本缓存把那次每帧分配变成一次引用计数自增。
///
/// `uids` 与 `indices` 同帧捕获、一一对应：拖拽负载的 source/additional 索引
/// 冻结在拖拽开始那一帧，投放时要靠这份同帧的 uid 指纹才能把"当时被拖的是
/// 哪几行"重解析为当前索引（见 [`InternalDragLedger`]）。
struct DragOthers {
    /// 选中 epoch：任何选中变更（含队列内容变化引发的清空，见 Queue 观察
    /// 者）都自增，让坐标与指纹整组重建，绝不半新半旧。
    epoch: u64,
    idx: usize,
    /// 喂给负载 `additional_indices` 的索引快照。
    indices: Arc<[usize]>,
    /// 同伴槽位在捕获时刻的 `QueueItemData::uid`。
    uids: Arc<[u64]>,
}

pub struct QueueItem {
    item: QueueItemData,
    current: usize,
    /// 本行上次重绘时的选中状态：selection 观察者只在该状态真的翻转时 notify。
    was_selected: bool,
    idx: usize,
    drag_drop_manager: Entity<DragDropListManager>,
    scroll_handle: UniformListScrollHandle,
    selection: Entity<QueueSelection>,
    /// 队列内重排拖拽的指纹账本（与列表共享同一实体）：拖拽开始时把本行
    /// 负载的坐标与槽位 uid 指纹记入账本，投放回调对账重验。
    drag_ledger: Entity<InternalDragLedger>,
    add_to: Option<Entity<AddToPlaylist>>,
    show_add_to: Option<Entity<bool>>,
    track_id: Option<i64>,
    is_liked: Option<i64>,
    /// 在视图构造时算一次即可：每帧对每条可见行做 `path.exists()` stat
    /// 开销太大。
    is_available: bool,
    /// 格式化后的时长文本，仅在条目时长变化时重建。
    duration_text: Option<SharedString>,
    cached_duration: Option<i64>,
    /// 缓存的拖拽负载基底，仅在该行显示名变化（元数据加载）时重建。
    /// 每帧只克隆它并重新盖上实时索引与选中状态，不再重新分配路径与名称。
    drag_base: Option<(SharedString, TrackDragData)>,
    /// 缓存的多选拖拽同伴（内容见 [`DragOthers`]）。
    drag_others: Option<DragOthers>,
}

impl HasLikedState for QueueItem {
    fn is_liked(&self) -> Option<i64> {
        self.is_liked
    }
    fn set_liked(&mut self, item_id: Option<i64>) {
        self.is_liked = item_id;
    }
}

impl QueueItem {
    // 模块内私有：签名携带本模块私有的 `InternalDragLedger`，公开到
    // `pub(in crate::ui)` 会触发 private_interfaces；唯一调用点在下方
    // 列表渲染处（同模块）。
    fn new(
        cx: &mut App,
        item: QueueItemData,
        idx: usize,
        drag_drop_manager: Entity<DragDropListManager>,
        scroll_handle: UniformListScrollHandle,
        selection: Entity<QueueSelection>,
        drag_ledger: Entity<InternalDragLedger>,
    ) -> Entity<Self> {
        cx.new(move |cx| {
            cx.on_release(|m: &mut QueueItem, cx| {
                queue_metadata::queue_drop_data(cx, &m.item);
            })
            .detach();

            let queue = cx.global::<Models>().queue.clone();
            cx.observe(&queue, |this: &mut QueueItem, queue, cx| {
                let position = queue.read(cx).position;
                // 行级精准守卫：只有本行「是否当前曲目」翻转才重绘（本行槽位
                // 与前后 position 比较）。旧写法缓存全局 position 再整体比较
                // !=，position 一变所有活行都满足条件而全量 notify（扇出 =
                // 活行数），与注释宣称的高亮移动精度不符——2026-11-08 改为
                // 谓词翻转判定，切歌的重绘从 ~viewport 行降到高亮移出的 2 行。
                // 播放态半边谓词由下方 playback_state 观察者覆盖；内容-only
                // 通知（position 不动）在此依旧零重绘。
                let was_current = this.current == this.idx;
                let is_current_now = position == this.idx;
                this.current = position;
                if was_current != is_current_now {
                    cx.notify();
                }
            })
            .detach();

            // 播放态镜像也要驱动行重绘：render 的 is_current 守卫读它，而
            // 停止/恢复不必然伴随 position 变化（thread.rs 的 stop() 只发
            // StateChanged，不发 QueuePositionChanged），只观察队列实体会让
            // 高亮在 Stopped↔Playing 转换上滞后到下一次队列通知。状态转换
            // 是用户操作频率，行数有界（虚拟化），扇出可忽略。
            let playback_state = cx.global::<PlaybackInfo>().playback_state.clone();
            cx.observe(&playback_state, |_, _, cx| {
                cx.notify();
            })
            .detach();

            let track_id = item.get_db_id();
            // 元数据住在 ui 侧旁表，以条目的 uid 为键（加载器与恢复分支同旧槽位）
            let data = queue_metadata::queue_ui_data(cx, &item);
            // 每个行视图生命周期只做一次 stat，而不是每帧一次
            let is_available = is_track_path_available(item.get_path());

            cx.observe(&data, |_, _, cx| {
                cx.notify();
            })
            .detach();

            // 与上面 position 观察者同一套值守卫：一次选中变更只影响旧/新两行，
            // 没有它时每个 keep-around 行都会跟着重绘（O(n) 扇出）。
            cx.observe(&selection, |this: &mut QueueItem, selection, cx| {
                let selected = selection.read(cx).contains(this.idx);
                if selected != this.was_selected {
                    this.was_selected = selected;
                    cx.notify();
                }
            })
            .detach();

            // 纯缓存读取、零 IO：liked-id 集合在每次收藏歌单变化时重载，
            // 且 is_song_liked 给出的正是每行存储的曲目 id 约定
            // （见 HasLikedState / is_song_liked）。
            let is_liked = track_id.and_then(|id| is_song_liked(cx, id));

            subscribe_liked_updates(cx, |this: &QueueItem| this.track_id);

            let was_selected = selection.read(cx).contains(idx);
            Self {
                item,
                idx,
                current: queue.read(cx).position,
                was_selected,
                drag_drop_manager,
                scroll_handle,
                selection,
                drag_ledger,
                // AddToPlaylist 面板（及其 show 旗标）只在右键菜单真正需要它
                // 的第一次才构建
                add_to: None,
                show_add_to: None,
                track_id,
                is_liked,
                is_available,
                duration_text: None,
                cached_duration: None,
                drag_base: None,
                drag_others: None,
            }
        })
    }

    /// 行被缓存后，槽位号只在这里变：点击处理与高亮都读 `idx`，不 notify
    /// 就会继续作用于旧槽位（重排/滚动后点错曲目）。
    pub fn update_idx(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.idx != idx {
            self.idx = idx;
            // 缓存行被复用到新槽位：守卫基线必须按新 idx 重算，否则下一次
            // 选中变更会拿旧槽位的选中态比较，漏掉本行应有的重绘。
            self.was_selected = self.selection.read(cx).contains(idx);
            cx.notify();
        }
    }

    /// 打开（首次使用时创建）该行的「加入歌单」面板并载入 `track_ids`；
    /// 单选行与多选右键菜单共用。
    fn open_add_to_palette(&mut self, track_ids: Vec<i64>, cx: &mut Context<Self>) {
        let show = self
            .show_add_to
            .get_or_insert_with(|| cx.new(|_| false))
            .clone();
        match &self.add_to {
            Some(add_to) => add_to.read(cx).set_track_ids(track_ids),
            None => self.add_to = Some(AddToPlaylist::new(cx, show.clone(), track_ids)),
        }
        show.write(cx, true);
        cx.notify();
    }
}

impl Render for QueueItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::QueueRow);
        let album_id = self.item.get_db_album_id();
        let track_id = self.item.get_db_id();
        let data_entity = queue_metadata::queue_ui_data(cx, &self.item);
        let ui_data = data_entity.read(cx);
        let theme = cx.global::<Theme>();
        let is_available = self.is_available;
        let is_selected = self.selection.read(cx).contains(self.idx);

        if let Some(item) = ui_data.as_ref() {
            // 仅在条目时长变化时重建时长文本。
            let duration_text = if self.cached_duration == item.duration {
                self.duration_text.clone()
            } else {
                let text = item
                    .duration
                    .map(|d| SharedString::from(crate::ui::util::format_duration_compact(d)));
                self.cached_duration = item.duration;
                self.duration_text = text.clone();
                text
            };
            let scrollbar_always_visible = {
                let settings = cx.global::<SettingsGlobal>();
                let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();

                settings.model.read(cx).interface.always_show_scrollbars
                    && scroll_handle.should_draw_vertical_scrollbar()
            };
            // queue.position 的 0 是双义值：停止态被播放线程置 0 作「无当前
            // 曲目」哨兵（thread.rs 的撤销到空路径发 QueuePositionChanged(0)，
            // 且 stop() 后 position 可能残留旧值），播放中的 0 则真的是队头
            // 在播——只比 current==idx 会让停止后的第 0 行假高亮（undo 清空
            // 播放历史后尤其误导）。播放态守卫：Stopped 下不高亮任何行。
            let playing =
                *cx.global::<PlaybackInfo>().playback_state.read(cx) != PlaybackState::Stopped;
            let is_current = playing && self.current == self.idx;
            let image_key = track_id.map(ManagedImageKey::Track).or_else(|| {
                #[cfg(feature = "online_sources")]
                {
                    // 在线（酷狗）曲目没有 db id；用它们的 HTTP 封面
                    if let Some(cover) = item.cover_url.clone().filter(|c| !c.is_empty()) {
                        return Some(ManagedImageKey::HttpCover(cover));
                    }
                }
                Some(ManagedImageKey::track_file(self.item.get_path()))
            });
            let idx = self.idx;
            // 停止态下 queue.position 是 0 哨兵/残留旧值（is_current 同款守卫），
            // 不作为 shift 区间选择的锚点——锚到不可见/误导行会展开错误区间。
            let current = playing.then_some(self.current);
            let selection = self.selection.clone();
            let selection_for_drag = selection.clone();
            let selection_for_aux = selection.clone();
            let selection_for_menu = selection.clone();
            let selection_read = selection.read(cx);
            let is_multi_selected = selection_read.is_multi() && selection_read.contains(idx);
            let single_track_id = self.track_id;
            let queue_item_entity = cx.entity();
            // add_to 是首次用菜单时才懒创建，它的存在与否不再能区分
            // 库内曲目与在线曲目
            let has_add_to = self.track_id.is_some();
            let is_liked = self.is_liked.is_some();

            let item_state =
                DragDropItemState::for_index(self.drag_drop_manager.read(cx), self.idx);

            let track_name = item
                .name
                .clone()
                .unwrap_or_else(|| tr!("UNKNOWN_TRACK").into());

            context(ElementId::View(cx.entity_id()))
                .with(
                    div()
                        .w_full()
                        .id("item-contents")
                        .flex()
                        .flex_shrink_0()
                        .overflow_x_hidden()
                        .gap(px(11.0))
                        .h(px(QUEUE_ITEM_HEIGHT))
                        .px(px(17.0))
                        .py(px(11.0))
                        // 常驻绘制滚动条时补额外内边距：
                        // 11px 队列行内边距 + 4px 滚动条 + 10px 余量
                        .when(scrollbar_always_visible, |div| div.pr(px(25.0)))
                        .when(is_available, |div| div.cursor_pointer())
                        .when(!is_available, |div| div.cursor_default().opacity(0.5))
                        .relative()
                        // 默认下边框——始终存在
                        .border_b(px(1.0))
                        .border_color(theme.border_color)
                        .when(item_state.is_being_dragged, |div| div.opacity(0.5))
                        .when(is_selected && !item_state.is_being_dragged, |div| {
                            div.bg(theme.queue_item_selected)
                        })
                        .when(
                            !is_selected && is_current && !item_state.is_being_dragged,
                            |div| {
                                div.bg(theme.queue_item_current)
                                    .border_l(px(3.0))
                                    .border_color(theme.button_primary)
                            },
                        )
                        .when(is_available, |div| {
                            div.on_click(move |event: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                let modifiers = event.modifiers();
                                let ctrl = modifiers.control || modifiers.platform;

                                let select_on_click = cx
                                    .global::<SettingsGlobal>()
                                    .model
                                    .read(cx)
                                    .interface
                                    .queue_select_on_click;

                                if event.click_count() == 2 {
                                    cx.global::<PlaybackInterface>().jump(idx);
                                } else if ctrl {
                                    selection.update(cx, |s, cx| s.ctrl_toggle(idx, cx));
                                } else if modifiers.shift {
                                    selection.update(cx, |s, cx| s.shift_range(idx, current, cx));
                                } else if select_on_click {
                                    selection.update(cx, |s, cx| s.select(idx, cx));
                                } else {
                                    cx.global::<PlaybackInterface>().jump(idx);
                                }
                            })
                        })
                        .when(
                            is_available && !is_selected && !item_state.is_being_dragged,
                            |div| {
                                div.hover(|div| div.bg(theme.queue_item_hover))
                                    .active(|div| div.bg(theme.queue_item_active))
                            },
                        )
                        .when(
                            is_available && is_selected && !item_state.is_being_dragged,
                            |div| {
                                div.hover(|div| div.bg(theme.queue_item_selected))
                                    .active(|div| div.bg(theme.queue_item_active))
                            },
                        )
                        .when(is_available, |div| {
                            // 拖拽负载基底只在显示名变化（元数据加载）时重建；
                            // 每帧只克隆它并重新盖上实时索引与选中状态，
                            // 不再重新分配路径与名称。
                            if self
                                .drag_base
                                .as_ref()
                                .is_none_or(|(name, _)| *name != track_name)
                            {
                                let base = if let Some(tid) = self.track_id {
                                    TrackDragData::from_track(
                                        tid,
                                        album_id,
                                        self.item.get_path().to_path_buf(),
                                        track_name.clone(),
                                    )
                                } else {
                                    TrackDragData::new(
                                        self.item.get_path().to_path_buf(),
                                        track_name.clone(),
                                    )
                                };
                                self.drag_base = Some((track_name.clone(), base));
                            }
                            let mut drag_data = self.drag_base.as_ref().unwrap().1.clone();
                            drag_data = drag_data.with_reorder_info(QUEUE_LIST_ID, idx);

                            if is_selected {
                                // 多选拖拽同伴：有序选中集减去本行。整个拖拽
                                // 会话用缓存而非每帧重算——为什么安全：
                                // 1. `QueueSelection::sorted()` 本就是每次变更
                                //    重建的快照，有序集合本身不会过期；本缓存
                                //    只去掉喂给负载的那次每帧 filter+collect 分配。
                                // 2. 每个选中变更器都由点击驱动
                                //    （`on_click`/`on_aux_click`），拖拽进行中
                                //    集合不可能变化。
                                // 3. 即使未来某个变更器在拖拽中途触发，它的
                                //    epoch 自增会在下一帧重建缓存——输出与旧的
                                //    每帧重算完全一致，绝不会过期。
                                // 4. 队列内容变化会清空选中（Queue 观察者）
                                //    → epoch 自增 → 坐标与 uid 指纹整组重建。
                                let selection_epoch = selection_for_drag.read(cx).epoch();
                                if self
                                    .drag_others
                                    .as_ref()
                                    .is_none_or(|c| c.epoch != selection_epoch || c.idx != idx)
                                {
                                    let others: Arc<[usize]> = selection_for_drag
                                        .read(cx)
                                        .sorted()
                                        .iter()
                                        .filter(|&&i| i != idx)
                                        .copied()
                                        .collect();
                                    // 同帧捕获同伴槽位的 uid 指纹（与上面的
                                    // 索引快照一一对应）：负载坐标在拖拽开始
                                    // 那一刻冻结，投放时要靠这份同帧指纹才能
                                    // 重验并重解析为当前索引（见
                                    // InternalDragLedger）。
                                    let others_uids: Arc<[u64]> = {
                                        let queue = cx.global::<Models>().queue.read(cx);
                                        let data = queue.data.read_recovered();
                                        others
                                            .iter()
                                            .map(|&i| data.get(i).map(|item| item.uid()))
                                            .collect::<Option<Vec<u64>>>()
                                            // 越界即选中集与队列脱钩（理论不可达：
                                            // 内容变化即清空选中）——记空指纹，
                                            // 投放侧按数量不符取消，而不是移错行
                                            .unwrap_or_default()
                                            .into()
                                    };
                                    self.drag_others = Some(DragOthers {
                                        epoch: selection_epoch,
                                        idx,
                                        indices: others,
                                        uids: others_uids,
                                    });
                                }
                                if let Some(cache) = &self.drag_others {
                                    // 引用计数自增：负载上的
                                    // `additional_indices` 是 `Arc<[usize]>`
                                    drag_data =
                                        drag_data.with_additional_indices(cache.indices.clone());
                                }
                            }

                            div.on_drag(drag_data, {
                                // 取一份克隆：下面的 move 闭包不能吞掉本次
                                // render 里后面还要复用的 `track_name`
                                let track_name = track_name.clone();
                                // 指纹与负载同帧冻结：source 的 uid 就是本行
                                // 条目（行视图以 uid 为键，与负载的 source 坐标
                                // 恒一致），同伴 uid 借上面的同帧缓存。
                                let own_uid = self.item.uid();
                                let others_uids =
                                    self.drag_others.as_ref().map(|cache| cache.uids.clone());
                                let drag_ledger = self.drag_ledger.clone();
                                // 本构造器由 gpui 在拖拽激活那一刻调用一次
                                // （官方语义：等价 on-drag-start）：把负载坐标与
                                // 同帧 uid 指纹一起记入账本——负载在整个拖拽
                                // 会话里恒定，这份账本就是投放侧唯一的对账依据。
                                move |drag, _, _, cx| {
                                    let entry = build_drag_fingerprints(
                                        drag,
                                        own_uid,
                                        others_uids.as_deref(),
                                    );
                                    drag_ledger.update(cx, |ledger, _| ledger.entry = entry);
                                    DragPreview::new(cx, track_name.clone())
                                }
                            })
                            .drag_over::<TrackDragData>(
                                move |style, _, _, _| style.bg(gpui::rgba(0x88888822)),
                            )
                        })
                        .on_aux_click(move |ev: &ClickEvent, _, cx| {
                            if ev.is_right_click() && !selection_for_aux.read(cx).contains(idx) {
                                selection_for_aux.update(cx, |s, cx| s.select(idx, cx));
                            }
                        })
                        .when_some(self.add_to.clone(), |this, that| this.child(that))
                        .child(DropIndicator::with_state(
                            item_state.is_drop_target_before,
                            item_state.is_drop_target_after,
                            theme.button_primary,
                        ))
                        .child(
                            div()
                                .relative()
                                .w(px(36.0))
                                .h(px(36.0))
                                .flex_shrink_0()
                                .child(
                                    div()
                                        .id("album-art")
                                        .rounded(px(theme.radius_sm))
                                        .bg(theme.album_art_background)
                                        .shadow_sm()
                                        .size_full()
                                        .when_some(image_key, |div, key| {
                                            div.child(
                                                managed_image(("queue-art", idx), key)
                                                    .size_full()
                                                    .object_fit(ObjectFit::Fill)
                                                    .rounded(px(theme.radius_sm))
                                                    .thumb(),
                                            )
                                        }),
                                )
                                .when(is_current, |container| {
                                    container.child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .rounded(px(theme.radius_sm))
                                            .bg(Rgba::new(0.0, 0.0, 0.0, 0.45))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .gap(px(2.0))
                                            .child(
                                                div()
                                                    .w(px(2.5))
                                                    .h(px(14.0))
                                                    .rounded_full()
                                                    .bg(theme.button_primary_text),
                                            )
                                            .child(
                                                div()
                                                    .w(px(2.5))
                                                    .h(px(10.0))
                                                    .rounded_full()
                                                    .bg(theme.button_primary_text),
                                            )
                                            .child(
                                                div()
                                                    .w(px(2.5))
                                                    .h(px(16.0))
                                                    .rounded_full()
                                                    .bg(theme.button_primary_text),
                                            ),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .line_height(rems(1.0))
                                .text_size(px(15.0))
                                .gap_1()
                                .w_full()
                                .overflow_x_hidden()
                                .child(
                                    div()
                                        .w_full()
                                        .text_ellipsis()
                                        .font_weight(FontWeight::EXTRA_BOLD)
                                        // 复用上面算好的每帧 `track_name`，
                                        // 不再每帧调一次 tr!
                                        .child(track_name),
                                )
                                .child(
                                    div()
                                        .overflow_x_hidden()
                                        .flex()
                                        .w_full()
                                        .max_w_full()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_ellipsis()
                                                .overflow_x_hidden()
                                                .flex_shrink(1.0)
                                                .child(item.artist_name.clone().unwrap_or_else(
                                                    || tr!("UNKNOWN_ARTIST").into(),
                                                )),
                                        )
                                        .when_some(duration_text, |child, text| {
                                            child.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .ml(px(6.0))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.text_secondary)
                                                    .child(text),
                                            )
                                        }),
                                ),
                        ),
                )
                .menu_on_open(move |_, cx| {
                    if is_multi_selected {
                        // 选中集与队列的快照在这里（菜单打开时）取，
                        // 而不是每帧渲染时取。
                        let remove_indices = selection_for_menu.read(cx).indices();
                        let remove_count = remove_indices.len();
                        // 每个选中行一条 (index, track_id, path)，在菜单打开时
                        // 取。菜单开着的时候队列可能并发变化（播放时自动插入、
                        // 从别的入口移除），所以「移除 N 项」回调删除前会重新
                        // 校验这份快照：光靠索引会把如今占在这些位置上的行删掉。
                        let remove_snapshot: Vec<(usize, Option<i64>, PathBuf)> = {
                            let queue = cx.global::<Models>().queue.read(cx);
                            let queue_data = queue.data.read_recovered();
                            remove_indices
                                .iter()
                                .filter_map(|&i| {
                                    queue_data
                                        .get(i)
                                        .map(|item| (i, item.get_db_id(), item.get_path().clone()))
                                })
                                .collect()
                        };
                        let selected_track_ids: Vec<i64> = remove_snapshot
                            .iter()
                            .filter_map(|&(_, id, _)| id)
                            .collect();
                        let add_to_ids = selected_track_ids.clone();
                        let entity_for_add = queue_item_entity.clone();

                        // 缓存的 liked-ids 集合（见 is_song_liked）：菜单只需要
                        // 已收藏/未收藏这个布尔量来决定图标与标签，且取消收藏
                        // 按曲目 id 删除——这里不需要 playlist_item 行 id，
                        // 也零 DB 访问。
                        let liked_ids: Vec<i64> = selected_track_ids
                            .iter()
                            .copied()
                            .filter(|id| is_song_liked(cx, *id).is_some())
                            .collect();
                        let any_liked = !liked_ids.is_empty();

                        menu()
                            .when(!add_to_ids.is_empty(), |menu| {
                                menu.item(menu_item(
                                    "add_to_playlist",
                                    Some(PLAYLIST_ADD),
                                    tr!("ADD_TO_PLAYLIST"),
                                    move |_, _, cx| {
                                        entity_for_add.update(cx, |item, cx| {
                                            item.open_add_to_palette(add_to_ids.clone(), cx);
                                        });
                                    },
                                ))
                                .item(menu_separator())
                            })
                            .when(!selected_track_ids.is_empty(), |menu| {
                                let track_ids_for_like = selected_track_ids.clone();
                                menu.item(menu_item(
                                    "toggle_like",
                                    Some(if any_liked { STAR_FILLED } else { STAR }),
                                    if any_liked {
                                        tr!("UNLIKE")
                                    } else {
                                        tr!("LIKE")
                                    },
                                    move |_, _, cx| {
                                        // 一次批量切换：对从未收藏过的 id，
                                        // 取消收藏的 DELETE 是空操作，所以无论
                                        // 哪一侧都只有已收藏的子集会变动
                                        // （见 toggle_album_like）。
                                        toggle_album_like(
                                            track_ids_for_like.clone(),
                                            any_liked,
                                            cx,
                                        );
                                    },
                                ))
                                .item(menu_separator())
                            })
                            .item(menu_item(
                                "remove_items",
                                Some(CROSS),
                                trn!(
                                    "REMOVE_N_FROM_QUEUE",
                                    "Remove {{count}} track from queue",
                                    "Remove {{count}} tracks from queue",
                                    count = remove_count
                                ),
                                move |_, _, cx| {
                                    // 仅当菜单打开时快照的每个槽位仍持有相同
                                    // 内容才执行；菜单开着期间队列被改动会让
                                    // 捕获的索引失效（见上面的 `remove_snapshot`）。
                                    if remove_snapshot.iter().all(|&(i, id, ref path)| {
                                        queue_slot_matches(cx, i, id, path)
                                    }) {
                                        cx.global::<PlaybackInterface>()
                                            .remove_items(remove_indices.clone());
                                    } else {
                                        error!(
                                            "queue remove cancelled: queue changed while the \
                                             Remove-N menu was open ({} rows)",
                                            remove_count
                                        );
                                        emit_stale_remove_toast();
                                    }
                                },
                            ))
                    } else {
                        let entity_for_add = queue_item_entity.clone();
                        // 在菜单打开时快照该行内容（与上面的多选快照同一时刻）：
                        // 点击落地前队列可能已变化，所以移除回调重新校验，
                        // 而不是直接相信捕获的 `idx`。
                        let remove_snapshot = {
                            let queue = cx.global::<Models>().queue.read(cx);
                            let queue_data = queue.data.read_recovered();
                            queue_data
                                .get(idx)
                                .map(|item| (item.get_db_id(), item.get_path().clone()))
                        };
                        let artist_ids = single_track_id
                            .and_then(|id| cx.artist_ids_for_track(id).ok())
                            .unwrap_or_default();
                        let can_go_to_artist = !artist_ids.is_empty();
                        // 「在文件管理器中显示」的数据源与移除回调共用同一份
                        // 打开时快照；克隆 path，别把 remove_snapshot 搬进闭包
                        let reveal_path = remove_snapshot
                            .as_ref()
                            .map(|(_, path)| path.clone());
                        menu()
                            .when(has_add_to, |menu| {
                                menu.item(
                                    menu_item(
                                        "go_to_album",
                                        Some(DISC),
                                        tr!("GO_TO_ALBUM", "Go to album"),
                                        move |_, _, cx| {
                                            if let Some(album_id) = album_id {
                                                let switcher =
                                                    cx.global::<Models>().switcher_model.clone();
                                                switcher.update(cx, |_, cx| {
                                                    cx.emit(ViewSwitchMessage::Release(
                                                        album_id, None,
                                                    ));
                                                })
                                            }
                                        },
                                    )
                                    .disabled(!is_available || album_id.is_none()),
                                )
                                .item(
                                    menu_item(
                                        "go_to_artist",
                                        Some(USERS),
                                        tr!("GO_TO_ARTIST", "Go to artist"),
                                        move |ev, _, cx| {
                                            navigate_to_artists(
                                                cx,
                                                artist_ids.clone(),
                                                ev.position(),
                                            );
                                        },
                                    )
                                    .disabled(!is_available || !can_go_to_artist),
                                )
                                .when_some(reveal_path, |menu, path| {
                                    menu.item(menu_item(
                                        "queue_show_in_file_manager",
                                        Some(FOLDER_SEARCH),
                                        crate::ui::library::context_menus::track_show_in_file_manager_label(),
                                        move |_, _, cx| {
                                            crate::ui::util::reveal_path_for_file_manager(
                                                path.as_path(),
                                                cx,
                                            );
                                        },
                                    ))
                                })
                                .item(menu_separator())
                                .item(menu_item(
                                    "add_to_playlist",
                                    Some(PLAYLIST_ADD),
                                    tr!("ADD_TO_PLAYLIST"),
                                    move |_, _, cx| {
                                        if let Some(track_id) = single_track_id {
                                            entity_for_add.update(cx, |item, cx| {
                                                item.open_add_to_palette(vec![track_id], cx);
                                            });
                                        }
                                    },
                                ))
                                .item(menu_separator())
                                .when_some(single_track_id, |menu, track_id| {
                                    let entity = queue_item_entity.clone();
                                    menu.item(
                                        menu_item(
                                            "toggle_like",
                                            Some(if is_liked { STAR_FILLED } else { STAR }),
                                            if is_liked { tr!("UNLIKE") } else { tr!("LIKE") },
                                            move |_, _, cx| {
                                                toggle_like(track_id, entity.clone(), cx);
                                            },
                                        )
                                        .disabled(!is_available),
                                    )
                                })
                                .item(menu_separator())
                            })
                            .item(menu_item(
                                "remove_item",
                                Some(CROSS),
                                tr!("REMOVE_FROM_QUEUE", "Remove from queue"),
                                move |_, _, cx| {
                                    // 与上面 remove_items 同样的过期索引守卫：
                                    // 只有快照的行内容仍在此位置时才删除。
                                    let unchanged =
                                        remove_snapshot.as_ref().is_some_and(|&(id, ref path)| {
                                            queue_slot_matches(cx, idx, id, path)
                                        });
                                    if unchanged {
                                        cx.global::<PlaybackInterface>().remove_item(idx);
                                    } else {
                                        error!(
                                            "queue remove cancelled: queue changed while the \
                                             remove menu was open (row {idx})"
                                        );
                                        emit_stale_remove_toast();
                                    }
                                },
                            ))
                    }
                    .into_any_element()
                })
                .into_any_element()
        } else {
            // 元数据仍在加载：用普通方块保住该行的形状，列表不跳动；
            // 数据到位后条目会自行重绘。分隔线画在底边，与已加载行
            // （border_b）同口径——顶边会与队头标题的 border_b 叠成 2px，
            // 且与下一行交界处漏线。
            div()
                .h(px(QUEUE_ITEM_HEIGHT))
                .border_b(px(1.0))
                .border_color(theme.border_color)
                .w_full()
                .id(ElementId::View(cx.entity_id()))
                .into_any_element()
        }
    }
}

/// 提醒用户：排队的移除操作被丢弃，因为队列在其右键菜单打开期间发生了变化
/// （见 `queue_slot_matches`）。
fn emit_stale_remove_toast() {
    emit_toast(Toast::warning(tr!(
        "QUEUE_REMOVE_CANCELLED_STALE",
        "Queue changed — removal cancelled"
    )));
}

/// 若活队列在 `index` 处仍持有右键菜单打开时捕获的内容（曲目 id 与路径都相同）
/// 则为真。
///
/// 缘由：守护队列右键菜单的移除动作——菜单开着的时候队列可以并发变化
/// （播放时自动插入、从别的入口移除），所以菜单打开时捕获的索引到点击落地时
/// 可能指向别的行，照删会删错行。播放侧的 `dequeue_many` 只会把越界索引钳到
/// 边界，无从知道本意是哪几行，因此内容校验必须放在这里、UI 侧。
fn queue_slot_matches(cx: &App, index: usize, track_id: Option<i64>, path: &PathBuf) -> bool {
    let queue = cx.global::<Models>().queue.read(cx);
    let queue_data = queue.data.read_recovered();
    queue_data
        .get(index)
        .is_some_and(|item| item.get_db_id() == track_id && item.get_path() == path)
}

/// 队列内重排拖拽的会话指纹：拖拽开始那一刻与负载坐标同帧捕获的各槽位
/// `QueueItemData::uid`（source 在首位、additional 按负载顺序随后）。
///
/// 缘由：拖拽负载的 source/additional 索引冻结在拖拽开始那一帧，而投放
/// 目标（指示线）是活跃坐标——拖拽期间 EOF 消费/stop-after-current/洗牌
/// 回绕任一发生，冻结索引就会与活跃坐标混算（`move_items` 移错行）。右键
/// 移除路径对同类并发有 `queue_slot_matches` 逐槽位内容守卫；拖拽路径的
/// 等价物就是本结构：按实例 uid（跨克隆稳定、永不复用）识别被拖的行，
/// 投放时重解析为当前索引（见 [`resolve_internal_drag_core`]）。
struct InternalDragFingerprints {
    /// 负载的 source_index（拖拽开始帧的坐标）。投放时与负载核对——账本
    /// 必须属于"这一次"拖拽会话，上一会话的残留不得复用。
    source_index: usize,
    /// 负载的 additional_indices（同一帧的坐标）。
    additional: Arc<[usize]>,
    /// 被拖各槽位的 uid：`[source, additional…]`，与上面两个坐标一一对应。
    uids: Arc<[u64]>,
}

/// 指纹账本：行视图在拖拽开始时写入，队列的投放回调读取对账。独立小实体
/// 而不是挂在 Queue 或 QueueSelection 的字段上——行与列表都持有句柄，共享
/// 方式与 `drag_drop_manager` 相同；它只在拖拽开始/投放这两个用户操作频率
/// 的时刻被触碰，不在任何渲染热路径上。
struct InternalDragLedger {
    /// 最近一次队列内拖拽的指纹；None = 没有可验证的会话（投放侧取消）。
    entry: Option<InternalDragFingerprints>,
}

/// 投放决策：指纹重验通过后，被拖条目在活队列里的当前坐标。
struct ResolvedDrag {
    source: usize,
    additional: Vec<usize>,
}

/// 拖拽开始时把负载坐标与同帧捕获的槽位 uid 组成会话指纹。负载与指纹
/// 必须出自同一帧渲染（坐标在负载里冻结、同伴 uid 在 `DragOthers` 缓存里
/// 冻结）；对不上号（source 缺失、多选坐标与同伴指纹数量不符）就记 None
/// ——账本里没有可验证的会话，投放时按取消处理。
fn build_drag_fingerprints(
    drag: &TrackDragData,
    source_uid: u64,
    others_uids: Option<&[u64]>,
) -> Option<InternalDragFingerprints> {
    let source_index = drag.source_index?;
    let additional = drag.additional_indices.clone();
    let mut uids = Vec::with_capacity(1 + additional.len());
    uids.push(source_uid);
    if !additional.is_empty() {
        let others = others_uids?;
        if others.len() != additional.len() {
            return None;
        }
        uids.extend_from_slice(others);
    }
    Some(InternalDragFingerprints {
        source_index,
        additional,
        uids: uids.into(),
    })
}

/// 投放决策的纯核心（无 gpui 依赖，便于单测）：账本指纹 ↔ 负载坐标 ↔
/// 活跃队列三方对账。
///
/// 返回 `Some` 当且仅当：账本存在且坐标与负载逐一相符（防上一会话残留），
/// 且每个被拖条目的 uid 都仍在活队列里——此时给出重解析后的当前索引，
/// 落点换算（`calculate_move_target` / `corrected_multi_drop_target`）全部
/// 以当前坐标进行。任一条目已离开队列即整组取消（返回 None）：用户拖了
/// 一个已经变化的队列，取消比移错行正确（与右键移除的守卫同一取舍，但
/// 那边校验的是"槽位内容没变"，这边是"被拖的行还在线"）。
///
/// uid 在队列内视为唯一（行视图 `views_model` 同以 uid 为键，重复实例会在
/// 更上游塌缩）。
fn resolve_internal_drag_core(
    entry: Option<&InternalDragFingerprints>,
    payload_source: Option<usize>,
    payload_additional: &[usize],
    queue_uids: &[u64],
) -> Option<ResolvedDrag> {
    let entry = entry?;
    let source_coord = payload_source?;
    if entry.source_index != source_coord || entry.additional.as_ref() != payload_additional {
        return None;
    }
    let index_by_uid: FxHashMap<u64, usize> = queue_uids
        .iter()
        .enumerate()
        .map(|(i, &uid)| (uid, i))
        .collect();
    let (source_uid, others_uids) = entry.uids.split_first()?;
    let source = *index_by_uid.get(source_uid)?;
    let additional = others_uids
        .iter()
        .map(|uid| index_by_uid.get(uid).copied())
        .collect::<Option<Vec<usize>>>()?;
    Some(ResolvedDrag { source, additional })
}

/// `resolve_internal_drag_core` 的 gpui 包装：读账本与活队列，喂给纯核心。
fn resolve_internal_drag(
    cx: &App,
    ledger: &Entity<InternalDragLedger>,
    drag: &TrackDragData,
) -> Option<ResolvedDrag> {
    let queue = cx.global::<Models>().queue.read(cx);
    let data = queue.data.read_recovered();
    let queue_uids: Vec<u64> = data.iter().map(|item| item.uid()).collect();
    resolve_internal_drag_core(
        ledger.read(cx).entry.as_ref(),
        drag.source_index,
        &drag.additional_indices,
        &queue_uids,
    )
}

pub struct Queue {
    /// 行视图以所属条目的 uid 为键（`QueueItemData::uid`）——跨克隆与队列变更
    /// 都稳定，不像旧的「每条目实体 id」键，且无需触碰任何实体就能取到。
    views_model: Entity<FxHashMap<u64, Entity<QueueItem>>>,
    show_queue: Entity<bool>,
    scroll_handle: UniformListScrollHandle,
    drag_drop_manager: Entity<DragDropListManager>,
    selection: Entity<QueueSelection>,
    /// 队列内重排拖拽的指纹账本（行视图在拖拽开始时写入，投放回调读取
    /// 对账，见 [`InternalDragLedger`]）。
    drag_ledger: Entity<InternalDragLedger>,
    last_queue_position: usize,
    queue_hovered: bool,
    follow_current_pending: bool,
    follow_frame_scheduled: bool,
    scroll_follow: SmoothScrollFollow,
    /// 格式化好的「(N 首) • (总时长)」摘要，对缓存的 `(queue_len, total_seconds)`
    /// 键有效。总时长是增量维护的（见 `rescan_views_and_summary`），所以渲染
    /// 路径只在键真的变动时才重新格式化。
    cached_summary: (usize, i64, SharedString),
    /// 迄今已解析出的时长之和；`rescan_views_and_summary` 每次队列内容变化都
    /// 重建，渲染期间随着待加载元数据落地继续累加。
    summary_total: i64,
    /// 时长尚未解析完成的索引集合。渲染只精确读这批（从而保持对它们元数据
    /// 实体的响应式依赖），而不是每帧重扫整个队列。
    pending_durations: Vec<usize>,
    /// 上一次执行行视图修剪所针对的槽位窗口 `(start, end)`。窗口没变时列表
    /// 闭包会跳过重建保留集；`rescan_views_and_summary` 每次队列内容变化都
    /// 重置它，因为槽位键随条目一起移动——变更后即便槽位窗口不变，键集合
    /// 也可能不同。仅位置变化的 notify 刻意不重置：没有键移动，而若跟随滚动
    /// 把另一段槽位范围带入视野，窗口元组本身就变了，修剪自然重跑。
    last_prune_window: (usize, usize),
    /// 面板隐藏期间收到队列内容变化（content_epoch 移动，见观察者）时置位：
    /// 行视图修剪与摘要重扫推迟到面板重新显示后的首次渲染。仅位置变化的
    /// notify 不置位——没有要补的账。
    needs_queue_rescan: bool,
    /// 截至上一次 `rescan_views_and_summary` 时 `models::Queue::content_epoch`
    /// 的镜像：观察者用来判别内容是否变化。位置移动但 epoch 没动的队列 notify
    /// （每次切歌走的 `QueuePositionChanged` 路径）整个跳过重扫；为什么这样
    /// 安全见观察者。
    last_seen_content_epoch: u64,
}

impl Queue {
    pub fn new(cx: &mut App, show_queue: Entity<bool>) -> Entity<Self> {
        cx.new(|cx| {
            let views_model = cx.new(|_| FxHashMap::default());
            let items = cx.global::<Models>().queue.clone();
            let initial_queue_position = items.read(cx).position;
            let initial_content_epoch = items.read(cx).content_epoch;
            let initial_has_current_track =
                cx.global::<PlaybackInfo>().current_track.read(cx).is_some();

            let config = DragDropListConfig::new(QUEUE_LIST_ID, px(QUEUE_ITEM_HEIGHT));
            let drag_drop_manager = DragDropListManager::new(cx, config);
            let selection = QueueSelection::new(cx);
            let drag_ledger = cx.new(|_| InternalDragLedger { entry: None });

            cx.observe(&items, move |this: &mut Queue, _, cx| {
                let (new_position, new_content_epoch) = {
                    let queue_state = cx.global::<Models>().queue.read(cx);
                    (queue_state.position, queue_state.content_epoch)
                };
                if this.last_queue_position != new_position {
                    this.last_queue_position = new_position;
                    this.follow_current_pending = true;
                    this.scroll_follow.cancel();
                }

                // 仅位置变化的通知（QueuePositionChanged：当前曲目索引移动了）：
                // 它不改动活 uid 集合，也不改任何条目的时长状态，所以重扫的三个
                // 产物对它全是空操作——行视图 retain 会保留同一批键，元数据旁表
                // GC 什么也不会丢弃，摘要状态（总时长 + 待解析索引）不变。
                // 这条路径整个跳过 O(n) 扫描；过去每次切歌都要付这笔开销。
                // 位置变化「确实需要」的事都按设计在别处发生：当前行高亮由
                // `QueueItem` 自己的观察者逐行守卫，跟随滚动的记账就在上面。
                // 判别依据是 content_epoch 镜像（只在事件桥收到 QueueUpdated
                // 时自增）：一批同时带内容变化与位置移动的事件仍会重扫，
                // 因为 epoch 动了。
                let content_changed = this.last_seen_content_epoch != new_content_epoch;
                this.last_seen_content_epoch = new_content_epoch;

                // 修剪与摘要重扫只在面板可见时才有意义；面板隐藏期间这些活
                // 推迟到重新显示后的首次渲染。
                if content_changed {
                    if *this.show_queue.read(cx) {
                        this.rescan_views_and_summary(cx);
                    } else {
                        this.needs_queue_rescan = true;
                        // 摘要/修剪可以推迟到重新显示，槽位回收不行：见
                        // `reclaim_stale_slots` 的缘由。
                        this.reclaim_stale_slots(cx);
                    }
                    // 仅位置切换时各行索引保持不变，此时清空选中只会让每条
                    // 活行重绘一遍，还白白丢掉一次多选。
                    this.selection.update(cx, |s, cx| s.clear(cx));
                }

                cx.notify();
            })
            .detach();

            Self {
                views_model,
                show_queue,
                scroll_handle: UniformListScrollHandle::new(),
                drag_drop_manager,
                selection,
                drag_ledger,
                last_queue_position: initial_queue_position,
                queue_hovered: false,
                follow_current_pending: initial_has_current_track,
                follow_frame_scheduled: false,
                scroll_follow: SmoothScrollFollow::new(QUEUE_FOLLOW_ANIMATION_DURATION),
                // usize::MAX 永远匹配不上真实的队列长度：强制构建一次
                cached_summary: (usize::MAX, 0, SharedString::default()),
                summary_total: 0,
                pending_durations: Vec::new(),
                // usize::MAX 永远匹配不上真实的槽位窗口：强制首次渲染就修剪
                last_prune_window: (usize::MAX, usize::MAX),
                // 首次渲染从零开始构建摘要状态
                needs_queue_rescan: true,
                // 与实体构造时的 epoch 一致；此后的第一次内容变化仍会重扫
                last_seen_content_epoch: initial_content_epoch,
            }
        })
    }

    /// 只回收"已经离开队列的槽位"：行视图与元数据旁表各 retain 一次。这是
    /// `rescan_views_and_summary` 里与显示无关的那一半，面板隐藏时由观察者单独跑。
    ///
    /// 为什么要跑：旁表以 `QueueItemData::uid` 为键，而 uid 是单调自增、永不复用的，
    /// 每个首次触到的条目都会 `cx.new` 出一个实体槽位。此前清理只发生在重扫里，
    /// 而重扫被面板可见性推迟——面板关着时 `Queue::render` 根本不入树，于是
    /// "下一首 / 加入队列"每动一次就永久留下一批槽位（无界增长），并把全部清理
    /// 压到重新显示后的第一帧。这里付一次 O(n) 的 uid 收集，换来上界与首帧平滑。
    fn reclaim_stale_slots(&mut self, cx: &mut App) {
        let key_set: FxHashSet<u64> = cx
            .global::<Models>()
            .queue
            .clone()
            .read(cx)
            .data
            .read_recovered()
            .iter()
            .map(|item| item.uid())
            .collect();

        self.views_model.update(cx, |m, _| {
            m.retain(|k, _| key_set.contains(k));
        });
        queue_metadata::gc_retaining(cx, key_set.into_iter());
    }

    /// 面板可见期间每次队列内容变化都跑一遍的对队列数据单趟扫描（仅位置变化的
    /// notify 会跳过——uid 集合与每条目的时长状态都没变，见观察者的
    /// content_epoch 判别），外加面板显示后的那次延迟首次渲染：丢弃 uid 已不在
    /// 队列里的行视图（以及元数据旁表槽位），并重建增量摘要状态
    /// （已解析总时长 + 时长仍待解析的索引）。
    fn rescan_views_and_summary(&mut self, cx: &mut Context<Self>) {
        let data = cx.global::<Models>().queue.read(cx).data.clone();
        let queue = data.read_recovered();

        // uid 总是拿得到（不需要每条目的实体），所以不像旧的实体 id 键那样
        // 会跳过某些行。
        let mut valid_keys: Vec<u64> = Vec::with_capacity(queue.len());
        let mut pending: Vec<usize> = Vec::new();
        let mut total: i64 = 0;
        for (i, item) in queue.iter().enumerate() {
            valid_keys.push(item.uid());
            match item.known_duration() {
                Some(secs) => total += secs,
                None => pending.push(i),
            }
        }
        let key_set: FxHashSet<u64> = valid_keys.iter().copied().collect();
        drop(queue);

        self.views_model.update(cx, |m, _| {
            m.retain(|k, _| key_set.contains(k));
        });
        // 对元数据旁表做 GC：条目已离开队列的槽位在这里释放其实体句柄
        // （它们的行刚被上面的 retain 丢弃，并在 release 时清空了内容）。
        queue_metadata::gc_retaining(cx, key_set.into_iter());
        // 槽位键随条目一起移动：即使槽位窗口本身没变，也要让下一次窗口修剪
        // 强制重跑
        self.last_prune_window = (usize::MAX, usize::MAX);
        self.summary_total = total;
        self.pending_durations = pending;
    }
}

impl Render for Queue {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Queue);
        check_drag_cancelled(self.drag_drop_manager.clone(), cx);

        // 面板隐藏期间到过的内容变化：现在就补上，在摘要与列表构建之前。
        if self.needs_queue_rescan {
            self.rescan_views_and_summary(cx);
            self.needs_queue_rescan = false;
        }

        let queue_len = {
            let queue = cx.global::<Models>().queue.clone().read(cx);
            queue.data.read_recovered().len()
        };
        // 只解析时长仍待完成的条目：在这里读取它们能保持对其元数据实体的
        // 响应式依赖，所以每次加载落地时摘要都正好刷新——不必每帧重扫整个队列。
        if !self.pending_durations.is_empty() {
            let data = cx.global::<Models>().queue.read(cx).data.clone();
            let queue = data.read_recovered();
            let mut i = 0;
            while i < self.pending_durations.len() {
                let idx = self.pending_durations[i];
                // 实体读取保持响应式依赖（元数据落地即刷新摘要）；但行被滚动
                // 修剪后 on_release 会清空仍在线条目的实体内容，而加载器早已
                // 同帧写过原子 known_duration——只看实体会让 pending 永不收
                // 敛、摘要在下次内容变化 rescan 前少计该曲时长。实体空时退回
                // 原子兜底（loader 同帧写两侧，不会双计）。
                let secs = queue.get(idx).and_then(|item| {
                    queue_metadata::queue_loaded_duration(cx, item)
                        .or_else(|| item.known_duration())
                });
                match secs {
                    Some(secs) => {
                        self.summary_total += secs;
                        self.pending_durations.swap_remove(i);
                    }
                    None => i += 1,
                }
            }
        }
        // (长度, 总秒数) 键不变时复用已格式化的摘要；只有任一值真的移动才
        // 重跑 format!/trn!。
        let queue_summary =
            if self.cached_summary.0 == queue_len && self.cached_summary.1 == self.summary_total {
                self.cached_summary.2.clone()
            } else {
                let summary = SharedString::from(format!(
                    "{} • {}",
                    trn!(
                        "QUEUE_SUMMARY_TRACKS",
                        "{{count}} song",
                        "{{count}} songs",
                        count = queue_len as i64
                    ),
                    crate::ui::util::format_duration_compact(self.summary_total)
                ));
                self.cached_summary = (queue_len, self.summary_total, summary.clone());
                summary
            };
        let views_model = self.views_model.clone();
        let this_entity = cx.entity();
        let scroll_handle = self.scroll_handle.clone();
        let item_scroll_handle = scroll_handle.clone();
        let drag_drop_manager = self.drag_drop_manager.clone();
        let selection = self.selection.clone();
        let drag_ledger = self.drag_ledger.clone();
        let reduced_motion = reduced_motion_enabled(cx);
        let is_dragging = self.drag_drop_manager.read(cx).state.is_dragging;

        if self.scroll_follow.is_active() && (self.queue_hovered || is_dragging) {
            self.scroll_follow.cancel();
        }

        if reduced_motion {
            if self.follow_current_pending || self.scroll_follow.is_active() {
                self.advance_follow_animation(window, cx, reduced_motion);
            }
        } else if (self.follow_current_pending || self.scroll_follow.is_active())
            && !self.queue_hovered
            && !is_dragging
        {
            self.schedule_follow_frame(window, cx);
        }

        // 在上面最后一次使用 `&mut cx` 之后才借用主题；下面的逐元素样式只读
        // 它的 Copy 字段。
        let theme = cx.global::<Theme>();

        div()
            .h_full()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .py(px(11.0))
                    .pl(px(18.0))
                    .pr(px(12.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border_color)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .line_height(rems(1.1))
                            .child(
                                div()
                                    .line_height(px(26.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(22.0))
                                    .child(tr!("QUEUE_TITLE", "Queue")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.text_secondary)
                                    .child(queue_summary),
                            ),
                    )
                    .child(
                        button()
                            .ml_auto()
                            .style(ButtonStyle::Minimal)
                            .size(ButtonSize::Large)
                            .child(icon(TRASH).size(ICON_SM).my_auto())
                            .child(tr!("CLEAR_QUEUE", "Clear"))
                            .id("clear-queue")
                            .on_click(|_, _, cx| {
                                cx.global::<PlaybackInterface>().clear_queue();
                            }),
                    )
                    .child(
                        nav_button("close", CROSS)
                            .on_click(cx.listener(|this: &mut Self, _, _, cx| {
                                this.show_queue.update(cx, |v, _| {
                                    *v = !(*v);
                                    crate::log_mem_event(if *v {
                                        "sidebar: queue show"
                                    } else {
                                        "sidebar: queue hide"
                                    });
                                })
                            }))
                            .tooltip(build_tooltip(tr!("CLOSE", "Close"))),
                    ),
            )
            .child(
                div()
                    .id("queue-list-container")
                    .flex()
                    .w_full()
                    .h_full()
                    .relative()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.selection.update(cx, |s, cx| s.clear(cx));
                    }))
                    .on_hover(cx.listener(|this, is_hovering: &bool, _, cx| {
                        if this.queue_hovered == *is_hovering {
                            return;
                        }

                        this.queue_hovered = *is_hovering;

                        if *is_hovering {
                            this.scroll_follow.cancel();
                        }

                        cx.notify();
                    }))
                    .on_drag_move::<TrackDragData>(cx.listener(
                        move |this: &mut Queue,
                              event: &DragMoveEvent<TrackDragData>,
                              window,
                              cx| {
                            let before = {
                                let manager = this.drag_drop_manager.read(cx);
                                (manager.state.is_dragging, manager.state.drop_target)
                            };
                            let scroll_handle: ScrollableHandle = this.scroll_handle.clone().into();
                            let scrolled = handle_track_drag_move(
                                this.drag_drop_manager.clone(),
                                scroll_handle,
                                event,
                                queue_len,
                                cx,
                                reduced_motion_enabled(cx),
                            );
                            this.finish_drag_move(before, scrolled, window, cx);
                        },
                    ))
                    .on_drag_move::<AlbumDragData>(cx.listener(
                        move |this: &mut Queue,
                              event: &DragMoveEvent<AlbumDragData>,
                              window,
                              cx| {
                            let before = {
                                let manager = this.drag_drop_manager.read(cx);
                                (manager.state.is_dragging, manager.state.drop_target)
                            };
                            let scroll_handle: ScrollableHandle = this.scroll_handle.clone().into();
                            let mouse_pos = event.event.position;
                            let container_bounds = event.bounds;
                            let scrolled = handle_external_drag_move(
                                this.drag_drop_manager.clone(),
                                scroll_handle,
                                mouse_pos,
                                container_bounds,
                                queue_len,
                                cx,
                                reduced_motion_enabled(cx),
                            );
                            this.finish_drag_move(before, scrolled, window, cx);
                        },
                    ))
                    .on_drop(cx.listener(
                        move |this: &mut Queue, drag_data: &TrackDragData, _, cx| {
                            let is_internal = drag_data
                                .source_list_id
                                .as_ref()
                                .map(|id| *id == QUEUE_LIST_ID.into())
                                .unwrap_or(false);

                            if is_internal {
                                // 回调内需再读一次投放目标来定补偿边界：handle_track_drop_multi
                                // 在回调返回之后才 end_drag 清空它（drag_drop.rs:726→730）。
                                let drop_manager = this.drag_drop_manager.clone();
                                let drag_ledger = this.drag_ledger.clone();
                                handle_track_drop_multi(
                                    this.drag_drop_manager.clone(),
                                    drag_data,
                                    cx,
                                    move |drag_data, _to, cx| {
                                        // 内容重验（照右键移除的 queue_slot_matches
                                        // 先例，但按实例 uid 而非槽位内容比对）：
                                        // 负载的 source/additional 索引冻结在拖拽
                                        // 开始那一帧，投放目标却是活跃坐标——期间
                                        // EOF 消费/stop-after-current/洗牌回绕任一
                                        // 发生即混算。先按指纹把被拖条目重解析为
                                        // 当前索引；任一条目已不在队列或账本对不上，
                                        // 就整个取消（用户拖了一个已经变化的队列，
                                        // 取消比移错行正确）。
                                        let drop_target = drop_manager.read(cx).state.drop_target;
                                        let Some((target_index, position)) = drop_target else {
                                            // 理论不可达：handle_track_drop_multi 仅在
                                            // drop_target 存在时才回调；保守取消。
                                            return;
                                        };
                                        let resolved =
                                            resolve_internal_drag(cx, &drag_ledger, drag_data);
                                        let Some(resolved) = resolved else {
                                            debug!(
                                                "queue drop cancelled: queue changed while \
                                                 dragging (source={:?}, additional={:?})",
                                                drag_data.source_index,
                                                drag_data.additional_indices.as_ref(),
                                            );
                                            return;
                                        };
                                        // 以重解析后的 source 重算落点：`_to` 是
                                        // handle_track_drop_multi 用冻结 source 算出的，
                                        // 队列变化后会偏离指示线；用同一纯函数以当前
                                        // source 重算，队列未变时与 `_to` 恒等。
                                        let to = calculate_move_target(
                                            resolved.source,
                                            target_index,
                                            position,
                                        );
                                        if drag_data.additional_indices.is_empty() {
                                            cx.global::<PlaybackInterface>()
                                                .move_item(resolved.source, to);
                                        } else {
                                            // 坐标系换算与边界公式的 rationale 见
                                            // corrected_multi_drop_target 契约；bound 是投放
                                            // 间隙的原始坐标上界（Before 目标行 t → t，After → t+1）。
                                            let bound = match position {
                                                DropPosition::Before => target_index,
                                                DropPosition::After => target_index + 1,
                                            };
                                            let corrected_to = corrected_multi_drop_target(
                                                to,
                                                &resolved.additional,
                                                bound,
                                            );
                                            let mut indices = resolved.additional;
                                            indices.push(resolved.source);
                                            indices.sort_unstable();
                                            indices.dedup();
                                            cx.global::<PlaybackInterface>()
                                                .move_items(indices, corrected_to);
                                        }
                                    },
                                );
                            } else {
                                let queue_item = QueueItemData::new(
                                    drag_data.path.clone(),
                                    drag_data.track_id,
                                    drag_data.album_id,
                                );

                                let drop_target = this.drag_drop_manager.read(cx).state.drop_target;

                                if let Some((target_index, position)) = drop_target {
                                    let insert_pos = match position {
                                        DropPosition::Before => target_index,
                                        DropPosition::After => target_index + 1,
                                    };
                                    cx.global::<PlaybackInterface>()
                                        .insert_at(queue_item, insert_pos);
                                } else {
                                    cx.global::<PlaybackInterface>().queue(queue_item);
                                }

                                this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                            }
                            cx.notify();
                        },
                    ))
                    // 专辑拖放
                    .on_drop(cx.listener(
                        move |this: &mut Queue, drag_data: &AlbumDragData, _, cx| {
                            use crate::ui::library_access::LibraryAccess;

                            if let Ok(tracks) = cx.list_tracks_in_album(drag_data.album_id) {
                                let queue_items: Vec<QueueItemData> = tracks
                                    .iter()
                                    .map(|track| {
                                        let item = QueueItemData::new(
                                            track.location.clone(),
                                            Some(track.id),
                                            Some(drag_data.album_id),
                                        );
                                        item.set_known_duration(Some(track.duration));
                                        item
                                    })
                                    .collect();

                                let drop_target = this.drag_drop_manager.read(cx).state.drop_target;

                                if let Some((target_index, position)) = drop_target {
                                    let insert_pos = match position {
                                        DropPosition::Before => target_index,
                                        DropPosition::After => target_index + 1,
                                    };
                                    cx.global::<PlaybackInterface>()
                                        .insert_list_at(queue_items, insert_pos);
                                } else {
                                    cx.global::<PlaybackInterface>().queue_list(queue_items);
                                }
                            }
                            this.drag_drop_manager.update(cx, |m, _| m.state.end_drag());
                            cx.notify();
                        },
                    ))
                    .child(
                        uniform_list("queue", queue_len, move |range, _, cx| {
                            let start = range.start;

                            let queue = cx
                                .global::<Models>()
                                .queue
                                .clone()
                                .read(cx)
                                .data
                                .read_recovered();

                            if range.end <= queue.len() {
                                // 每行的视图键就是其条目的 uid：跨克隆稳定且
                                // 总能拿到——持有队列守卫期间不需要 `cx`
                                let keys: Vec<(usize, u64)> =
                                    (start..range.end).map(|i| (i, queue[i].uid())).collect();

                                // 滚动修剪：行视图存在 `views_model` 里、以条目 uid
                                // 为键，单向滚动时不会移除任何一个，所以长队列会为
                                // 每个渲染过的槽位累积一个活行实体（连带其观察者）。
                                // 只保留可见窗口 ± 表格用的同一条带；被修剪的行按需
                                // 重建。保留集只在窗口移动时才重建；队列变化会在
                                // `rescan_views_and_summary` 里重置
                                // `last_prune_window`，所以变更后即便槽位窗口不变
                                // 也一定重新修剪。
                                let last_prune_window = this_entity.read(cx).last_prune_window;
                                let prune_start = start.saturating_sub(VIEW_KEEP_AROUND);
                                let prune_end = (range.end + VIEW_KEEP_AROUND).min(queue.len());
                                let keep = if last_prune_window != (prune_start, prune_end) {
                                    Some(
                                        (prune_start..prune_end)
                                            .filter_map(|i| queue.get(i).map(|item| item.uid()))
                                            .collect::<FxHashSet<u64>>(),
                                    )
                                } else {
                                    None
                                };

                                drop(queue);

                                if let Some(keep) = keep {
                                    views_model
                                        .update(cx, |m, _| m.retain(|k, _| keep.contains(k)));
                                    this_entity.update(cx, |this, _| {
                                        this.last_prune_window = (prune_start, prune_end);
                                    });
                                }

                                keys.into_iter()
                                    .map(|(idx, item_key)| {
                                        let drag_drop_manager = drag_drop_manager.clone();
                                        let scroll_handle = item_scroll_handle.clone();
                                        let item_selection = selection.clone();
                                        let item_ledger = drag_ledger.clone();

                                        // uid 总在手上，所以「为了读实体 id 而先
                                        // 构建元数据实体」这个旧兜底已移除
                                        let view = create_or_retrieve_view(
                                            &views_model,
                                            item_key,
                                            move |cx| {
                                                // 只在行视图被（重新）构建时克隆该行，
                                                // 缓存命中的帧不再克隆
                                                let item = cx
                                                    .global::<Models>()
                                                    .queue
                                                    .clone()
                                                    .read(cx)
                                                    .data
                                                    .read_recovered()
                                                    .get(idx)
                                                    .cloned();
                                                // 行键遍历与本次重建之间队列变短了
                                                // （实践中从未观测到）：构建一个占位行，
                                                // 而不是让这一帧 panic
                                                let item = match item {
                                                    Some(item) => item,
                                                    None => QueueItemData::new(
                                                        PathBuf::new(),
                                                        None,
                                                        None,
                                                    ),
                                                };
                                                QueueItem::new(
                                                    cx,
                                                    item,
                                                    idx,
                                                    drag_drop_manager,
                                                    scroll_handle,
                                                    item_selection,
                                                    item_ledger,
                                                )
                                            },
                                            cx,
                                        );
                                        if view.read(cx).idx != idx {
                                            view.update(cx, |q, cx| q.update_idx(idx, cx));
                                        }

                                        // 缓存行视图：行的动态输入（队列位置/元数据/
                                        // 选中/索引）全部有 notify 契约，拖拽期 gpui 的
                                        // 整窗 refresh 也会绕过缓存。不缓存时每一帧根
                                        // 重绘都会按可见行数重跑 render（2026-10-17
                                        // 实测 queue-row≈8×Queue 次数）。
                                        AnyView::from(view).cached(
                                            StyleRefinement::default()
                                                .w_full()
                                                .h(px(QUEUE_ITEM_HEIGHT))
                                                .flex_shrink_0(),
                                        )
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        })
                        .w_full()
                        .h_full()
                        .flex()
                        .flex_col()
                        .track_scroll(&scroll_handle),
                    )
                    .child(floating_scrollbar("queue_scrollbar", scroll_handle).right(px(4.0))),
            )
    }
}

impl Queue {
    /// 两个拖拽移动处理器的共用尾部：本次移动滚动了列表时才安排边缘自动滚动
    /// ——带守卫、最多一条待处理帧链（过去按事件调度会累积出多条链：滚动速度
    /// 被鼠标上报频率成倍放大）；并且只在有可见内容移动时才重绘
    /// （门控方式与歌单视图的处理器相同）。
    fn finish_drag_move(
        &mut self,
        before: (bool, Option<(usize, DropPosition)>),
        scrolled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if scrolled {
            request_edge_scroll(
                self.drag_drop_manager.clone(),
                self.scroll_handle.clone().into(),
                window,
                cx,
            );
        }

        let changed = {
            let manager = self.drag_drop_manager.read(cx);
            scrolled || (manager.state.is_dragging, manager.state.drop_target) != before
        };
        if changed {
            cx.notify();
        }
    }

    fn schedule_follow_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.follow_frame_scheduled {
            return;
        }

        self.follow_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.follow_frame_scheduled = false;
            let reduced_motion = reduced_motion_enabled(cx);
            this.advance_follow_animation(window, cx, reduced_motion);
        });
    }

    fn advance_follow_animation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) {
        if self.queue_hovered || self.drag_drop_manager.read(cx).state.is_dragging {
            self.scroll_follow.cancel();
            return;
        }

        if self.follow_current_pending {
            match self.compute_follow_target(cx) {
                FollowTarget::PendingLayout => {
                    self.schedule_follow_frame(window, cx);
                    return;
                }
                FollowTarget::NoScrollNeeded => {
                    self.follow_current_pending = false;
                    return;
                }
                FollowTarget::Target(target_scroll_top) => {
                    let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
                    self.scroll_follow
                        .follow_to(&scroll_handle, target_scroll_top, reduced_motion);
                    self.follow_current_pending = false;
                }
            }
        }

        let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
        let changed = self
            .scroll_follow
            .advance_or_snap(&scroll_handle, reduced_motion);

        if !changed {
            return;
        }

        if self.scroll_follow.is_active() {
            self.schedule_follow_frame(window, cx);
        }

        cx.notify();
    }

    fn compute_follow_target(&self, cx: &App) -> FollowTarget {
        let queue = cx.global::<Models>().queue.read(cx);
        let position = queue.position;
        let queue_len = queue.data.read_recovered().len();

        if queue_len == 0 || position >= queue_len {
            return FollowTarget::NoScrollNeeded;
        }

        let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
        let bounds = scroll_handle.bounds();
        let viewport_height = bounds.size.height;

        if viewport_height <= px(0.0) {
            return FollowTarget::PendingLayout;
        }

        let current_scroll_top = -scroll_handle.offset().y;
        let current_scroll_bottom = current_scroll_top + viewport_height;
        let max_scroll_top = scroll_handle.max_offset().y.max(px(0.0));

        let item_top = px(position as f32 * QUEUE_ITEM_HEIGHT);
        let item_bottom = item_top + px(QUEUE_ITEM_HEIGHT);

        let target_scroll_top = if item_top < current_scroll_top {
            item_top
        } else if item_bottom > current_scroll_bottom {
            (item_bottom - viewport_height).min(max_scroll_top)
        } else {
            return FollowTarget::NoScrollNeeded;
        };

        if (target_scroll_top - current_scroll_top).abs() <= px(0.1) {
            FollowTarget::NoScrollNeeded
        } else {
            FollowTarget::Target(target_scroll_top)
        }
    }
}

enum FollowTarget {
    PendingLayout,
    NoScrollNeeded,
    Target(Pixels),
}

/// 多选拖放的落点补偿：把 `calculate_move_target` 输出的"仅移除 source_index 后"
/// 坐标系插入位，换算成 `move_items` 契约（queue_manager.rs：移除全部选中行后的
/// 插入位）的落点。
///
/// `bound` 是投放间隙在原始坐标里的上界（Before 目标行 t → t，After → t+1）：
/// 间隙上方（原始坐标 `< bound`）的每个额外选中行被移除后，插入位都前移一格。
/// 边界随分支变向——Before/s<t 时 to=t-1 恰是间隙上一行（须计入 a==to），
/// After/s>t 时 to=t+1 是间隙下一行（必须排除 a==to），统一按 `to` 比较的
/// `<to` / `<=to` 各只对一半分支成立：`<to` 漏掉选中块尾行紧贴投放间隙的
/// 整块差一（M4 原始 bug），`<=to` 则在向上拖经块首时反向差一。故必须以
/// drop_target 的原始坐标为界。
fn corrected_multi_drop_target(to: usize, additional: &[usize], bound: usize) -> usize {
    to.saturating_sub(additional.iter().filter(|&&idx| idx < bound).count())
}

#[cfg(test)]
mod tests {
    #[test]
    fn selection_gate_only_notifies_on_flips() {
        let mut was_selected = false;
        let mut notifies = 0;
        for now in [false, false, true, true, false, true] {
            if now != was_selected {
                was_selected = now;
                notifies += 1;
            }
        }
        assert_eq!(notifies, 3, "重复值不得再次触发行重绘");
    }

    /// 扇出形状对照：无守卫时每次选中变更通知全部 keep-around 行；加值守卫
    /// 后只有选中态真正翻转的两行受影响。这里模拟的是守卫逻辑本身，不是
    /// 端到端 render 计数（后者由 `tally_render(QueueRow)` 在实机观察）。
    #[test]
    fn selection_gate_collapses_notify_fanout() {
        const ROWS: usize = 128;
        const CHANGES: usize = 1000;
        let mut selected = [false; ROWS];
        let mut was_selected = [false; ROWS];
        let mut ungated = 0usize;
        let mut gated = 0usize;

        for change in 0..CHANGES {
            let old = change % ROWS;
            let new = (change + 1) % ROWS;
            if old != new {
                selected[old] = false;
                selected[new] = true;
            }
            ungated += ROWS;
            for row in 0..ROWS {
                if selected[row] != was_selected[row] {
                    was_selected[row] = selected[row];
                    gated += 1;
                }
            }
        }
        eprintln!(
            "selection notify fanout/{CHANGES} changes x {ROWS} rows: ungated={ungated} gated={gated}"
        );
        assert_eq!(ungated, ROWS * CHANGES);
        assert!(gated <= 2 * CHANGES);
        assert!(gated < ungated / 10);
    }

    use super::{
        InternalDragFingerprints, QUEUE_LIST_ID, build_drag_fingerprints,
        corrected_multi_drop_target, resolve_internal_drag_core,
    };
    use crate::ui::components::drag_drop::{DropPosition, TrackDragData, calculate_move_target};
    use std::sync::Arc;

    /// 模拟 queue_manager::move_items 的"先移除全部选中、再从 to 起连续插回"语义
    /// （queue_manager.rs:1119 契约；含超出尾端的钳位）。
    fn apply_multi_move(len: usize, selected: &[usize], to: usize) -> Vec<usize> {
        let queue: Vec<usize> = (0..len).collect();
        let mut kept: Vec<usize> = Vec::with_capacity(queue.len());
        let mut moved: Vec<usize> = Vec::new();
        for (idx, item) in queue.iter().enumerate() {
            if selected.contains(&idx) {
                moved.push(*item);
            } else {
                kept.push(*item);
            }
        }
        let insert_at = to.min(kept.len());
        kept.splice(insert_at..insert_at, moved);
        kept
    }

    /// 端到端复现投放回调的完整链路（calculate_move_target → 落点补偿 →
    /// move_items 语义）：五元素队列把选中块 {1,2} 拖回原位时整队恒等。
    /// 旧 blanket `idx < to` 在 Before 尾贴间隙 / After 目标行选中两处差一，
    /// 反向拟改 `idx <= to` 则在向上拖的两处差一——四个场景合钉全部边界。
    fn assert_drag_block_returns_identity(
        source: usize,
        additional: &[usize],
        target: usize,
        position: DropPosition,
    ) {
        let to = calculate_move_target(source, target, position);
        let bound = match position {
            DropPosition::Before => target,
            DropPosition::After => target + 1,
        };
        let corrected = corrected_multi_drop_target(to, additional, bound);
        let mut selected = additional.to_vec();
        selected.push(source);
        selected.sort_unstable();
        assert_eq!(
            apply_multi_move(5, &selected, corrected),
            vec![0, 1, 2, 3, 4],
            "source={source} additional={additional:?} target={target} \
             {position:?} to={to} bound={bound} corrected={corrected}",
        );
    }

    /// M4 原例：Before、向下拖、选中块尾行（2）紧贴投放间隙。
    #[test]
    fn multi_drop_before_tail_flush_with_gap() {
        assert_drag_block_returns_identity(1, &[2], 3, DropPosition::Before);
    }

    /// After、向上拖、选中块首行（1）紧贴间隙下方——排除 `<=to` 拟改法。
    #[test]
    fn multi_drop_after_head_flush_with_gap() {
        assert_drag_block_returns_identity(2, &[1], 0, DropPosition::After);
    }

    /// After、向下拖、目标行本身 ∈ 选中集（t=2 ∈ S）——旧逻辑的隐藏镜像 bug。
    #[test]
    fn multi_drop_after_target_inside_selection() {
        assert_drag_block_returns_identity(1, &[2], 2, DropPosition::After);
    }

    /// Before、向上拖、目标行本身 ∈ 选中集（t=1 ∈ S）。
    #[test]
    fn multi_drop_before_target_inside_selection() {
        assert_drag_block_returns_identity(2, &[1], 1, DropPosition::Before);
    }

    // ─── A2：拖拽索引跨秒过期的内容重验（指纹账本） ─────────────────────

    /// 共用夹具：拖拽开始帧 source 在槽位 2（uid 30）、additional 在槽位 3
    /// （uid 40）的会话指纹。
    fn drag_entry_30_40() -> InternalDragFingerprints {
        InternalDragFingerprints {
            source_index: 2,
            additional: Arc::from([3usize]),
            uids: Arc::from([30u64, 40]),
        }
    }

    /// 队列未变：重解析结果与冻结坐标一致。
    #[test]
    fn drag_fingerprints_resolve_identity_when_queue_unchanged() {
        let entry = drag_entry_30_40();
        let queue = [10u64, 20, 30, 40, 50];
        let resolved =
            resolve_internal_drag_core(Some(&entry), Some(2), &[3], &queue).expect("应可解析");
        assert_eq!(resolved.source, 2);
        assert_eq!(resolved.additional, vec![3]);
    }

    /// 拖拽期间上游条目被 EOF 消费：被拖条目仍在线，重解析为前移后的
    /// 当前索引——用户抓的还是那两行，落点换算全部用当前坐标。
    #[test]
    fn drag_fingerprints_resolve_shifted_after_upstream_consumption() {
        let entry = drag_entry_30_40();
        // 拖拽开始时 uid 30/40 在槽位 2/3；投放时 uid 10 已被消费
        let queue = [20u64, 30, 40, 50];
        let resolved =
            resolve_internal_drag_core(Some(&entry), Some(2), &[3], &queue).expect("应可解析");
        assert_eq!(resolved.source, 1);
        assert_eq!(resolved.additional, vec![2]);
    }

    /// 任一被拖条目已离开队列（source 本人被消费、或同伴被并发移除）：
    /// 整组取消——宁可不动也不能移错行。
    #[test]
    fn drag_fingerprints_cancel_when_any_dragged_item_left_queue() {
        let entry = drag_entry_30_40();
        // source（uid 30）被消费
        assert!(resolve_internal_drag_core(Some(&entry), Some(2), &[3], &[20, 40, 50]).is_none());
        // 同伴（uid 40）被移除
        assert!(resolve_internal_drag_core(Some(&entry), Some(2), &[3], &[20, 30, 50]).is_none());
    }

    /// 账本与负载坐标对不上（上一会话残留 / 负载缺 source / 根本没记账）：
    /// 一律按不可验证取消，绝不带着存疑坐标执行 move。
    #[test]
    fn drag_fingerprints_cancel_on_ledger_payload_mismatch() {
        let entry = drag_entry_30_40();
        let queue = [10u64, 20, 30, 40, 50];
        // source 坐标不符
        assert!(resolve_internal_drag_core(Some(&entry), Some(1), &[3], &queue).is_none());
        // additional 坐标不符
        assert!(resolve_internal_drag_core(Some(&entry), Some(2), &[4], &queue).is_none());
        // 负载缺 source 坐标（handle_track_drop_multi 已在上游拦截，
        // 这里钉住纯核心自己的守卫）
        assert!(resolve_internal_drag_core(Some(&entry), None, &[3], &queue).is_none());
        // 没有账本
        assert!(resolve_internal_drag_core(None, Some(2), &[3], &queue).is_none());
    }

    /// 记账侧守卫：负载坐标与同伴指纹必须同帧对上号，对不上就记 None
    /// （投放时按不可验证取消）。单拖的指纹就是 source 自己。
    #[test]
    fn drag_fingerprints_build_rejects_snapshot_mismatch() {
        let multi = TrackDragData::new("a.mp3", "A")
            .with_reorder_info(QUEUE_LIST_ID, 2)
            .with_additional_indices(vec![3, 4]);
        // 同伴指纹数量与负载坐标不符
        assert!(build_drag_fingerprints(&multi, 30, Some(&[40])).is_none());
        // 多选负载却缺同伴指纹
        assert!(build_drag_fingerprints(&multi, 30, None).is_none());
        // 对上号：source 在首位、同伴按负载顺序随后
        let entry = build_drag_fingerprints(&multi, 30, Some(&[40, 50])).expect("应可记账");
        assert_eq!(entry.source_index, 2);
        assert_eq!(entry.additional.as_ref(), &[3, 4]);
        assert_eq!(entry.uids.as_ref(), &[30, 40, 50]);
        // 单拖（无 additional）：指纹就是 source 本人，不需要同伴快照
        let single = TrackDragData::new("a.mp3", "A").with_reorder_info(QUEUE_LIST_ID, 0);
        let entry = build_drag_fingerprints(&single, 7, None).expect("应可记账");
        assert_eq!(entry.uids.as_ref(), &[7]);
    }

    /// A2 坐标层端到端（良性场景）：上游条目被消费后把选中块投回它自己的
    /// 间隙，重解析坐标保持恒等——用户看到"放回原处"就真的原处。
    ///
    /// 拖拽开始时队列 [a,C,D,e]、拖 C(source=1)+D(additional=[2])；投放前
    /// a 被消费 → 活队列 [C,D,e]（值 0..2），指纹重解析 C→0、D→1；指示线
    /// 在 D 之后（活坐标 target=1, After）＝块自己的间隙。
    #[test]
    fn multi_drop_shifted_queue_returns_identity() {
        let to = calculate_move_target(0, 1, DropPosition::After);
        let corrected = corrected_multi_drop_target(to, &[1], 2);
        assert_eq!(apply_multi_move(3, &[0, 1], corrected), vec![0, 1, 2]);
    }

    /// A2 坐标层端到端（混算对照）：拖拽期间上游条目被消费后，重解析坐标 +
    /// 以当前 source 重算落点移动的是用户真正抓住的行；同一投放若用冻结
    /// 坐标计算，选中的是别的行——这正是本修复要消除的"移错行"。
    ///
    /// 拖拽开始时队列 [a,C,X,D,e]、拖 C(source=1)+D(additional=[3])；投放前
    /// a 被消费 → 活队列 [C,X,D,e]（值 0..3 即 C,X,D,e），指纹重解析 C→0、
    /// D→2；指示线在 D 之后（活坐标 target=2, After）。
    #[test]
    fn multi_drop_shifted_queue_uses_fingerprint_coords() {
        let resolved_source = 0;
        let resolved_additional = [2usize];
        let to = calculate_move_target(resolved_source, 2, DropPosition::After);
        let bound = 3; // After → target + 1
        let corrected = corrected_multi_drop_target(to, &resolved_additional, bound);
        let mut selected = resolved_additional.to_vec();
        selected.push(resolved_source);
        selected.sort_unstable();
        // 选中块 [C,D] 落到 X 与 e 之间：块尾贴着投放间隙
        assert_eq!(apply_multi_move(4, &selected, corrected), vec![1, 0, 2, 3]);

        // 对照（旧混算，非现行为）：冻结坐标 source=1/additional=[3] 在活
        // 队列上选中 X 与 e，把它们挪去队尾——移的是用户根本没拖的行。
        let stale_to = calculate_move_target(1, 2, DropPosition::After);
        let stale_corrected = corrected_multi_drop_target(stale_to, &[3], 3);
        assert_eq!(
            apply_multi_move(4, &[1, 3], stale_corrected),
            vec![0, 2, 1, 3]
        );
    }
}
