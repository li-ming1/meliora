//! 本应用用到的酷狗端点，每个端点一个方法（数量会随功能增减，不在这里记账）。
//! 与 `KuGouMusicApi/module/` 里的对应模块一一对照。

use serde_json::Value;

use super::client::{
    APPID, CLIENTVER, GATEWAY, KugouClient, KugouError, KugouRequest, KugouResponse, SRCAPPID,
    UserProfile, today_utc, unix_now_secs,
};
use super::crypto;

/// 二维码在移动端 App 里打开的 h5 登录页；轮询请求的 `qrcode_txt` 指向同一个页面。
const QR_LOGIN_PAGE_URL: &str = "https://h5.kugou.com/apps/loginQRCode/html/index.html";

/// 二维码登录的轮询状态，对照 `login_qr_check.js`。
#[derive(Debug, Clone, PartialEq)]
pub enum QrStatus {
    Expired,
    Waiting,
    Scanned,
    Success { token: String, userid: i64 },
}

/// 编码进二维码、供移动端 App 扫描的 URL。
pub fn qr_login_url(key: &str) -> String {
    format!("{QR_LOGIN_PAGE_URL}?qrcode={key}")
}

impl KugouClient {
    /// 二维码登录第 1 步：申请一个新的二维码 key。
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
            .ok_or_else(|| KugouError::local("qrcode key missing from response"))
    }

    /// 二维码登录第 2 步：轮询该 key，直到移动端 App 确认。
    pub async fn qr_check(&self, key: &str) -> Result<QrStatus, KugouError> {
        let session = self.credentials();
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
                        error_code: None,
                    });
                }
                Ok(QrStatus::Success { token, userid })
            }
            other => Err(KugouError::Api {
                status: other,
                msg: format!("unexpected qr status {other}"),
                error_code: None,
            }),
        }
    }

    /// 已登录用户的缓存资料（昵称 + 头像）。网络上只打一次；后续读取都来自持久化会话，
    /// 因此打开酷狗页面不会重新拉取。
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
        // 酷狗返回的是带 {size} 占位符的头像模板，UI 里固定成小尺寸。
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

    /// 已登录用户的资料（昵称、VIP 状态等）。
    pub async fn user_detail(&self) -> Result<KugouResponse, KugouError> {
        let session = self.credentials();
        let clienttime = unix_now_secs();
        // 除服务端控制的 token 之外信封是固定的；若它不再装得进 128 字节的 RSA 分块，
        // 就让本次请求失败而不是 panic 掉整个进程（token 是持久化的，panic 会在每次
        // 启动重现，直到重新登录）。
        let envelope = serde_json::json!({
            "token": session.token.clone().unwrap_or_default(),
            "clienttime": clienttime,
        })
        .to_string();
        let envelope_len = envelope.len();
        let pk = crypto::rsa_raw_encrypt(&envelope)
            .ok_or_else(|| {
                KugouError::local(format!(
                    "user_detail envelope is {envelope_len} bytes, exceeds the \
                     128-byte RSA block (server token format changed?)"
                ))
            })?
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

    /// VIP 权益明细：账号是否会员、哪些商品（`svip`/`tvip`）在生效以及到什么时候。
    /// 对照 `youth_union_vip.js`（kugouvip 主机上的 `/v1/get_union_vip`）。
    pub async fn user_vip_detail(&self) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new("https://kugouvip.kugou.com", "/v1/get_union_vip")
            .param("busi_type", "concept")
            .param("opt_product_types", "dvip,qvip")
            .param("product_type", "svip");
        self.request(spec).await
    }

    /// 已登录用户的全部歌单。对照 `user_playlist.js`。
    pub async fn user_playlists(
        &self,
        page: i64,
        pagesize: i64,
    ) -> Result<KugouResponse, KugouError> {
        let (userid, token) = self.userid_token();
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

    /// 单个歌单的曲目。对照 `playlist_track_all_new.js`：参考客户端取用户自建歌单与
    /// 收藏歌单用的就是这个端点。曲目在 `data.info` 下返回（字段 `hash`、`name`、
    /// `mixsongid`、`album_id`、`timelen`、`singerinfo[].name`）。更早的
    /// `/pubsongs/v2/get_other_list_file_nofilt` 不用：它对个人歌单返回空列表。
    pub async fn playlist_tracks(
        &self,
        listid: i64,
        page: i64,
        pagesize: i64,
    ) -> Result<KugouResponse, KugouError> {
        let (userid, token) = self.userid_token();
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

    /// 向用户歌单加歌。对照 `playlist_tracks_add.js`；`listid: 2` 指向内置的「我喜欢的」列表。
    pub async fn playlist_add_songs(
        &self,
        listid: i64,
        songs: &[(String, String, i64, i64)],
    ) -> Result<KugouResponse, KugouError> {
        let (userid, token) = self.userid_token();
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

    /// 按歌曲在歌单内的 file id 从用户歌单移除歌曲。对照 `playlist_tracks_del.js`；歌曲按
    /// `fileid`（来自 `/v4/get_list_all_file`）匹配，`listid: 2` 是「我喜欢的」列表。
    pub async fn playlist_remove_songs(
        &self,
        listid: i64,
        fileids: &[i64],
    ) -> Result<KugouResponse, KugouError> {
        let (userid, token) = self.userid_token();

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

    /// 新建一个空的用户歌单。对照 `playlist_add.js`（`/cloudlist.service/v5/add_list`）；
    /// 返回载荷由 `request` 校验，调用方通常按名字从随后的 `user_playlists` 调用里解析出
    /// 新的数字 `listid`。
    pub async fn create_playlist(&self, name: &str) -> Result<KugouResponse, KugouError> {
        let (userid, token) = self.userid_token();
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

    /// 歌曲搜索。Android v3 端点已被服务端下线（它对参考客户端也回 error 152），所以这里
    /// 用 web v2 端点 + web 签名与它自己的一套参数（clienttime 为毫秒、hex 形式的
    /// mid/uuid、appid 1014）。
    pub async fn search(
        &self,
        keyword: &str,
        page: i64,
        pagesize: i64,
    ) -> Result<KugouResponse, KugouError> {
        let session = self.credentials();
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

    /// 歌词搜索：hash/keyword -> (id, accesskey) 对。对照 `search_lyric.js`：不带默认
    /// 参数；参考客户端的 `notSign` 旗标是空操作，所以这里仍走 android 签名。
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

    /// 歌词端点共用的 `/download` 请求；只有 `fmt` 不同。
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

    /// 歌词下载。`fmt = "lrc"` 时 content 是纯 base64 的 LRC 文本，本方法负责解码。
    /// 对照 `lyric.js`。
    pub async fn lyric_lrc(&self, id: &str, accesskey: &str) -> Result<String, KugouError> {
        let response = self.lyric_download(id, accesskey, "lrc").await?;
        match response.body.get("content").and_then(Value::as_str) {
            Some(content) => {
                use base64::Engine;
                use base64::engine::general_purpose::STANDARD;
                let bytes = STANDARD
                    .decode(content)
                    .map_err(|_| KugouError::local("lyric content is not valid base64"))?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            None => Ok(String::new()),
        }
    }

    /// 酷狗卡拉OK（KRC）格式的歌词下载。`content` 字段是加密且经 zlib 压缩的 KRC 文档的
    /// 原始 base64；解密在 `ui::lyrics::krc::decrypt_krc`，所以此处文本保持不透明。
    pub async fn lyric_krc(&self, id: &str, accesskey: &str) -> Result<String, KugouError> {
        let response = self.lyric_download(id, accesskey, "krc").await?;
        Ok(response
            .body
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// 榜单列表（`/ocean/v6/rank/list`），每一项都带一张封面。对照
    /// `RawRankApi.GetRankListAsync`，但用 `withsong: 0`：否则服务端会把每个榜单的完整
    /// 歌曲实体挂到每一项上（几 MB 的 JSON，UI 从不读——榜单卡片只显示名称 + 封面），
    /// 而 serde_json::Value 树会让进入视图时的内存步进多出 ~100 MB。
    pub async fn rank_list(&self) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new(GATEWAY, "/ocean/v6/rank/list")
            .param("plat", 2)
            .param("withsong", 0)
            .param("parentid", 0);
        self.request(spec).await
    }

    /// 单个榜单的歌曲（`/openapi/kmr/v2/rank/audio`），分页。需要 `kg-tid: 369` 头。
    /// 对照 `RawRankApi.GetRankAudioAsync`。
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

    /// 每日推荐歌曲（`/everyday_song_recommend`），经 `x-router` 路由到每日推荐上游。
    /// 对照 `RawDiscoveryApi.GetRecommendSongAsync`。
    pub async fn everyday_recommend(&self) -> Result<KugouResponse, KugouError> {
        let userid = self.credentials().userid.unwrap_or(0).to_string();
        let spec = KugouRequest::new(GATEWAY, "/everyday_song_recommend")
            .post()
            .router("everydayrec.service.kugou.com")
            .json(serde_json::json!({
                "platform": "android",
                "userid": userid,
            }));
        self.request(spec).await
    }

    /// 单个 hash 的播放 URL。对照 `song_url.js`：带 key、走 android 签名（该模块的
    /// `notSign` 旗标在参考客户端里是空操作），dfid 每次请求随机。`free_part` 对仅 VIP
    /// 可听的歌曲返回试听片段（约 60s @128k）。
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

/// 每日免费 VIP 领取流程的结果。对照 .NET 参考客户端的 youth/日 VIP 权益：先领一天，再升级。
#[derive(Debug, Clone, PartialEq)]
pub enum VipClaimOutcome {
    /// 该账号今天拿到了一天 VIP（全新领取，或把已有的 `tvip` 奖励升级）。
    Claimed,
    /// 今天已经覆盖——无事可做。
    AlreadyClaimed,
    /// 账号未登录；没有尝试领取。
    NotLoggedIn,
    /// 服务端某一步失败（记录查询或领取本身）。
    Failed { reason: String },
}

impl KugouClient {
    /// 今天免费的一天 VIP。对照 `youth_day_vip.js`：对 youth 充值端点发完不管。
    pub async fn receive_one_day_vip(&self) -> Result<KugouResponse, KugouError> {
        let today = today_utc();
        let spec = KugouRequest::new(GATEWAY, "/youth/v1/recharge/receive_vip_listen_song")
            .post()
            .param("source_id", 90139)
            .param("receive_day", today);
        self.request(spec).await
    }

    /// 升级当天的 VIP 奖励（tvip -> dvip/svip）。对照 `youth_day_vip_upgrade.js`。
    pub async fn upgrade_vip_reward(&self) -> Result<KugouResponse, KugouError> {
        let session = self.credentials();
        let spec = KugouRequest::new(GATEWAY, "/youth/v1/listen_song/upgrade_vip_reward")
            .post()
            .param("kugouid", session.userid.unwrap_or(0))
            .param("ad_type", 1);
        self.request(spec).await
    }

    /// 当月的 VIP 领取记录。对照 `youth_month_vip_record.js`；调用方检查 `data/list` 里
    /// 今天那条，判断是否还需要一次全新领取。
    pub async fn get_vip_record(&self) -> Result<KugouResponse, KugouError> {
        let spec = KugouRequest::new(GATEWAY, "/youth/v1/activity/get_month_vip_record")
            .param("latest_limit", 100);
        self.request(spec).await
    }

    /// 拉取 VIP 权益明细并缓存到会话上，好让设置页之后直接展示。把错误返回给调用方，
    /// 以便区分「拉取失败」与「服务端说没有生效中的 VIP」。
    pub async fn refresh_vip_detail(&self) -> Result<(), KugouError> {
        let response = self.user_vip_detail().await?;
        self.store_vip_detail(response.body);
        Ok(())
    }

    /// 保证该账号今天有 VIP，但在 VIP 确实生效期间，每个自然日最多只打一次领取端点。
    ///
    /// 防循环：首次尝试就把尝试标记持久化，于是并发的「启动/登录」触发不会重复领取。
    /// 跳过守卫还额外要求今天的 VIP 在缓存明细里确实生效；失败的尝试会在下次应用启动或
    /// 重新登录时重试，这正是让会员在登录过期窗口之间不断档的原因。
    pub async fn ensure_daily_vip(&self) -> VipClaimOutcome {
        if !self.logged_in() {
            return VipClaimOutcome::NotLoggedIn;
        }

        let today = today_utc();

        // 同日守卫：只有今天的尝试确实拿到了 VIP 才跳过。失败的尝试（例如在登录已过期时
        // 发起、在 get_vip_record 处以 51002 失败）不该把这一天用掉——否则标记会毒化领取，
        // 即使用户重新登录也再也不重试。重试频率仍有界：ensure_daily_vip 只在应用启动和
        // 重新登录之后运行。
        if self.last_claim_day().as_deref() == Some(today.as_str()) && self.has_active_vip() {
            return VipClaimOutcome::AlreadyClaimed;
        }

        // 立刻标记这次尝试，好让同一会话里相互竞争的启动与登录触发不重复领取；守卫读的
        // 是真实 VIP 状态，所以失败的尝试下次启动仍会重试。
        self.mark_claim_attempted();

        let record = match self.get_vip_record().await {
            Ok(response) => response,
            Err(err) => {
                return VipClaimOutcome::Failed {
                    reason: format!("query vip record: {err}"),
                };
            }
        };

        let list = record.body.pointer("/data/list").and_then(Value::as_array);

        // 顺带刷新缓存的 VIP 明细给设置页（尽力而为）。
        let _ = self.refresh_vip_detail().await;

        let today_entry = list.and_then(|entries| {
            entries
                .iter()
                .find(|item| item.get("day").and_then(Value::as_str) == Some(today.as_str()))
        });

        match today_entry {
            // 今天还没领取：领一天再升级。
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
            // 已领取但仍是最基础的 tvip 奖励，升级它。
            Some(entry)
                if entry.get("receive_vip").and_then(Value::as_i64) == Some(1)
                    && entry.get("vip_type").and_then(Value::as_str) == Some("tvip") =>
            {
                let _ = self.upgrade_vip_reward().await;
                let _ = self.refresh_vip_detail().await;
                VipClaimOutcome::Claimed
            }
            // 已覆盖。
            _ => VipClaimOutcome::AlreadyClaimed,
        }
    }
}
