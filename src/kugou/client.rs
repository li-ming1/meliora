//! 酷狗 HTTP 客户端：设备身份、会话持久化与签名的网关请求引擎，
//! 对照 `KuGouMusicApi/util/request.js`。

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zed_reqwest::{Client, Method, header::HeaderMap, header::HeaderName, header::HeaderValue};

use super::{crypto, sign};
use crate::session_io;
use crate::sync::RecoveredMutex;
use crate::toasts::{Toast, emit_toast};

/// 实测伴随「酷狗登录已过期」请求的 payload error_code（歌单、VIP 明细等）：同一会话会
/// 看到这些错误，而为旧 token 签名的流 URL 则返回 403。
const KUGOU_ERR_LOGIN_EXPIRED: i64 = 20017;

/// 「登录已过期」toast 的间隔：提醒不能在每个失败的请求上重复弹，但后面的时间窗也失败时
/// 应当再现。
const LOGIN_EXPIRED_TOAST_THROTTLE: Duration = Duration::from_secs(30 * 60);

fn login_expired_toast_state() -> &'static Mutex<Option<Instant>> {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

fn notify_login_expired() {
    let last = login_expired_toast_state();
    let mut guard = last.lock_recovered();
    if let Some(at) = *guard
        && at.elapsed() < LOGIN_EXPIRED_TOAST_THROTTLE
    {
        return;
    }
    *guard = Some(Instant::now());
    drop(guard);
    emit_toast(Toast::warning(tr!(
        "KUGOU_LOGIN_EXPIRED",
        "Kugou login expired - sign in again in Settings - KuGou Music."
    )));
}

fn reset_login_expired_toast() {
    *login_expired_toast_state().lock_recovered() = None;
}

pub const GATEWAY: &str = "https://gateway.kugou.com";
pub const APPID: i64 = 3116;
pub const CLIENTVER: i64 = 11440;
pub const SRCAPPID: i64 = 2919;
const USER_AGENT: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

/// Unix epoch 起的秒数；时钟早于 epoch 时为 0。
pub(super) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 今天的 UTC 日期，格式 `yyyy-MM-dd`，用作每日 VIP 领取的标记。
pub(super) fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum KugouError {
    #[error("network error: {0}")]
    Http(#[from] zed_reqwest::Error),
    #[error("api error {status}: {msg}")]
    Api {
        status: i64,
        msg: String,
        /// 载荷层的 `error_code`（响应体带了才非空）；调用方按它匹配，而不是按消息文本。
        error_code: Option<i64>,
    },
}

impl KugouError {
    /// 本地判定出来的失败（响应缺字段、信封装不进 RSA 分块、载荷不是 JSON…）：
    /// `status` 取 `-1` 这个"没有 HTTP 状态"哨兵，`error_code: None` 让它不参与
    /// 重登判定。此前四个站点逐字重复这两行默认值。
    pub fn local(msg: impl Into<String>) -> Self {
        Self::Api {
            status: -1,
            msg: msg.into(),
            error_code: None,
        }
    }
}

/// 持久的设备 + 登录状态，与其他应用数据并存为 `kugou_session.json`。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KugouSession {
    pub guid: String,
    /// 请求期的 mid：`calculate_mid(dfid)`，每次会话进入内存时刷新（旧文件存的是
    /// `calculate_mid(guid)`，而没有任何请求用那个值签过名）。
    #[serde(default)]
    pub mid: String,
    pub dev: String,
    pub dfid: String,
    pub token: Option<String>,
    pub userid: Option<i64>,
    /// 可选的 VIP 明细，从 `/v1/get_union_vip` 取来用于展示。
    pub vip_detail: Option<Value>,
    /// 最后一次尝试每日 VIP 领取的 UTC 日期（yyyy-MM-dd）。用于避免同一天跨重启重复领取
    /// （以及重复打端点）。
    pub last_claim_day: Option<String>,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
}

