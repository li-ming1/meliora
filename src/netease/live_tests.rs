//! Live tests against the real NetEase servers, all `#[ignore]`d: run them
//! manually with
//! `cargo test --release --features netease live_ -- --ignored --nocapture`.
//!
//! `live_offline_flow` is fully automatic (anonymous registration, search,
//! play URL, lyric). `live_qr_login` needs a human to scan a QR code with
//! the NetEase Cloud Music mobile app, then prints the account profile and
//! playlists.

use super::api::QrStatus;
use super::NeteaseClient;
use crate::paths;

fn client() -> NeteaseClient {
    NeteaseClient::new(&paths::data_dir())
}

fn first_search_result(body: &serde_json::Value) -> Option<serde_json::Value> {
    body.pointer("/result/songs")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
}

#[test]
#[ignore = "live network call against NetEase servers"]
fn live_offline_flow() {
    let client = client();
    let report = crate::RUNTIME.block_on(async {
        let mut report = String::new();

        let search = client.cloudsearch("晴天", 1, 5, 0).await;
        let song = match search {
            Ok(resp) => {
                let song = first_search_result(&resp.body);
                report.push_str(&format!(
                    "cloudsearch: ok, {} results\n",
                    resp.body
                        .pointer("/result/songCount")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                ));
                song
            }
            Err(e) => {
                report.push_str(&format!("cloudsearch: FAILED {e}\n"));
                None
            }
        };

        let song_id = song.as_ref().and_then(|song| song.get("id").and_then(|v| v.as_i64()));
        if let Some(song) = &song {
            report.push_str(&format!(
                "first song: {}\n",
                serde_json::to_string_pretty(song).unwrap_or_default()
            ));
        }
        if let Some(song_id) = song_id {
            match client.song_url(song_id, "standard").await {
                Ok(resp) => report.push_str(&format!(
                    "song_url: ok\n{}\n",
                    serde_json::to_string_pretty(&resp.body).unwrap_or_default()
                )),
                Err(e) => report.push_str(&format!("song_url: FAILED {e}\n")),
            }

            match client.lyric_new(song_id).await {
                Ok(resp) => {
                    let keys = ["lrc", "tlyric", "yrc", "klyric"];
                    for key in keys {
                        let present = resp
                            .body
                            .get(key)
                            .and_then(|v| v.get("lyric"))
                            .and_then(|v| v.as_str())
                            .map(str::len)
                            .unwrap_or(0);
                        report.push_str(&format!("lyric[{key}]: {present} chars\n"));
                    }
                    report.push_str(&format!(
                        "lyric sample: {}\n",
                        resp.body
                            .pointer("/lrc/lyric")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect::<String>()
                    ));
                }
                Err(e) => report.push_str(&format!("lyric_new: FAILED {e}\n")),
            }
        }

        match client.toplist().await {
            Ok(resp) => {
                report.push_str(&format!(
                    "toplist first item keys: {:?}\n",
                    resp.body
                        .pointer("/list/0")
                        .and_then(|v| v.as_object())
                        .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
                ));
                report.push_str(&format!(
                    "toplist first cover field: {:?}\n",
                    resp.body.pointer("/list/0/coverImgUrl"),
                ));
                report.push_str(&format!(
                    "toplist: ok, {} charts\n",
                    resp.body
                        .pointer("/list")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0)
                ));
            }
            Err(e) => report.push_str(&format!("toplist: FAILED {e}\n")),
        }

        report
    });
    println!("{report}");
}

/// End-to-end playback path test: search -> play URL -> open the stream
/// through the HTTP media source -> start symphonia decode -> read metadata.
#[test]
#[ignore = "live network call against NetEase servers"]
fn live_http_source_decodes_netease_stream() {
    let client = client();
    // 1. resolve a play URL inside the runtime
    let url = crate::RUNTIME.block_on(async {
        let resp = client.cloudsearch("晴天", 1, 1, 0).await.expect("search");
        let song = first_search_result(&resp.body).expect("search result");
        let song_id = song.get("id").and_then(|v| v.as_i64()).expect("song id");
        let url_resp = client.song_url(song_id, "standard").await.expect("song url");
        url_resp
            .body
            .pointer("/data/0/url")
            .and_then(|v| v.as_str())
            .expect("url field")
            .to_string()
    });
    println!("stream url ok ({} chars)", url.len());

    // 2. open the stream OUTSIDE the runtime, like the playback thread does.
    let path = std::path::PathBuf::from(&url);
    let mut stream = crate::media::http_source::open_http_media(&path).expect("open http stream");
    stream.start_playback().expect("start playback");
    let duration = stream.duration_ms().expect("duration");
    let metadata = stream.read_metadata().ok();
    println!(
        "decode ok: duration {}ms, title={:?}",
        duration,
        metadata.as_ref().and_then(|m| m.name.clone()).unwrap_or_default()
    );
    assert!(duration > 30_000, "expected a real stream, got {duration}ms");
}

