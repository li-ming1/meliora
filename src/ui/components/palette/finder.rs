use std::{cell::Cell, marker::PhantomData, rc::Rc, sync::Arc, time::Duration};

use cntp_i18n::{I18nString, trn};
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, ElementId, Entity, EventEmitter, FontWeight,
    InteractiveElement, IntoElement, ListAlignment, ListState, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, img, list, prelude::FluentBuilder,
    px,
};
use nucleo::{
    Config, Matcher, Nucleo, Utf32String,
    pattern::{CaseMatching, Normalization},
};
use rustc_hash::FxHashMap;
use tokio::sync::mpsc::channel;
use tracing::{debug, trace};

use crate::ui::{
    components::{context::ContextMenuBuilder, context::context, input::EnrichedInputAction},
    theme::Theme,
};

const MAX_VISIBLE_PER_CATEGORY: usize = 5;
/// 每次查询刷新（`get_matches`）收集到的匹配命中数上限。
const MAX_MATCHES: usize = 100;

/// 条目列表的指针相等判定：同样的 `Arc` 同样的顺序就是同样的条目，从而避免每 25ms 一次
/// matcher 轮询上做深 `PartialEq` 遍历（逐项 String 比较）。
fn same_items<T>(a: &[Arc<T>], b: &[Arc<T>]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| Arc::ptr_eq(a, b))
}

pub trait PaletteItem {
    fn left_content(&self, cx: &mut App) -> Option<FinderItemLeft>;
    fn middle_content(&self, cx: &mut App) -> SharedString;
    fn right_content(&self, cx: &mut App) -> Option<SharedString>;
    fn is_enabled(&self, _cx: &App) -> bool {
        true
    }
    /// 本条目是否属于"每次查询变化都整体替换"的集合（例如在线搜索结果）。易变条目不进 nucleo
    /// 索引——注入它们会让整个索引在每次刷新时重建——而是在收集命中时
    /// （`Finder::get_matches`）直接与当前查询比对。
    fn is_volatile(&self) -> bool {
        false
    }
    fn category(&self) -> Option<I18nString> {
        None
    }
    fn on_middle_click(&self, _cx: &mut App) {}
    /// 右键菜单构建器：这一行每次渲染都会被调用，无论菜单是否真的打开；返回的构建器只在菜单
    /// 真正打开时运行。两者都只能读缓存与快照——绝不发起阻塞查询或系统调用。
    fn context_menu(&self, _window: &mut Window, _cx: &mut App) -> Option<ContextMenuBuilder> {
        None
    }
    /// 提供一个渲染在右键菜单旁边的视图，用于曲目右键菜单里的"加入歌单"项。只在右键菜单打开时
    /// 调用（`FinderItem::render` 的 `menu_on_open` 构建器里），所以从未用过右键菜单的行
    /// 永远不会创建这些 entity。
    fn context_menu_overlay(&self, _window: &mut Window, _cx: &mut App) -> Option<AnyView> {
        None
    }
}

#[derive(Clone)]
pub struct ExtraItem {
    pub left: Option<FinderItemLeft>,
    pub middle: SharedString,
    pub right: Option<SharedString>,
    /// 主线程专用回调：调用点全部在 gpui 的 UI 线程闭包内，与
    /// `ExtraItemProvider` 及全仓 `Rc<dyn Fn(&mut App)>` 同一无 `Send` 约定，
    /// 允许捕获 `Rc` 等主线程状态（如 add_to_playlist 的共享选集）。
    pub on_accept: Arc<dyn Fn(&mut App)>,
}

pub type ExtraItemProvider = Arc<dyn Fn(&str) -> Vec<ExtraItem> + 'static>;

#[allow(type_alias_bounds)]
type ViewsModel<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
= Entity<FxHashMap<usize, Entity<FinderItem<T, MatcherFunc, OnAccept>>>>;

pub enum DisplayEntry<T> {
    Header(I18nString),
    Item(Arc<T>),
    ShowMore(I18nString, usize),
}

