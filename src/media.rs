pub mod builtin;
pub mod errors;
// HTTP(S) range-request media source for online sources (kugou et al.)
#[cfg(feature = "online_sources")]
pub mod http_source;
pub mod lookup_table;
pub mod metadata;
pub mod pipeline;
pub mod traits;

use std::path::Path;

/// Whether `path` carries an HTTP(S) URL instead of a filesystem location.
/// Available unconditionally: queue metadata and availability checks consult
/// it regardless of which online-source feature is compiled in.
pub fn is_http_path(path: &Path) -> bool {
    let Some(text) = path.as_os_str().to_str() else {
        return false;
    };
    let bytes = text.as_bytes();
    (bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://"))
        || (bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://"))
}
