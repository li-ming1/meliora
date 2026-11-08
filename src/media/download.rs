//! 在线音源（酷狗）曲目下载共用的 IO 层。
//!
//! F046 各自独立实现了下载路径，在 `ui::kugou::download` 留下逐字节相同的
//! 副本；本模块是它现在唯一调用的那份，因此
//! 行为不会再漂移。真正 provider 专属的部分留在调用方：URL 质量梯队、
//! 歌词 sidecar 格式，以及酷狗的 UA 头（经 `extra_headers` 传入）。

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
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

/// 半成品路径计数器：`.part` 文件名的进程内唯一后缀。唯一性靠计数不靠时序，
/// Relaxed 足够（不同调用拿到不同的 n 就是全部所需，不存在先后关系）。
static PART_SEQ: AtomicU64 = AtomicU64::new(0);

/// 半成品路径：与目标同目录的 `.{n}.part` 后缀，保证 rename 在同一卷内完成，
/// 且进程内**每次调用唯一**。
///
/// 唯一性是正确性而非风格：两路并发下载可能撞上同一个 dest（同名曲目的
/// artist+title 逐字相同、hash 不同，调用方按 hash 的去重挡不住），共享的
/// 确定性 `.part` 会被第二句柄成功 truncate（Rust 默认 share mode 允许并发
/// 写），交错写 + 先完成者 rename 后旧句柄仍持有被改名文件继续写，直接污染
/// dest；损坏曲目经合法扩展名被扫描器静默入库。唯一临时路径从结构上排除
/// 该冲突。
fn partial_path(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_owned();
    path.push(format!(
        ".{}.part",
        PART_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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

    #[test]
    fn partial_path_is_unique_per_call_for_same_dest() {
        let dir = crate::test_support::TestDir::new("download-part-unique");
        let dest = dir.path().join("song.mp3");
        let a = partial_path(&dest);
        let b = partial_path(&dest);
        assert_ne!(a, b, "同一 dest 连续两次取半成品路径必须不同");
        assert!(a.to_string_lossy().ends_with(".part"));
        assert!(b.to_string_lossy().ends_with(".part"));
        // 与 dest 同目录：rename 的同卷约束保持
        assert_eq!(a.parent(), dest.parent());
        assert_eq!(b.parent(), dest.parent());
    }

    /// 并发下载撞同一 dest 的回归测试：临时路径唯一化后两路各写各的 `.part`，
    /// rename 后 dest 恰为两份载荷之一（字节级完整），目录里不残留半成品。
    /// 修复前共享的确定性 `.part` 会让两路交错写、先完成者 rename 后旧句柄
    /// 继续写污染 dest——本测试在该形态下会读到两份载荷的混合字节。
    #[test]
    fn concurrent_downloads_to_same_dest_stay_intact() {
        use std::sync::mpsc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = crate::test_support::TestDir::new("download-part-concurrent");
        let dest = dir.path().join("artist - title.mp3");
        // 等长但逐字节不同的两份载荷：任何交错都会产出混合字节，断言必失败
        let body_a = vec![0xAAu8; 4096];
        let body_b = vec![0xBBu8; 4096];

        let (port_tx, port_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        // 任务侧副本：dest 与两份载荷要被 async move 搬走，测试线程随后还要
        // 用它们做断言，故克隆出任务专用的独立所有权。
        let dest_task = dest.clone();
        let body_a_task = body_a.clone();
        let body_b_task = body_b.clone();
        crate::RUNTIME.spawn(async move {
            let dest = dest_task;
            let body_a = body_a_task;
            let body_b = body_b_task;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            port_tx.send(port).unwrap();

            // 服务端臂：顺序 accept 两条连接（先到先得），各自回一份完整载荷；
            // 每条响应 Connection: close，两个客户端各开各的连接。
            let server = async {
                for body in [&body_a, &body_b] {
                    let (mut sock, _) = listener.accept().await.unwrap();
                    // 读到请求头结束再回（不解析内容），防半开时序
                    let mut buf = Vec::new();
                    let mut piece = [0u8; 1024];
                    loop {
                        let n = sock.read(&mut piece).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&piece[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    sock.write_all(head.as_bytes()).await.unwrap();
                    sock.write_all(&body[..]).await.unwrap();
                    sock.shutdown().await.unwrap();
                }
            };

            let url = format!("http://127.0.0.1:{port}/audio.mp3");
            let (ra, rb, ()) = tokio::join!(
                http_download_to_file(&url, &dest, &[]),
                http_download_to_file(&url, &dest, &[]),
                server,
            );
            done_tx.send((ra, rb)).unwrap();
        });

        // 端口先回来 = 服务端已 bind；随后等两路下载的整体结果
        let _port = port_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("test http server did not bind");
        let (ra, rb) = done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("concurrent downloads did not finish");
        assert!(ra.is_ok(), "下载 A 失败: {ra:?}");
        assert!(rb.is_ok(), "下载 B 失败: {rb:?}");

        // dest 恰为两份载荷之一（字节级完整，无交错）
        let final_bytes = std::fs::read(&dest).unwrap();
        assert!(
            final_bytes == body_a || final_bytes == body_b,
            "dest 内容既不是载荷 A 也不是载荷 B（疑似交错损坏）"
        );
        // 无残留半成品
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "残留半成品: {leftovers:?}");
    }
}

#[test]
fn embed_tags_writes_all_fields_and_round_trips() {
    // 十五轮测试缺口矩阵中危档：embed_tags 是下载链路的最后一跳（注释里
    // 的两个事故形态——截断已下载文件、半成品入库——都在它上游的
    // http_download_to_file，本测试钉住「写进去的标签能原样读回来」）。
    use crate::test_support::audio_fixtures;
    use lofty::tag::TagType;

    let dir = crate::test_support::TestDir::new("download-embed-tags");
    for (fixture, ext, tag_type) in [
        ("fixture.mp3", "mp3", TagType::Id3v2),
        ("fixture.flac", "flac", TagType::VorbisComments),
    ] {
        let path = dir.join(&format!("embed.{ext}"));
        std::fs::write(&path, audio_fixtures::fixture(fixture)).unwrap();
        let cover = audio_fixtures::cover_jpeg();
        let lrc = "[00:01.00] 测试歌词行";

        embed_tags(
            path.as_path(),
            "嵌入标题",
            "嵌入歌手",
            "嵌入专辑",
            Some(&cover),
            Some(lrc),
        )
        .unwrap_or_else(|e| panic!("{fixture}: embed failed: {e}"));

        let tagged = lofty::read_from_path(&path).unwrap_or_else(|e| panic!("{fixture}: {e}"));
        use lofty::file::TaggedFileExt;
        let tag = tagged
            .tag(tag_type)
            .unwrap_or_else(|| panic!("{fixture}: no tag"));
        assert_eq!(tag.get_string(ItemKey::TrackTitle), Some("嵌入标题"));
        assert_eq!(tag.get_string(ItemKey::TrackArtist), Some("嵌入歌手"));
        assert_eq!(tag.get_string(ItemKey::AlbumTitle), Some("嵌入专辑"));
        assert_eq!(
            tag.get_string(ItemKey::UnsyncLyrics),
            Some(lrc),
            "{fixture} 歌词"
        );
        let pictures = tag.pictures();
        assert_eq!(pictures.len(), 1, "{fixture} 封面");
        assert_eq!(
            pictures[0].mime_type(),
            Some(&lofty::picture::MimeType::Jpeg)
        );
        // 每个容器一个 tag：保存是整体替换而非追加
        assert_eq!(tagged.tags().len(), 1, "{fixture}");
        assert_eq!(tag.tag_type(), tag_type);
    }
}
