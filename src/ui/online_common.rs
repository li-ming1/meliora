//! Shared glue for the online providers' UI layer: play-intent fetch dedup,
//! QR login image rendering and tolerant JSON field extraction. Both
//! provider glue modules (`ui::kugou` / `ui::netease`) consume these;
//! everything here must stay provider-agnostic.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock, RwLock, RwLockWriteGuard},
    time::{Duration, Instant},
};

use gpui::RenderImage;
use serde_json::Value;
use smallvec::SmallVec;

/// Intent of an online-track activation: start playing now, or append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlayIntent {
    Now,
    Queue,
}

/// Song ids whose play-URL fetch is currently in flight, plus ids whose
/// fetch recently succeeded (tagged with the intent it served). Concurrent
/// clicks on the same song coalesce into the first fetch so the track can't
/// be queued twice while the (slow) URL request is still outstanding, and a
/// short same-intent cooldown after a success keeps a click burst from
/// running the full fetch + open churn several times — GPUI delivers one
/// click event per click of a multi-click sequence, so a double-click is
/// two `play_track` calls a few hundred ms apart.
pub(crate) struct PlayFetchDedup {
    pub(crate) in_flight: HashSet<i64>,
    pub(crate) recent: HashMap<i64, (u8, Instant)>,
}

/// How long a successful fetch suppresses an identical-intent re-request.
pub(crate) const FETCH_COOLDOWN: Duration = Duration::from_millis(800);

static PENDING_FETCHES: OnceLock<RwLock<PlayFetchDedup>> = OnceLock::new();

pub(crate) fn pending_fetches() -> &'static RwLock<PlayFetchDedup> {
    PENDING_FETCHES.get_or_init(|| {
        RwLock::new(PlayFetchDedup {
            in_flight: HashSet::new(),
            recent: HashMap::new(),
        })
    })
}

/// Poison-recovering write lock for the fetch dedup table: a panicking
/// holder must not wedge every later click behind a poisoned lock.
pub(crate) fn write_pending_fetches() -> RwLockWriteGuard<'static, PlayFetchDedup> {
    pending_fetches().write().unwrap_or_else(|e| e.into_inner())
}

/// Renders a URL as a black-on-white QR code image. Uses the same frame
/// construction as the album art pipeline.
pub(crate) fn build_qr_render_image(url: &str) -> anyhow::Result<Arc<RenderImage>> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    let mut image: image::RgbaImage = code
        .render::<image::Rgba<u8>>()
        .quiet_zone(true)
        .min_dimensions(320, 320)
        .build();

    crate::ui::components::managed_image::rgb_to_bgr(&mut image);

    let mut frames: SmallVec<[_; 1]> = SmallVec::new();
    frames.push(image::Frame::new(image));
    Ok(Arc::new(RenderImage::new(frames)))
}

/// First non-empty string among `keys`, or an empty string.
pub(crate) fn string_field(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(Value::String(s)) = value.get(*key)
            && !s.is_empty()
        {
            return s.clone();
        }
    }
    String::new()
}

/// First numeric value among `keys` (numbers directly, numeric strings
/// parsed), or 0.
pub(crate) fn i64_field(value: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        match value.get(*key) {
            Some(Value::Number(n)) => return n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => {
                if let Ok(parsed) = s.parse::<i64>() {
                    return parsed;
                }
            }
            _ => {}
        }
    }
    0
}

/// Resolves `pointer` to a JSON array and maps each entry through `parse`,
/// dropping entries it rejects. Shared shell for the provider response
/// parsers (search / playlist / rank / recommend endpoints).
pub(crate) fn parse_pointer_list<T>(
    body: &Value,
    pointer: &str,
    parse: impl Fn(&Value) -> Option<T>,
) -> Vec<T> {
    body.pointer(pointer)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse).collect())
        .unwrap_or_default()
}
