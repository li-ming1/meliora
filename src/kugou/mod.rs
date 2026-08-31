//! KuGou Music integration: protocol crypto, request signing, the gateway
//! client and the endpoint wrappers. Everything here is gated behind the
//! `kugou` cargo feature.

pub mod api;
pub mod client;
pub mod crypto;
pub mod import;
pub mod sign;

#[cfg(all(test, feature = "kugou"))]
mod live_tests;

pub use client::{KugouClient, KugouError, LoginExtras};

use std::sync::{Arc, OnceLock};

static SHARED_CLIENT: OnceLock<Arc<KugouClient>> = OnceLock::new();

/// Process-wide KuGou client. Session persistence is handled internally
/// (kugou_session.json in the data directory), so this is safe to call from
/// anywhere and cheap after the first use.
pub fn shared_client() -> Arc<KugouClient> {
    SHARED_CLIENT
        .get_or_init(|| Arc::new(KugouClient::new(&crate::paths::data_dir())))
        .clone()
}