pub struct Finder<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    query: String,
    matcher: Nucleo<Arc<T>>,
    views_model: ViewsModel<T, MatcherFunc, OnAccept>,
    render_counter: Entity<usize>,
    last_match: Vec<Arc<T>>,
    /// 当前注入 matcher 的稳定条目，用于对账更新：纯追加的变化只推入新的尾部，而不重建整个
    /// matcher（见 `set_items`）。
    injected: Vec<Arc<T>>,
    /// 易变条目（`PaletteItem::is_volatile`，例如在线搜索结果）连同其搜索文本：不进 matcher
    /// 索引，而是在 `get_matches` 里与当前查询比对，因此刷新结果永远不会重启索引。
    dynamic_items: Vec<(Arc<T>, Utf32String)>,
    /// 针对 `dynamic_items` 的打分 matcher，之所以留着是因为 `Matcher::new` 会急切地分配一大块
    /// slab。
    dynamic_matcher: Matcher,
    // 用 Arc 包好，让每帧的 render 克隆只是一次引用计数自增；只在
    // `rebuild_display_list` / `recompute_extra_items` 里重建。
    display_list: Arc<Vec<DisplayEntry<T>>>,
    extra_providers: Vec<ExtraItemProvider>,
    // `ExtraItem` 回调非 `Send`（主线程专用），容器相应走 `Rc`。
    extra_items: Rc<Vec<ExtraItem>>,
    list_state: ListState,
    current_selection: Entity<usize>,
    expanded_categories: Vec<I18nString>,
    on_accept: Arc<OnAccept>,
    phantom: PhantomData<MatcherFunc>,
}

