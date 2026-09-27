//! Shared online-source layer: provider registries (stream URL → track),
//! play-URL fetching/refreshing and per-provider data types, used by both the
//! UI and the playback/stats threads.
//!
//! A-1 step-① sink: the provider-agnostic plumbing moved out of `crate::ui`
//! so playback/stats depend on this layer instead of back on the UI. Plain
//! module functions/types only — the `OnlineSourceProvider` trait + registry
//! that folds these into per-provider implementations is step ②. This module
//! must never depend on `crate::ui` or `crate::playback`.

/// Identifies which online service an HTTP queue item came from, enough to
/// re-fetch a fresh (non-expired) stream URL for it after a restart.
///
/// ①-step sink from `playback::queue` (which re-exports it to keep its
/// existing use sites unchanged); step ② replaces the enum matching with
/// trait-based identity resolution.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OnlineIdentity {
    /// KuGou: `song_url(hash, mix_song_id, album_id, quality, free_part)`.
    Kugou {
        hash: String,
        mix_song_id: i64,
        album_id: i64,
    },
    /// NetEase: `song_url(id, level)`.
    Netease { id: i64 },
}

#[cfg(feature = "kugou")]
pub mod kugou;
#[cfg(feature = "netease")]
pub mod netease;
pub mod refresh;

// ①-step sink: the shared refresh entry point lives in the `refresh`
// submodule; re-exported here so callers depend on `crate::online_sources`
// as a unit. Fold away in the ②-step trait consolidation.
pub use refresh::{OnlineDisplay, refresh_online_url};