#[test]
#[ignore = "requires scanning the QR code with the NetEase Cloud Music mobile app"]
fn live_qr_login() {
    let client = client();
    crate::RUNTIME.block_on(async {
        let key = client.qr_create_key().await.expect("qr key request");
        let url = super::api::qr_login_url(&key);
        let qr_path = paths::data_dir().join("netease_qr.png");
        let code = qrcode::QrCode::new(url.as_bytes()).expect("qr encode");
        let image = code
            .render::<image::Rgba<u8>>()
            .quiet_zone(true)
            .min_dimensions(360, 360)
            .build();
        image.save(&qr_path).expect("save qr png");
        println!("=== 用手机网易云音乐 App 扫码（几分钟内） ===");
        println!("QR-PNG: {}", qr_path.display());
        println!("QR-URL: {url}");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match client.qr_check(&key).await {
                Ok(QrStatus::Waiting) => {}
                Ok(QrStatus::Scanned) => println!("已扫码，请在手机上点确认"),
                Ok(QrStatus::Success) => {
                    println!("=== 登录成功 ===");
                    match client.refresh_user_profile().await {
                        Ok(Some(profile)) => {
                            println!("profile: {} ({})", profile.nickname, profile.avatar_url)
                        }
                        Ok(None) => println!("profile: empty"),
                        Err(e) => println!("profile: FAILED {e}"),
                    }
                    let uid = client.user_id().unwrap_or(0);
                    match client.user_playlists(uid, 30, 0).await {
                        Ok(resp) => println!(
                            "user_playlists: {} playlists",
                            resp.body
                                .pointer("/playlist")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0)
                        ),
                        Err(e) => println!("user_playlists: FAILED {e}"),
                    }
                    match client.like_list(uid).await {
                        Ok(ids) => println!("like_list: {} liked songs", ids.len()),
                        Err(e) => println!("like_list: FAILED {e}"),
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

/// Probes `/api/song/lyric/v1` parameter variants for word-level YRC.
#[test]
#[ignore = "live network call against NetEase servers"]
fn live_lyric_probe() {
    use serde_json::json;

    let client = client();
    crate::RUNTIME.block_on(async {
        for (name, id) in [
            ("晴天", 186016i64),
            ("孤勇者", 1901371647),
            ("起风了", 1330348068),
        ] {
            for variant in [
                ("yv0", json!({"id": id, "cp": false, "tv": 0, "lv": 0, "rv": 0, "kv": 0, "yv": 0, "ytv": 0, "yrv": 0})),
                ("yv1", json!({"id": id, "cp": false, "tv": 0, "lv": 0, "rv": 0, "kv": 0, "yv": 1, "ytv": 1, "yrv": 1})),
                ("yv-1", json!({"id": id, "cp": false, "tv": -1, "lv": -1, "rv": -1, "kv": -1, "yv": -1, "ytv": -1, "yrv": -1})),
            ] {
                let (tag, data) = variant;
                match client.request(crate::netease::client::Crypto::Eapi, "/api/song/lyric/v1", data).await {
                    Ok(resp) => {
                        let len = |key: &str| {
                            resp.body.get(key)
                                .and_then(|v| v.get("lyric"))
                                .and_then(|v| v.as_str())
                                .map(str::len)
                                .unwrap_or(0)
                        };
                        println!("{name}({id}) [{tag}]: lrc={} tlyric={} yrc={} yrc_is_string={:?}",
                            len("lrc"), len("tlyric"), len("yrc"),
                            resp.body.get("yrc").map(|v| v.is_string()));
                        if tag == "yv0"
                            && let Some(yrc) = resp.body.pointer("/yrc/lyric").and_then(|v| v.as_str())
                        {
                            println!("yrc head: {}", yrc.chars().take(300).collect::<String>());
                        }
                    }
                    Err(e) => println!("{name}({id}) [{tag}]: FAILED {e}"),
                }
            }
        }
    });
}