impl<T, MatcherFunc, OnAccept> Finder<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    pub fn new(
        cx: &mut App,
        items: Vec<Arc<T>>,
        get_item_display: Arc<MatcherFunc>,
        on_accept: Arc<OnAccept>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let config = Config::DEFAULT;

            // 建立通知通道
            let (sender, mut receiver) = channel(10);
            let notify = Arc::new(move || {
                // 通道满了也无所谓，反正本来就要更新
                _ = sender.try_send(());
            });

            let views_model = cx.new(|_| FxHashMap::default());
            let render_counter = cx.new(|_| 0);

            let dynamic_matcher = Matcher::new(config.clone());
            let matcher = Nucleo::new(config, notify, None, 1);
            let injector = matcher.injector();

            // 稳定条目注入 matcher 索引；易变的（例如在线搜索结果）改为与当前查询比对
            // ——见 `set_items`。
            let mut injected = Vec::with_capacity(items.len());
            let mut dynamic_items = Vec::new();
            for item in &items {
                let search_text = (get_item_display)(item, cx);
                if item.is_volatile() {
                    dynamic_items.push((item.clone(), search_text));
                } else {
                    trace!("Injecting item with search text: '{search_text}'");
                    injector.push(item.clone(), move |_v, dest| {
                        dest[0] = search_text;
                    });
                    injected.push(item.clone());
                }
            }

            let weak_self = cx.weak_entity();
            cx.spawn(async move |_, cx| {
                loop {
                    // palette entity 一没就退出；这个检查原本在 `needs_update` 里面，
                    // 于是已关闭的 palette 会把轮询循环永远转下去。
                    let Some(entity) = weak_self.upgrade() else {
                        return;
                    };

                    // 把所有更新通知取空
                    // 万一一次收到多条
                    let mut needs_update = false;
                    while receiver.try_recv().is_ok() {
                        needs_update = true;
                    }

                    if needs_update {
                        entity.update(cx, |this: &mut Self, cx| {
                            this.tick(10);

                            let matches: Vec<Arc<T>> = this.get_matches();
                            if !same_items(&matches, &this.last_match) {
                                this.rebuild_display_list(&matches, cx);
                                this.last_match = matches;
                                cx.notify();
                            }
                        });
                    }

                    cx.background_executor()
                        .timer(Duration::from_millis(25))
                        .await;
                }
            })
            .detach();

            // 查询更新时刷新
            cx.subscribe(&cx.entity(), |this, _, ev: &SharedString, cx| {
                this.set_query(ev.to_string(), cx);
            })
            .detach();

            // 处理键盘导航
            let on_accept_clone = on_accept.clone();
            cx.subscribe(
                &cx.entity(),
                move |this, _, ev: &EnrichedInputAction, cx| match ev {
                    EnrichedInputAction::Previous => {
                        let idx = *this.current_selection.read(cx);
                        this.move_selection(this.prev_enabled_index(idx, cx), cx);
                    }
                    EnrichedInputAction::Next => {
                        let idx = *this.current_selection.read(cx);
                        this.move_selection(this.next_enabled_index(idx, cx), cx);
                    }
                    EnrichedInputAction::Accept => {
                        let idx = *this.current_selection.read(cx);
                        if !this.index_is_enabled(idx, cx) {
                            return;
                        }

                        if idx < this.extra_items.len() {
                            (this.extra_items[idx].on_accept)(cx);
                        } else {
                            let display_idx = idx - this.extra_items.len();
                            match this.display_list.get(display_idx) {
                                Some(DisplayEntry::Item(item)) => {
                                    on_accept_clone(item, cx);
                                }
                                Some(DisplayEntry::ShowMore(cat, _)) => {
                                    this.expand_category(cat.clone(), cx);
                                }
                                _ => {}
                            }
                        }
                    }
                },
            )
            .detach();

            // 处理条目列表更新
            let get_item_display_for_updates = get_item_display.clone();
            cx.subscribe(&cx.entity(), move |this, _, items: &Vec<Arc<T>>, cx| {
                this.set_items(items, &get_item_display_for_updates, cx);

                cx.notify();
            })
            .detach();

            let current_selection = cx.new(|_| 0);

            Self {
                query: String::new(),
                matcher,
                views_model,
                last_match: Vec::new(),
                injected,
                dynamic_items,
                dynamic_matcher,
                display_list: Arc::new(Vec::new()),
                extra_providers: Vec::new(),
                extra_items: Rc::new(Vec::new()),
                render_counter,
                current_selection,
                expanded_categories: Vec::new(),
                list_state: Self::make_list_state(None),
                on_accept,
                phantom: PhantomData,
            }
        })
    }

    pub fn register_extra_provider(&mut self, provider: ExtraItemProvider, cx: &mut Context<Self>) {
        self.extra_providers.push(provider);
        self.recompute_extra_items();
        self.regenerate_list_state(cx);
    }

    fn build_display_list(
        matches: &[Arc<T>],
        expanded_categories: &[I18nString],
        expand_all: bool,
    ) -> Vec<DisplayEntry<T>> {
        let mut category_order: Vec<I18nString> = Vec::new();
        let mut category_counts: Vec<usize> = Vec::new();
        let mut has_uncategorized = false;

        // 分类只查一次并缓存：`category()` 内部是 `tr!`（全局 RwLock 读 + 哈希
        // 查表 + I18nString 构造），而下面共有三趟全量扫描——各自重取就是
        // n×(2+k) 次（n ≤ MAX_MATCHES=100、k≈4-5 ≈ 700 次/重建）。本函数在
        // 匹配集每次变化时都跑（连打字符即每秒数次）。
        let cats: Vec<Option<I18nString>> = matches.iter().map(|item| item.category()).collect();

        for cat in cats.iter() {
            if let Some(cat) = cat {
                if let Some(pos) = category_order.iter().position(|c| c == cat) {
                    category_counts[pos] += 1;
                } else {
                    category_order.push(cat.clone());
                    category_counts.push(1);
                }
            } else {
                has_uncategorized = true;
            }
        }

        if category_order.is_empty() {
            return matches
                .iter()
                .map(|item| DisplayEntry::Item(item.clone()))
                .collect();
        }

        let mut display_list = Vec::with_capacity(matches.len() + category_order.len());

        if has_uncategorized {
            for (item, cat) in matches.iter().zip(cats.iter()) {
                if cat.is_none() {
                    display_list.push(DisplayEntry::Item(item.clone()));
                }
            }
        }

        for (cat_idx, cat) in category_order.iter().enumerate() {
            display_list.push(DisplayEntry::Header(cat.clone()));
            let total = category_counts[cat_idx];
            let is_expanded = expand_all || expanded_categories.iter().any(|c| c == cat);
            let limit = if is_expanded || total <= MAX_VISIBLE_PER_CATEGORY {
                total
            } else {
                MAX_VISIBLE_PER_CATEGORY
            };

            let mut emitted = 0;
            for (item, item_cat) in matches.iter().zip(cats.iter()) {
                if item_cat.as_ref() == Some(cat) {
                    if emitted < limit {
                        display_list.push(DisplayEntry::Item(item.clone()));
                    }
                    emitted += 1;

                    if emitted >= total {
                        break;
                    }
                }
            }

            if !is_expanded && total > MAX_VISIBLE_PER_CATEGORY {
                display_list.push(DisplayEntry::ShowMore(cat.clone(), total));
            }
        }

        display_list
    }

    fn total_items(&self) -> usize {
        self.extra_items.len() + self.display_list.len()
    }

    fn index_is_enabled(&self, idx: usize, cx: &App) -> bool {
        if idx < self.extra_items.len() {
            return true;
        }
        match self.display_list.get(idx - self.extra_items.len()) {
            Some(DisplayEntry::Item(item)) => item.is_enabled(cx),
            Some(DisplayEntry::ShowMore(..)) => true,
            Some(DisplayEntry::Header(_)) | None => false,
        }
    }

    fn next_enabled_index(&self, current: usize, cx: &App) -> Option<usize> {
        ((current + 1)..self.total_items()).find(|idx| self.index_is_enabled(*idx, cx))
    }

    fn prev_enabled_index(&self, current: usize, cx: &App) -> Option<usize> {
        (0..current)
            .rev()
            .find(|idx| self.index_is_enabled(*idx, cx))
    }

    fn first_enabled_index(&self, cx: &App) -> Option<usize> {
        (0..self.total_items()).find(|idx| self.index_is_enabled(*idx, cx))
    }

    /// 有 `target` 时把选择移过去，然后让列表保持在（可能没变的）选中项上。
    fn move_selection(&mut self, target: Option<usize>, cx: &mut Context<Self>) {
        if let Some(idx) = target {
            self.current_selection.update(cx, |sel, cx| {
                *sel = idx;
                cx.notify();
            });
        }

        let idx = *self.current_selection.read(cx);
        self.list_state.scroll_to_reveal_item(idx);
    }

    fn recompute_extra_items(&mut self) {
        self.extra_items = Rc::new(
            self.extra_providers
                .iter()
                .flat_map(|provider| (provider)(&self.query))
                .collect(),
        );
    }

    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        debug!("Setting query: '{}' (previous: '{}')", query, self.query);
        self.expanded_categories.clear();

        self.matcher
            .pattern
            .reparse(0, &query, CaseMatching::Smart, Normalization::Smart, false);

        self.query = query;

        // 依据查询重算动态附加条目
        self.recompute_extra_items();

        // 先立刻备好一批匹配
        self.tick(20);

        let matches = self.get_matches();

        // 有附加项、或条目变了，就重新生成列表状态
        if !same_items(&matches, &self.last_match) || !self.extra_items.is_empty() {
            self.rebuild_display_list(&matches, cx);
            self.last_match = matches;
        }

        self.current_selection.update(cx, |sel, cx| {
            *sel = self.first_enabled_index(cx).unwrap_or(0);
            cx.notify();
        });
        self.list_state
            .scroll_to_reveal_item(*self.current_selection.read(cx));

        cx.notify();
    }

    fn tick(&mut self, iterations: u32) {
        self.matcher.tick(iterations as u64);
    }

    /// 用新的条目列表对账 matcher。稳定条目注入 nucleo，索引跨更新保持纯追加：只有稳定集
    /// 内部发生变化（本地索引重载、库重扫描）才触发完整 `restart` + 重新注入。易变条目
    /// （例如在线搜索结果，每次查询变化都整体替换）永不进索引——它们连同搜索文本存在这里，
    /// 在 `get_matches` 里与当前查询打分，于是刷新它们的成本是 O(易变条目数)，
    /// 而不是一次索引重建。
    fn set_items(&mut self, items: &[Arc<T>], get_item_display: &MatcherFunc, cx: &mut App) {
        self.dynamic_items.clear();
        self.dynamic_items.extend(
            items
                .iter()
                .filter(|item| item.is_volatile())
                .map(|item| (item.clone(), (get_item_display)(item, cx))),
        );

        // 新稳定条目能复现当前已注入前缀的多少（同样的 `Arc`、同样的相对顺序，跳过易变条目）。
        let mut common = 0usize;
        let mut append_only = true;
        for item in items {
            if item.is_volatile() {
                continue;
            }
            match self.injected.get(common) {
                Some(injected_item) if Arc::ptr_eq(injected_item, item) => common += 1,
                _ => {
                    append_only = false;
                    break;
                }
            }
        }
        let append_only = append_only && common == self.injected.len();

        if !append_only {
            self.matcher.restart(false);
        }

        let injector = self.matcher.injector();
        // `restart` 会清空 matcher 的条目存储，所以非纯追加的变化必须重新注入每一个稳定条目
        // ——只推变化的尾部会把未变的前缀（例如整个本地索引）从后续本次会话的匹配中丢掉。
        for (stable_idx, item) in items.iter().filter(|item| !item.is_volatile()).enumerate() {
            if append_only && stable_idx < common {
                continue;
            }
            let item = item.clone();
            let search_text = (get_item_display)(&item, cx);
            injector.push(item, move |_v, dest| {
                dest[0] = search_text;
            });
        }

        self.injected.clear();
        self.injected
            .extend(items.iter().filter(|item| !item.is_volatile()).cloned());
    }

    fn get_matches(&mut self) -> Vec<Arc<T>> {
        let snapshot = self.matcher.snapshot();
        let count = snapshot.matched_item_count();
        let limit = MAX_MATCHES.min(count as usize) as u32;

        let mut matches: Vec<Arc<T>> = snapshot
            .matched_items(..limit)
            .map(|item| item.data.clone())
            .collect();

        if self.dynamic_items.is_empty() {
            return matches;
        }

        // 易变条目从未进入 matcher 索引，所以在这里与当前查询打分。它们的数量是"每个音源一页
        // 结果"，因此即使索引本身还在后台重新匹配，这里也保持微秒级。
        // `set_query` 让索引的 pattern 与 `self.query` 保持同步（reparse 走的是与
        // `Pattern::parse` 相同的原子管线），所以直接复用它，而不是每个轮询 tick 重解析一次查询。
        let pattern = self.matcher.pattern.column_pattern(0);
        let mut dynamic: Vec<(u32, Arc<T>)> = Vec::with_capacity(self.dynamic_items.len());
        for (item, text) in &self.dynamic_items {
            if let Some(score) = pattern.score(text.slice(..), &mut self.dynamic_matcher) {
                dynamic.push((score, item.clone()));
            }
        }
        // 稳定排序：分数相同时保持音源自带的结果顺序。
        dynamic.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        matches.extend(dynamic.into_iter().map(|(_, item)| item));

        matches
    }

    /// `Palette::reset`（palette.rs，本域外）仍调用此入口；finder 内部调用点
    /// 一律把已有匹配集直接传给 `rebuild_display_list`，避免同一批匹配被
    /// 二次 snapshot+collect。
    pub fn regenerate_list_state(&mut self, cx: &mut Context<Self>) {
        let matches = self.get_matches();
        self.rebuild_display_list(&matches, cx);
    }

    /// 由调用方提供匹配集重建展示列表与滚动状态；`matches` 通常就是
    /// `last_match`，借切片免二次收集。
    fn rebuild_display_list(&mut self, matches: &[Arc<T>], cx: &mut Context<Self>) {
        let display_list =
            Self::build_display_list(matches, &self.expanded_categories, self.query.is_empty());
        self.apply_display_list(display_list, cx);
    }

    /// 安装构建好的展示列表并重置滚动/视图状态。计算与安装分开，调用方
    /// （如 `expand_category`）才能用 `&self` 只读借用先算出列表、再整体
    /// 突变 `self`，免去克隆整份匹配集。
    fn apply_display_list(&mut self, display_list: Vec<DisplayEntry<T>>, cx: &mut Context<Self>) {
        let curr_scroll = self.list_state.logical_scroll_top();

        self.display_list = Arc::new(display_list);

        self.views_model = cx.new(|_| FxHashMap::default());
        self.render_counter = cx.new(|_| 0);

        let total = self.total_items();
        self.list_state = Self::make_list_state(Some(total));
        self.list_state.scroll_to(curr_scroll);
    }

    fn expand_category(&mut self, category: I18nString, cx: &mut Context<Self>) {
        if !self.expanded_categories.iter().any(|c| c == &category) {
            self.expanded_categories.push(category);
        }
        // 展开分类不改匹配集，直接复用 `last_match`，不重新 snapshot。
        let display_list = Self::build_display_list(
            &self.last_match,
            &self.expanded_categories,
            self.query.is_empty(),
        );
        self.apply_display_list(display_list, cx);
        cx.notify();
    }

    fn make_list_state(total_count: Option<usize>) -> ListState {
        match total_count {
            Some(count) => ListState::new(count, ListAlignment::Top, px(300.0)),
            None => ListState::new(0, ListAlignment::Top, px(64.0)),
        }
    }
}

