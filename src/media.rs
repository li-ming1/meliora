pub mod errors;
// 在线音源（酷狗）共用的下载 IO 层。
#[cfg(feature = "online_sources")]
pub mod download;
// 在线音源（酷狗等）使用的 HTTP(S) range 请求媒体源。
#[cfg(feature = "online_sources")]
pub mod http_source;
// 占位形态：播放层的非门控签名（OnlineRefresh / 刷新钩子 / open_stream 参数）
// 引用 MediaUrlRefresh 类型名，online_sources 缺席时模块无真实实现，只需
// 保持类型名与形态（真身见门控侧 http_source.rs:79）。2026-10-06 台账挂账，
// 2026-10-25 清偿——默认 feature 集从"编不过"恢复可编译。
#[cfg(not(feature = "online_sources"))]
pub mod http_source {
    use std::{path::PathBuf, sync::Arc};
    pub type MediaUrlRefresh = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;
}
pub mod lofty;
pub mod lookup_table;
pub mod metadata;
pub mod pipeline;
// 远程音源与解码器之间的后台预取。
#[cfg(feature = "online_sources")]
pub mod prefetch;
// 在线音源透明本地磁盘缓存。
#[cfg(feature = "online_sources")]
pub mod audio_cache;
pub mod symphonia;
pub mod text;
pub mod traits;

use std::{borrow::Cow, path::Path};

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

/// 日志用的媒体路径脱敏：在线流 URL 的路径段本身就是取流能力（拿到即可下载，
/// 直至过期），整条写进轮转日志等于把凭据放进用户可能分享的文件——只保留
/// `scheme://host/….ext`。本地路径原样返回：那是用户自己的文件，排查时恰恰
/// 需要全貌。与 reqwest 错误的 `without_url()`（token 在 query 里）同一卫生
/// 口径。
///
/// 不用于封面 URL：封面路径的尺寸档位（`/stdmusic/4096/`）是"背景还是糊"类
/// 投诉的判读依据（AGENTS.md 沉浸页背景图内存契约），且不构成取流能力。
pub fn redact_media_path(path: &str) -> Cow<'_, str> {
    // 前缀判定必须按字节比较：str 数字切片（path[..7]）在非 char boundary
    // 处 panic——本地路径如 D:\音乐\x.mp3 的字节 7 落在多字节字符中间，而
    // release panic=abort 下任何线程 panic 都是全进程终止。与上方
    // is_http_path 字节比较同口径；命中分支的 prefix_len 落在 ASCII 边界，
    // 后续 &path[prefix_len..] 切片安全。
    let bytes = path.as_bytes();
    let prefix_len = if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        7
    } else if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        8
    } else {
        return Cow::Borrowed(path);
    };
    let rest = &path[prefix_len..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..host_end];
    let last_segment = rest[host_end..].rsplit('/').next().unwrap_or_default();
    let stem = last_segment.split(['?', '#']).next().unwrap_or_default();
    let ext = stem.rsplit('.').find(|_| stem.contains('.')).filter(|ext| {
        !ext.is_empty() && ext.len() <= 5 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    });
    let scheme = &path[..prefix_len];
    match ext {
        Some(ext) => Cow::Owned(format!("{scheme}{host}/….{ext}")),
        None => Cow::Owned(format!("{scheme}{host}/…")),
    }
}

#[cfg(test)]
mod tests {
    use super::redact_media_path;

    #[test]
    fn local_paths_pass_through_unchanged() {
        assert_eq!(
            redact_media_path(r"D:\music\a b.flac"),
            r"D:\music\a b.flac"
        );
        assert_eq!(redact_media_path("/music/sub/a.flac"), "/music/sub/a.flac");
        // 多字节本地路径：字节 7 落在『乐』（字节 6-8）中间，旧代码的 str
        // 数字切片在此 panic；字节比较原样放行
        assert_eq!(redact_media_path(r"D:\音乐\x.mp3"), r"D:\音乐\x.mp3");
        assert_eq!(redact_media_path(""), "");
    }

    #[test]
    fn http_urls_keep_only_host_and_extension() {
        // 路径段（日期目录、两级哈希、文件名里的 id 串）全部是取流能力的一部分
        let redacted = redact_media_path(
            "http://fs.youthandroid2.kugou.com/202610212019/9483acfe5242699b1431979c16589b2d/v3/e3b2a274db3fdde5644b9f2c4c1dd70f/yp/full/ap3116_us840978440_df6ooy58w0ayivrdm0ciwe7r9l_pi411_mx722192798_quflac_pa967177915_ct410100_s2016918262.flac",
        );
        assert_eq!(redacted, "http://fs.youthandroid2.kugou.com/….flac");
    }

    #[test]
    fn https_query_string_and_extensionless_segments() {
        assert_eq!(
            redact_media_path("https://example.com/a/b?token=secret"),
            "https://example.com/…"
        );
        assert_eq!(redact_media_path("http://host/noext/path"), "http://host/…");
        assert_eq!(redact_media_path("http://host"), "http://host/…");
        // 尾段无点号（或"点号在中间段"）时不得把整段当扩展名
        assert_eq!(
            redact_media_path("http://host/a/verylongsegment.mp3/file"),
            "http://host/…"
        );
    }
}
