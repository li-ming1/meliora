mod cache;
pub mod file_context_menu;
pub mod file_row;
mod loader;
mod model;
mod tree;

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use gpui::*;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use cntp_i18n::tr;

use crate::{
    playback::interface::PlaybackInterface,
    settings::SettingsGlobal,
    ui::{
        app::Pool,
        availability::{BATCH_ACTION_TICKET, available_paths},
        components::{
            icons::{MINIMIZE, REFRESH},
            nav_button::nav_button,
            tooltip::build_tooltip,
        },
        library::view_header::view_header,
        util::{create_or_retrieve_view, prune_views},
    },
};

use file_row::{FileRowItem, ROW_HEIGHT};
use loader::{collect_audio_recursive, load_dir_entries, queue_items_from_entries};
pub use model::{FlatRow, RawEntry, TrackRef};
use tree::{ChildState, FileNode, FileTree, collect_expanded_paths};

/// 每个选中行的批量右键菜单都要消费的多元选载荷。用 Rc 共享，所以每个选中行的 render
/// 只 clone 一个引用计数，而不是重建（或深拷贝）整批数据。
type BatchItems = (Rc<Vec<(Arc<Path>, Option<TrackRef>)>>, Rc<Vec<i64>>);
/// 同一选区的持有所有权的 `Vec` 形态，即 [`FilesView::selected_batch_items`]
/// 为菜单快照返回的类型。
type SelectedBatch = (Vec<(Arc<Path>, Option<TrackRef>)>, Vec<i64>);

/// 设置里当前配置的扫描根路径。
fn configured_root_paths(cx: &App) -> Vec<PathBuf> {
    cx.global::<SettingsGlobal>()
        .model
        .read(cx)
        .scanning
        .paths
        .iter()
        .map(|p| p.as_std_path().to_path_buf())
        .collect()
}

/// 滚动 uniform list，使其顶部落在列表起点下方 `top` 像素处
/// （滚动句柄把这个编码成负的 y 偏移）。
fn set_scroll_top(handle: &UniformListScrollHandle, top: f32) {
    handle.0.borrow().base_handle.set_offset(Point {
        x: px(0.0),
        y: px(-top),
    });
}

// 这一整套刻意别扭：它在内存占用与体验之间做权衡。可以简化，但那样离开这个标签页之后
// 就要占用更多内存。
pub struct FilesView {
    tree: FileTree,
    flat: Arc<Vec<FlatRow>>,
    /// 在途目录加载标记：仅用于回调幂等与 refresh_all 取消，不携带数据。
    pending: FxHashSet<PathBuf>,
    selected: FxHashSet<PathBuf>,
    anchor: Option<PathBuf>,
    scroll_handle: UniformListScrollHandle,
    row_views: Entity<FxHashMap<usize, Entity<FileRowItem>>>,
    render_counter: Entity<usize>,
    loaded_dirs: VecDeque<PathBuf>,
    dir_child_counts: FxHashMap<PathBuf, usize>,
    cached_node_count: usize,
    restore_expanded: FxHashSet<PathBuf>,
    pending_scroll: Option<f32>,
    /// 每次 `FilesView::render` 只重算一次（不是每个选中行一次），
    /// 选中不足两行时为 `None`。
    selected_batch: Option<BatchItems>,
}