impl<T, MatcherFunc, OnAccept> EventEmitter<SharedString> for Finder<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}

impl<T, MatcherFunc, OnAccept> EventEmitter<Vec<Arc<T>>> for Finder<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}

impl<T, MatcherFunc, OnAccept> EventEmitter<EnrichedInputAction>
    for Finder<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}

impl<T, MatcherFunc, OnAccept> Render for Finder<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::ui::caching::meliora_cache;

        let display_list = self.display_list.clone();
        let extra_items = self.extra_items.clone();
        let views_model = self.views_model.clone();
        let render_counter = self.render_counter.clone();
        let current_selection = self.current_selection.clone();
        let weak_finder = cx.weak_entity();

        use crate::ui::util::prune_views;

        // 每次渲染只 prune 一次（首行触发；闭包随渲染每帧重建，Cell 天然
        // 逐帧复位），render_counter 仍逐行推进，band 演化与原逐行调用一致，
        // 删除集合与现状一致（同 table.rs 的 prune_gate）
        let prune_gate = Cell::new(false);

        div()
            .w_full()
            .h_full()
            .image_cache(meliora_cache("finder-cache", 50))
            .id("finder")
            .flex()
            .p(px(4.0))
            .child(
                list(self.list_state.clone(), move |idx, _, cx| {
                    if prune_gate.replace(true) {
                        render_counter.update(cx, |m, _| *m = idx);
                    } else {
                        prune_views(&views_model, &render_counter, idx, cx);
                    }

                    let extras_len = extra_items.len();
                    if idx < extras_len {
                        render_extra_item(
                            &extra_items[idx],
                            idx,
                            &views_model,
                            &current_selection,
                            &weak_finder,
                            cx,
                        )
                    } else {
                        let display_idx = idx - extras_len;
                        match display_list.get(display_idx) {
                            Some(DisplayEntry::Header(header)) => render_header(header, cx),
                            Some(DisplayEntry::Item(item)) => render_item(
                                item,
                                idx,
                                &views_model,
                                &current_selection,
                                &weak_finder,
                                cx,
                            ),
                            Some(DisplayEntry::ShowMore(_, count)) => render_show_more(
                                idx,
                                *count,
                                extras_len,
                                &current_selection,
                                &weak_finder,
                                cx,
                            ),
                            None => div().into_any_element(),
                        }
                    }
                })
                .flex()
                .flex_col()
                .gap(px(2.0))
                .w_full()
                .h_full(),
            )
    }
}

