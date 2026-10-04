//! Shared IO layer for the online-source track downloads (KuGou / NetEase).
//!
//! F046 fixed both download paths independently and left byte-identical
//! copies of this machinery in `ui::kugou::download` and
//! `ui::netease::download`; this module is the single copy both call now, so
//! the behavior can no longer drift. What is genuinely provider-specific
//! stays in the callers: URL quality ladders, lyrics sidecar formats, and
//! KuGou's UA header (passed through `extra_headers`).

use std::{path::Path, time::Duration};

use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::ItemKey,
    tag::{Accessor, ItemValue, Tag, TagExt, TagItem, TagType},
};
use tokio::io::AsyncWriteExt;

use super::http_source::shared_http_client;

/// Windows 保留设备名：文件名首个点号前的 stem 命中（不区分大小写）即被
/// Win32 当作设备引用，即使带扩展名（如 "CON.flac"）。
const WINDOWS_RESERVED_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Strips characters that are illegal in Windows file names, and prefixes an
/// underscore to reserved device-name stems (CON, COM1-9, ...), which Win32
/// resolves to a device even when a file extension follows.
pub fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        return "track".into();
    }
    let stem = trimmed.split('.').next().unwrap_or_default();
    if WINDOWS_RESERVED_DEVICES
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
    {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Upper bound on waiting for any single network event (response headers,
/// one body chunk). 请求级不设总死线：整曲 FLAC 几十至上百 MB，慢速链路下
/// 固定总超时必然误杀正常下载，改为按单次网络事件限时、停滞即报错。
pub const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Streams `url` straight into `dest` through a fixed ~64KB write buffer:
/// the whole track is never held in memory. Returns the accumulated byte
/// count. Any failure — including fewer than 1KB downloaded — removes the
/// partial file first, so an `Err` never leaves debris on disk.
///
/// `extra_headers` carries provider-specific headers on top of the shared
/// client's defaults (KuGou's CDN 403s the default reqwest UA and must pass
/// its Android UA here).
///
/// 走 `chunk()` 而非 `bytes_stream()`：后者被 reqwest 的 `stream` feature
/// 门控（本仓未启用），`chunk()` 无 feature 门，同为逐块拉取。
pub async fn http_download_to_file(
    url: &str,
    dest: &Path,
    extra_headers: &[(&'static str, String)],
) -> Result<u64, String> {
    let mut request = shared_http_client().get(url);
    for (name, value) in extra_headers {
        request = request.header(*name, value.as_str());
    }
    let mut response = tokio::time::timeout(STREAM_IO_TIMEOUT, request.send())
        .await
        .map_err(|_| format!("network stalled: no response for {STREAM_IO_TIMEOUT:?}"))?
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let mut total = 0u64;
    let result = async {
        let mut file = tokio::io::BufWriter::with_capacity(
            // 固定 ~64KB：落盘系统调用粒度与网络块到达节奏解耦。
            64 * 1024,
            tokio::fs::File::create(dest)
                .await
                .map_err(|err| format!("write audio: {err}"))?,
        );
        loop {
            let chunk = tokio::time::timeout(STREAM_IO_TIMEOUT, response.chunk())
                .await
                .map_err(|_| format!("network stalled: no data for {STREAM_IO_TIMEOUT:?}"))?
                .map_err(|err| err.to_string())?;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk)
                .await
                .map_err(|err| format!("write audio: {err}"))?;
            total += chunk.len() as u64;
        }
        // BufWriter 被 drop 不会自动冲刷，成功路径必须显式冲刷并关闭。
        file.flush()
            .await
            .map_err(|err| format!("write audio: {err}"))?;
        file.shutdown()
            .await
            .map_err(|err| format!("write audio: {err}"))?;
        Ok::<(), String>(())
    }
    .await;

    if let Err(err) = result {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(err);
    }
    if total < 1024 {
        let _ = tokio::fs::remove_file(dest).await;
        return Err("downloaded file is empty".into());
    }
    Ok(total)
}

/// Writes title/artist/album, lyrics and the cover into the downloaded file.
/// Callers destructure the fields out of their own track type (this module is
/// agnostic of the provider structs).
pub fn embed_tags(
    path: &Path,
    title: &str,
    artist: &str,
    album: &str,
    cover: Option<&[u8]>,
    lrc: Option<&str>,
) -> Result<(), String> {
    let tag_type = match path.extension().and_then(|ext| ext.to_str()) {
        Some("flac") => TagType::VorbisComments,
        _ => TagType::Id3v2,
    };
    let mut tag = Tag::new(tag_type);
    if !title.is_empty() {
        tag.set_title(title.to_string());
    }
    if !artist.is_empty() {
        tag.set_artist(artist.to_string());
    }
    if !album.is_empty() {
        tag.set_album(album.to_string());
    }
    if let Some(lrc) = lrc {
        tag.insert(TagItem::new(
            ItemKey::UnsyncLyrics,
            ItemValue::Text(lrc.to_string()),
        ));
    }
    if let Some(bytes) = cover {
        // JPEG magic bytes; anything else is assumed to be PNG.
        let mime = if bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            MimeType::Jpeg
        } else {
            MimeType::Png
        };
        // lofty 的 Picture::unchecked 只收 owned Vec（内部 Cow::Owned），
        // 这一次拷贝是封面全量在下载链路上的唯一一份。
        let picture = Picture::unchecked(bytes.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build();
        tag.push_picture(picture);
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_device_stems_get_underscore_prefix() {
        for name in ["CON", "PRN", "AUX", "NUL"] {
            assert_eq!(sanitize_filename(name), format!("_{name}"));
        }
        for n in 1..=9u8 {
            for prefix in ["COM", "LPT"] {
                let name = format!("{prefix}{n}");
                assert_eq!(sanitize_filename(&name), format!("_{name}"));
            }
        }
    }

    #[test]
    fn reserved_device_stems_match_case_insensitively() {
        assert_eq!(sanitize_filename("con"), "_con");
        assert_eq!(sanitize_filename("Nul"), "_Nul");
        assert_eq!(sanitize_filename("cOm5"), "_cOm5");
    }

    #[test]
    fn reserved_stem_with_extension() {
        assert_eq!(sanitize_filename("CON.flac"), "_CON.flac");
        assert_eq!(sanitize_filename("lpt3.mp3"), "_lpt3.mp3");
    }

    #[test]
    fn legal_names_pass_through_unchanged() {
        assert_eq!(sanitize_filename("Congo - Live"), "Congo - Live");
        assert_eq!(sanitize_filename("com10"), "com10");
        assert_eq!(sanitize_filename("my.CON"), "my.CON");
        assert_eq!(sanitize_filename("a<b:c"), "a_b_c");
        assert_eq!(sanitize_filename(""), "track");
    }
}
