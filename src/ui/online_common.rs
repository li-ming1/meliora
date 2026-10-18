//! Shared glue for the online provider's UI layer: play-intent fetch dedup,
//! QR login image rendering and tolerant JSON field extraction. The
//! provider glue module (`ui::kugou`) consumes these;
//! everything here must stay provider-agnostic.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, RwLockWriteGuard},
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    RenderImage, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, div, prelude::FluentBuilder, px, uniform_list,
};
use serde_json::Value;
use smallvec::SmallVec;

use crate::sync::{RecoveredMutex, RecoveredRwLock};
use crate::ui::components::{button::button, scrollbar::floating_scrollbar};

/// Intent of an online-track activation: start playing now, or append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlayIntent {
    Now,
    Queue,
}

/// Song ids whose play-URL fetch is currently in flight, plus ids whose
/// fetch recently succeeded (tagged with the intent it served). Concurrent
/// clicks on the same song coalesce into the first fetch so the track can't
/// be queued twice while the (slow) URL request is still outstanding, and a
/// short same-intent cooldown after a success keeps a click burst from
/// running the full fetch + open churn several times — GPUI delivers one
/// click event per click of a multi-click sequence, so a double-click is
/// two `play_track` calls a few hundred ms apart.
pub(crate) struct PlayFetchDedup {
    pub(crate) in_flight: HashSet<i64>,
    pub(crate) recent: HashMap<i64, (u8, Instant)>,
}

/// How long a successful fetch suppresses an identical-intent re-request.
pub(crate) const FETCH_COOLDOWN: Duration = Duration::from_millis(800);

static PENDING_FETCHES: OnceLock<RwLock<PlayFetchDedup>> = OnceLock::new();

pub(crate) fn pending_fetches() -> &'static RwLock<PlayFetchDedup> {
    PENDING_FETCHES.get_or_init(|| {
        RwLock::new(PlayFetchDedup {
            in_flight: HashSet::new(),
            recent: HashMap::new(),
        })
    })
}

/// Poison-recovering write lock for the fetch dedup table: a panicking
/// holder must not wedge every later click behind a poisoned lock.
pub(crate) fn write_pending_fetches() -> RwLockWriteGuard<'static, PlayFetchDedup> {
    pending_fetches().write_recovered()
}

/// Decision core of the play-URL fetch dedup: `true` when the fetch may
/// proceed (the key is tagged in-flight), `false` when this click is a
/// duplicate suppressed by the in-flight tag or the same-intent cooldown.
/// `now` is injected so the cooldown expiry is unit-testable.
pub(crate) fn claim_fetch(
    dedup: &mut PlayFetchDedup,
    key: i64,
    intent: PlayIntent,
    now: Instant,
) -> bool {
    let intent_key = matches!(intent, PlayIntent::Now) as u8;
    dedup
        .recent
        .retain(|_, (_, at)| now.duration_since(*at) < FETCH_COOLDOWN);
    let duplicate = dedup
        .recent
        .get(&key)
        .is_some_and(|&(seen_intent, _)| seen_intent == intent_key)
        || !dedup.in_flight.insert(key);
    !duplicate
}

/// Records a finished fetch: clears the in-flight tag and stamps the
/// same-intent cooldown. `now` is injected for testability.
pub(crate) fn settle_fetch(dedup: &mut PlayFetchDedup, key: i64, intent: PlayIntent, now: Instant) {
    let intent_key = matches!(intent, PlayIntent::Now) as u8;
    dedup.in_flight.remove(&key);
    dedup.recent.insert(key, (intent_key, now));
}

/// Renders a URL as a black-on-white QR code image. Uses the same frame
/// construction as the album art pipeline.
pub(crate) fn build_qr_render_image(url: &str) -> anyhow::Result<Arc<RenderImage>> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    let mut image: image::RgbaImage = code
        .render::<image::Rgba<u8>>()
        .quiet_zone(true)
        .min_dimensions(320, 320)
        .build();

    crate::ui::components::managed_image::rgb_to_bgr(&mut image);

    let mut frames: SmallVec<[_; 1]> = SmallVec::new();
    frames.push(image::Frame::new(image));
    Ok(Arc::new(RenderImage::new(frames)))
}

/// First non-empty string among `keys`, or an empty string.
pub(crate) fn string_field(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(Value::String(s)) = value.get(*key)
            && !s.is_empty()
        {
            return s.clone();
        }
    }
    String::new()
}

/// First numeric value among `keys` (numbers directly, numeric strings
/// parsed), or 0.
pub(crate) fn i64_field(value: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        match value.get(*key) {
            Some(Value::Number(n)) => return n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => {
                if let Ok(parsed) = s.parse::<i64>() {
                    return parsed;
                }
            }
            _ => {}
        }
    }
    0
}