fn render_header(header: &I18nString, cx: &mut App) -> AnyElement {
    let theme = cx.global::<Theme>();
    div()
        .w_full()
        .px(px(10.0))
        .py(px(4.0))
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(theme.text_secondary)
        .child(SharedString::from(header.to_string()))
        .into_any_element()
}

fn render_extra_item<T, MatcherFunc, OnAccept>(
    extra: &ExtraItem,
    idx: usize,
    views_model: &ViewsModel<T, MatcherFunc, OnAccept>,
    current_selection: &Entity<usize>,
    weak_finder: &WeakEntity<Finder<T, MatcherFunc, OnAccept>>,
    cx: &mut App,
) -> AnyElement
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    use crate::ui::util::create_or_retrieve_view;

    let current_selection = current_selection.clone();
    let weak_finder = weak_finder.clone();
    let extra = extra.clone();

    div()
        .w_full()
        .child(create_or_retrieve_view(
            views_model,
            idx,
            move |cx| {
                FinderItem::new_extra(
                    cx,
                    ("finder-extra-item", idx),
                    idx,
                    &current_selection,
                    weak_finder.clone(),
                    extra.clone(),
                )
            },
            cx,
        ))
        .into_any_element()
}

fn render_item<T, MatcherFunc, OnAccept>(
    item: &Arc<T>,
    idx: usize,
    views_model: &ViewsModel<T, MatcherFunc, OnAccept>,
    current_selection: &Entity<usize>,
    weak_finder: &WeakEntity<Finder<T, MatcherFunc, OnAccept>>,
    cx: &mut App,
) -> AnyElement
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    use crate::ui::util::create_or_retrieve_view;

    let item: Arc<T> = item.clone();
    let current_selection = current_selection.clone();
    let weak_finder = weak_finder.clone();

    div()
        .w_full()
        .child(create_or_retrieve_view(
            views_model,
            idx,
            move |cx| {
                FinderItem::new(
                    cx,
                    ("finder-item", idx),
                    &item,
                    idx,
                    &current_selection,
                    weak_finder.clone(),
                    item.clone(),
                )
            },
            cx,
        ))
        .into_any_element()
}

