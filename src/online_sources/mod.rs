//! 共享在线源层：provider 注册表（流 URL → 曲目）、播放 URL 的获取/刷新与各 provider 自己
//! 的数据类型，UI 与播放/统计线程都用它。
//!
//! A-1 第①步下沉：与 provider 无关的管线从 `crate::ui` 搬出来，让 playback/stats 依赖这一层
//! 而不再回调 UI。第②步加上 [`OnlineSourceProvider`] trait + 编译期注册表，于是共享管线按
//! trait 分派而不是 match identity 枚举。本模块绝不能依赖 `crate::ui` 或 `crate::playback`。

use std::path::Path;

/// 标出一条 HTTP 队列条目来自哪个在线服务，信息量足以在重启后为它重新取一个新鲜（未过期）
/// 的流 URL。
///
/// 由 `playback::queue` 下沉而来（第①步；该模块再导出它，以保持既有使用点不变）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OnlineIdentity {
    /// 酷狗：`song_url(hash, mix_song_id, album_id, quality, free_part)`。
    Kugou {
        hash: String,
        mix_song_id: i64,
        album_id: i64,
    },
}

/// 每次调用的刷新输入：每个编进来的 provider 的音质设置，加上队列条目的展示元数据。音质
/// 字段按 feature 门控以匹配实际存在的 provider，所以没有在线 feature 的构建里不带死字段。
pub struct RefreshContext<'a> {
    #[cfg(feature = "kugou")]
    pub kugou_quality: &'a str,
    pub display: OnlineDisplay,
}

/// 与 provider 无关的持久化流路径视图：从该 provider 的注册表恢复出的 identity，加上为它
/// 记下的展示元数据。
pub struct OnlineTrackMatch {
    pub identity: OnlineIdentity,
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// 第②步的接缝：每个编进来的 provider 一个实现。`kugou` 子模块里的自由函数仍是事实来源，
/// impl 只委托给它们，所以「加一个 provider」= 实现这个 trait 并注册进 [`providers`]——
/// 共享管线里不需要新的 match 分支。
#[async_trait::async_trait]
pub trait OnlineSourceProvider: Sync {
    /// `identity` 属于本 provider 的某个变体时返回 true。
    fn handles(&self, identity: &OnlineIdentity) -> bool;
    /// 经该 provider 的流注册表把持久化的流路径解析回 identity + 展示元数据。
    fn identify_path(&self, path: &Path) -> Option<OnlineTrackMatch>;
    /// 为 `identity` 重新取一个新鲜的流 URL，并把它重新登记进该 provider 的流注册表
    /// （歌词/喜欢/下载都按那个注册表解析，所以刷新过的 URL 必须重新注册，否则这些坏掉）。
    /// provider 再也产不出可播放 URL 时返回 `None`。
    async fn refresh_url(
        &self,
        identity: &OnlineIdentity,
        ctx: &RefreshContext<'_>,
    ) -> Option<String>;
}

/// 编译期注册表：每个编进来的 provider feature 一个条目。`default` feature 下为空——那时
/// 整个在线源表面都是关的。
pub fn providers() -> &'static [&'static dyn OnlineSourceProvider] {
    &[
        #[cfg(feature = "kugou")]
        &kugou::KugouSource,
    ]
}

/// 让每个已注册的 provider 依次解析一条持久化的流路径。
pub fn identify_path(path: &Path) -> Option<OnlineTrackMatch> {
    providers().iter().find_map(|p| p.identify_path(path))
}

pub mod cover_art;
#[cfg(feature = "kugou")]
pub mod kugou;
pub mod refresh;
// 流注册表持久化设施（与 provider 无关；每个 provider 各持一份实例）。
pub mod stream_registry;

// 共享的刷新入口住在 `refresh` 子模块里；这里再导出，让调用方把 `crate::online_sources`
// 当作一个整体来依赖。
pub use refresh::{OnlineDisplay, refresh_online_url};