impl KugouSession {
    /// 全新的随机设备身份（对照 server.js 里的 cookie 引导：guid = md5(uuid4)，dev 随机）。
    /// 请求用 `mid = calculate_mid(dfid)` 签名，所以存下来的 mid 源自 dfid（初始为 "-"）
    /// 而不是 guid。
    pub fn generate() -> Self {
        let guid = crypto::md5_hex(&crypto::uuid_v4());
        let dfid = "-".to_string();
        Self {
            guid,
            mid: crypto::calculate_mid(&dfid),
            dev: crypto::random_string(10),
            dfid,
            token: None,
            userid: None,
            vip_detail: None,
            last_claim_day: None,
            nickname: None,
            avatar_url: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Encrypt {
    Android,
    Web,
}

pub struct KugouRequest {
    pub base_url: &'static str,
    pub path: &'static str,
    pub method: Method,
    pub query: BTreeMap<String, String>,
    /// 原样发送的额外请求头（例如榜单端点上的 `kg-tid`）。
    pub headers: Vec<(String, String)>,
    /// JSON 请求体：只序列化一次，签名与发送用的都是那个精确字符串。
    pub json_body: Option<Value>,
    pub x_router: Option<&'static str>,
    pub encrypt: Encrypt,
    /// 加上 /v5/url 要求的 `key` 参数
    pub need_key: bool,
    /// 仅对本次请求把 dfid 换成随机值
    pub random_dfid: bool,
    pub clear_default_params: bool,
}

impl KugouRequest {
    pub fn new(base_url: &'static str, path: &'static str) -> Self {
        Self {
            base_url,
            path,
            method: Method::GET,
            query: BTreeMap::new(),
            headers: Vec::new(),
            json_body: None,
            x_router: None,
            encrypt: Encrypt::Android,
            need_key: false,
            random_dfid: false,
            clear_default_params: false,
        }
    }

    pub fn header(mut self, name: &str, value: impl ToString) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn post(mut self) -> Self {
        self.method = Method::POST;
        self
    }

    pub fn param(mut self, key: &str, value: impl ToString) -> Self {
        self.query.insert(key.to_string(), value.to_string());
        self
    }

    pub fn json(mut self, body: Value) -> Self {
        self.json_body = Some(body);
        self
    }

    pub fn router(mut self, router: &'static str) -> Self {
        self.x_router = Some(router);
        self
    }

    pub fn web(mut self) -> Self {
        self.encrypt = Encrypt::Web;
        self
    }

    pub fn with_key(mut self) -> Self {
        self.need_key = true;
        self
    }

    pub fn random_dfid(mut self) -> Self {
        self.random_dfid = true;
        self
    }

    pub fn no_default_params(mut self) -> Self {
        self.clear_default_params = true;
        self
    }

    fn body_string(&self) -> String {
        self.json_body
            .as_ref()
            .map(|json| json.to_string())
            .unwrap_or_default()
    }
}

pub struct KugouResponse {
    pub body: Value,
}

/// 共享的资料结构住在 `session_io`；这里再导出，让既有的 `client::UserProfile` 路径继续可解析。
pub use crate::session_io::UserProfile;

/// API 方法构造签名请求时所读的会话字段的窄拷贝。整体 clone 会话会连带可能很大的
/// `vip_detail` JSON 树；构造请求的调用方只会读这些标量。
#[derive(Clone, Debug)]
pub struct KugouCredentials {
    pub guid: String,
    pub dev: String,
    pub dfid: String,
    pub token: Option<String>,
    pub userid: Option<i64>,
}

pub struct KugouClient {
    http: Client,
    session: Mutex<KugouSession>,
    session_path: PathBuf,
}

impl KugouClient {
    pub fn new(data_dir: &Path) -> Self {
        let session_path = data_dir.join("kugou_session.json");
        let mut session =
            session_io::load(&session_path, "kugou").unwrap_or_else(KugouSession::generate);
        // dfid 在进程内从不改变，所以这次重建是每次启动唯一的一次成本，换来 `build_signed`
        // 可以放心使用缓存的 mid。
        session.mid = crypto::calculate_mid(&session.dfid);
        Self {
            // 卡死的网关请求不能把 `pending_fetches` 永久钉住：没有超时的话，一首解析失败
            // 的曲目将永远无法重试（合并守卫会丢掉之后的每次点击）
            http: match session_io::gateway_http_builder().build() {
                Ok(client) => client,
                Err(error) => {
                    // 与更新器/导入器的退化做法保持一致：拒绝接受这个定制 builder 的 TLS
                    // 后端不该让启动中止。
                    tracing::warn!(%error, "kugou: falling back to the default HTTP client");
                    Client::new()
                }
            },
            session: Mutex::new(session),
            session_path,
        }
    }

    /// 会话锁。毒化恢复策略统一在 [`crate::sync`]：一个已经 panic 的线程不
    /// 该把播放或 UI 一起带走。
    fn session_guard(&self) -> MutexGuard<'_, KugouSession> {
        self.session.lock_recovered()
    }

    /// 供构造请求的调用方做的窄凭据读取（见 [`KugouCredentials`]）；避免每个请求 clone
    /// 一次 `vip_detail`。
    pub fn credentials(&self) -> KugouCredentials {
        let session = self.session_guard();
        KugouCredentials {
            guid: session.guid.clone(),
            dev: session.dev.clone(),
            dfid: session.dfid.clone(),
            token: session.token.clone(),
            userid: session.userid,
        }
    }

    /// 歌单类接口只要这两样：`userid` 缺省 0、`token` 缺省空串（五个接口逐字
    /// 重复过这段取值）。走 `credentials()` 会顺手克隆 guid/dev/dfid 三个用不到
    /// 的字符串。
    pub fn userid_token(&self) -> (i64, String) {
        let session = self.session_guard();
        (
            session.userid.unwrap_or(0),
            session.token.clone().unwrap_or_default(),
        )
    }

    pub fn logged_in(&self) -> bool {
        session_io::credential_present(self.session_guard().token.as_deref())
    }

    pub fn store_login(&self, token: String, userid: i64) {
        let mut session = self.session_guard();
        session.token = Some(token);
        session.userid = Some(userid);
        // VIP 明细属于上一次登录：丢掉它，好让设置页显示新账号的状态（fetch_profile 会刷新）。
        session.vip_detail = None;
        // 在上一次（可能已过期）登录下做的领取尝试，不该替新会话把今天的尝试用掉：过期登录
        // 会在 get_vip_record 处让领取失败，否则即使用户刚重新登录，也会一直挡住重试直到明天。
        session.last_claim_day = None;
        session_io::save(&*session, &self.session_path, "kugou");
        // toast 路径会拿它自己的节流锁并触碰 UI 层；别让并发的请求构造者在会话互斥锁上等
        // 它跑完。
        drop(session);
        // 新登录可能晚于上一个节流窗口的起点。
        reset_login_expired_toast();
    }

    pub fn logout(&self) {
        let mut session = self.session_guard();
        session.token = None;
        session.userid = None;
        session.nickname = None;
        session.avatar_url = None;
        session.vip_detail = None;
        session_io::save(&*session, &self.session_path, "kugou");
    }

    /// 从持久化会话取资料，不碰网络。
    pub fn cached_user_profile(&self) -> Option<UserProfile> {
        let session = self.session_guard();
        let nickname = session.nickname.clone()?;
        if nickname.is_empty() {
            return None;
        }
        Some(UserProfile {
            nickname,
            avatar_url: session.avatar_url.clone().unwrap_or_default(),
        })
    }

    /// 把取到的资料持久化给之后的会话。
    pub fn update_user_profile(&self, profile: &UserProfile) {
        let mut session = self.session_guard();
        session.nickname = Some(profile.nickname.clone());
        session.avatar_url = Some(profile.avatar_url.clone());
        session_io::save(&*session, &self.session_path, "kugou");
    }

    /// 最后一次每日 VIP 领取尝试的 UTC 日期串（yyyy-MM-dd），从未尝试过则为 `None`。持久化
    /// 它，好让失败的领取不会在同一天跨重启反复重试。
    pub fn last_claim_day(&self) -> Option<String> {
        self.session_guard().last_claim_day.clone()
    }

    /// 把今天的 VIP 领取标为已尝试并持久化。
    pub fn mark_claim_attempted(&self) {
        let today = today_utc();
        let mut session = self.session_guard();
        session.last_claim_day = Some(today);
        session_io::save(&*session, &self.session_path, "kugou");
    }

    /// 存下 `/v1/get_union_vip` 的原始载荷，让设置页之后无需再发请求就能渲染 VIP 状态。
    pub fn store_vip_detail(&self, detail: Value) {
        let mut session = self.session_guard();
        session.vip_detail = Some(detail);
        session_io::save(&*session, &self.session_path, "kugou");
    }

    /// 缓存在会话上的 `/v1/get_union_vip` 原始载荷（若有）。
    pub fn vip_detail(&self) -> Option<Value> {
        self.session_guard().vip_detail.clone()
    }

    /// 缓存的 VIP 明细里是否有至少一个生效商品（`busi_vip[*].is_vip == 1`）。该端点的顶层
    /// `is_vip` 即使商品生效也仍是 0，所以这里要扫描数组。
    pub fn has_active_vip(&self) -> bool {
        let session = self.session_guard();
        let Some(detail) = session.vip_detail.as_ref() else {
            return false;
        };
        detail
            .pointer("/data/busi_vip")
            .or_else(|| detail.get("busi_vip"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|item| item.get("is_vip").and_then(Value::as_i64) == Some(1))
    }

    /// 为 `spec` 构造签名请求。`body` 是调用方精确序列化一次的请求体：在这里签名，随后
    /// 移进出站的请求。全程按引用持会话锁：本函数从不 await，守卫不可能在一个 poll 点
    /// 之间被挂起。
    fn build_signed(
        &self,
        spec: &KugouRequest,
        body: String,
    ) -> Result<zed_reqwest::RequestBuilder, KugouError> {
        let session = self.session_guard();
        let dfid = if spec.random_dfid {
            crypto::random_string(24)
        } else {
            session.dfid.clone()
        };
        let clienttime = unix_now_secs();

        let mut params = BTreeMap::new();
        // 概念版客户端在请求期从（通常随机的）dfid 推导 mid，而不是用固定的设备 mid。
        // 只有随机 dfid 那条路会重算：会话的 dfid 在进程内固定，所以它的 mid 缓存在会话上
        // （由 `new`/`generate` 刷新）。
        let mid = if spec.random_dfid {
            crypto::calculate_mid(&dfid)
        } else {
            session.mid.clone()
        };
        if !spec.clear_default_params {
            params.insert("dfid".to_string(), dfid.clone());
            params.insert("mid".to_string(), mid.clone());
            params.insert("uuid".to_string(), "-".to_string());
            params.insert("appid".to_string(), APPID.to_string());
            params.insert("clientver".to_string(), CLIENTVER.to_string());
            params.insert("clienttime".to_string(), clienttime.to_string());
            if let Some(token) = &session.token {
                params.insert("token".to_string(), token.clone());
            }
            if let Some(userid) = session.userid.filter(|id| *id != 0) {
                params.insert("userid".to_string(), userid.to_string());
            }
        }
        for (key, value) in &spec.query {
            params.insert(key.clone(), value.clone());
        }

        if spec.need_key {
            let userid = params.get("userid").cloned().unwrap_or_else(|| "0".into());
            let key = sign::sign_key(
                params.get("hash").map(|s| s.as_str()).unwrap_or(""),
                params.get("mid").map(|s| s.as_str()).unwrap_or(""),
                &userid,
                params.get("appid").map(|s| s.as_str()).unwrap_or("3116"),
            );
            params.insert("key".to_string(), key);
        }

        let signature = match spec.encrypt {
            Encrypt::Android => sign::signature_android(&params, &body),
            Encrypt::Web => sign::signature_web(&params),
        };
        if !signature.is_empty() {
            params.insert("signature".to_string(), signature);
        }

        let mut headers = HeaderMap::new();
        let set_header = |headers: &mut HeaderMap, name: &str, value: &str| {
            if let (Ok(name), Ok(value)) =
                (HeaderName::try_from(name), HeaderValue::try_from(value))
            {
                headers.insert(name, value);
            }
        };
        set_header(&mut headers, "User-Agent", USER_AGENT);
        set_header(&mut headers, "dfid", &dfid);
        set_header(&mut headers, "clienttime", &clienttime.to_string());
        set_header(&mut headers, "mid", &mid);
        set_header(&mut headers, "kg-rc", "1");
        set_header(&mut headers, "kg-thash", "5d816a0");
        set_header(&mut headers, "kg-rec", "1");
        set_header(&mut headers, "kg-rf", "B9EDA08A64250DEFFBCADDEE00F8F25F");
        for (name, value) in &spec.headers {
            set_header(&mut headers, name, value);
        }
        if let Some(router) = spec.x_router {
            set_header(&mut headers, "x-router", router);
        }
        if spec.json_body.is_some() {
            set_header(&mut headers, "Content-Type", "application/json");
        }

        let url = format!("{}{}", spec.base_url, spec.path);
        let mut builder = self
            .http
            .request(spec.method.clone(), &url)
            .headers(headers)
            .query(&params);
        if spec.json_body.is_some() {
            builder = builder.body(body);
        }
        Ok(builder)
    }

    /// 执行签名请求并检查载荷层的 status 字段（JS 客户端把 `status == 0` / 非零
    /// `error_code` 当作失败）。响应字节就地解析：某些端点返回的几 MB 响应体在解析前不允许
    /// 再多复制一次。既不是 JSON 也不是 JSONP 的响应（例如网关的 HTML 错误页）是显式错误，
    /// 而不是一个全空的成功载荷。
    pub async fn request(&self, spec: KugouRequest) -> Result<KugouResponse, KugouError> {
        let body = spec.body_string();
        let response = self
            .build_signed(&spec, body)?
            .send()
            .await
            .inspect_err(|err| {
                tracing::debug!(path = spec.path, error = %err, "kugou request failed");
            })?;
        let bytes = response.bytes().await.inspect_err(|err| {
            tracing::debug!(path = spec.path, error = %err, "kugou request failed");
        })?;
        let body: Value = match serde_json::from_slice(&bytes)
            .ok()
            .or_else(|| parse_jsonp(&bytes))
        {
            Some(body) => body,
            None => {
                tracing::warn!(
                    path = spec.path,
                    bytes = bytes.len(),
                    "kugou response is not JSON/JSONP"
                );
                return Err(KugouError::local(format!(
                    "response is not JSON/JSONP ({} bytes)",
                    bytes.len()
                )));
            }
        };
        if let Err(err) = check_payload_status(&body) {
            if is_login_expired(&err) {
                notify_login_expired();
            }
            return Err(err);
        }
        Ok(KugouResponse { body })
    }
}

/// 某些 web 端点用 JSONP 包装应答（`callback123({...})`）；剥掉它，载荷才能按普通 JSON 解析。
fn parse_jsonp(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    let start = text.find('(')?;
    let end = text.rfind(')')?;
    if start >= end {
        return None;
    }
    serde_json::from_str(&text[start + 1..end]).ok()
}

fn check_payload_status(body: &Value) -> Result<(), KugouError> {
    let status = body.get("status").and_then(Value::as_i64);
    let error_code = body.get("error_code").and_then(Value::as_i64);
    if status == Some(0) || error_code.is_some_and(|code| code != 0) {
        let msg = body
            .get("msg")
            .or_else(|| body.get("err_msg"))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        // 这里不贴完整响应体片段：失败的响应体可达几 MB，仅为一条错误串就序列化它们，会让
        // 已经失败的请求的峰值翻倍。status/error_code 已带上信号。
        return Err(KugouError::Api {
            status: status.unwrap_or(-1),
            msg,
            error_code,
        });
    }
    Ok(())
}

/// 载荷错误是否为「登录过期」的特征码。
fn is_login_expired(err: &KugouError) -> bool {
    match err {
        KugouError::Api { error_code, .. } => *error_code == Some(KUGOU_ERR_LOGIN_EXPIRED),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_has_consistent_identity() {
        let session = KugouSession::generate();
        assert_eq!(session.mid, crypto::calculate_mid(&session.dfid));
        assert_eq!(session.dfid, "-");
        assert_eq!(session.dev.len(), 10);
        assert!(
            session
                .dev
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn session_round_trips_through_disk() {
        let dir = crate::test_support::TestDir::new("kugou-session-test");
        let path = dir.join("kugou_session.json");
        let session = KugouSession {
            token: Some("tok".into()),
            userid: Some(42),
            ..KugouSession::generate()
        };
        session_io::save(&session, &path, "kugou");
        let loaded = session_io::load::<KugouSession>(&path, "kugou").expect("session loads");
        assert_eq!(loaded.token.as_deref(), Some("tok"));
        assert_eq!(loaded.userid, Some(42));
        assert_eq!(loaded.guid, session.guid);
    }
}
