//! 共享的「刷新陈旧在线流 URL」管线。
//!
//! 酷狗的直链 URL 是签名的、会过期（HTTP 403），而歌单条目会把它们持久化下来。调用方凭
//! provider identity 重新取一个新鲜 URL：`app.rs` 里的会话恢复流程，以及播放线程在打开
//! 持久化 URL 失败时的一次性重试。
//!
//! 由 `crate::ui::online` 下沉而来（A-1 第①步）。第②步把 identity 的 match 换成了编译期
//! 注册表：各 provider 的方法体住在 [`super::OnlineSourceProvider`] 的 impl 里，本模块只做路由。

use super::{OnlineIdentity, OnlineSourceProvider};

/// 在线源共享的展示元数据结构：`(name, artist, duration, cover_url)`。
pub type OnlineDisplay = (Option<String>, Option<String>, Option<i64>, Option<String>);

/// 为一个在线 identity 重新取新鲜的流 URL，并把它重新登记进该 provider 的流注册表
/// （歌词/喜欢/下载都按那个注册表解析，所以刷新过的 URL 必须重新注册，否则这些坏掉）。
///
/// provider 在编译期注册表里查找；音质参数随 feature 门控的 [`super::RefreshContext`] 一起
/// 带，与调用点传的东西一致（即编进来的那些 provider 的设置）。
///
/// 没有已注册的 provider 认领这个 identity，或 provider 再也产不出可播放 URL 时返回 `None`。
pub async fn refresh_online_url(
    identity: &OnlineIdentity,
    ctx: &super::RefreshContext<'_>,
) -> Option<String> {
    let provider: &dyn OnlineSourceProvider = super::providers()
        .iter()
        .copied()
        .find(|p| p.handles(identity))?;
    provider.refresh_url(identity, ctx).await
}
