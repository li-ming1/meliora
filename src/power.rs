use gpui::{AppContext, Entity, Global};

use crate::playback::thread::PlaybackState;

// --- Windows: Win32 PowerRequest (PowerRequestExecutionRequired) ---

#[cfg(target_os = "windows")]
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Power::{
                PowerClearRequest, PowerCreateRequest, PowerRequestExecutionRequired,
                PowerSetRequest,
            },
            SystemServices::POWER_REQUEST_CONTEXT_VERSION,
            Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
        },
    },
    core::{PWSTR, w},
};

#[cfg(target_os = "windows")]
struct PlatformPower {
    handle: Option<HANDLE>,
}

#[cfg(target_os = "windows")]
impl PlatformPower {
    fn new() -> Self {
        Self { handle: None }
    }

    fn inhibit(&mut self) {
        // 与 macOS 分支对齐的重入门：重复 inhibit 会覆盖旧 POWER_REQUEST
        // 句柄，旧句柄从此再无 CloseHandle 机会。
        if self.handle.is_some() {
            return;
        }
        let reason = w!("Playing music");

        unsafe {
            let context = REASON_CONTEXT {
                Version: POWER_REQUEST_CONTEXT_VERSION,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    // very cool that this requires a mut pointer even though the string is never
                    // mutated! i love windows
                    SimpleReasonString: PWSTR(reason.as_ptr() as *mut _),
                },
            };

            let Ok(handle) = PowerCreateRequest(&context) else {
                tracing::error!("Failed to create power request handle, not inhibiting");
                return;
            };

            if let Err(e) = PowerSetRequest(handle, PowerRequestExecutionRequired) {
                tracing::error!("Failed to set power request: {:?}", e)
            }

            self.handle = Some(handle);
        }
    }

    fn uninhibit(&mut self) {
        unsafe {
            if let Some(handle) = self.handle.take() {
                if let Err(e) = PowerClearRequest(handle, PowerRequestExecutionRequired) {
                    tracing::error!("Failed to clear power request: {:?}", e)
                }

                if let Err(e) = CloseHandle(handle) {
                    tracing::error!("Failed to close power request handle: {:?}", e)
                }
            }
        }
    }
}

// --- macOS: NSProcessInfo beginActivity/endActivity ---

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

#[cfg(target_os = "macos")]
struct PlatformPower {
    activity: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

#[cfg(target_os = "macos")]
impl PlatformPower {
    fn new() -> Self {
        Self { activity: None }
    }

    fn inhibit(&mut self) {
        if self.activity.is_some() {
            return;
        }
        let process_info = NSProcessInfo::processInfo();
        let reason = NSString::from_str("Meliora is playing media");
        self.activity =
            Some(process_info.beginActivityWithOptions_reason(
                NSActivityOptions::IdleDisplaySleepDisabled,
                &reason,
            ));
    }

    fn uninhibit(&mut self) {
        let Some(activity) = self.activity.take() else {
            return;
        };
        unsafe { NSProcessInfo::processInfo().endActivity(&activity) };
    }
}

// --- Linux: org.freedesktop.ScreenSaver via zbus ---

#[cfg(target_os = "linux")]
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
use tokio::sync::Mutex;
#[cfg(target_os = "linux")]
use zbus::Connection;

#[cfg(target_os = "linux")]
const SERVICE: &str = "org.freedesktop.ScreenSaver";
#[cfg(target_os = "linux")]
const PATH: &str = "/org/freedesktop/ScreenSaver";

#[cfg(target_os = "linux")]
struct State {
    connection: Option<Connection>,
    cookie: Option<u32>,
}

#[cfg(target_os = "linux")]
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

#[cfg(target_os = "linux")]
fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            connection: None,
            cookie: None,
        })
    })
}

#[cfg(target_os = "linux")]
async fn connection(state: &mut State) -> Option<Connection> {
    if state.connection.is_none() {
        match Connection::session().await {
            Ok(conn) => state.connection = Some(conn),
            Err(e) => {
                tracing::warn!("failed to connect to session bus: {e}");
                return None;
            }
        }
    }
    state.connection.clone()
}

#[cfg(target_os = "linux")]
struct PlatformPower;

#[cfg(target_os = "linux")]
impl PlatformPower {
    fn new() -> Self {
        Self
    }

    fn inhibit(&mut self) {
        crate::RUNTIME.spawn(async {
            let mut state = state().lock().await;
            if state.cookie.is_some() {
                return;
            }
            let Some(conn) = connection(&mut state).await else {
                return;
            };
            let result: Result<u32, _> = conn
                .call_method(
                    Some(SERVICE),
                    PATH,
                    Some(SERVICE),
                    "Inhibit",
                    &("Meliora", "Playing media"),
                )
                .await
                .and_then(|r| r.body().deserialize());

            match result {
                Ok(cookie) => state.cookie = Some(cookie),
                Err(e) => tracing::warn!("failed to inhibit screen saver: {e}"),
            }
        });
    }

    fn uninhibit(&mut self) {
        crate::RUNTIME.spawn(async {
            let mut state = state().lock().await;
            let Some(cookie) = state.cookie.take() else {
                return;
            };
            let Some(conn) = connection(&mut state).await else {
                return;
            };
            if let Err(e) = conn
                .call_method(Some(SERVICE), PATH, Some(SERVICE), "UnInhibit", &(cookie))
                .await
            {
                tracing::warn!("failed to uninhibit screen saver: {e}");
            }
        });
    }
}

// --- Unsupported platforms: no-op stub ---

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
struct PlatformPower;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl PlatformPower {
    fn new() -> Self {
        Self
    }

    fn inhibit(&mut self) {}

    fn uninhibit(&mut self) {}
}

struct PowerManagerInner {
    platform: PlatformPower,
    playing: bool,
    prevent_idle: bool,
}

impl PowerManagerInner {
    fn new(prevent_idle: bool) -> Self {
        Self {
            platform: PlatformPower::new(),
            playing: false,
            prevent_idle,
        }
    }

    fn set_state(&mut self, state: PlaybackState) {
        let playing = state == PlaybackState::Playing;
        if self.playing == playing {
            return;
        }
        self.playing = playing;
        self.update();
    }

    fn set_prevent_idle(&mut self, prevent_idle: bool) {
        if self.prevent_idle == prevent_idle {
            return;
        }
        self.prevent_idle = prevent_idle;
        self.update();
    }

    fn update(&mut self) {
        if self.playing && self.prevent_idle {
            self.platform.inhibit();
        } else {
            self.platform.uninhibit();
        }
    }
}

#[derive(Clone)]
pub struct PowerManager(Entity<PowerManagerInner>);

impl Global for PowerManager {}

impl PowerManager {
    pub fn new(cx: &mut gpui::App, prevent_idle: bool) -> Self {
        Self(cx.new(|_| PowerManagerInner::new(prevent_idle)))
    }

    pub fn set_state<C: AppContext>(&self, cx: &mut C, state: PlaybackState) {
        self.0.update(cx, |inner, _| inner.set_state(state));
    }

    pub fn set_prevent_idle<C: AppContext>(&self, cx: &mut C, prevent_idle: bool) {
        self.0
            .update(cx, |inner, _| inner.set_prevent_idle(prevent_idle));
    }
}