/// Resolves `pointer` to a JSON array and maps each entry through `parse`,
/// dropping entries it rejects. Shared shell for the provider response
/// parsers (search / playlist / rank / recommend endpoints).
pub(crate) fn parse_pointer_list<T>(
    body: &Value,
    pointer: &str,
    parse: impl Fn(&Value) -> Option<T>,
) -> Vec<T> {
    body.pointer(pointer)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 排行榜（discovery）页共享件的历史注记：netease 摘除前（2026-10-14），
// kugou_ranks 与 netease_ranks 的 switch_tab / load_more / tab_button 与
// liked 缓存预热四处逐字相同，曾以"共享体 + 参数注入 + 一行转发"收编于此。
// 只剩酷狗一个 provider 后，四处均退化回单调用点，注入参数只为虚构的第二个
// 调用点存在——2026-10-15 按 §12/§27 内联回各调用点（kugou_ranks.rs /
// ui/kugou.rs），共享层的"双 provider"框架自此只保留仍有多调用件的部分
// （parse_pointer_list / claim_fetch / CachedRoot 等）。
// ---------------------------------------------------------------------------

/// Session-level cache TTL for discovery-page root lists. The view entities
/// are rebuilt on every tab switch, so without a cache reopening a discovery
/// page re-fetches its root list from scratch. These lists are slow-moving
/// content (rank lists refresh on hour/day cadences, user playlists only
/// change on explicit mutation), so half an hour of staleness is invisible
/// while re-entry becomes instant. Only successful loads are cached; failure
/// paths and explicit refresh/mutation call sites bypass the cache and
/// refill it on success.
pub(crate) const ROOTS_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

pub(crate) struct CachedRoot<T> {
    at: Instant,
    value: T,
}

/// Fresh entry out of a root-list cache, `None` on miss or TTL expiry.
pub(crate) fn cached_root<T: Clone>(cache: &LazyLock<Mutex<Option<CachedRoot<T>>>>) -> Option<T> {
    let guard = cache.lock_recovered();
    let cached = guard.as_ref()?;
    (cached.at.elapsed() < ROOTS_CACHE_TTL).then(|| cached.value.clone())
}

/// Store a successful load into a root-list cache, replacing any stale entry.
pub(crate) fn store_root<T>(cache: &LazyLock<Mutex<Option<CachedRoot<T>>>>, value: T) {
    let mut guard = cache.lock_recovered();
    *guard = Some(CachedRoot {
        at: Instant::now(),
        value,
    });
}

/// Drop a root-list cache entry entirely — for content keyed to an account
/// (user playlists), so an in-session account switch can never be served the
/// previous account's data however fresh the entry is.
pub(crate) fn clear_root<T>(cache: &LazyLock<Mutex<Option<CachedRoot<T>>>>) {
    *cache.lock_recovered() = None;
}

// ---------------------------------------------------------------------------
// 发现页（discovery）列表容器共享件：kugou_ranks 与 kugou_playlists 的曲目
// 列表容器、页面级滚动容器与 Load More 药丸逐行相同（2026-10-15 收编台阶
// D）。布局差异（relative / pb / 标题行 / id）全部经参数注入，不做"顺手
// 统一"——那些差异各有来历（滚动条定位、底部留白），规范化会改行为。
// ---------------------------------------------------------------------------

/// The "Load More" pill below a virtualized list: centered button. Shared by
/// [`online_list_container`] and any call site that needs the escape hatch
/// outside a container.
pub(crate) fn load_more_pill<V: 'static>(
    id: &'static str,
    label: AnyElement,
    on_click: impl Fn(&mut V, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> impl IntoElement {
    div().flex().justify_center().pt(px(12.0)).child(
        button()
            .id(id)
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx))),
    )
}

/// [`LoadMoreSpec`] 的点击回调：与 gpui 的 `cx.listener` 同型，收进别名
/// 是为了让结构体字段读起来是"意图"而不是一长串类型。
pub(crate) type LoadMoreHandler<V> = Box<dyn Fn(&mut V, &mut Context<V>)>;

/// Optional Load More pill spec for [`online_list_container`]: `None`
/// renders no pill at all.
pub(crate) struct LoadMoreSpec<V> {
    pub id: &'static str,
    pub label: AnyElement,
    pub on_click: LoadMoreHandler<V>,
}

