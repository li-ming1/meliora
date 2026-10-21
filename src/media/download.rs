//! 在线音源（酷狗）曲目下载共用的 IO 层。
//!
//! F046 各自独立实现了下载路径，在 `ui::kugou::download` 留下逐字节相同的
//! 副本；本模块是它现在唯一调用的那份，因此
//! 行为不会再漂移。真正 provider 专属的部分留在调用方：URL 质量梯队、
//! 歌词 sidecar 格式，以及酷狗的 UA 头（经 `extra_headers` 传入）。

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

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

/// 剥掉 Windows 文件名中的非法字符，并给保留设备名的 stem（CON、COM1-9 …）
/// 加下划线前缀——即使后面跟着扩展名，Win32 仍会把它们解析成设备。
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

/// 等待任意单个网络事件（响应头、一个 body 块）的上限。
/// 请求级不设总死线：整曲 FLAC 几十至上百 MB，慢速链路下
/// 固定总超时必然误杀正常下载，改为按单次网络事件限时、停滞即报错。
pub const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 经固定 ~64KB 写缓冲把 `url` 直接灌入 `dest`：全程不把整曲留在内存里。
/// 返回累计字节数。任何失败——包括下载不足 1KB——都会先删掉半成品文件，
/// 所以 `Err` 绝不会在磁盘上留下残骸。
///
/// `extra_headers` 在共用 client 的默认头之上追加 provider 专属头
/// （酷狗 CDN 对 reqwest 默认 UA 返 403，必须在这里传它的 Android UA）。
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

    // 先写同目录的 `.part`，成功后再 rename 就位：直接写目标会先截断已有文件，
    // 网络失败时删除它等于把用户上次下载好的曲目也删掉；进程中途死掉则会留下
    // 半截音频，下一次扫描会把它当成正常曲目入库。
    let tmp = partial_path(dest);
    let mut total = 0u64;
    let result = async {
        let mut file = tokio::io::BufWriter::with_capacity(
            // 固定 ~64KB：落盘系统调用粒度与网络块到达节奏解耦。
            64 * 1024,
            tokio::fs::File::create(&tmp)
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
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    if total < 1024 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err("downloaded file is empty".into());
    }
    if let Err(err) = tokio::fs::rename(&tmp, dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("finalize download: {err}"));
    }
    Ok(total)
}

/// 半成品路径：与目标同目录的 `.part` 后缀，保证 rename 在同一卷内完成。
fn partial_path(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_owned();
    path.push(".part");
    PathBuf::from(path)
}

/// 把 标题/歌手/专辑、歌词与封面写入已下载的文件。
/// 调用方自行从各自的曲目类型里拆出字段（本模块不认识 provider 的结构体）。
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
        // JPEG 魔数；其余一律按 PNG 处理。
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
