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

/// Live footprint of every `MelioraImageCache` instance combined, reported
/// by the `[mem]` probe. Bytes are credited when an entry's load task first
/// resolves (see `load`) and debited on eviction / entity release, so the
/// probe curve attributes the view caches' share of resident memory exactly
/// instead of leaving it inside the unattributed plateau.
static IMAGE_CACHE_ENTRIES: AtomicU64 = AtomicU64::new(0);
static IMAGE_CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

/// (live entries, decoded MiB) across all `MelioraImageCache` instances.
pub fn image_cache_stats() -> (u64, u64) {
    (
        IMAGE_CACHE_ENTRIES.load(Ordering::Relaxed),
        IMAGE_CACHE_BYTES.load(Ordering::Relaxed) / (1024 * 1024),
    )
}

pub fn meliora_cache(
    id: impl Into<ElementId>,
    max_items: usize,
) -> MelioraImageCacheProvider {
    MelioraImageCacheProvider {
        id: id.into(),
        max_items,
    }
}

pub struct MelioraImageCacheProvider {
    id: ElementId,
    max_items: usize,
}

impl ImageCacheProvider for MelioraImageCacheProvider {
    fn provide(&mut self, window: &mut gpui::Window, cx: &mut App) -> gpui::AnyImageCache {
        window
            .with_global_id(self.id.clone(), |id, window| {
                window.with_element_state(id, |cache, _| {
                    let cache =
                        cache.unwrap_or_else(|| MelioraImageCache::new(self.max_items, cx));

                    (cache.clone(), cache)
                })
            })
            .into()
    }
}

pub struct MelioraImageCache {
    max_items: usize,
    usage_list: VecDeque<u64>,
    /// (loading task, resource, decoded-pixel bytes credited to the `[mem]`
    /// probe — stays 0 until the load task resolves).
    cache: FxHashMap<u64, (ImageCacheItem, Resource, u64)>,
}

impl MelioraImageCache {
    pub fn new(max_items: usize, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            trace!("Creating MelioraImageCache");
            cx.on_release(|this: &mut Self, cx| {
                let entries = this.cache.len() as u64;
                for (idx, (mut image, resource, recorded)) in take(&mut this.cache) {
                    IMAGE_CACHE_BYTES.fetch_sub(recorded, Ordering::Relaxed);
                    if let Some(Ok(image)) = image.get() {
                        trace!("Dropping image {idx}");
                        queue_orphan_tile_drop(image);
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
            // Fast path: the LRU-hot item is already at the front of
            // usage_list, which covers the vast majority of hits; only cold
            // hits pay for the O(n) scan + reorder.
            if self.usage_list.front() != Some(&hash) {
                let current_idx = self
                    .usage_list
                    .iter()
                    .position(|item| *item == hash)
                    .expect("cache has an item usage_list doesn't");

                self.usage_list.remove(current_idx);
                self.usage_list.push_front(hash);
            }

            // Credit the entry's decoded pixels once, the first time the
            // shared load task resolves, so the [mem] probe reports the view
            // caches' live footprint without scanning per tick.
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
            let mut image = self
                .cache
                .remove(&oldest)
                .expect("usage_list has an item cache doesn't");

            if let Some(Ok(image)) = image.0.get() {
                trace!("requesting image to be dropped");
                // 驱逐发生在 img 的 request_layout/paint 调用栈内：直接
                // drop_image 会在同帧释放图集页（sprite 已记录）并可能踩
                // etagere 断言。推进回收漏斗，由事件循环的 drain 帧间释放。
                queue_orphan_tile_drop(image);
            }

            IMAGE_CACHE_ENTRIES.fetch_sub(1, Ordering::Relaxed);
            IMAGE_CACHE_BYTES.fetch_sub(image.2, Ordering::Relaxed);
            ImageSource::Resource(image.1).remove_asset(cx);
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
