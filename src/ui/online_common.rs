//! 在线音源 UI 层共用的胶水：播放意图的抓取去重、登录二维码渲染、宽容的 JSON
//! 字段提取。音源胶水模块（`ui::kugou`）消费这些函数；这里的一切都必须与具体
//! 音源无关。

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

/// 激活在线曲目的意图：立刻开始播放，还是追加到队列。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlayIntent {
    Now,
    Queue,
}

/// 播放 URL 抓取已在途的歌曲 id，外加最近抓取成功的 id（标上当时服务的意图）。
/// 对同一首歌的并发点击合并进第一次抓取，于是那趟（慢）URL 请求还挂着的时候
/// 这首歌不可能被入队两次；成功后的短暂同意图冷却又让连点不至于把「整趟抓取 +
/// 打开」的震荡跑好几遍——GPUI 对连点序列的每一次点击都发一个点击事件，
/// 所以双击就是相隔几百 ms 的两次 `play_track` 调用。
pub(crate) struct PlayFetchDedup {
    pub(crate) in_flight: HashSet<i64>,
    pub(crate) recent: HashMap<i64, (u8, Instant)>,
}

/// 一次成功的抓取会在多久之内压制同意图的重复请求。
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

/// 抓取去重表的毒化可恢复写锁：持有者 panic 绝不能把此后所有点击都卡在毒化的
/// 锁后面。
pub(crate) fn write_pending_fetches() -> RwLockWriteGuard<'static, PlayFetchDedup> {
    pending_fetches().write_recovered()
}

/// 播放 URL 抓取去重的决策核心：可以抓取时返回 `true`（键被标为已在途），
/// 这次点击被「已在途」标记或同意图冷却压制时返回 `false`。`now` 由外部注入，
/// 好让冷却到期能做单元测试。
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

/// 记录一次抓取结束：清掉「已在途」标记并盖上同意图冷却时间戳。`now` 由外部
/// 注入以便测试。
pub(crate) fn settle_fetch(dedup: &mut PlayFetchDedup, key: i64, intent: PlayIntent, now: Instant) {
    let intent_key = matches!(intent, PlayIntent::Now) as u8;
    dedup.in_flight.remove(&key);
    dedup.recent.insert(key, (intent_key, now));
}

/// 把一个 URL 渲染成白底黑字的二维码图片。用的是与专辑封面管线相同的框架构造。
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

/// `keys` 中第一个非空字符串，全都取不到时返回空字符串。
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

/// `keys` 中第一个数值（数字直接取，数字字符串解析），全都取不到时返回 0。
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

/// 把 `pointer` 解析成 JSON 数组并把每个条目过一遍 `parse`，丢弃被拒绝的条目。
/// 各音源响应解析器（搜索 / 歌单 / 榜单 / 推荐端点）共用的外壳。
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

/// 发现页根列表的会话级缓存 TTL。视图实体每次切标签都会重建，没有缓存的话重进
/// 发现页就要从零重取它的根列表。这些列表属于慢变内容（榜单按小时/天刷新，
/// 用户歌单只在显式变更时改变），所以半小时的陈旧完全看不出来，而重进变成秒开。
/// 只有成功的加载才入缓存；失败路径与显式刷新/变更的调用点绕过缓存，
/// 并在成功时回填。
pub(crate) const ROOTS_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

pub(crate) struct CachedRoot<T> {
    at: Instant,
    value: T,
}

/// 从根列表缓存里取一条新鲜条目；未命中或 TTL 到期返回 `None`。
pub(crate) fn cached_root<T: Clone>(cache: &LazyLock<Mutex<Option<CachedRoot<T>>>>) -> Option<T> {
    let guard = cache.lock_recovered();
    let cached = guard.as_ref()?;
    (cached.at.elapsed() < ROOTS_CACHE_TTL).then(|| cached.value.clone())
}

/// 把一次成功的加载存进根列表缓存，替换掉任何陈旧条目。
pub(crate) fn store_root<T>(cache: &LazyLock<Mutex<Option<CachedRoot<T>>>>, value: T) {
    let mut guard = cache.lock_recovered();
    *guard = Some(CachedRoot {
        at: Instant::now(),
        value,
    });
}

/// 整条丢弃一个根列表缓存条目——用于以账号为键的内容（用户歌单），这样会话内
/// 换了账号之后，无论条目多新鲜都绝不可能把上一个账号的数据端出去。
pub(crate) fn clear_root<T>(cache: &LazyLock<Mutex<Option<CachedRoot<T>>>>) {
    *cache.lock_recovered() = None;
}

// ---------------------------------------------------------------------------
// 发现页（discovery）列表容器共享件：kugou_ranks 与 kugou_playlists 的曲目
// 列表容器、页面级滚动容器与 Load More 药丸逐行相同（2026-10-15 收编台阶
// D）。布局差异（relative / pb / 标题行 / id）全部经参数注入，不做"顺手
// 统一"——那些差异各有来历（滚动条定位、底部留白），规范化会改行为。
// ---------------------------------------------------------------------------

/// 虚拟化列表下方的「加载更多」药丸按钮：居中的按钮。由
/// [`online_list_container`] 与任何需要在容器之外使用该逃逸口的调用点共用。
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

/// [`online_list_container`] 的可选「加载更多」药丸规格：`None` 表示完全不渲染
/// 药丸。
pub(crate) struct LoadMoreSpec<V> {
    pub id: &'static str,
    pub label: AnyElement,
    pub on_click: LoadMoreHandler<V>,
}

/// 酷狗发现页共用的虚拟化曲目列表容器：一个固定行高的 `uniform_list`，它自己就是
/// 自己的滚动容器（所以无论多少页，累积起来的行始终保持虚拟化），可选的标题行，
/// 可选的「加载更多」药丸，以及浮动滚动条。
///
/// 两个视图之间不同的东西全是参数——元素 id（必须保持互不相同：它们是滚动状态与
/// 图片缓存的键）、`container_pb`（0 = 无）、`container_relative`、标题、条目
/// 切片与行构建器。行构建器收到的是 `&App`（行是在 uniform_list 闭包里构建的，
/// 那里不存在 `Context`），所以逐行的监听器必须经由传入的实体到达视图。
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

/// 给回退状态（加载中 / 出错 / 空）用的页面级滚动容器：与列表容器同一套 900px
/// 列几何，带 `overflow_y_scroll` 和页面滚动句柄。浮动滚动条是兄弟子节点——
/// 由调用方带着自己的 id 加进去。
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
        // 另一首歌绝不会被「别人那首的已在途标记」压制
        assert!(claim_fetch(&mut dedup, 2, PlayIntent::Now, now));
    }

    #[test]
    fn cooldown_suppresses_same_intent_only_until_expiry() {
        let mut dedup = empty_dedup();
        let t0 = Instant::now();
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Now, t0));
        settle_fetch(&mut dedup, 1, PlayIntent::Now, t0);

        // 冷却窗口内的同意图会被吞掉……
        let mid = t0 + Duration::from_millis(FETCH_COOLDOWN.as_millis() as u64 / 2);
        assert!(!claim_fetch(&mut dedup, 1, PlayIntent::Now, mid));
        // ……但「先加入队列后立刻播放」这种有意点击不会被吞（它会发起自己的抓取，
        // 我们像点击处理器那样把它结算掉）
        assert!(claim_fetch(&mut dedup, 1, PlayIntent::Queue, mid));
        settle_fetch(&mut dedup, 1, PlayIntent::Queue, mid);
        // ……而冷却到期后，同意图就能通过
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