/// The virtualized track-list container shared by the KuGou discovery
/// views: a fixed-height-row `uniform_list` that is its own scroll
/// container (so accumulated rows stay virtualized regardless of page
/// count), an optional title line, the optional Load More pill and the
/// floating scrollbar.
///
/// Everything that differs between the two views is a parameter — element
/// ids (must stay distinct: they key scroll state and image caches),
/// `container_pb` (0 = none), `container_relative`, the title, the item
/// slice and the row builder. The row builder receives `&App` (rows are
/// built inside the uniform_list closure, where no `Context` exists), so
/// per-row listeners must reach the view through the passed entity.
#[allow(clippy::too_many_arguments)] // builder-style container API; every parameter is a real per-view delta
pub(crate) fn online_list_container<V: 'static, T>(
    cx: &mut Context<V>,
    container_id: &'static str,
    list_id: &'static str,
    scrollbar_id: &'static str,
    scroll_handle: &UniformListScrollHandle,
    row_height: f32,
    count: usize,
    container_pb: f32,
    container_relative: bool,
    title: Option<SharedString>,
    items: impl Fn(&V) -> &[T] + 'static,
    render_row: impl Fn(&V, usize, &T, &Entity<V>, &App) -> AnyElement + 'static,
    load_more: Option<LoadMoreSpec<V>>,
) -> impl IntoElement {
    let list_entity = cx.entity();

    let mut container = div()
        .id(container_id)
        .when(container_relative, |this| this.relative())
        .w_full()
        .max_w(px(900.0))
        .mr_auto()
        .ml_auto()
        .flex()
        .flex_col()
        .flex_grow(1.0)
        .min_h(px(0.0))
        .px(px(16.0))
        .pt(px(4.0))
        .when(container_pb > 0.0, |this| this.pb(px(container_pb)))
        .when_some(title, |this, name| {
            this.child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .pb(px(8.0))
                    .child(name),
            )
        })
        .child(
            div()
                .relative()
                .w_full()
                .flex_grow(1.0)
                .min_h(px(0.0))
                .child(
                    uniform_list(list_id, count, move |range, _, cx| {
                        let start = range.start;
                        let view = list_entity.read(cx);
                        items(view)[range]
                            .iter()
                            .enumerate()
                            .map(|(i, track)| {
                                div().h(px(row_height)).child(render_row(
                                    view,
                                    start + i,
                                    track,
                                    &list_entity,
                                    cx,
                                ))
                            })
                            .collect()
                    })
                    .w_full()
                    .h_full()
                    .track_scroll(scroll_handle),
                ),
        );

    if let Some(spec) = load_more {
        container = container.child(load_more_pill(spec.id, spec.label, spec.on_click, cx));
    }
    container.child(floating_scrollbar(scrollbar_id, scroll_handle.clone()))
}

/// The page-level scroll container for the fallback (loading / error /
/// empty) states: the same 900px column geometry as the list containers,
/// with `overflow_y_scroll` and the page scroll handle. The floating
/// scrollbar is a sibling child — the caller adds it with its own id.
pub(crate) fn page_scroll_container(
    id: &'static str,
    scroll_handle: &ScrollHandle,
    content: AnyElement,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .max_w(px(900.0))
        .mr_auto()
        .ml_auto()
        .flex()
        .flex_col()
        .flex_grow(1.0)
        .min_h(px(0.0))
        .px(px(16.0))
        .pt(px(4.0))
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_dedup() -> PlayFetchDedup {
        PlayFetchDedup {
            in_flight: HashSet::new(),
            recent: HashMap::new(),
        }
    }

    #[test]
    fn second_click_while_in_flight_is_suppressed() {
        let mut dedup = empty_dedup();
        let now = Instant::now();
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Now, now));
        assert!(!claim_fetch(&mut dedup, 1, PlayIntent::Now, now));
        // a different song is never suppressed by another's in-flight tag
        assert!(claim_fetch(&mut dedup, 2, PlayIntent::Now, now));
    }

    #[test]
    fn cooldown_suppresses_same_intent_only_until_expiry() {
        let mut dedup = empty_dedup();
        let t0 = Instant::now();
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Now, t0));
        settle_fetch(&mut dedup, 1, PlayIntent::Now, t0);

        // same intent inside the cooldown window is swallowed...
        let mid = t0 + Duration::from_millis(FETCH_COOLDOWN.as_millis() as u64 / 2);
        assert!(!claim_fetch(&mut dedup, 1, PlayIntent::Now, mid));
        // ...but a deliberate play right after a queue-add is not (it starts
        // its own fetch, which we settle like the click handler would)
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Queue, mid));
        settle_fetch(&mut dedup, 1, PlayIntent::Queue, mid);
        // ...and the same intent passes once the cooldown expires
        let after = t0 + FETCH_COOLDOWN + Duration::from_millis(1);
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Now, after));
    }

    #[test]
    fn settle_releases_the_in_flight_tag() {
        let mut dedup = empty_dedup();
        let now = Instant::now();
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Queue, now));
        settle_fetch(&mut dedup, 1, PlayIntent::Queue, now);
        assert!(
            !dedup.in_flight.contains(&1),
            "a finished fetch must not stay tagged in-flight"
        );
    }
}
