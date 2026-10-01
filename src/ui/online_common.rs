//! Shared glue for the online providers' UI layer: play-intent fetch dedup,
//! QR login image rendering and tolerant JSON field extraction. Both
//! provider glue modules (`ui::kugou` / `ui::netease`) consume these;
//! everything here must stay provider-agnostic.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, OnceLock, RwLock, RwLockWriteGuard},
    time::{Duration, Instant},
};

use gpui::{
    Context, ParentElement, RenderImage, ScrollHandle, SharedString, UniformListScrollHandle,
};
use serde_json::Value;
use smallvec::SmallVec;

use crate::ui::components::button::{ButtonIntent, InteractiveButton, button};

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
    pending_fetches().write().unwrap_or_else(|e| e.into_inner())
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
// 排行榜（discovery）页共享件：kugou_ranks / netease_ranks 的 switch_tab、
// load_more、tab_button 与 liked 缓存预热四处逐字相同（仅 provider 名/id
// 前缀与各自私有字段不同），收编于此；两侧调用点保留原方法名，只做一行
// 转发。差异点全部经参数注入。
// ---------------------------------------------------------------------------

/// 排行榜页 `switch_tab` 的共享体。与原实现行为一致：同页点击早退；换页时
/// 先换 tab、再重建两个滚动句柄；切到日推页且 recommend 尚未加载过才触发
/// 首次加载；最后 notify。各视图经参数接入自己的 tab / recommend 字段与方法。
///
/// `same_tab` 与 `recommend_idle` 在调用点求值：两者都是对字段的纯读取，
/// 提前求值与原实现的短路求值行为等价（`recommend_idle` 传
/// `tab == Tab::DailyRecommend && matches!(self.recommend, RecommendState::Idle)`）。
pub(crate) fn switch_ranks_tab<V: 'static>(
    view: &mut V,
    cx: &mut Context<V>,
    same_tab: bool,
    set_tab: impl FnOnce(&mut V),
    scroll_handles: impl FnOnce(&mut V) -> (&mut ScrollHandle, &mut UniformListScrollHandle),
    recommend_idle: bool,
    load_recommend: impl FnOnce(&mut V, &mut Context<V>),
) {
    if same_tab {
        return;
    }
    set_tab(view);
    let (scroll_handle, tracks_scroll_handle) = scroll_handles(view);
    *scroll_handle = ScrollHandle::new();
    *tracks_scroll_handle = UniformListScrollHandle::new();
    if recommend_idle {
        load_recommend(view, cx);
    }
    cx.notify();
}

/// 排行榜页 `load_more` 的共享体。`next_page`（= track_page + 1）由调用点
/// 传入，纯计算，提前求值与原实现等价；翻页动作经 `load_page` 注入。
pub(crate) fn load_more_rank_tracks<V: 'static>(
    view: &mut V,
    cx: &mut Context<V>,
    has_more_tracks: bool,
    a_page_is_loading: bool,
    next_page: i64,
    load_page: impl FnOnce(&mut V, i64, &mut Context<V>),
) {
    // one page in flight at a time: tracks_state stays Loading from the
    // click until the response lands, so extra clicks are ignored
    if has_more_tracks && !a_page_is_loading {
        load_page(view, next_page, cx);
    }
}

/// 排行榜页头部 `tab_button` 的共享体：两侧按钮的构建/样式/点击管线完全
/// 相同，仅 element id 前缀与各自的 tab 枚举不同。`on_select` 接各视图自己的
/// `switch_tab`（点击回调会触发多次，须 `Fn`）。
pub(crate) fn rank_tab_button<V: 'static>(
    id_prefix: &str,
    tab: impl Copy + std::fmt::Debug,
    active: bool,
    label: impl Into<SharedString>,
    on_select: impl Fn(&mut V, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> InteractiveButton {
    let label = label.into();
    button()
        .id(format!("{id_prefix}-rank-tab-{:?}", tab))
        .intent(if active {
            ButtonIntent::Primary
        } else {
            ButtonIntent::Secondary
        })
        .child(label)
        .on_click(cx.listener(move |this, _, _, cx| on_select(this, cx)))
}

/// 后台预热 provider 的 liked 缓存（`ui::kugou::prime_liked_cache` /
/// `ui::netease::prime_liked_cache` 的共享体）：provider 专属的加载锁与刷新
/// 函数经参数传入，函数体两侧原本逐字相同。
pub(crate) fn prime_online_liked_cache<L, R, Fut>(
    liked_load_lock: L,
    refresh_liked_set_from_service: R,
) where
    L: Fn() -> &'static tokio::sync::Mutex<()> + Send + 'static,
    R: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    crate::RUNTIME.spawn(async move {
        // Same lock as the lazy load in `online_track_is_liked` so the two
        // entry points cannot double-fetch the whole liked list.
        let _guard = liked_load_lock().lock().await;
        refresh_liked_set_from_service().await;
    });
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
