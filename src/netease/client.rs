//! NetEase HTTP client: device identity, session persistence, cookie
//! synthesis and the weapi/eapi request engine, mirroring
//! `NeteaseCloudMusicApi/util/request.js`.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zed_reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};

use super::crypto;

pub const DOMAIN: &str = "https://music.163.com";
pub const EAPI_DOMAIN: &str = "https://interfacepc.music.163.com";

/// Default desktop client identity from the reference `osMap.pc`.
const OSVER: &str = "Microsoft-Windows-10-Professional-build-19045-64bit";
const APPVER: &str = "3.1.17.204416";
const CHANNEL: &str = "netease";
const RESOLUTION: &str = "1920x1080";
/// weapi requests pretend to be the web player (Chrome on macOS).
const WEAPI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0";
/// eapi requests use the official mobile client UA (reference
/// `chooseUserAgent('api', 'iphone')`).
const EAPI_UA: &str = "NeteaseMusic 9.0.90/5038 (iPhone; iOS 16.2; zh_CN)";

const CONTENT_TYPE_FORM: &str = "application/x-www-form-urlencoded";

/// Transport-level success codes: 200 plus the special codes the reference
/// client treats as "delivered" (QR states 800-803, login errors 502, ...).
/// Callers decide what a 301 (logged out) or 502 actually means.
const SPECIAL_STATUS_CODES: [i64; 8] = [201, 302, 400, 502, 800, 801, 802, 803];

#[derive(Debug, thiserror::Error)]
pub enum NeteaseError {
    #[error("network error: {0}")]
    Http(#[from] zed_reqwest::Error),
    #[error("api error {status}: {msg}")]
    Api { status: i64, msg: String },
}

impl NeteaseError {
    /// The payload/HTTP status code, when the error came from the server.
    pub fn status(&self) -> i64 {
        match self {
            NeteaseError::Http(_) => -1,
            NeteaseError::Api { status, .. } => *status,
        }
    }
}

/// Persistent device + login state, stored as `netease_session.json` next to
/// the other app data.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NeteaseSession {
    /// 52 uppercase hex chars (mirrors `generateDeviceId`), stable per install.
    pub device_id: String,
    /// 64 hex chars (32 bytes), the `_ntes_nuid` cookie.
    pub ntes_nuid: String,
    /// Guest token (`MUSIC_U` fallback), fetched once via
    /// `/api/register/anonimous`.
    pub music_a: Option<String>,
    /// Login token from QR login; `None` when logged out.
    pub music_u: Option<String>,
    pub csrf: Option<String>,
    pub user_id: Option<i64>,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
    /// Server-issued `NMTID` cookie; captured per-process like the reference
    /// client (a fresh fallback is generated for every launch), so it is not
    /// persisted.
    #[serde(default)]
    pub nmtid: Option<String>,
}

impl NeteaseSession {
    pub fn generate() -> Self {
        Self {
            device_id: crypto::generate_device_id(),
            ntes_nuid: crypto::random_hex(64),
            music_a: None,
            music_u: None,
            csrf: None,
            user_id: None,
            nickname: None,
            avatar_url: None,
            nmtid: None,
        }
    }

    fn load(path: &Path) -> Option<Self> {
        let contents = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents)
            .map_err(|err| tracing::warn!(%err, "failed to decode netease session"))
            .ok()
    }

    fn save(&self, path: &Path) {
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%err, "failed to create netease session dir");
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(err) = std::fs::write(path, json) {
                    tracing::warn!(%err, "failed to persist netease session");
                }
            }
            Err(err) => tracing::warn!(%err, "failed to serialize netease session"),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Crypto {
    Weapi,
    Eapi,
}

pub struct NeteaseResponse {
    pub body: Value,
    /// Raw `Set-Cookie` header values, for login flows that need to pick up
    /// `MUSIC_U` (the QR poll endpoint returns it both here and in the body).
    pub set_cookies: Vec<String>,
}

