//! NetEase Cloud Music integration: protocol crypto (weapi/eapi), the cookie
//! request engine and the endpoint wrappers. Everything here is gated behind
//! the `netease` cargo feature.

pub mod api;
pub mod client;
pub mod crypto;

#[cfg(all(test, feature = "netease"))]
mod live_tests;

pub use client::NeteaseClient;

use std::sync::{Arc, OnceLock};

static SHARED_CLIENT: OnceLock<Arc<NeteaseClient>> = OnceLock::new();

/// Process-wide NetEase client. Session persistence is handled internally
/// (netease_session.json in the data directory), so this is safe to call from
/// anywhere and cheap after the first use.
pub fn shared_client() -> Arc<NeteaseClient> {
    SHARED_CLIENT
        .get_or_init(|| Arc::new(NeteaseClient::new(&crate::paths::data_dir())))
        .clone()
}
