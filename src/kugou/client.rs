//! KuGou HTTP client: device identity, session persistence and the signed
//! gateway request engine, mirroring `KuGouMusicApi/util/request.js`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zed_reqwest::{Client, Method, header::HeaderMap, header::HeaderName, header::HeaderValue};

use super::{crypto, sign};
use crate::toasts::{Toast, emit_toast};

/// Payload error_code that empirically accompanies requests made with an
/// expired KuGou login (playlists, VIP detail, ...): the same session sees
/// these while stream URLs signed for the old token return 403.
const KUGOU_ERR_LOGIN_EXPIRED: i64 = 20017;

/// How long between "login expired" toasts: the reminder must not re-fire on
/// every failing request, but should re-appear if a later window also fails.
const LOGIN_EXPIRED_TOAST_THROTTLE: Duration = Duration::from_secs(30 * 60);

fn login_expired_toast_state() -> &'static Mutex<Option<Instant>> {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

fn notify_login_expired() {
    let last = login_expired_toast_state();
    let mut guard = last.lock().unwrap();
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
    *login_expired_toast_state().lock().unwrap() = None;
}

pub const GATEWAY: &str = "https://gateway.kugou.com";
pub const APPID: i64 = 3116;
pub const CLIENTVER: i64 = 11440;
pub const SRCAPPID: i64 = 2919;
const USER_AGENT: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

/// Seconds since the Unix epoch, or 0 if the clock is before it.
pub(super) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Today's UTC date as `yyyy-MM-dd`, used as the daily VIP-claim marker.
pub(super) fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum KugouError {
    #[error("network error: {0}")]
    Http(#[from] zed_reqwest::Error),
    #[error("api error {status}: {msg}")]
    Api { status: i64, msg: String },
}

/// Persistent device + login state, stored as `kugou_session.json` next to
/// the other app data.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KugouSession {
    pub guid: String,
    pub mid: String,
    pub dev: String,
    pub dfid: String,
    pub token: Option<String>,
    pub userid: Option<i64>,
    /// opaque ticket returned by the token refresh endpoint
    pub t1: Option<String>,
    pub vip_type: Option<i64>,
    pub vip_token: Option<String>,
    /// Optional VIP detail fetched from `/v1/get_union_vip` for display.
    pub vip_detail: Option<Value>,
    /// Local date (yyyy-MM-dd) the daily VIP claim was last attempted. Used
    /// to avoid re-claiming (and re-hitting the endpoint) across restarts on
    /// the same day.
    pub last_claim_day: Option<String>,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
}

impl KugouSession {
    /// Fresh random device identity (mirrors the cookie bootstrap in
    /// server.js: guid = md5(uuid4), mid = calculate_mid(guid), dev random).
    pub fn generate() -> Self {
        let guid = crypto::md5_hex(&crypto::uuid_v4());
        let mid = crypto::calculate_mid(&guid);
        Self {
            guid,
            mid,
            dev: crypto::random_string(10).to_uppercase(),
            dfid: "-".to_string(),
            token: None,
            userid: None,
            t1: None,
            vip_type: None,
            vip_token: None,
            vip_detail: None,
            last_claim_day: None,
            nickname: None,
            avatar_url: None,
        }
    }

