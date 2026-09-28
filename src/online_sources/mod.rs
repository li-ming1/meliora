//! Shared online-source layer: provider registries (stream URL → track),
//! play-URL fetching/refreshing and per-provider data types, used by both the
//! UI and the playback/stats threads.
//!
//! A-1 step-① sink: the provider-agnostic plumbing moved out of `crate::ui`
//! so playback/stats depend on this layer instead of back on the UI. Step ②
//! adds the [`OnlineSourceProvider`] trait + compile-time registry so the
//! shared plumbing dispatches by trait instead of matching the identity enum.
//! This module must never depend on `crate::ui` or `crate::playback`.

use std::path::Path;

/// Identifies which online service an HTTP queue item came from, enough to
/// re-fetch a fresh (non-expired) stream URL for it after a restart.
///
/// ①-step sink from `playback::queue` (which re-exports it to keep its
/// existing use sites unchanged).
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

/// Per-call refresh inputs: the quality settings of every provider compiled
/// in, plus the queue item's display metadata. The quality fields are
/// feature-gated to match the providers present, so builds without online
/// features carry no dead fields.
pub struct RefreshContext<'a> {
    #[cfg(feature = "kugou")]
    pub kugou_quality: &'a str,
    #[cfg(feature = "netease")]
    pub netease_quality: &'a str,
    pub display: OnlineDisplay,
}

/// Provider-agnostic view of a persisted stream path: the identity recovered
/// from the provider's registry plus the display metadata remembered for it.
pub struct OnlineTrackMatch {
    pub identity: OnlineIdentity,
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// The ②-step seam: one implementation per compiled-in provider. The free
/// functions in the `kugou`/`netease` submodules stay the source of truth and
/// the impls delegate to them, so "add a provider" = implement this trait and
/// register it in [`providers`] — no new match arms in the shared plumbing.
#[async_trait::async_trait]
pub trait OnlineSourceProvider: Sync {
    /// True when `identity` is one of this provider's variants.
    fn handles(&self, identity: &OnlineIdentity) -> bool;
    /// Resolves a persisted stream path back to identity + display metadata
    /// via the provider's stream registry.
    fn identify_path(&self, path: &Path) -> Option<OnlineTrackMatch>;
    /// Re-fetches a fresh stream URL for `identity` and re-records it in the
    /// provider's stream registry (lyrics / like / download resolve by that
    /// registry, so a refreshed URL must be re-registered or those break).
    /// Returns `None` when the provider can no longer produce a playable URL.
    async fn refresh_url(
        &self,
        identity: &OnlineIdentity,
        ctx: &RefreshContext<'_>,
    ) -> Option<String>;
}

/// Compile-time registry: one entry per provider feature compiled in. Empty
/// under `default` features, where the whole online-source surface is.
pub fn providers() -> &'static [&'static dyn OnlineSourceProvider] {
    &[
        #[cfg(feature = "kugou")]
        &kugou::KugouSource,
        #[cfg(feature = "netease")]
        &netease::NeteaseSource,
    ]
}

/// Resolves a persisted stream path through every registered provider.
pub fn identify_path(path: &Path) -> Option<OnlineTrackMatch> {
    providers().iter().find_map(|p| p.identify_path(path))
}

pub mod cover_art;
#[cfg(feature = "kugou")]
pub mod kugou;
#[cfg(feature = "netease")]
pub mod netease;
pub mod refresh;

// The shared refresh entry point lives in the `refresh` submodule;
// re-exported here so callers depend on `crate::online_sources` as a unit.
pub use refresh::{OnlineDisplay, refresh_online_url};
