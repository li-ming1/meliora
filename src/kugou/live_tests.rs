//! 打真实酷狗服务器的联机测试。两个都标了 `#[ignore]`：手动运行用
//! `cargo test --release --features kugou live_ -- --ignored --nocapture`。
//!
//! `live_offline_flow` 全自动（设备注册、搜索、播放 URL、歌词）。`live_qr_login` 需要人
//! 用酷狗移动端 App 扫一次二维码，然后列出已登录用户的歌单。

use super::KugouClient;
use super::api::QrStatus;
use crate::paths;

fn client() -> KugouClient {
    KugouClient::new(&paths::data_dir())
}

fn first_search_result(body: &serde_json::Value) -> Option<serde_json::Value> {
    body.pointer("/data/lists")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
}

/// 搜索 API 已知会用到的 hash 字段里取第一个（历史遗留的 `FileHash`，然后是 `hash`）。
fn song_hash(song: &serde_json::Value) -> Option<&str> {
    song.get("FileHash")
        .or_else(|| song.get("hash"))
        .and_then(|v| v.as_str())
}

/// `song_url` 要的 (album_audio_id, album_id)，搜索结果缺字段时各自默认为 0。
fn song_ids(song: &serde_json::Value) -> (i64, i64) {
    let album_audio_id = song
        .get("MixSongID")
        .or_else(|| song.get("SongID"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let album_id = song
        .get("AlbumID")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    (album_audio_id, album_id)
}

#[test]
#[ignore = "live network call against KuGou servers"]
fn live_offline_flow() {
    let client = client();
    let report = crate::RUNTIME.block_on(async {
        let mut report = String::new();

        // 设备注册在客户端会话内部隐式完成
        report.push_str(&format!("dfid: {}\n", client.credentials().dfid));

        let search = client.search("周杰伦", 1, 5).await;
        let song = match search {
            Ok(resp) => {
                let song = first_search_result(&resp.body);
                report.push_str(&format!(
                    "search: ok, {} results\n",
                    resp.body
                        .pointer("/data/lists")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0)
                ));
                song
            }
            Err(e) => {
                report.push_str(&format!("search: FAILED {e}\n"));
                None
            }
        };

        if let Some(song) = &song {
            report.push_str(&format!(
                "first song: {}\n",
                serde_json::to_string_pretty(song).unwrap_or_default()
            ));
            let hash = song_hash(song).unwrap_or("");
            let (album_audio_id, album_id) = song_ids(song);

            match client
                .song_url(hash, album_audio_id, album_id, "128", true)
                .await
            {
                Ok(resp) => report.push_str(&format!(
                    "song_url: ok\n{}\n",
                    serde_json::to_string_pretty(&resp.body).unwrap_or_default()
                )),
                Err(e) => report.push_str(&format!("song_url: FAILED {e}\n")),
            }

            match client.search_lyric(hash, "", 0).await {
                Ok(resp) => {
                    report.push_str(&format!(
                        "search_lyric: ok | {}\n",
                        serde_json::to_string(&resp.body)
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    ));
                    let id = resp
                        .body
                        .pointer("/candidates/0/id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let accesskey = resp
                        .body
                        .pointer("/candidates/0/accesskey")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    if let (Some(id), Some(accesskey)) = (id, accesskey) {
                        match client.lyric_lrc(&id, &accesskey).await {
                            Ok(lrc) => report.push_str(&format!(
                                "lyric_lrc: ok ({} chars)\n{}\n",
                                lrc.len(),
                                lrc.chars().take(200).collect::<String>()
                            )),
                            Err(e) => report.push_str(&format!("lyric_lrc: FAILED {e}\n")),
                        }
                    } else {
                        report.push_str("lyric_lrc: no candidates\n");
                    }
                }
                Err(e) => report.push_str(&format!("search_lyric: FAILED {e}\n")),
            }
        }

        report
    });
    println!("{report}");
}

/// 端到端播放路径测试：搜索 -> 播放 URL -> 经 HTTP 媒体源打开流 -> 启动 symphonia 解码
/// -> 读元数据。
#[test]
#[ignore = "live network call against KuGou servers"]
fn live_http_source_decodes_kugou_stream() {
    let client = client();
    // 1. 在运行时里解析出一个播放 URL
    let (url, _time_length) = crate::RUNTIME.block_on(async {
        let resp = client.search("晴天 周杰伦", 1, 1).await.expect("search");
        let song = first_search_result(&resp.body).expect("search result");
        let hash = song_hash(&song).expect("hash");
        let (album_audio_id, album_id) = song_ids(&song);

        let url_resp = client
            .song_url(hash, album_audio_id, album_id, "128", true)
            .await
            .expect("song url");
        let url = url_resp
            .body
            .get("url")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .expect("url field")
            .to_string();
        let time_length = url_resp.body.get("timeLength").and_then(|v| v.as_i64());
        println!(
            "stream url ok ({} chars), timeLength={time_length:?}",
            url.len()
        );
        (url, time_length)
    });

    // 2. 在运行时之外打开流，和播放线程的做法一致。
    // （open_http_media 内部会调 Runtime::block_on，所以另一个 block_on 正在进行时不能跑它。）
    let path = std::path::PathBuf::from(&url);
    let mut stream =
        crate::media::http_source::open_http_media(&path, None).expect("open http stream");
    stream.start_playback().expect("start playback");
    let duration = stream.duration_ms().expect("duration");
    let metadata = stream.read_metadata().ok();
    println!(
        "decode ok: duration {}ms, title={:?}",
        duration,
        metadata
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or_default()
    );
    assert!(
        duration > 30_000,
        "expected a real stream, got {duration}ms"
    );
}

#[test]
#[ignore = "requires scanning the QR code with the KuGou mobile app"]
fn live_qr_login() {
    let client = client();
    crate::RUNTIME.block_on(async {
        let key = client.qr_create_key().await.expect("qr key request");
        let url = super::api::qr_login_url(&key);
        let qr_path = paths::data_dir().join("kugou_qr.png");
        let code = qrcode::QrCode::new(url.as_bytes()).expect("qr encode");
        let image = code
            .render::<image::Rgba<u8>>()
            .quiet_zone(true)
            .min_dimensions(360, 360)
            .build();
        image.save(&qr_path).expect("save qr png");
        println!("=== 用手机酷狗 App 扫码（5 分钟内） ===");
        println!("QR-PNG: {}", qr_path.display());
        println!("QR-URL: {url}");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            match client.qr_check(&key).await {
                Ok(QrStatus::Waiting) => {}
                Ok(QrStatus::Scanned) => println!("已扫码，请在手机上点确认"),
                Ok(QrStatus::Success { token, userid }) => {
                    client.store_login(token, userid);
                    println!("=== 登录成功，userid={userid} ===");

                    match client.user_detail().await {
                        Ok(resp) => println!(
                            "user_detail: {}",
                            serde_json::to_string_pretty(&resp.body).unwrap_or_default()
                        ),
                        Err(e) => println!("user_detail: FAILED {e}"),
                    }

                    match client.user_playlists(1, 30).await {
                        Ok(resp) => {
                            println!(
                                "user_playlists: {}",
                                serde_json::to_string_pretty(&resp.body).unwrap_or_default()
                            );
                            let first_listid = resp
                                .body
                                .pointer("/data/info/0/listid")
                                .and_then(|v| v.as_i64());
                            if let Some(listid) = first_listid {
                                match client.playlist_tracks(listid, 1, 10).await {
                                    Ok(resp) => println!(
                                        "playlist_tracks: {}",
                                        serde_json::to_string_pretty(&resp.body)
                                            .unwrap_or_default()
                                    ),
                                    Err(e) => println!("playlist_tracks: FAILED {e}"),
                                }
                            }
                        }
                        Err(e) => println!("user_playlists: FAILED {e}"),
                    }
                    return;
                }
                Ok(QrStatus::Expired) => panic!("二维码已过期，重跑测试换新的"),
                Err(e) => println!("poll error: {e}"),
            }
            if tokio::time::Instant::now() > deadline {
                panic!("5 分钟内未完成扫码");
            }
        }
    });
}

/// 榜单列表 + 榜单歌曲 + 每日推荐端点。打印原始结构（各一次），好让 API 漂移时能调整
/// `ui::kugou` 里的解析器。
#[test]
#[ignore = "live network call against KuGou servers"]
fn live_rank_and_recommend() {
    let client = client();
    let report = crate::RUNTIME.block_on(async {
        let mut report = String::new();

        match client.rank_list().await {
            Ok(resp) => {
                report.push_str(&format!(
                    "rank_list: ok\n{}\n",
                    serde_json::to_string(&resp.body)
                        .unwrap_or_default()
                        .chars()
                        .take(800)
                        .collect::<String>()
                ));
                let first_rankid = resp
                    .body
                    .pointer("/data/info/0/rankid")
                    .and_then(|v| v.as_i64());
                report.push_str(&format!("first rankid: {first_rankid:?}\n"));

                if let Some(rankid) = first_rankid {
                    match client.rank_audio(rankid, 1, 3).await {
                        Ok(audio) => report.push_str(&format!(
                            "rank_audio: ok, total={}\n",
                            audio
                                .body
                                .pointer("/data/total")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0)
                        )),
                        Err(e) => report.push_str(&format!("rank_audio: FAILED {e}\n")),
                    }
                    let tracks = client
                        .rank_audio(rankid, 1, 3)
                        .await
                        .ok()
                        .and_then(|r| r.body.pointer("/data/songlist/0").cloned());
                    if let Some(song) = tracks {
                        report.push_str(&format!(
                            "rank song fields: songname={} dur={} hash={} top_time_length={} album_info={}\n",
                            song.get("songname")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            song.pointer("/deprecated/duration")
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                            song.pointer("/deprecated/hash")
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                            song.get("time_length")
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                            serde_json::to_string(song.pointer("/album_info").unwrap_or(&song))
                                .unwrap_or_default(),
                        ));
                    }
                }
            }
            Err(e) => report.push_str(&format!("rank_list: FAILED {e}\n")),
        }

        match client.everyday_recommend().await {
            Ok(resp) => {
                let song = resp.body.pointer("/data/song_list/0").cloned();
                if let Some(song) = song {
                    report.push_str(&format!(
                        "recommend song fields: songname={} time_length={} hash={} cover={} album={}\n",
                        song.get("songname").and_then(|v| v.as_str()).unwrap_or(""),
                        song.get("time_length").map(|v| v.to_string()).unwrap_or_default(),
                        song.get("hash").map(|v| v.to_string()).unwrap_or_default(),
                        song.get("sizable_cover").map(|v| v.to_string()).unwrap_or_default(),
                        song.get("album_name").and_then(|v| v.as_str()).unwrap_or(""),
                    ));
                } else {
                    report.push_str("everyday_recommend: ok, no first song\n");
                }
            }
            Err(e) => report.push_str(&format!("everyday_recommend: FAILED {e}\n")),
        }

        report
    });
    println!("{report}");
}
