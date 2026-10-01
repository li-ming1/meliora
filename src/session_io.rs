//! 在线音源（kugou/netease）会话凭据文件共用的读写；`provider` 只进日志
//! 文本，其措辞是既有的日志排查口径，不要随手改写。

use std::path::Path;

pub fn load<T: serde::de::DeserializeOwned>(path: &Path, provider: &str) -> Option<T> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        // Missing credentials is the normal first-run path; stay quiet.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(%err, "no {provider} session on disk yet");
            return None;
        }
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "failed to read {provider} session");
            return None;
        }
    };
    serde_json::from_str(&contents)
        .map_err(|err| tracing::warn!(%err, "failed to decode {provider} session"))
        .ok()
}

pub fn save<T: serde::Serialize + ?Sized>(value: &T, path: &Path, provider: &str) {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(%err, "failed to create {provider} session dir");
        return;
    }
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(%err, "failed to serialize {provider} session");
            return;
        }
    };
    // Write to a temporary file and rename it into place (same pattern as
    // `playback/session_storage.rs`): an in-place truncate+rewrite can
    // leave a truncated session file behind when the process dies mid-write.
    // `fs::rename` replaces an existing target on Windows, so the swap is
    // atomic on every supported platform.
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = std::fs::write(&tmp, json) {
        tracing::warn!(%err, "failed to write {provider} session temp file");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        tracing::warn!(%err, "failed to persist {provider} session");
        let _ = std::fs::remove_file(&tmp);
    }
}
