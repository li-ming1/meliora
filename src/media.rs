pub mod errors;
// 在线音源（酷狗）共用的下载 IO 层。
#[cfg(feature = "online_sources")]
pub mod download;
// 在线音源（酷狗等）使用的 HTTP(S) range 请求媒体源。
#[cfg(feature = "online_sources")]
pub mod http_source;
pub mod lofty;
pub mod lookup_table;
pub mod metadata;
pub mod pipeline;
// 远程音源与解码器之间的后台预取。
#[cfg(feature = "online_sources")]
pub mod prefetch;
pub mod symphonia;
pub mod text;
pub mod traits;

use std::path::Path;

/// 所有内置 provider 都声明支持的扩展名；两者解析同一批格式。
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "ogg", "oga", "aac", "flac", "wav", "mp3", "m4a", "aiff", "opus",
];

/// `path` 承载的是 HTTP(S) URL 而非文件系统位置。
/// 无条件可用：队列元数据与可用性检查都会查它，与编译进哪个在线音源
/// feature 无关。
pub fn is_http_path(path: &Path) -> bool {
    let Some(text) = path.as_os_str().to_str() else {
        return false;
    };
    let bytes = text.as_bytes();
    (bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://"))
        || (bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://"))
}