fn render_show_more<T, MatcherFunc, OnAccept>(
    idx: usize,
    count: usize,
    extras_len: usize,
    current_selection: &Entity<usize>,
    weak_finder: &WeakEntity<Finder<T, MatcherFunc, OnAccept>>,
    cx: &mut App,
) -> AnyElement
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    let theme = cx.global::<Theme>();
    let remaining = count.saturating_sub(MAX_VISIBLE_PER_CATEGORY);
    let current_sel = *current_selection.read(cx);
    let is_selected = current_sel == idx;
    let weak = weak_finder.clone();

    div()
        .w_full()
        .px(px(10.0))
        .py(px(6.0))
        .text_xs()
        .text_color(theme.text_secondary)
        .cursor_pointer()
        .id(("finder-show-more", idx))
        .border_1()
        .hover(|this| {
            this.bg(theme.palette_item_hover)
                .border_color(theme.palette_item_border_hover)
        })
        .when(is_selected, |this| {
            this.bg(theme.palette_item_hover)
                .border_color(theme.palette_item_border_hover)
        })
        .rounded(px(theme.radius_md))
        .on_click(move |_, _, cx| {
            weak.update(cx, |finder, cx| {
                let display_idx = idx.saturating_sub(extras_len);
                if let Some(DisplayEntry::ShowMore(cat, _)) = finder.display_list.get(display_idx) {
                    finder.expand_category(cat.clone(), cx);
                }
            })
            .expect("finder was rendered without existing")
        })
        .child(trn!(
            "PALETTE_SHOW_MORE",
            "Show {{count}} more...",
            "Show {{count}} more...",
            count = remaining
        ))
        .into_any_element()
}

