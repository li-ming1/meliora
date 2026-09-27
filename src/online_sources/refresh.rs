//! Shared "refresh a stale online stream URL" plumbing.
//!
//! KuGou / NetEase direct-link URLs are signed and expire (HTTP 403), but
//! playlist items persist them. Both callers re-fetch a fresh URL from the
//! provider identity: the session-restore pass in `app.rs` and the playback
//! thread's one-shot retry when opening a persisted URL fails.
//!
//! A-1 step-① sink from `crate::ui::online`. Step ② replaced the identity
//! match with the compile-time registry: the per-provider bodies live in the
//! [`super::OnlineSourceProvider`] impls and this module only routes.

use super::{OnlineIdentity, OnlineSourceProvider};

/// Display metadata shape shared by both providers:
/// `(name, artist, duration, cover_url)`.
pub type OnlineDisplay = (Option<String>, Option<String>, Option<i64>, Option<String>);

/// Re-fetches a fresh stream URL for an online identity and re-records it in
/// the provider's stream registry (lyrics / like / download resolve by that
/// registry, so a refreshed URL must be re-registered or those break).
///
/// The provider is looked up in the compile-time registry; quality arguments
/// ride in the feature-gated [`super::RefreshContext`], matching what the
/// call sites pass (the settings of the providers compiled in).
///
/// Returns `None` when no registered provider claims the identity or the
/// provider can no longer produce a playable URL.
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