    fn load(path: &Path) -> Option<Self> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // Missing credentials is the normal first-run path; stay quiet.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(%err, "no kugou session on disk yet");
                return None;
            }
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "failed to read kugou session");
                return None;
            }
        };
        serde_json::from_str(&contents)
            .map_err(|err| tracing::warn!(%err, "failed to decode kugou session"))
            .ok()
    }

    fn save(&self, path: &Path) {
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%err, "failed to create kugou session dir");
            return;
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(%err, "failed to serialize kugou session");
                return;
            }
        };
        // Write to a temporary file and rename it into place (same pattern as
        // `playback/session_storage.rs`): an in-place truncate+rewrite can
        // leave a truncated session file behind when the process dies mid-write.
        // `fs::rename` replaces an existing target on Windows, so the swap is
        // atomic on every supported platform.
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json) {
            tracing::warn!(%err, "failed to write kugou session temp file");
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            tracing::warn!(%err, "failed to persist kugou session");
            let _ = std::fs::remove_file(&tmp);
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
    /// Extra headers sent verbatim (e.g. `kg-tid` on rank endpoints).
    pub headers: Vec<(String, String)>,
    /// JSON body: serialized once, that exact string is signed and sent.
    pub json_body: Option<Value>,
    pub x_router: Option<&'static str>,
    pub encrypt: Encrypt,
    /// adds the `key` parameter required by /v5/url
    pub need_key: bool,
    /// replaces dfid with a random one for this request only
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

/// Cached profile of the logged-in user (nickname + avatar). Persisted inside
/// `KugouSession` so it never needs a network call after the first fetch.
#[derive(Clone, Debug)]
pub struct UserProfile {
    pub nickname: String,
    pub avatar_url: String,
}

pub struct KugouClient {
    http: Client,
    session: Mutex<KugouSession>,
    session_path: PathBuf,
}

impl KugouClient {
    pub fn new(data_dir: &Path) -> Self {
        let session_path = data_dir.join("kugou_session.json");
        let session = KugouSession::load(&session_path).unwrap_or_else(KugouSession::generate);
        Self {
            // a hung gateway request must not pin `pending_fetches` forever:
            // without a timeout a track that fails to resolve can never be
            // retried (the merge guard drops every later click)
            http: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            session: Mutex::new(session),
            session_path,
        }
    }

    /// Locks the session, recovering the inner value if the lock was poisoned
    /// by a panicked thread rather than panicking the caller. The app installs
    /// a panic hook, so a poisoned session lock must not take down playback or
    /// the UI.
    fn session_guard(&self) -> std::sync::MutexGuard<'_, KugouSession> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn session_snapshot(&self) -> KugouSession {
        self.session_guard().clone()
    }

    pub fn logged_in(&self) -> bool {
        self.session_guard()
            .token
            .as_deref()
            .is_some_and(|t| !t.is_empty())
    }

    pub fn store_login(&self, token: String, userid: i64, extra: LoginExtras) {
        let mut session = self.session_guard();
        session.token = Some(token);
        session.userid = Some(userid);
        session.t1 = extra.t1.or(session.t1.take());
        session.vip_type = extra.vip_type;
        session.vip_token = extra.vip_token;
        // VIP detail belongs to the previous login: drop it so the settings
        // page shows the fresh account's status (fetch_profile refreshes it).
        session.vip_detail = None;
        // A claim attempt made under the previous (possibly expired) login
        // must not consume today's attempt for the fresh session: an expired
        // login fails the claim at get_vip_record and would otherwise block
        // retrying until tomorrow even though the user just signed in again.
        session.last_claim_day = None;
        session.save(&self.session_path);
        // A fresh login may outlive the previous throttle window's start.
        reset_login_expired_toast();
    }

    pub fn logout(&self) {
        let mut session = self.session_guard();
        session.token = None;
        session.userid = None;
        session.t1 = None;
        session.vip_type = None;
        session.vip_token = None;
        session.nickname = None;
        session.avatar_url = None;
        session.vip_detail = None;
        session.save(&self.session_path);
    }

    /// Profile from the persisted session, without touching the network.
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

    /// Persists the fetched profile for later sessions.
    pub fn update_user_profile(&self, profile: &UserProfile) {
        let mut session = self.session_guard();
        session.nickname = Some(profile.nickname.clone());
        session.avatar_url = Some(profile.avatar_url.clone());
        session.save(&self.session_path);
    }

    /// Local date string of the last daily-VIP claim attempt (yyyy-MM-dd),
    /// or `None` if never attempted. Persisted so a failed claim isn't retried
    /// over and over across restarts on the same day.
    pub fn last_claim_day(&self) -> Option<String> {
        self.session_guard().last_claim_day.clone()
    }

    /// Marks today's VIP claim as attempted and persists it.
    pub fn mark_claim_attempted(&self) {
        let today = today_utc();
        let mut session = self.session_guard();
        session.last_claim_day = Some(today);
        session.save(&self.session_path);
    }

    /// Stores the raw `/v1/get_union_vip` payload for the settings page to
    /// render VIP status without another request later.
    pub fn store_vip_detail(&self, detail: Value) {
        let mut session = self.session_guard();
        session.vip_detail = Some(detail);
        session.save(&self.session_path);
    }

    /// Raw `/v1/get_union_vip` payload cached on the session, if any.
    pub fn vip_detail(&self) -> Option<Value> {
        self.session_guard().vip_detail.clone()
    }

    /// Whether the cached VIP detail shows at least one active product
    /// (`busi_vip[*].is_vip == 1`). The top-level `is_vip` on that endpoint
    /// stays 0 even while products are active, so this scans the array.
    pub fn has_active_vip(&self) -> bool {
        let Some(detail) = self.vip_detail() else {
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

    /// Builds the signed request for `spec`.
    fn build_signed(&self, spec: &KugouRequest) -> Result<zed_reqwest::RequestBuilder, KugouError> {
        let session = self.session_guard().clone();
        let dfid = if spec.random_dfid {
            crypto::random_string(24)
        } else {
            session.dfid.clone()
        };
        let clienttime = unix_now_secs();

        let mut params = BTreeMap::new();
        // The lite client derives mid from the (usually random) dfid at
        // request time rather than a fixed device mid.
        let mid = crypto::calculate_mid(&dfid);
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

        let body = spec.body_string();
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
        if let Some(json) = &spec.json_body {
            builder = builder.body(json.to_string());
        }
        Ok(builder)
    }

    /// Executes a signed request and checks the payload-level status fields
    /// (the JS client treats `status == 0` / non-zero `error_code` as failure).
    /// The response bytes are parsed in place: the multi-megabyte bodies some
    /// endpoints return must not be copied once more before parsing.
    pub async fn request(&self, spec: KugouRequest) -> Result<KugouResponse, KugouError> {
        let response = self.build_signed(&spec)?.send().await?;
        let bytes = response.bytes().await?;
        let body: Value = serde_json::from_slice(&bytes)
            .ok()
            .or_else(|| parse_jsonp(&bytes))
            .unwrap_or(Value::Null);
        if let Err(err) = check_payload_status(&body) {
            if is_login_expired(&err) {
                notify_login_expired();
            }
            return Err(err);
        }
        Ok(KugouResponse { body })
    }
}

#[derive(Default)]
pub struct LoginExtras {
    pub t1: Option<String>,
    pub vip_type: Option<i64>,
    pub vip_token: Option<String>,
}

/// Some web endpoints answer with a JSONP wrapper (`callback123({...})`);
/// strip it so the payload parses as plain JSON.
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
        // No full-body snippet here: the failing bodies can be multi-MB and
        // serializing them just for an error string doubles the peak of an
        // already failed request. status/error_code carry the signal.
        return Err(KugouError::Api {
            status: status.unwrap_or(-1),
            msg: format!("{msg} | error_code={error_code:?}"),
        });
    }
    Ok(())
}

/// Whether the payload error is the login-expired signature.
fn is_login_expired(err: &KugouError) -> bool {
    match err {
        KugouError::Api { msg, .. } => {
            msg.contains(&format!("error_code=Some({KUGOU_ERR_LOGIN_EXPIRED})"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_has_consistent_identity() {
        let session = KugouSession::generate();
        assert_eq!(session.mid, crypto::calculate_mid(&session.guid));
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
        session.save(&path);
        let loaded = KugouSession::load(&path).expect("session loads");
        assert_eq!(loaded.token.as_deref(), Some("tok"));
        assert_eq!(loaded.userid, Some(42));
        assert_eq!(loaded.guid, session.guid);
    }
}