type OnAcceptOverride = Option<Arc<dyn Fn(&mut App)>>;

pub struct FinderItem<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    id: ElementId,
    left: Option<FinderItemLeft>,
    middle: SharedString,
    right: Option<SharedString>,
    idx: usize,
    current_selection: usize,
    is_enabled: bool,
    weak_parent: WeakEntity<Finder<T, MatcherFunc, OnAccept>>,
    item_data: Option<Arc<T>>,
    on_accept_override: OnAcceptOverride,
    /// 右键菜单的 overlay 视图（例如 AddToPlaylist）。第一次打开右键菜单时惰性构建并存在条目上，
    /// 好让模态框在菜单关闭后仍然存活。
    context_overlay: Option<AnyView>,
}

#[derive(Clone)]
pub enum FinderItemLeft {
    Text(SharedString),
    Icon(SharedString),
    Image(SharedString),
}

impl<T, MatcherFunc, OnAccept> FinderItem<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    /// 两个构造器共享的选择观察者：一行的渲染输出只取决于 "selected"，所以只有这个谓词翻转时
    /// 才需要重绘；方向键移动会广播给每一行，若不这样就有 N-2 行为毫无视觉变化的东西重渲染。
    fn observe_selection_flip(cx: &mut Context<Self>, current_selection: &Entity<usize>) {
        cx.observe(
            current_selection,
            |this: &mut Self, selection_model, cx: &mut Context<Self>| {
                let sel = *selection_model.read(cx);
                let was_selected = this.current_selection == this.idx;
                let is_selected = sel == this.idx;
                this.current_selection = sel;
                if was_selected != is_selected {
                    cx.notify();
                }
            },
        )
        .detach();
    }

    pub fn new(
        cx: &mut App,
        id: impl Into<ElementId>,
        item: &Arc<T>,
        idx: usize,
        current_selection: &Entity<usize>,
        weak_parent: WeakEntity<Finder<T, MatcherFunc, OnAccept>>,
        item_data: Arc<T>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            Self::observe_selection_flip(cx, current_selection);

            let left = item.left_content(cx);
            let middle = item.middle_content(cx);
            let right = item.right_content(cx);
            let is_enabled = item.is_enabled(cx);

            Self {
                id: id.into(),
                left,
                middle,
                right,
                idx,
                current_selection: *current_selection.read(cx),
                is_enabled,
                weak_parent,
                item_data: Some(item_data),
                on_accept_override: None,
                context_overlay: None,
            }
        })
    }

    pub fn new_extra(
        cx: &mut App,
        id: impl Into<ElementId>,
        idx: usize,
        current_selection: &Entity<usize>,
        weak_parent: WeakEntity<Finder<T, MatcherFunc, OnAccept>>,
        extra: ExtraItem,
    ) -> Entity<Self> {
        cx.new(|cx| {
            Self::observe_selection_flip(cx, current_selection);

            Self {
                id: id.into(),
                left: extra.left.clone(),
                middle: extra.middle.clone(),
                right: extra.right.clone(),
                idx,
                current_selection: *current_selection.read(cx),
                is_enabled: true,
                weak_parent,
                item_data: None,
                on_accept_override: Some(extra.on_accept.clone()),
                context_overlay: None,
            }
        })
    }
}