/// Cached profile of the logged-in user (nickname + avatar).
#[derive(Clone, Debug)]
pub struct UserProfile {
    pub nickname: String,
    pub avatar_url: String,
}

pub struct NeteaseClient {
    http: Client,
    session: Mutex<NeteaseSession>,
    session_path: PathBuf,
    /// Per-process `WNMCID` cookie (6 random lowercase letters + timestamp).
    wnm_cid: String,
    /// Guards the one-shot anonymous registration against stampedes.
    registering: AtomicBool,
}

impl NeteaseClient {
    pub fn new(data_dir: &Path) -> Self {
        let session_path = data_dir.join("netease_session.json");
        let session = NeteaseSession::load(&session_path).unwrap_or_else(NeteaseSession::generate);
        let letters: String = (0..6)
            .map(|_| (b'a' + rand::random_range(0..26u8)) as char)
            .collect();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            // same as the KuGou client: bound every request so a stalled
            // connection can't wedge pending fetches or login polling
            http: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            session: Mutex::new(session),
            session_path,
            wnm_cid: format!("{letters}.{now_ms}.01.0"),
            registering: AtomicBool::new(false),
        }
    }

    /// Locks the session, recovering the inner value if the lock was poisoned
    /// by a panicked thread rather than panicking the caller (same rationale
    /// as the KuGou client: playback must survive panics).
    fn session_guard(&self) -> std::sync::MutexGuard<'_, NeteaseSession> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn logged_in(&self) -> bool {
        self.session_guard()
            .music_u
            .as_deref()
            .is_some_and(|token| !token.is_empty())
    }

    fn save_session(&self, session: &NeteaseSession) {
        session.save(&self.session_path);
    }

    /// Stores QR-login credentials picked up from the poll response.
    pub fn store_login(&self, music_u: String, csrf: Option<String>) {
        let mut session = self.session_guard();
        session.music_u = Some(music_u);
        if let Some(csrf) = csrf {
            session.csrf = Some(csrf);
        }
        self.save_session(&session);
    }

    /// Persists the account profile fetched right after login.
    pub fn update_user_profile(&self, user_id: i64, profile: &UserProfile) {
        let mut session = self.session_guard();
        session.user_id = Some(user_id);
        session.nickname = Some(profile.nickname.clone());
        session.avatar_url = Some(profile.avatar_url.clone());
        self.save_session(&session);
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

    pub fn user_id(&self) -> Option<i64> {
        self.session_guard().user_id
    }

    /// Test-only accessor for asserting on session state.
    #[cfg(test)]
    pub(crate) fn session_snapshot(&self) -> NeteaseSession {
        self.session_guard().clone()
    }

    /// Clears the login state locally and on the server (best-effort logout
    /// call is made by the API wrapper before this).
    pub fn clear_login(&self) {
        let mut session = self.session_guard();
        session.music_u = None;
        session.user_id = None;
        session.nickname = None;
        session.avatar_url = None;
        session.csrf = None;
        self.save_session(&session);
    }

    /// The anonymous guest token, registering one if the session has neither
    /// login nor guest cookie yet.
    async fn ensure_anonymous(&self) -> Result<(), NeteaseError> {
        loop {
            if self
                .session_guard()
                .music_a
                .as_deref()
                .is_some_and(|token| !token.is_empty())
            {
                return Ok(());
            }
            // Someone else is already registering: wait for them to finish.
            if self
                .registering
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let result = self.register_anonymous_inner().await;
        self.registering.store(false, Ordering::SeqCst);
        result
    }

    /// Calls `/api/register/anonimous` (weapi) and captures the guest
    /// `MUSIC_A` cookie. Mirrors `register_anonimous.js` (which uses the
    /// xeapi fork extension; plain weapi on the same path works identically).
    async fn register_anonymous_inner(&self) -> Result<(), NeteaseError> {
        let username = {
            let session = self.session_guard();
            crypto::anonymous_username(&session.device_id)
        };
        let data = serde_json::json!({ "username": username });
        let response = self
            .request_uncached(Crypto::Weapi, "/api/register/anonimous", data)
            .await?;
        let code = response.body.get("code").and_then(Value::as_i64);
        if code != Some(200) {
            tracing::warn!(?code, "netease: anonymous registration failed");
            return Ok(()); // best-effort: requests continue without MUSIC_A
        }
        self.capture_cookies(&response.set_cookies);
        tracing::info!("netease: registered anonymous guest token");
        Ok(())
    }

    /// Picks login-relevant cookies (`NMTID`, `__csrf`, `MUSIC_A`, `MUSIC_U`)
    /// out of a response's Set-Cookie headers and merges them into the
    /// session. `NMTID` stays in-memory (regenerated per launch, like the
    /// reference client); the rest are persisted.
    fn capture_cookies(&self, set_cookies: &[String]) {
        let mut nmtid = None;
        let mut csrf = None;
        let mut music_a = None;
        let mut music_u = None;
        for cookie in set_cookies {
            let first = cookie.split(';').next().unwrap_or("");
            let Some((name, value)) = first.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match name {
                "NMTID" => nmtid = Some(value.to_string()),
                "__csrf" => csrf = Some(value.to_string()),
                "MUSIC_A" => music_a = Some(value.to_string()),
                "MUSIC_U" => music_u = Some(value.to_string()),
                _ => {}
            }
        }
        if nmtid.is_none() && csrf.is_none() && music_a.is_none() && music_u.is_none() {
            return;
        }
        if let Some(nmtid) = nmtid {
            // in-memory only: regenerated per launch like the reference client
            self.session_guard().nmtid = Some(nmtid);
        }
        let mut session = self.session_guard();
        let mut changed = false;
        if let Some(csrf) = csrf
            && session.csrf.as_deref() != Some(csrf.as_str())
        {
            session.csrf = Some(csrf);
            changed = true;
        }
        if let Some(music_a) = music_a
            && session.music_a.as_deref() != Some(music_a.as_str())
        {
            session.music_a = Some(music_a);
            changed = true;
        }
        if let Some(music_u) = music_u
            && session.music_u.as_deref() != Some(music_u.as_str())
        {
            session.music_u = Some(music_u);
            changed = true;
        }
        if changed {
            self.save_session(&session);
        }
    }

    /// Extracts `MUSIC_U` / `__csrf` from a raw cookie string (the QR login
    /// response also carries them in `body.cookie`; used as fallback when the
    /// Set-Cookie headers were not preserved by a proxy).
    pub fn store_login_from_cookie_string(&self, cookie: &str) -> Option<String> {
        let mut music_u = None;
        let mut csrf = None;
        for part in cookie.split(';') {
            let Some((name, value)) = part.split_once('=') else {
                continue;
            };
            match name.trim() {
                "MUSIC_U" => music_u = Some(value.trim().to_string()),
                "__csrf" => csrf = Some(value.trim().to_string()),
                _ => {}
            }
        }
        if let Some(music_u) = music_u {
            self.store_login(music_u, csrf);
        }
        self.session_guard().music_u.clone()
    }

    /// Ordered `(name, value)` pairs of the full anonymous/user cookie object
    /// the weapi path sends as its `Cookie` header.
    fn weapi_cookie_map(&self, session: &NeteaseSession, now_ms: u128) -> Vec<(String, String)> {
        let mut cookies: Vec<(String, String)> = vec![
            ("__remember_me".into(), "true".into()),
            ("ntes_kaola_ad".into(), "1".into()),
            ("_ntes_nuid".into(), session.ntes_nuid.clone()),
            (
                "_ntes_nnid".into(),
                format!("{},{}", session.ntes_nuid, now_ms),
            ),
            ("WNMCID".into(), self.wnm_cid.clone()),
            ("WEVNSM".into(), "1.0.0".into()),
            ("osver".into(), OSVER.into()),
            ("deviceId".into(), session.device_id.clone()),
            ("os".into(), "pc".into()),
            ("channel".into(), CHANNEL.into()),
            ("appver".into(), APPVER.into()),
        ];
        let nmtid = session
            .nmtid
            .clone()
            .unwrap_or_else(|| format!("00O{}", crypto::random_hex(38)));
        cookies.push(("NMTID".into(), nmtid));
        if let Some(music_u) = &session.music_u {
            cookies.push(("MUSIC_U".into(), music_u.clone()));
        } else if let Some(music_a) = &session.music_a {
            cookies.push(("MUSIC_A".into(), music_a.clone()));
        }
        if let Some(csrf) = &session.csrf {
            cookies.push(("__csrf".into(), csrf.clone()));
        }
        cookies
    }

    /// The device `header` map eapi embeds in both the encrypted payload and
    /// the `Cookie` header.
    fn eapi_header(&self, session: &NeteaseSession, now_ms: u128) -> Vec<(String, String)> {
        let now_secs = now_ms / 1000;
        let request_id = format!("{now_ms}_{:04}", rand::random_range(0..1000u32));
        let mut header: Vec<(String, String)> = vec![
            ("osver".into(), OSVER.into()),
            ("deviceId".into(), session.device_id.clone()),
            ("os".into(), "pc".into()),
            ("appver".into(), APPVER.into()),
            ("versioncode".into(), "140".into()),
            ("mobilename".into(), String::new()),
            ("buildver".into(), now_secs.to_string()),
            ("resolution".into(), RESOLUTION.into()),
            ("__csrf".into(), session.csrf.clone().unwrap_or_default()),
            ("channel".into(), CHANNEL.into()),
            ("requestId".into(), request_id),
        ];
        if let Some(music_u) = &session.music_u {
            header.push(("MUSIC_U".into(), music_u.clone()));
        } else if let Some(music_a) = &session.music_a {
            header.push(("MUSIC_A".into(), music_a.clone()));
        }
        let nmtid = session
            .nmtid
            .clone()
            .unwrap_or_else(|| format!("00O{}", crypto::random_hex(38)));
        header.push(("NMTID".into(), nmtid));
        header
    }

    fn cookie_header_value(pairs: &[(String, String)]) -> String {
        pairs
            .iter()
            .map(|(name, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(name),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Executes an encrypted POST against the NetEase web APIs. Every call
    /// MUST run on the Tokio runtime (`crate::RUNTIME`) - reqwest's DNS
    /// resolver panics on the gpui main-thread executor.
    pub async fn request(
        &self,
        crypto_type: Crypto,
        uri: &str,
        data: Value,
    ) -> Result<NeteaseResponse, NeteaseError> {
        self.ensure_anonymous().await?;
        let response = self
            .request_uncached(crypto_type, uri, data)
            .await
            .inspect_err(|err| {
                tracing::debug!(uri, error = %err, "netease request failed");
            })?;
        self.capture_cookies(&response.set_cookies);
        Ok(response)
    }

    /// The raw request path without the anonymous-token pre-flight (used by
    /// the registration call itself and by tests).
    async fn request_uncached(
        &self,
        crypto_type: Crypto,
        uri: &str,
        mut data: Value,
    ) -> Result<NeteaseResponse, NeteaseError> {
        let session = self.session_guard().clone();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        // `e_r: false` on every payload: responses come back as plain JSON
        // (the reference client also disables response encryption).
        if let Some(object) = data.as_object_mut() {
            object.insert("e_r".into(), Value::Bool(false));
        }

        let (url, body, user_agent, referer) = match crypto_type {
            Crypto::Weapi => {
                let object = data.as_object_mut().expect("request data is an object");
                object.insert(
                    "csrf_token".into(),
                    Value::String(session.csrf.clone().unwrap_or_default()),
                );
                let (params, enc_sec_key) = crypto::weapi(&data.to_string());
                let url = format!("{DOMAIN}/weapi/{}", uri.strip_prefix("/api/").unwrap_or(uri));
                let body = format!(
                    "params={}&encSecKey={}",
                    urlencoding::encode(&params),
                    urlencoding::encode(&enc_sec_key)
                );
                (url, body, WEAPI_UA, Some(DOMAIN))
            }
            Crypto::Eapi => {
                let header = self.eapi_header(&session, now_ms);
                if let Some(object) = data.as_object_mut() {
                    object.insert(
                        "header".into(),
                        Value::Object(
                            header
                                .iter()
                                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                .collect(),
                        ),
                    );
                }
                let params = crypto::eapi(uri, &data.to_string());
                let url =
                    format!("{EAPI_DOMAIN}/eapi/{}", uri.strip_prefix("/api/").unwrap_or(uri));
                let body = format!("params={}", urlencoding::encode(&params));
                (url, body, EAPI_UA, None)
            }
        };

        let cookie_pairs = match crypto_type {
            Crypto::Weapi => self.weapi_cookie_map(&session, now_ms),
            Crypto::Eapi => self.eapi_header(&session, now_ms),
        };

        let mut headers = HeaderMap::new();
        let set_header = |headers: &mut HeaderMap, name: &str, value: &str| {
            if let (Ok(name), Ok(value)) = (
                HeaderName::try_from(name),
                HeaderValue::try_from(value),
            ) {
                headers.insert(name, value);
            }
        };
        set_header(&mut headers, "User-Agent", user_agent);
        set_header(&mut headers, "Cookie", &Self::cookie_header_value(&cookie_pairs));
        set_header(&mut headers, "Content-Type", CONTENT_TYPE_FORM);
        if let Some(referer) = referer {
            set_header(&mut headers, "Referer", referer);
        }

        let response = self
            .http
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let set_cookies: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_string))
            .collect();
        let http_status = response.status().as_u16() as i64;
        let bytes = response.bytes().await?;
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        // Payload-size telemetry: the parsed `Value` tree costs a multiple of
        // this, so an endpoint that quietly ships megabytes (toplist carries
        // per-chart track lists) shows up here before it shows up as a memory
        // step in the `[mem]` probe.
        tracing::info!(
            target: "meliora::netease",
            endpoint = uri,
            raw_bytes = bytes.len(),
            "netease response"
        );

        let code = body.get("code").and_then(Value::as_i64);
        let status = code.unwrap_or(http_status);
        if status == 200 || SPECIAL_STATUS_CODES.contains(&status) {
            return Ok(NeteaseResponse {
                body,
                set_cookies,
            });
        }
        let msg = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        Err(NeteaseError::Api { status, msg })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_round_trips_through_disk() {
        let dir = crate::test_support::TestDir::new("netease-session-test");
        let path = dir.join("netease_session.json");
        let session = NeteaseSession {
            music_u: Some("tok".into()),
            user_id: Some(42),
            ..NeteaseSession::generate()
        };
        session.save(&path);
        let loaded = NeteaseSession::load(&path).expect("session loads");
        assert_eq!(loaded.music_u.as_deref(), Some("tok"));
        assert_eq!(loaded.user_id, Some(42));
        assert_eq!(loaded.device_id, session.device_id);
    }

    #[test]
    fn generated_session_has_reference_shape() {
        let session = NeteaseSession::generate();
        assert_eq!(session.device_id.len(), 52);
        assert!(
            session
                .device_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
        assert_eq!(session.ntes_nuid.len(), 64);
        assert!(session.ntes_nuid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cookie_string_extracts_login_state() {
        let dir = crate::test_support::TestDir::new("netease-cookie-test");
        let client = NeteaseClient::new(dir.path());
        let token = client
            .store_login_from_cookie_string(
                "MUSIC_U=abc123; __csrf_token=x; __csrf=csrf9; Expires=Wed, 01 Jan 2026 00:00:00 GMT",
            )
            .expect("token captured");
        assert_eq!(token, "abc123");
        assert!(client.logged_in());
        let snapshot = client.session_snapshot();
        assert_eq!(snapshot.csrf.as_deref(), Some("csrf9"));
    }
}
