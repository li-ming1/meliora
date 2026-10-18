//! 两个酷狗发现视图（`kugou_ranks` 与 `kugou_playlists`）的共享部分：固定的曲目行高、
//! 赞/取消赞结果监视器、曲目行视图构造器。与它的两个使用者一起受 `kugou` cargo
//! feature 门控。
//!
//! 这里放的是当所有真实差异——元素 id 前缀、时长补齐、已点赞谓词、回滚快照——都改为
//! 参数注入后，两个视图之间逐字节相同的片段（2026-10-15 收编台阶 D）。同一轮收编里与
//! 音源无关的布局片段（列表容器、页面滚动器、Load More 药丸）住在 `ui::online_common`；
//! 本文件只收天生属于酷狗的部分（liked-set 谓词、播放/点赞/下载回调）。

use std::{sync::Arc, time::Duration};

use gpui::{AnyElement, App, Context, Entity, IntoElement};

use crate::ui::{
    kugou::{KugouTrackInfo, play_track_now},
    online_track_row::track_row,
    theme::Theme,
};

/// uniform_list 按第一行量出的单一固定高度步进各行，所以曲目行钉死为自然高度：
/// 上下 padding 8px 各一次、一个 text_sm 标题行、1px 间隔、一个 text_xs 副标题行，
/// 再加 1px 下边框。两个发现视图渲染同一套行布局，因此共用这个值。
pub(crate) const ONLINE_TRACK_ROW_HEIGHT: f32 = 60.0;

/// 点赞/取消赞监视器在回滚乐观更新之前，等多久让共享辅助函数经全局 liked-set 确认结果。
pub(crate) const LIKE_WATCH_TIMEOUT: Duration = Duration::from_secs(30);

/// 监视一次赞/取消赞请求，直到进程级 liked-set 确认它（或截止时间到），
/// 两条路径都会释放视图的「已在途」点击去重标记，失败时把乐观快照回滚。
///
/// 共享的赞/取消赞辅助函数成功时翻转全局 liked-set、失败时原样不动，所以结果可由
/// `crate::ui::kugou::liked_set_contains` 观测；请求没落地时必须撤销乐观更新，
/// 否则一次失败的点赞会留下错误的星标。`take_in_flight` 丢掉每视图的去重标记
/// （两条路径都调）；`rollback` 还原点击前的快照（只在失败路径调，因此是 `FnOnce`
/// ——每个视图在那里捕获自己的快照）。
pub(crate) fn spawn_like_watcher<V: 'static>(
    cx: &mut Context<V>,
    hash: String,
    want_liked: bool,
    take_in_flight: impl Fn(&mut V, &str) + Send + 'static,
    rollback: impl FnOnce(&mut V, &str) + Send + 'static,
) {
    cx.spawn(async move |this, cx| {
        let deadline = std::time::Instant::now() + LIKE_WATCH_TIMEOUT;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            if crate::ui::kugou::liked_set_contains(&hash) == want_liked {
                // 已落地：释放去重标记，无需回滚
                this.update(cx, |this, _| take_in_flight(this, &hash)).ok();
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        // 从未被确认：撤销乐观更新
        this.update(cx, |this, cx| {
            take_in_flight(this, &hash);
            rollback(this, &hash);
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// 为两个视图之一构造一行发现曲目。行本身是 [`track_row`]；两个视图之间的差异只有
/// 元素 id 前缀、时长是否补齐到整分钟、已点赞谓词与点赞回调——全部注入，于是逐行的
/// 闭包管道（用 Arc clone 而非深拷贝、弱实体 upgrade）只存在一份而不是两份。
///
/// 行是在 uniform_list 的渲染闭包内部构造的，那里只有 `&App`，因此这里没有
/// `cx.listener` 可用：点赞处理器改经弱句柄抵达视图。
#[allow(clippy::too_many_arguments)] // 构建器风格的行 API，与它喂给的 track_row 同形
pub(crate) fn kugou_track_row_view<V: 'static>(
    view: &V,
    entity: &Entity<V>,
    track: &Arc<KugouTrackInfo>,
    index: usize,
    cx: &App,
    id_prefix: &'static str,
    pad_minutes: bool,
    is_liked: impl Fn(&V, &str) -> bool,
    on_like: impl Fn(&mut V, &Arc<KugouTrackInfo>, &mut Context<V>) + 'static,
) -> AnyElement {
    let theme = cx.global::<Theme>();
    let liked = is_liked(view, &track.hash);
    // 行闭包必须持有自己数据的拥有权（'static）：捕获 Arc clone（只加引用计数），
    // 而不是每个可见行每次重绘三次深拷贝——完整 clone 每次都重新分配 `hash: String`。
    let play = track.clone();
    let like = track.clone();
    let download = track.clone();
    let weak = entity.downgrade();

    track_row(
        theme,
        track,
        index,
        id_prefix,
        true,
        pad_minutes,
        liked,
        move |_, _, cx| {
            play_track_now(cx, &play);
        },
        move |_, _, cx| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.update(cx, |this, cx| on_like(this, &like, cx));
        },
        move |_, _, cx| {
            crate::ui::kugou::download_track_ui(cx, (*download).clone());
        },
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钉住「两个视图必须一致」这个契约：uniform_list 用 `ONLINE_TRACK_ROW_HEIGHT`
    /// 给每一行步进，所以某个视图悄悄偏离这个值就会把行步进算错。
    #[test]
    fn shared_row_height_matches_the_row_layout_contract() {
        assert_eq!(ONLINE_TRACK_ROW_HEIGHT, 60.0);
        assert_eq!(LIKE_WATCH_TIMEOUT, Duration::from_secs(30));
    }
}