impl<T, MatcherFunc, OnAccept> Render for FinderItem<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        let weak_parent = self.weak_parent.clone();
        let item_data = self.item_data.clone();
        let on_accept_override = self.on_accept_override.clone();
        let is_enabled = self.is_enabled;

        let item = div()
            .px(px(10.0))
            .py(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .when(is_enabled, |this| this.cursor_pointer())
            .when(!is_enabled, |this| this.cursor_default().opacity(0.5))
            .id(self.id.clone())
            .border_1()
            .when(is_enabled, |this| {
                this.hover(|this| {
                    this.bg(theme.palette_item_hover)
                        .border_color(theme.palette_item_border_hover)
                })
                .active(|this| {
                    this.bg(theme.palette_item_active)
                        .border_color(theme.palette_item_border_active)
                })
            })
            .when(self.current_selection == self.idx && is_enabled, |this| {
                this.bg(theme.palette_item_hover)
                    .border_color(theme.palette_item_border_hover)
            })
            .rounded(px(theme.radius_md))
            .on_click(cx.listener({
                let item_data = item_data.clone();
                move |_, _, _, cx| {
                    if !is_enabled {
                        return;
                    }

                    if let Some(override_fn) = on_accept_override.as_deref() {
                        override_fn(cx);
                    } else if let Some(parent) = weak_parent.upgrade()
                        && let Some(item) = item_data.as_ref()
                    {
                        parent.update(cx, |finder, cx| {
                            (finder.on_accept)(item, cx);
                        });
                    }
                }
            }))
            .on_aux_click(move |ev, _, cx| {
                if ev.is_middle_click()
                    && let Some(item) = item_data.as_ref()
                {
                    item.on_middle_click(cx);
                }
            })
            .when_some(self.left.clone(), |div_outer, left| {
                div_outer.child(match left {
                    FinderItemLeft::Text(text) => div()
                        .child(text)
                        .text_ellipsis()
                        .text_sm()
                        .text_color(theme.text_secondary)
                        .mr(px(4.0)),
                    FinderItemLeft::Icon(icon_name) => {
                        use crate::ui::components::icons::icon;
                        div()
                            .child(icon(icon_name).w(px(16.0)).h(px(16.0)))
                            .mr(px(8.0))
                    }
                    FinderItemLeft::Image(image_path) => div()
                        .rounded(px(2.0))
                        .bg(theme.album_art_background)
                        .shadow_sm()
                        .w(px(16.0))
                        .h(px(16.0))
                        .flex_shrink_0()
                        .mr(px(8.0))
                        .child(img(image_path).w(px(16.0)).h(px(16.0)).rounded(px(2.0))),
                })
            })
            .child(
                div()
                    .flex_shrink(1.0)
                    .font_weight(FontWeight::BOLD)
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.middle.clone()),
            )
            .when_some(self.right.clone(), |div_outer, right| {
                div_outer.child(
                    div()
                        .ml_auto()
                        .pl(px(8.0))
                        .flex_shrink(1.0)
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_sm()
                        .text_color(theme.text_secondary)
                        .child(right),
                )
            });

        let context_menu = self
            .item_data
            .as_ref()
            .and_then(|v| v.context_menu(window, cx));
        let overlay = self.context_overlay.clone();

        let base = if let Some(menu_builder) = context_menu {
            let item_data = self.item_data.clone();
            let weak_item = cx.weak_entity();
            context((self.id.clone(), "context_menu"))
                .with(item)
                // 菜单树只在菜单打开时构建；overlay（例如 AddToPlaylist）也在这里、在打开时构建，
                // 于是它解析到与菜单里"加入歌单"项写入的同一个 keyed state。它存在条目上，
                // 好让模态框在菜单关闭后仍然存活（而没用过的行什么都不创建）。
                .menu_on_open(move |window, cx| {
                    let menu = menu_builder(window, cx);
                    let overlay = item_data
                        .as_ref()
                        .and_then(|data| data.context_menu_overlay(window, cx));
                    if let Some(this) = weak_item.upgrade() {
                        this.update(cx, |this, cx| {
                            if this.context_overlay != overlay {
                                this.context_overlay = overlay;
                                cx.notify();
                            }
                        });
                    }
                    menu
                })
                .into_any_element()
        } else {
            item.into_any_element()
        };

        if let Some(overlay) = overlay {
            div().child(base).child(overlay).into_any_element()
        } else {
            base
        }
    }
}
