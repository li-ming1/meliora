//! Shared "refresh a stale online stream URL" plumbing.
//!
//! KuGou / NetEase direct-link URLs are signed and expire (HTTP 403), but
//! playlist items persist them. Both callers re-fetch a fresh URL from the
//! provider identity: the session-restore pass in `app.rs` and the playback
//! thread's one-shot retry when opening a persisted URL fails.
//!
//! ①步下沉后的 UI 侧兼容层（A-1 第①步）：实现已整体搬至
//! `crate::online_sources`（`refresh` / `kugou` / `netease`），本模块仅按原
//! 路径再导出，保持调用点零改动；②步收编（ui 视图层迁移）时删除本模块。

// ①-step sink: the real definitions moved to `crate::online_sources`; these
// re-exports keep every existing UI-side call site unchanged. Fold this shim
// away in the ②-step consolidation.

/// Re-exposed for the playback thread's expired-URL retry: resolves a KuGou
/// stream URL to its remembered track (by the mixsongid embedded in the URL),
/// which is how playlist items, which persist only the URL, recover their
/// provider identity.
#[cfg(feature = "kugou")]
pub use crate::online_sources::kugou::online_track_matching_path;

/// NetEase counterpart of the KuGou re-export above, aliased so both provider
/// features can be enabled at once without an ambiguous name.
#[cfg(feature = "netease")]
pub use crate::online_sources::netease::online_track_matching_path as netease_online_track_matching_path;

pub use crate::online_sources::{OnlineDisplay, refresh_online_url};
