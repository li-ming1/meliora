//! The NetEase endpoints this app actually uses, one method each, mirroring
//! the corresponding modules in `NeteaseCloudMusicApi/module/`. All requests
//! are POSTs; the crypto type of each endpoint matches the reference module.

use serde_json::{Value, json};

use super::client::{Crypto, NeteaseClient, NeteaseError, NeteaseResponse, UserProfile};

/// QR login poll states, mirroring `login_qr_check.js` response codes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QrStatus {
    /// Code 800: the key expired, generate a fresh one.
    Expired,
    /// Code 801: waiting for the mobile app to scan.
    Waiting,
    /// Code 802: scanned, waiting for the confirmation tap.
    Scanned,
    /// Code 803: authorized; `MUSIC_U` was captured into the session.
    Success,
}

/// URL to encode into a QR code for the mobile app to scan
/// (mirrors `login_qr_create.js`).
pub fn qr_login_url(key: &str) -> String {
    format!("https://music.163.com/login?codekey={key}")
}

impl NeteaseClient {
    /// Step 1 of QR login: request a fresh QR key. Mirrors `login_qr_key.js`
    /// (`/api/login/qrcode/unikey`, eapi, `{type: 3}`).
    pub async fn qr_create_key(&self) -> Result<String, NeteaseError> {
        let response = self
            .request(
                Crypto::Eapi,
                "/api/login/qrcode/unikey",
                json!({ "type": 3 }),
            )
            .await?;
        response
            .body
            .pointer("/data/unikey")
            .or_else(|| response.body.pointer("/unikey"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| NeteaseError::Api {
                status: -1,
                msg: "qr unikey missing from response".into(),
            })
    }

    /// Step 2 of QR login: poll the key. Mirrors `login_qr_check.js`
    /// (`/api/login/qrcode/client/login`, eapi). On 803 the login cookies are
    /// captured from the Set-Cookie headers; the raw `body.cookie` string is
    /// used as a fallback so the login survives proxies that drop them.
    pub async fn qr_check(&self, key: &str) -> Result<QrStatus, NeteaseError> {
        let response = self
            .request(
                Crypto::Eapi,
                "/api/login/qrcode/client/login",
                json!({ "key": key, "type": 3 }),
            )
            .await?;
        match response.body.get("code").and_then(Value::as_i64) {
            Some(800) => Ok(QrStatus::Expired),
            Some(801) => Ok(QrStatus::Waiting),
            Some(802) => Ok(QrStatus::Scanned),
            Some(803) => {
                if !self.logged_in() {
                    let cookie = response
                        .body
                        .get("cookie")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.store_login_from_cookie_string(cookie);
                }
                if self.logged_in() {
                    Ok(QrStatus::Success)
                } else {
                    Err(NeteaseError::Api {
                        status: 803,
                        msg: "login succeeded but MUSIC_U cookie missing".into(),
                    })
                }
            }
            other => Err(NeteaseError::Api {
                status: other.unwrap_or(-1),
                msg: format!("unexpected qr check response: {other:?}"),
            }),
        }
    }

    /// Account of the logged-in user. Mirrors `login_status.js`
    /// (`/api/w/nuser/account/get`, weapi). The response carries
    /// `profile{userId, nickname, avatarUrl}` and `account`.
    pub async fn login_status(&self) -> Result<NeteaseResponse, NeteaseError> {
        self.request(Crypto::Weapi, "/api/w/nuser/account/get", json!({}))
            .await
    }

    /// Fetches the profile of the logged-in user and persists it into the
    /// session. Returns `Ok(None)` when not logged in (the endpoint answers
    /// with an empty profile).
    pub async fn refresh_user_profile(&self) -> Result<Option<UserProfile>, NeteaseError> {
        let response = self.login_status().await?;
        let profile = response.body.pointer("/profile");
        let field = |name: &str| profile.and_then(|profile| profile.get(name));
        let Some(user_id) = field("userId").and_then(Value::as_i64) else {
            return Ok(None);
        };
        let nickname = field("nickname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // NetEase CDN serves originals unless a `?param=WxH` size is appended;
        // the avatar renders at ~40px, so 128 covers it at high DPI.
        let avatar_url = field("avatarUrl")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let avatar_url = if avatar_url.is_empty() || avatar_url.contains("?param=") {
            avatar_url.to_string()
        } else {
            format!("{avatar_url}?param=128y128")
        };
        let profile = UserProfile {
            nickname,
            avatar_url,
        };
        self.update_user_profile(user_id, &profile);
        Ok(Some(profile))
    }

    /// Logs out on the server, then clears the local login state. Mirrors
    /// `logout.js` (`/api/logout`, eapi).
    pub async fn logout(&self) -> Result<(), NeteaseError> {
        let result = self.request(Crypto::Eapi, "/api/logout", json!({})).await;
        self.clear_login();
        result.map(|_| ())
    }

    /// Playlists owned by and subscribed to by `uid`. Mirrors
    /// `user_playlist.js` (`/api/user/playlist`, weapi): entries with
    /// `creator.userId == uid` are the user's own, the rest are subscribed.
    pub async fn user_playlists(
        &self,
        uid: i64,
        limit: i64,
        offset: i64,
    ) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Weapi,
            "/api/user/playlist",
            json!({
                "uid": uid,
                "limit": limit,
                "offset": offset,
                "includeVideo": true,
            }),
        )
        .await
    }

