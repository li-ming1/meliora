//! The 13 KuGou endpoints this app actually uses, one method each.
//! Mirrors the corresponding modules in `KuGouMusicApi/module/`.

use serde_json::Value;

use super::client::{
    APPID, CLIENTVER, GATEWAY, KugouClient, KugouError, KugouRequest, KugouResponse, SRCAPPID,
    UserProfile, today_utc, unix_now_secs,
};
use super::crypto;

/// The h5 login page that QR codes open in the mobile app; the poll request's
/// `qrcode_txt` points at the same page.
const QR_LOGIN_PAGE_URL: &str = "https://h5.kugou.com/apps/loginQRCode/html/index.html";

/// QR login poll states, mirroring `login_qr_check.js`.
#[derive(Debug, Clone, PartialEq)]
pub enum QrStatus {
    Expired,
    Waiting,
    Scanned,
    Success { token: String, userid: i64 },
}

/// URL to encode into a QR code for the mobile app to scan.
pub fn qr_login_url(key: &str) -> String {
    format!("{QR_LOGIN_PAGE_URL}?qrcode={key}")
}

impl KugouClient {
    /// Step 1 of QR login: request a fresh QR key.
    pub async fn qr_create_key(&self) -> Result<String, KugouError> {
        let spec = KugouRequest::new("https://login-user.kugou.com", "/v2/qrcode")
            .web()
            .param("appid", 1001)
            .param("type", 1)
            .param("plat", 4)
            .param("qrcode_txt", format!("{QR_LOGIN_PAGE_URL}?appid={APPID}&"))
            .param("srcappid", SRCAPPID);
        let response = self.request(spec).await?;
        response
            .body
            .pointer("/data/qrcode")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| KugouError::Api {
                status: -1,
                msg: "qrcode key missing from response".into(),
            })
    }

    /// Step 2 of QR login: poll the key until the mobile app confirms.
    pub async fn qr_check(&self, key: &str) -> Result<QrStatus, KugouError> {
        let session = self.session_snapshot();
        let mut spec = KugouRequest::new("https://login-user.kugou.com", "/v2/get_userinfo_qrcode")
            .web()
            .param("plat", 4)
            .param("appid", APPID)
            .param("srcappid", SRCAPPID)
            .param("qrcode", key);
        if !session.dev.is_empty() {
            spec = spec.param("dev", session.dev);
        }
        let response = self.request(spec).await?;
        let status = response
            .body
            .pointer("/data/status")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        match status {
            0 => Ok(QrStatus::Expired),
            1 => Ok(QrStatus::Waiting),
            2 => Ok(QrStatus::Scanned),
            4 => {
                let data = response.body.get("data").cloned().unwrap_or(Value::Null);
                let token = data
                    .get("token")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let userid = data.get("userid").and_then(Value::as_i64).unwrap_or(0);
                if token.is_empty() {
                    return Err(KugouError::Api {
                        status: 4,
                        msg: "login succeeded but token missing".into(),
                    });
                }
                Ok(QrStatus::Success { token, userid })
            }
            other => Err(KugouError::Api {
                status: other,
                msg: format!("unexpected qr status {other}"),
            }),
        }
    }

    /// Cached profile (nickname + avatar) of the logged-in user. Hits the
    /// network exactly once; later reads come from the persisted session, so
    /// opening the KuGou pages never re-fetches it.
    pub async fn user_profile(&self) -> Result<UserProfile, KugouError> {
        if let Some(profile) = self.cached_user_profile() {
            return Ok(profile);
        }

        let response = self.user_detail().await?;
        let nickname = response
            .body
            .pointer("/data/nickname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // KuGou returns an avatar template with a {size} placeholder; pin it
        // to something small for the UI.
        let avatar_url = response
            .body
            .pointer("/data/pic")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .replace("{size}", "200");
        let profile = UserProfile {
            nickname,
            avatar_url,
        };
        self.update_user_profile(&profile);
        Ok(profile)
    }

    /// Profile of the logged-in user (nickname, vip state...).
    pub async fn user_detail(&self) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let clienttime = unix_now_secs();
        let pk = crypto::rsa_raw_encrypt(
            &serde_json::json!({
                "token": session.token.clone().unwrap_or_default(),
                "clienttime": clienttime,
            })
            .to_string(),
        )
        .to_uppercase();
        let spec = KugouRequest::new(GATEWAY, "/v3/get_my_info")
            .post()
            .router("usercenter.kugou.com")
            .param("plat", 1)
            .json(serde_json::json!({
                "visit_time": clienttime,
                "usertype": 1,
                "p": pk,
                "userid": session.userid.unwrap_or(0),
            }));
        self.request(spec).await
    }

    /// VIP entitlement detail: is the account a member, which products
    /// (`svip`/`tvip`) are active and until when. Mirrors `youth_union_vip.js`
    /// (`/v1/get_union_vip` on the kugouvip host).
    pub async fn user_vip_detail(&self) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new("https://kugouvip.kugou.com", "/v1/get_union_vip")
            .param("busi_type", "concept")
            .param("opt_product_types", "dvip,qvip")
            .param("product_type", "svip");
        self.request(spec).await
    }

    /// All playlists of the logged-in user. Mirrors `user_playlist.js`.
    pub async fn user_playlists(
        &self,
        page: i64,
        pagesize: i64,
    ) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let userid = session.userid.unwrap_or(0);
        let token = session.token.clone().unwrap_or_default();
        let spec = KugouRequest::new(GATEWAY, "/v7/get_all_list")
            .post()
            .router("cloudlist.service.kugou.com")
            .param("plat", 1)
            .param("userid", userid)
            .param("token", token.clone())
            .json(serde_json::json!({
                "userid": userid,
                "token": token,
                "total_ver": 979,
                "type": 2,
                "page": page,
                "pagesize": pagesize,
            }));
        self.request(spec).await
    }

    /// Tracks of one playlist. Mirrors `playlist_track_all_new.js`: this is the
    /// endpoint the reference client uses for the user's own and collected
    /// playlists. Songs come back under `data.info` (fields `hash`, `name`,
    /// `mixsongid`, `album_id`, `timelen`, `singerinfo[].name`). The older
    /// `/pubsongs/v2/get_other_list_file_nofilt` is NOT used: it returns an
    /// empty list for personal playlists.
    pub async fn playlist_tracks(
        &self,
        listid: i64,
        page: i64,
        pagesize: i64,
    ) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let userid = session.userid.unwrap_or(0);
        let token = session.token.clone().unwrap_or_default();
        let spec = KugouRequest::new(GATEWAY, "/v4/get_list_all_file")
            .post()
            .router("cloudlist.service.kugou.com")
            .param("plat", 1)
            .param("userid", userid)
            .param("token", token.clone())
            .json(serde_json::json!({
                "listid": listid,
                "userid": userid,
                "area_code": 1,
                "show_relate_goods": 0,
                "pagesize": pagesize,
                "allplatform": 1,
                "show_cover": 1,
                "type": 0,
                "token": token,
                "page": page,
            }));
        self.request(spec).await
    }

    /// Adds songs to a user playlist. Mirrors `playlist_tracks_add.js`;
    /// `listid: 2` targets the built-in "liked songs" list.
    pub async fn playlist_add_songs(
        &self,
        listid: i64,
        songs: &[(String, String, i64, i64)],
    ) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let userid = session.userid.unwrap_or(0);
        let token = session.token.clone().unwrap_or_default();
        let clienttime = unix_now_secs();

        let data: Vec<Value> = songs
            .iter()
            .map(|(name, hash, album_id, mixsongid)| {
                serde_json::json!({
                    "number": 1,
                    "name": name,
                    "hash": hash,
                    "size": 0,
                    "sort": 0,
                    "timelen": 0,
                    "bitrate": 0,
                    "album_id": album_id,
                    "mixsongid": mixsongid,
                })
            })
            .collect();

        let spec = KugouRequest::new(GATEWAY, "/cloudlist.service/v6/add_song")
            .post()
            .param("last_time", clienttime)
            .param("last_area", "gztx")
            .param("userid", userid)
            .param("token", token.clone())
            .json(serde_json::json!({
                "userid": userid,
                "token": token,
                "listid": listid,
                "list_ver": 0,
                "type": 0,
                "slow_upload": 1,
                "scene": "false;null",
                "data": data,
            }));
        self.request(spec).await
    }

    /// Removes songs from a user playlist by their in-playlist file ids.
    /// Mirrors `playlist_tracks_del.js`; songs are matched by `fileid` (from
    /// `/v4/get_list_all_file`), and `listid: 2` is the "liked songs" list.
    pub async fn playlist_remove_songs(
        &self,
        listid: i64,
        fileids: &[i64],
    ) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let userid = session.userid.unwrap_or(0);
        let token = session.token.clone().unwrap_or_default();

        let data: Vec<Value> = fileids
            .iter()
            .map(|id| serde_json::json!({ "fileid": id }))
            .collect();

        let spec = KugouRequest::new(GATEWAY, "/v4/delete_songs")
            .post()
            .router("cloudlist.service.kugou.com")
            .param("userid", userid)
            .param("token", token.clone())
            .json(serde_json::json!({
                "userid": userid,
                "token": token,
                "listid": listid,
                "list_ver": 0,
                "type": 0,
                "data": data,
            }));
        self.request(spec).await
    }

    /// Creates a new empty user playlist. Mirrors `playlist_add.js`
    /// (`/cloudlist.service/v5/add_list`); the returned payload is checked by
    /// `request`, callers normally resolve the new numeric `listid` from a
    /// subsequent `user_playlists` call by name.
    pub async fn create_playlist(&self, name: &str) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let userid = session.userid.unwrap_or(0);
        let token = session.token.clone().unwrap_or_default();
        let clienttime = unix_now_secs();

        let spec = KugouRequest::new(GATEWAY, "/cloudlist.service/v5/add_list")
            .post()
            .router("cloudlist.service.kugou.com")
            .param("last_time", clienttime)
            .param("last_area", "gztx")
            .param("userid", userid)
            .param("token", token.clone())
            .json(serde_json::json!({
                "userid": userid,
                "token": token,
                "total_ver": 0,
                "name": name,
                "type": 0,
                "source": 1,
                "is_pri": 0,
                "list_create_userid": userid,
                "list_create_listid": "1",
                "list_create_gid": "",
                "from_shupinmv": 0,
            }));
        self.request(spec).await
    }

    /// Song search. The Android v3 endpoint was retired server-side (it
    /// answers error 152 for the reference client too), so this uses the
    /// web v2 endpoint with the web signature and its own param set
    /// (clienttime in milliseconds, hex mid/uuid, appid 1014).
    pub async fn search(
        &self,
        keyword: &str,
        page: i64,
        pagesize: i64,
    ) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let clienttime_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let spec = KugouRequest::new(GATEWAY, "/v2/search/song")
            .router("complexsearch.kugou.com")
            .web()
            .no_default_params()
            .param("appid", 1014)
            .param("bitrate", 0)
            .param("callback", "callback123")
            .param("clienttime", clienttime_ms)
            .param("clientver", 1000)
            .param("dfid", session.dfid)
            .param("filter", 10)
            .param("inputtype", 0)
            .param("iscorrection", 1)
            .param("isfuzzy", 0)
            .param("keyword", keyword)
            .param("mid", session.guid.clone())
            .param("page", page)
            .param("pagesize", pagesize)
            .param("platform", "WebFilter")
            .param("privilege_filter", 0)
            .param("srcappid", SRCAPPID)
            .param("token", session.token.clone().unwrap_or_default())
            .param("userid", session.userid.unwrap_or(0))
            .param("uuid", session.guid);
        self.request(spec).await
    }

    /// Lyric search: hash/keyword -> (id, accesskey) pairs.
    /// Mirrors `search_lyric.js`: no default params; the reference client's
    /// `notSign` flag is a no-op, so this stays android-signed.
    pub async fn search_lyric(
        &self,
        hash: &str,
        keyword: &str,
        duration: i64,
    ) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new("https://lyrics.kugou.com", "/v1/search")
            .no_default_params()
            .param("album_audio_id", 0)
            .param("appid", APPID)
            .param("clientver", CLIENTVER)
            .param("duration", duration)
            .param("hash", hash)
            .param("keyword", keyword)
            .param("lrctxt", 1)
            .param("man", "no");
        self.request(spec).await
    }

    /// Shared `/download` request of the lyric endpoints; only `fmt` differs.
    async fn lyric_download(
        &self,
        id: &str,
        accesskey: &str,
        fmt: &str,
    ) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new("https://lyrics.kugou.com", "/download")
            .param("ver", 1)
            .param("client", "android")
            .param("id", id)
            .param("accesskey", accesskey)
            .param("fmt", fmt)
            .param("charset", "utf8");
        self.request(spec).await
    }

    /// Lyric download. With `fmt = "lrc"` the content is plain base64 LRC
    /// text, which this method decodes. Mirrors `lyric.js`.
    pub async fn lyric_lrc(&self, id: &str, accesskey: &str) -> Result<String, KugouError> {
        let response = self.lyric_download(id, accesskey, "lrc").await?;
        match response.body.get("content").and_then(Value::as_str) {
            Some(content) => {
                use base64::Engine;
                use base64::engine::general_purpose::STANDARD;
                let bytes = STANDARD.decode(content).map_err(|_| KugouError::Api {
                    status: -1,
                    msg: "lyric content is not valid base64".into(),
                })?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            None => Ok(String::new()),
        }
    }

    /// Lyric download in KuGou Karaoke (KRC) format. The `content` field is
    /// the raw base64 of an encrypted, zlib-compressed KRC document; decoding
    /// lives in `ui::lyrics::krc::decrypt_krc` so the text stays opaque here.
    pub async fn lyric_krc(&self, id: &str, accesskey: &str) -> Result<String, KugouError> {
        let response = self.lyric_download(id, accesskey, "krc").await?;
        Ok(response
            .body
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Rank list (`/ocean/v6/rank/list`), each entry carrying a cover.
    /// Mirrors `RawRankApi.GetRankListAsync`, but with `withsong: 0`: the
    /// server otherwise attaches the full song entities of every rank to
    /// every entry (megabytes of JSON the UI never reads — the rank cards
    /// only show name + cover), and the serde_json::Value tree inflated the
    /// view-entry memory step by ~100 MB.
    pub async fn rank_list(&self) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new(GATEWAY, "/ocean/v6/rank/list")
            .param("plat", 2)
            .param("withsong", 0)
            .param("parentid", 0);
        self.request(spec).await
    }

    /// Songs of one rank (`/openapi/kmr/v2/rank/audio`), paged. Requires the
    /// `kg-tid: 369` header. Mirrors `RawRankApi.GetRankAudioAsync`.
    pub async fn rank_audio(
        &self,
        rank_id: i64,
        page: i64,
        page_size: i64,
    ) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new(GATEWAY, "/openapi/kmr/v2/rank/audio")
            .header("kg-tid", "369")
            .post()
            .json(serde_json::json!({
                "show_portrait_mv": 1,
                "show_type_total": 1,
                "filter_original_remarks": 1,
                "area_code": 1,
                "pagesize": page_size,
                "rank_cid": 0,
                "type": 1,
                "page": page,
                "rank_id": rank_id,
            }));
        self.request(spec).await
    }

    /// Daily recommend songs (`/everyday_song_recommend`), routed to the
    /// everyday-recommend upstream via `x-router`. Mirrors
    /// `RawDiscoveryApi.GetRecommendSongAsync`.
    pub async fn everyday_recommend(&self) -> Result<KugouResponse, KugouError> {
        let userid = self.session_snapshot().userid.unwrap_or(0).to_string();
        let spec = KugouRequest::new(GATEWAY, "/everyday_song_recommend")
            .post()
            .router("everydayrec.service.kugou.com")
            .json(serde_json::json!({
                "platform": "android",
                "userid": userid,
            }));
        self.request(spec).await
    }

    /// Play URL for one hash. Mirrors `song_url.js`: keyed and android-signed
    /// (the module's `notSign` flag is a no-op in the reference client), with
    /// a random per-request dfid. `free_part` returns the trial clip for
    /// VIP-only songs (about 60s @128k).
    pub async fn song_url(
        &self,
        hash: &str,
        album_audio_id: i64,
        album_id: i64,
        quality: &str,
        free_part: bool,
    ) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new(GATEWAY, "/v5/url")
            .router("trackercdn.kugou.com")
            .with_key()
            .random_dfid()
            .param("album_id", album_id)
            .param("area_code", 1)
            .param("hash", hash.to_lowercase())
            .param("ssa_flag", "is_fromtrack")
            .param("version", 11430)
            .param("page_id", 967177915)
            .param("quality", if quality.is_empty() { "128" } else { quality })
            .param("album_audio_id", album_audio_id)
            .param("behavior", "play")
            .param("pid", 411)
            .param("cmd", 26)
            .param("pidversion", 3001)
            .param("IsFreePart", i32::from(free_part))
            .param("ppage_id", "356753938,823673182,967485191")
            .param("cdnBackup", 1)
            .param("module", "")
            .param("clientver", 11430);
        self.request(spec).await
    }
}