impl FilesView {
    pub fn new(
        cx: &mut App,
        restore_expanded: Vec<PathBuf>,
        initial_scroll: Option<f32>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let root_paths = configured_root_paths(cx);

            let tree = FileTree::new(root_paths);
            let flat = tree.flatten();
            let restore_set: FxHashSet<PathBuf> = restore_expanded.into_iter().collect();

            let scroll_handle = UniformListScrollHandle::new();

            let pending_scroll = if restore_set.is_empty() {
                if let Some(offset) = initial_scroll {
                    set_scroll_top(&scroll_handle, offset);
                }
                None
            } else {
                initial_scroll
            };

            FilesView {
                tree,
                flat,
                pending: FxHashSet::default(),
                selected: FxHashSet::default(),
                anchor: None,
                scroll_handle,
                row_views: cx.new(|_| FxHashMap::default()),
                render_counter: cx.new(|_| 0_usize),
                loaded_dirs: VecDeque::new(),
                dir_child_counts: FxHashMap::default(),
                cached_node_count: 0,
                restore_expanded: restore_set,
                pending_scroll,
                selected_batch: None,
            }
        })
    }

    pub fn toggle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(node) = self.tree.find_node_mut(&path) else {
            return;
        };

        match &node.children {
            ChildState::Unloaded => {
                self.start_loading(path, cx);
            }
            ChildState::Loading => {}
            ChildState::Loaded(_) => {
                node.expanded = !node.expanded;
                if node.expanded {
                    self.touch_lru(path);
                }
                self.rebuild_flat(cx);
            }
        }
    }

    pub fn select(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.len() == 1 && self.selected.contains(&path) {
            return;
        }
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path);
        cx.notify();
    }

    pub fn toggle_selection(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.remove(&path) {
            if self.anchor.as_ref() == Some(&path) {
                self.anchor = self.selected.iter().next().cloned();
            }
        } else {
            self.selected.insert(path.clone());
            self.anchor = Some(path);
        }
        cx.notify();
    }

    pub fn select_range(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(target_idx) = self
            .flat
            .iter()
            .position(|r| r.path.as_ref() == path.as_path())
        else {
            return;
        };
        let anchor_idx = self
            .anchor
            .as_ref()
            .and_then(|a| {
                self.flat
                    .iter()
                    .position(|r| r.path.as_ref() == a.as_path())
            })
            .unwrap_or(target_idx);
        self.anchor = Some(self.flat[anchor_idx].path.to_path_buf());

        let (start, end) = (anchor_idx.min(target_idx), anchor_idx.max(target_idx));
        self.selected.clear();
        self.selected
            .extend(self.flat[start..=end].iter().map(|r| r.path.to_path_buf()));
        cx.notify();
    }

    pub fn selection_contains(&self, path: &Path) -> bool {
        self.selected.contains(path)
    }

    pub fn is_multi(&self) -> bool {
        self.selected.len() > 1
    }

    /// 当前多元选的 `(含 track refs 的音频文件, 曲目 ids)` 快照，
    /// 供批量加入歌单菜单使用。
    pub fn selected_batch_items(&self) -> SelectedBatch {
        let mut audio_items = Vec::with_capacity(self.selected.len());
        let mut track_ids = Vec::with_capacity(self.selected.len());

        for row in self
            .flat
            .iter()
            .filter(|r| self.selected.contains(r.path.as_ref()))
        {
            if row.is_audio {
                audio_items.push((row.path.clone(), row.track.clone()));
            }
            if let Some(track) = &row.track {
                track_ids.push(track.id);
            }
        }

        (audio_items, track_ids)
    }

    pub fn play_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent() else {
            return;
        };

        let Some(parent_node) = self.tree.find_node(parent) else {
            return;
        };

        let ChildState::Loaded(entries) = &parent_node.children else {
            return;
        };

        let audio_entries: SmallVec<[(PathBuf, Option<TrackRef>); 16]> = entries
            .iter()
            .filter(|entry| entry.entry.is_audio)
            .map(|entry| (entry.entry.path.to_path_buf(), entry.entry.track.clone()))
            .collect();

        if audio_entries.is_empty() {
            return;
        }

        // 逐项可用性 stat 改在阻塞池上跑（见 available_paths），而不是在 UI 线程上
        // 每个同级项 stat 一次；队列交换随后照旧发生，只是晚一个任务跳转。
        let paths: Vec<PathBuf> = audio_entries
            .iter()
            .map(|(entry_path, _)| entry_path.clone())
            .collect();
        let ticket = BATCH_ACTION_TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        cx.spawn(async move |_this, cx| {
            let available = crate::RUNTIME
                .spawn_blocking(move || available_paths(paths))
                .await
                .unwrap_or_default();
            // 更新的点击已取代这一次（后点击优先，见 BATCH_ACTION_TICKET）
            if ticket != BATCH_ACTION_TICKET.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }

            cx.update(|cx| {
                let audio_entries: Vec<(PathBuf, Option<TrackRef>)> = audio_entries
                    .into_iter()
                    .filter(|(entry_path, _)| available.contains(entry_path.as_path()))
                    .collect();

                if audio_entries.is_empty() {
                    return;
                }

                let items = queue_items_from_entries(&audio_entries);

                let playback = cx.global::<PlaybackInterface>();
                if let Some(idx) = audio_entries.iter().position(|(p, _)| *p == path) {
                    playback.replace_queue_with_index(items, idx);
                } else {
                    playback.replace_queue(items);
                }
            });
        })
        .detach();
    }

    pub fn play_folder_recursive(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        Self::collect_folder_then(path, true, cx);
    }

    pub fn queue_folder_recursive(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        Self::collect_folder_then(path, false, cx);
    }

    fn collect_folder_then(path: PathBuf, replace: bool, cx: &mut Context<Self>) {
        let pool = cx.global::<Pool>().0.clone();
        cx.spawn(async move |_this, cx| {
            let collected = crate::RUNTIME
                .spawn(collect_audio_recursive(path, pool))
                .await
                .unwrap_or_default();
            if collected.is_empty() {
                return;
            }
            cx.update(|cx| {
                let items = queue_items_from_entries(&collected);
                let playback = cx.global::<PlaybackInterface>();
                if replace {
                    playback.replace_queue(items);
                } else {
                    playback.queue_list(items);
                }
            });
        })
        .detach();
    }

    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.tree.collapse_all();
        self.evict_lru();
        self.rebuild_flat(cx);
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let root_paths = configured_root_paths(cx);

        self.tree = FileTree::new(root_paths);
        self.pending.clear();
        self.loaded_dirs.clear();
        self.dir_child_counts.clear();
        self.cached_node_count = 0;
        self.restore_expanded.clear();
        self.pending_scroll = None;
        self.rebuild_flat(cx);
    }

    pub fn refresh_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(node) = self.tree.find_node(&path)
            && matches!(node.children, ChildState::Loading)
        {
            return;
        }
        self.unload_subtree(&path);
        self.start_loading(path, cx);
    }

    pub fn get_scroll_offset(&self) -> f32 {
        let offset = self.scroll_handle.0.borrow().base_handle.offset();
        (-offset.y).into()
    }

    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for root in &self.tree.roots {
            collect_expanded_paths(root, &mut out);
        }
        out
    }

    fn start_loading(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(node) = self.tree.find_node_mut(&path) else {
            return;
        };
        node.children = ChildState::Loading;
        node.expanded = true;

        let pool = cx.global::<Pool>().0.clone();

        let handle = crate::RUNTIME.spawn({
            let path = path.clone();
            async move { load_dir_entries(path, pool).await }
        });

        self.pending.insert(path.clone());
        self.rebuild_flat(cx);

        cx.spawn(async move |this, cx| {
            let entries = handle.await.unwrap_or_default();
            this.update(cx, |view: &mut FilesView, cx| {
                if view.pending.contains(&path) {
                    view.install_entries(path, entries, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn install_entries(&mut self, path: PathBuf, entries: Vec<RawEntry>, cx: &mut Context<Self>) {
        self.pending.remove(&path);

        let children: Vec<FileNode> = entries.into_iter().map(FileNode::from_raw).collect();
        let count = children.len();

        let Some(node) = self.tree.find_node_mut(&path) else {
            return;
        };
        node.children = ChildState::Loaded(children);

        self.touch_lru(path.clone());
        self.dir_child_counts.insert(path.clone(), count);
        self.cached_node_count += count;

        self.continue_restore_cascade(&path, cx);
        self.finish_restore_if_done();

        if self.restore_expanded.is_empty() {
            self.evict_lru();
            if let Some(offset) = self.pending_scroll.take() {
                set_scroll_top(&self.scroll_handle, offset);
            }
        }

        self.rebuild_flat(cx);
    }

    fn finish_restore_if_done(&mut self) {
        if !self.restore_expanded.is_empty() && self.pending.is_empty() {
            self.restore_expanded.clear();
            if let Some(offset) = self.pending_scroll.take() {
                set_scroll_top(&self.scroll_handle, offset);
            }
        }
    }

    fn continue_restore_cascade(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.restore_expanded.remove(path) {
            return;
        }

        let Some(node) = self.tree.find_node_mut(path) else {
            return;
        };
        node.expanded = true;

        let ChildState::Loaded(children) = &node.children else {
            return;
        };
        let children_to_load: SmallVec<[PathBuf; 8]> = children
            .iter()
            .filter(|c| {
                c.entry.is_dir
                    && self.restore_expanded.contains(c.entry.path.as_ref())
                    && matches!(c.children, ChildState::Unloaded)
            })
            .map(|c| c.entry.path.to_path_buf())
            .collect();

        for child_path in children_to_load {
            self.start_loading(child_path, cx);
        }
    }

    fn rebuild_flat(&mut self, cx: &mut Context<Self>) {
        self.flat = self.tree.flatten();

        if !self.selected.is_empty() {
            let visible: FxHashSet<&Path> = self.flat.iter().map(|r| r.path.as_ref()).collect();
            self.selected.retain(|p| visible.contains(p.as_path()));
            if let Some(anchor) = &self.anchor
                && !self.selected.contains(anchor)
            {
                self.anchor = self.selected.iter().next().cloned();
            }
        }

        self.clear_view_cache(cx);
        cx.notify();
    }

    fn clear_view_cache(&mut self, cx: &mut Context<Self>) {
        self.row_views.update(cx, |m, _| m.clear());
        self.render_counter.update(cx, |c, _| *c = 0);
    }

    fn process_restore(&mut self, cx: &mut Context<Self>) {
        if self.restore_expanded.is_empty() {
            return;
        }
        let roots_to_load: SmallVec<[PathBuf; 8]> = self
            .tree
            .roots
            .iter()
            .filter(|r| {
                self.restore_expanded.contains(r.entry.path.as_ref())
                    && matches!(r.children, ChildState::Unloaded)
            })
            .map(|r| r.entry.path.to_path_buf())
            .collect();
        for path in roots_to_load {
            self.start_loading(path, cx);
        }
        self.finish_restore_if_done();
    }
}

impl Render for FilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.process_restore(cx);

        // 多元选批量每帧只构造一次；之后每个选中行的右键菜单都读这份共享快照，
        // 而不是各自行走一遍整个扁平列表（那过去是每帧 O(行数 x 选区)）。
        self.selected_batch = if self.selected.len() > 1 {
            let (audio_items, track_ids) = self.selected_batch_items();
            Some((Rc::new(audio_items), Rc::new(track_ids)))
        } else {
            None
        };

        let flat = self.flat.clone();
        let row_views = self.row_views.clone();
        let render_counter = self.render_counter.clone();
        let scroll_handle = self.scroll_handle.clone();
        let self_entity = cx.entity();

        let collapse_entity = cx.entity();
        let refresh_entity = cx.entity();

        let header = view_header(tr!("FILES").to_string()).right(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    nav_button("files-collapse-all", MINIMIZE)
                        .tooltip(build_tooltip(tr!("FILES_COLLAPSE_ALL", "Collapse all")))
                        .on_click(move |_, _, cx| {
                            collapse_entity.update(cx, |view, cx| view.collapse_all(cx));
                        }),
                )
                .child(
                    nav_button("files-refresh", REFRESH)
                        .tooltip(build_tooltip(tr!("FILES_REFRESH", "Refresh")))
                        .on_click(move |_, _, cx| {
                            refresh_entity.update(cx, |view, cx| view.refresh_all(cx));
                        }),
                ),
        );

        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .child(header)
            .child(
                uniform_list(
                    "files-view",
                    flat.len(),
                    move |range: std::ops::Range<usize>, _window, cx| {
                        // 每次窗口重建只做一次 prune（与 `ui/components/table.rs` 的
                        // `render` 同一修法）：逐行
                        // 调用是 O(可见行数×缓存键数) 的键迭代，且每次调用各自分配
                        // `to_remove` Vec。`0..1` 是 uniform_list 的模板测量渲染，
                        // 不是可见窗口；按它 prune 只会把 center 钉在 0。
                        if range.start != 0 || range.end != 1 {
                            let center = range.start + (range.end - range.start) / 2;
                            prune_views(&row_views, &render_counter, center, cx);
                        }

                        range
                            .map(|idx| {
                                let row = flat[idx].clone();
                                let fv = self_entity.clone();

                                div()
                                    .h(px(ROW_HEIGHT))
                                    .w_full()
                                    .child(create_or_retrieve_view(
                                        &row_views,
                                        idx,
                                        move |cx| FileRowItem::new(cx, row, fv),
                                        cx,
                                    ))
                                    .into_any_element()
                            })
                            .collect()
                    },
                )
                .h_full()
                .w_full()
                .track_scroll(&scroll_handle),
            )
    }
}