    /// Playlist metadata incl. the full `trackIds` list. Mirrors
    /// `playlist_detail.js` (`/api/v6/playlist/detail`, eapi). `n: 0` keeps
    /// the server from materializing `playlist.tracks` (full song entities
    /// for every track — megabytes of JSON whose serde_json::Value tree
    /// inflated opening a playlist/rank by up to ~120 MB); the separate
    /// `trackIds` array is always returned in full regardless of `n`.
    pub async fn playlist_detail(&self, id: i64) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Eapi,
            "/api/v6/playlist/detail",
            json!({ "id": id, "n": 0, "s": 8 }),
        )
        .await
    }

    /// Full `trackIds` list of a playlist (playlist order, newest-first),
    /// straight from `playlist_detail`. Cache it in the caller and page over
    /// it with `playlist_tracks_page`, so only the first page pays for the
    /// detail fetch.
    pub async fn playlist_track_ids(&self, id: i64) -> Result<Vec<i64>, NeteaseError> {
        let detail = self.playlist_detail(id).await?;
        Ok(detail
            .body
            .pointer("/playlist/trackIds")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_i64))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Song objects for one page of an already-known `track_ids` list: slices
    /// `offset..offset+limit` and resolves the ids via `/api/v3/song/detail`.
    /// This is the second half of the old `playlist_track_all` without the
    /// per-page `playlist_detail` re-fetch that made paging O(n²).
    pub async fn playlist_tracks_page(
        &self,
        track_ids: &[i64],
        limit: i64,
        offset: i64,
    ) -> Result<NeteaseResponse, NeteaseError> {
        let slice: Vec<i64> = track_ids
            .iter()
            .skip(offset.max(0) as usize)
            .take(limit.max(0) as usize)
            .copied()
            .collect();
        if slice.is_empty() {
            return Ok(NeteaseResponse {
                body: json!({ "songs": [] }),
                set_cookies: Vec::new(),
            });
        }
        self.song_detail(&slice).await
    }

    /// Song objects for up to ~1000 ids. Mirrors `song_detail.js`
    /// (`/api/v3/song/detail`, weapi, `c: '[{"id":...},...]'`).
    pub async fn song_detail(&self, ids: &[i64]) -> Result<NeteaseResponse, NeteaseError> {
        let c = format!(
            "[{}]",
            ids.iter()
                .map(|id| format!(r#"{{"id":{id}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        self.request(Crypto::Weapi, "/api/v3/song/detail", json!({ "c": c }))
            .await
    }

    /// Play URL for one song at the requested quality `level`. Mirrors
    /// `song_url_v1.js` (`/api/song/enhance/player/url/v1`); this fork ships
    /// the endpoint over the new xeapi handshake, plain eapi on the same path
    /// works identically. `data[0].url` may be `null` and may be a
    /// `freeTrialInfo` trial clip for VIP songs.
    pub async fn song_url(&self, id: i64, level: &str) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Eapi,
            "/api/song/enhance/player/url/v1",
            json!({
                "ids": format!("[{id}]"),
                "level": level,
                "encodeType": "flac",
            }),
        )
        .await
    }

    /// Lyrics incl. word-level YRC. Mirrors `lyric_new.js`
    /// (`/api/song/lyric/v1`, eapi): `lrc.lyric`, `tlyric.lyric`
    /// (translation), `yrc.lyric` (karaoke).
    pub async fn lyric_new(&self, id: i64) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Eapi,
            "/api/song/lyric/v1",
            json!({
                "id": id,
                "cp": false,
                "tv": 0,
                "lv": 0,
                "rv": 0,
                "kv": 0,
                "yv": 0,
                "ytv": 0,
                "yrv": 0,
            }),
        )
        .await
    }

    /// Song search. Mirrors `cloudsearch.js` (`/api/cloudsearch/pc`, eapi);
    /// `type`: 1 songs, 10 albums, 100 artists, 1000 playlists.
    pub async fn cloudsearch(
        &self,
        keywords: &str,
        search_type: i64,
        limit: i64,
        offset: i64,
    ) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Eapi,
            "/api/cloudsearch/pc",
            json!({
                "s": keywords,
                "type": search_type,
                "limit": limit,
                "offset": offset,
                "total": true,
            }),
        )
        .await
    }

    /// Likes (red hearts) / unlikes a song. Mirrors `like.js`
    /// (`/api/radio/like`, weapi). Requires a logged-in session.
    pub async fn like(&self, track_id: i64, like: bool) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Weapi,
            "/api/radio/like",
            json!({
                "alg": "itembased",
                "trackId": track_id,
                "like": like,
                "time": "3",
            }),
        )
        .await
    }

    /// The unordered id list of the user's liked (red-hearted) songs.
    /// Mirrors `likelist.js` (`/api/song/like/get`, eapi).
    pub async fn like_list(&self, uid: i64) -> Result<Vec<i64>, NeteaseError> {
        let response = self
            .request(Crypto::Eapi, "/api/song/like/get", json!({ "uid": uid }))
            .await?;
        Ok(response
            .body
            .pointer("/ids")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_i64).collect::<Vec<i64>>())
            .unwrap_or_default())
    }

    /// All charts (榜单). Mirrors `toplist.js` (`/api/toplist`, eapi); every
    /// entry is itself a playlist id, so tracks come from
    /// `playlist_track_ids` + `playlist_tracks_page`.
    pub async fn toplist(&self) -> Result<NeteaseResponse, NeteaseError> {
        self.request(Crypto::Eapi, "/api/toplist", json!({})).await
    }

    /// Daily recommended songs (requires login). Mirrors `recommend_songs.js`
    /// (`/api/v3/discovery/recommend/songs`, weapi) → `data.dailySongs[]`.
    pub async fn recommend_songs(&self) -> Result<NeteaseResponse, NeteaseError> {
        self.request(
            Crypto::Weapi,
            "/api/v3/discovery/recommend/songs",
            json!({}),
        )
        .await
    }
}
