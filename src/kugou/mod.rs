//! 酷狗音乐集成：协议加密、请求签名、网关客户端与端点包装。这里的一切都在
//! `kugou` cargo feature 之后。

pub mod api;
pub mod client;
pub mod crypto;
pub mod import;
pub mod sign;

#[cfg(all(test, feature = "kugou"))]
mod live_tests;

pub use client::{KugouClient, KugouError};

use std::sync::{Arc, OnceLock};

static SHARED_CLIENT: OnceLock<Arc<KugouClient>> = OnceLock::new();

/// 进程级共享的酷狗客户端。会话持久化它自己在内部处理（数据目录里的
/// kugou_session.json），所以从任何地方调用都安全，首次之后更是廉价。
pub fn shared_client() -> Arc<KugouClient> {
    SHARED_CLIENT
        .get_or_init(|| Arc::new(KugouClient::new(&crate::paths::data_dir())))
        .clone()
}
