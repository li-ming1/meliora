use std::{
    collections::VecDeque,
    mem::take,
    sync::atomic::{AtomicU64, Ordering},
};

use futures::FutureExt;
use gpui::{
    App, AppContext, Asset, AssetLogger, ElementId, Entity, ImageAssetLoader, ImageCache,
    ImageCacheItem, ImageCacheProvider, ImageSource, Resource, hash,
};
use rustc_hash::{FxBuildHasher, FxHashMap};
use tracing::{error, trace};

use crate::ui::components::managed_image::{image_bytes, queue_orphan_tile_drop};

/// 所有 `MelioraImageCache` 实例合起来的实活占用，由 `[mem]` 探针上报。
/// 字节在某个条目的加载任务第一次 resolve 时计入（见 `load`），在逐出/实体释放时
/// 扣除，于是探针曲线能精确归因出各视图缓存占的常驻内存，而不把它留在
/// 无法归因的平台期里。
static IMAGE_CACHE_ENTRIES: AtomicU64 = AtomicU64::new(0);
static IMAGE_CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

/// 跨所有 `MelioraImageCache` 实例的统计（活条目数、已解码 MiB）。
pub fn image_cache_stats() -> (u64, u64) {
    (
        IMAGE_CACHE_ENTRIES.load(Ordering::Relaxed),
        IMAGE_CACHE_BYTES.load(Ordering::Relaxed) / (1024 * 1024),
    )
}

/// 创建 `image_cache(...)` 调用点使用的有界逐视图图片缓存，最多容纳 `max_items`
/// 个条目。
pub fn meliora_cache(id: impl Into<ElementId>, max_items: usize) -> MelioraImageCacheProvider {
    MelioraImageCacheProvider {
        id: id.into(),
        max_items,
    }
}

/// 对每个元素 id 复用一枚 [`MelioraImageCache`] 的 gpui [`ImageCacheProvider`]
/// （缓存本体存在窗口的跨帧元素状态里）。
pub struct MelioraImageCacheProvider {
    id: ElementId,
    max_items: usize,
}

impl ImageCacheProvider for MelioraImageCacheProvider {
    fn provide(&mut self, window: &mut gpui::Window, cx: &mut App) -> gpui::AnyImageCache {
        window
            .with_global_id(self.id.clone(), |id, window| {
                window.with_element_state(id, |cache, _| {
                    let cache = cache.unwrap_or_else(|| MelioraImageCache::new(self.max_items, cx));

                    (cache.clone(), cache)
                })
            })
            .into()
    }
}

/// 单个视图的有界 LRU 图片缓存。被逐出的图片与整缓存释放都走孤儿瓦片回收漏斗，
/// 而不是就地丢弃（见 `queue_orphan_tile_drop`）。
pub struct MelioraImageCache {
    max_items: usize,
    usage_list: VecDeque<u64>,
    /// （加载任务、资源、计入 `[mem]` 探针的已解码像素字节——在加载任务 resolve
    /// 之前一直是 0）。
    cache: FxHashMap<u64, (ImageCacheItem, Resource, u64)>,
}

impl MelioraImageCache {
    pub fn new(max_items: usize, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            trace!("Creating MelioraImageCache");
            cx.on_release(|this: &mut Self, cx| {
                let entries = this.cache.len() as u64;
                for (idx, (mut item, resource, recorded)) in take(&mut this.cache) {
                    IMAGE_CACHE_BYTES.fetch_sub(recorded, Ordering::Relaxed);
                    if let Some(Ok(render_image)) = item.get() {
                        trace!("Dropping image {idx}");
                        queue_orphan_tile_drop(render_image);
                    }

                    ImageSource::Resource(resource).remove_asset(cx);
                }
                IMAGE_CACHE_ENTRIES.fetch_sub(entries, Ordering::Relaxed);
            })
            .detach();

            MelioraImageCache {
                max_items,
                usage_list: VecDeque::with_capacity(max_items),
                cache: FxHashMap::with_capacity_and_hasher(max_items, FxBuildHasher),
            }
        })
    }
}

impl ImageCache for MelioraImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Option<Result<std::sync::Arc<gpui::RenderImage>, gpui::ImageCacheError>> {
        let hash = hash(resource);

        if let Some(item) = self.cache.get_mut(&hash) {
            // 快路径：LRU 最热的条目已经在 usage_list 最前面，这覆盖绝大多数命中；
            // 只有冷命中才付那次 O(n) 扫描 + 重排。
            if self.usage_list.front() != Some(&hash) {
                let current_idx = self
                    .usage_list
                    .iter()
                    .position(|item| *item == hash)
                    .expect("cache has an item usage_list doesn't");

                self.usage_list.remove(current_idx);
                self.usage_list.push_front(hash);
            }

            // 共享加载任务第一次 resolve 时把该条目的已解码像素计入一次，
            // 于是 [mem] 探针不必每 tick 扫描就能报出各视图缓存的实活占用。
            let resolved = item.0.get();
            if item.2 == 0
                && let Some(Ok(image)) = &resolved
            {
                let bytes = image_bytes(image);
                item.2 = bytes;
                IMAGE_CACHE_BYTES.fetch_add(bytes, Ordering::Relaxed);
            }
            return resolved;
        }

        let load_future = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
        let task = cx.background_executor().spawn(load_future).shared();

        if self.usage_list.len() >= self.max_items {
            trace!("Image cache is full, evicting oldest item");

            let oldest = self.usage_list.pop_back().unwrap();
            let mut entry = self
                .cache
                .remove(&oldest)
                .expect("usage_list has an item cache doesn't");

            if let Some(Ok(render_image)) = entry.0.get() {
                trace!("requesting image to be dropped");
                // 逐出发生在 img 的 request_layout/paint 调用栈内：直接
                // drop_image 会在同帧释放图集页（sprite 已记录）并可能踩
                // etagere 断言。推进回收漏斗，由事件循环的 drain 帧间释放。
                queue_orphan_tile_drop(render_image);
            }

            IMAGE_CACHE_ENTRIES.fetch_sub(1, Ordering::Relaxed);
            IMAGE_CACHE_BYTES.fetch_sub(entry.2, Ordering::Relaxed);
            ImageSource::Resource(entry.1).remove_asset(cx);
        }

        self.cache.insert(
            hash,
            (
                gpui::ImageCacheItem::Loading(task.clone()),
                resource.clone(),
                0,
            ),
        );
        IMAGE_CACHE_ENTRIES.fetch_add(1, Ordering::Relaxed);
        self.usage_list.push_front(hash);

        let entity = window.current_view();

        window
            .spawn(cx, async move |cx| {
                let result = task.await;

                if let Err(err) = result {
                    error!("error loading image into cache: {:?}", err);
                }

                cx.on_next_frame(move |_, cx| {
                    cx.notify(entity);
                });
            })
            .detach();

        None
    }
}
