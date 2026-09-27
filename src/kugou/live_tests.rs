//! Live tests against the real KuGou servers. Both are `#[ignore]`d: run
//! them manually with
//! `cargo test --release --features kugou live_ -- --ignored --nocapture`.
//!
//! `live_offline_flow` is fully automatic (device registration, search,
//! play URL, lyric). `live_qr_login` needs a human to scan a QR code with
//! the KuGou mobile app, then lists the logged-in user's playlists.

use super::KugouClient;
use super::api::QrStatus;
use super::client::LoginExtras;
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

/// First of the hash fields the search API is known to use (legacy
/// `FileHash`, then `hash`).
fn song_hash(song: &serde_json::Value) -> Option<&str> {
    song.get("FileHash")
        .or_else(|| song.get("hash"))
        .and_then(|v| v.as_str())
}

/// (album_audio_id, album_id) for `song_url`, each defaulting to 0 when the
/// search result lacks the field.
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

        // device registration happens implicitly inside the client session
        report.push_str(&format!("dfid: {}\n", client.session_snapshot().dfid));

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

            {
                match client.search_lyric(hash, "", 0).await {
                    Ok(resp) => report.push_str(&format!(
                        "search_lyric body: {}\n",
                        serde_json::to_string(&resp.body)
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    )),
                    Err(e) => report.push_str(&format!("search_lyric: FAILED {e}\n")),
                }
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
                                &lrc.chars().take(200).collect::<String>()
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

/// End-to-end playback path test: search -> play URL -> open the stream
/// through the HTTP media source -> start symphonia decode -> read metadata.
#[test]
#[ignore = "live network call against KuGou servers"]
fn live_http_source_decodes_kugou_stream() {
    let client = client();
    // 1. resolve a play URL inside the runtime
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

    // 2. open the stream OUTSIDE the runtime, like the playback thread does.
    // (open_http_media calls Runtime::block_on internally, so it must not run
    // while another block_on is in progress.)
    let path = std::path::PathBuf::from(&url);
    let mut stream = crate::media::http_source::open_http_media(&path).expect("open http stream");
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
                    client.store_login(token, userid, LoginExtras::default());
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

/// Rank list + rank songs + daily recommend endpoints. Prints the raw shapes
/// (once) so the parsers in `ui::kugou` can be adjusted if the API drifts.
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