/// Result of the daily free VIP claim flow. Mirrors the .NET reference
/// client's youth/day-VIP benefit: receive one day, then upgrade it.
#[derive(Debug, Clone, PartialEq)]
pub enum VipClaimOutcome {
    /// This account earned one day of VIP today (fresh claim or an upgrade
    /// of an existing `tvip` reward).
    Claimed,
    /// Already covered for today — nothing to do.
    AlreadyClaimed,
    /// The account is not logged in; no claim was attempted.
    NotLoggedIn,
    /// Something failed server-side (record query or the claim itself).
    Failed { reason: String },
}

impl KugouClient {
    /// Free one-day VIP for today. Mirrors `youth_day_vip.js`: fire and
    /// forget against the youth recharge endpoint.
    pub async fn receive_one_day_vip(&self) -> Result<KugouResponse, KugouError> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let spec = KugouRequest::new(GATEWAY, "/youth/v1/recharge/receive_vip_listen_song")
            .post()
            .param("source_id", 90139)
            .param("receive_day", today);
        self.request(spec).await
    }

    /// Upgrade the day's VIP reward (tvip -> dvip/svip). Mirrors
    /// `youth_day_vip_upgrade.js`.
    pub async fn upgrade_vip_reward(&self) -> Result<KugouResponse, KugouError> {
        let session = self.session_snapshot();
        let spec = KugouRequest::new(GATEWAY, "/youth/v1/listen_song/upgrade_vip_reward")
            .post()
            .param("kugouid", session.userid.unwrap_or(0))
            .param("ad_type", 1);
        self.request(spec).await
    }

    /// Month of VIP claim records. Mirrors `youth_month_vip_record.js`; the
    /// caller inspects `data/list` for today's entry to decide whether a
    /// fresh claim is still needed.
    pub async fn get_vip_record(&self) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new(GATEWAY, "/youth/v1/activity/get_month_vip_record")
            .param("latest_limit", 100);
        self.request(spec).await
    }

    /// Fetches VIP entitlement detail and caches it on the session so the
    /// settings page can display it later. Returns the error so callers can
    /// tell "fetch failed" apart from "the server says no active VIP".
    pub async fn refresh_vip_detail(&self) -> Result<(), KugouError> {
        let response = self.user_vip_detail().await?;
        self.store_vip_detail(response.body);
        Ok(())
    }

    /// Ensures this account has VIP for today, but never hits the claim
    /// endpoints more than once per calendar day while VIP is actually
    /// active.
    ///
    /// Loop protection: the attempt marker is persisted on the first try so
    /// concurrent startup/login triggers don't double-claim. The skip guard
    /// additionally requires today's VIP to actually be active in the cached
    /// detail; a failed attempt retries on the next app start or fresh login,
    /// which is what keeps membership alive across login-expiry windows.
    pub async fn ensure_daily_vip(&self) -> VipClaimOutcome {
        if !self.logged_in() {
            return VipClaimOutcome::NotLoggedIn;
        }

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // Same-day guard: skip only when today's attempt actually secured
        // VIP. A failed attempt (e.g. one made while the login was expired,
        // failing at get_vip_record with 51002) must not consume the day -
        // without this the marker poisons the claim and it never retries
        // even after the user signs in again. Retry frequency stays bounded:
        // ensure_daily_vip only runs at app startup and after a fresh login.
        if self.last_claim_day().as_deref() == Some(today.as_str()) && self.has_active_vip() {
            return VipClaimOutcome::AlreadyClaimed;
        }

        // Mark the attempt now so startup and login triggers racing in the
        // same session don't double-claim; the guard reads real VIP state,
        // so a failed attempt still retries on the next launch.
        self.mark_claim_attempted();

        let record = match self.get_vip_record().await {
            Ok(response) => response,
            Err(err) => {
                return VipClaimOutcome::Failed {
                    reason: format!("query vip record: {err}"),
                };
            }
        };

        let list = record
            .body
            .pointer("/data/list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Refresh cached VIP detail for the settings page (best-effort).
        let _ = self.refresh_vip_detail().await;

        let today_entry = list
            .iter()
            .find(|item| item.get("day").and_then(Value::as_str) == Some(today.as_str()));

        match today_entry {
            // Not yet claimed today: claim a day then upgrade it.
            None => {
                if let Err(err) = self.receive_one_day_vip().await {
                    return VipClaimOutcome::Failed {
                        reason: format!("receive vip: {err}"),
                    };
                }
                let _ = self.upgrade_vip_reward().await;
                let _ = self.refresh_vip_detail().await;
                VipClaimOutcome::Claimed
            }
            // Already claimed but still a base tvip reward, upgrade it.
            Some(entry)
                if entry.get("receive_vip").and_then(Value::as_i64) == Some(1)
                    && entry.get("vip_type").and_then(Value::as_str) == Some("tvip") =>
            {
                let _ = self.upgrade_vip_reward().await;
                let _ = self.refresh_vip_detail().await;
                VipClaimOutcome::Claimed
            }
            // Covered.
            _ => VipClaimOutcome::AlreadyClaimed,
        }
    }
}
