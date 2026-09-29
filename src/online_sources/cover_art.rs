//! Cover-art URL helpers for full-screen display.
//!
//! Search/queue payloads carry *thumbnail* cover URLs (KuGou serves
//! `stdmusic/256`, NetEase often no `?param=` at all). Stretching those
//! across the immersive backdrop magnifies them 4-8× and reads as mush. The
//! providers host larger variants of the same art at well-known URL shapes:
//! KuGou's `stdmusic` path takes a size segment (probed 2026-09-28:
//! 480/1280/1920/2048/4096 all serve real pixels — but only for covers whose
//! large variants were actually generated), NetEase images accept a
//! `?param=WxH` query. [`display_cover_url`] rewrites a thumbnail URL to its
//! largest variant; [`fetch_display_cover_bytes`] walks a size LADDER and
//! keeps the biggest variant that actually serves pixels, falling back to
//! the original thumbnail only when every rung misses. A silent jump from a
//! 404'd 2048 straight to the 256px thumbnail is exactly the "immersive
//! backdrop is mush" report of 2026-09-29 — the ladder plus the winner log
//! line make the resolution the backdrop actually gets observable.

use std::{
    collections::HashSet,
    sync::{LazyLock, Mutex},
};

use anyhow::Context as _;
use tracing::{info, warn};

/// KuGou `stdmusic` size buckets tried from largest to smallest before the
/// original URL. The host resizes on the fly to the requested bucket (probed
/// 2026-09-28: 480/1280/1920/2048/4096 all serve real pixels) — but not
/// every cover has every variant generated server-side, so the ladder keeps
/// the largest variant that actually exists. 4096 leads the ladder since the
/// 2026-09-29 "backdrop must be as sharp as the disc label" directive: at
/// 4096 the backdrop's band resamples to ~1920 device px at ~0.47×, whose
/// averaging matches the 4×-downscaled disc label; absent variants cost one
/// 404 and are negative-cached for the session.
const KUGOU_SIZE_LADDER: [&str; 4] = ["4096", "2048", "1280", "480"];
/// NetEase `?param=` ladder, same rationale.
const NETEASE_PARAM_LADDER: [&str; 3] = ["2048y2048", "1024y1024", "512y512"];

/// Display-variant URLs that returned no pixels this session. A 404 costs a
/// round trip; without this set every backdrop retrieve for that cover would
/// re-pay every failed rung before reaching the cached winner. Session-
/// scoped on purpose: next launch retries, so a CDN hiccup self-heals.
static VARIANT_MISSES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn variant_missed(url: &str) -> bool {
    VARIANT_MISSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(url)
}

fn note_variant_miss(url: &str) {
    VARIANT_MISSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(url.to_string());
}

/// The display-variant URLs for `url`, largest first, ending with the
/// original thumbnail itself. Pure so the ladder order stays unit-testable.
pub fn display_cover_candidates(url: &str) -> Vec<String> {
    let mut rungs = Vec::with_capacity(KUGOU_SIZE_LADDER.len() + 1);
    if url.contains("/stdmusic/") {
        for size in KUGOU_SIZE_LADDER {
            rungs.push(rewrite_kugou_stdmusic_with(url, size));
        }
    } else if url.contains("music.126.net") {
        for param in NETEASE_PARAM_LADDER {
            rungs.push(rewrite_netease_param_with(url, param));
        }
    }
    rungs.push(url.to_string());
    rungs
}

/// KuGou album art lives at `…/stdmusic/{size}/{date}/{hash}.jpg` with size
/// buckets 150/240/256/400/480. Bump whatever bucket the thumbnail carries to
/// `size`; anything after `stdmusic/` is left untouched.
fn rewrite_kugou_stdmusic_with(url: &str, size: &str) -> String {
    let marker = "/stdmusic/";
    let Some(pos) = url.find(marker).map(|pos| pos + marker.len()) else {
        return url.to_string();
    };
    let rest = &url[pos..];
    let Some(size_len) = rest.find('/') else {
        return url.to_string();
    };
    if !rest[..size_len].bytes().all(|b| b.is_ascii_digit()) || size_len == 0 {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len() + 2);
    out.push_str(&url[..pos]);
    out.push_str(size);
    out.push_str(&rest[size_len..]);
    out
}

/// NetEase image URLs are `host/hash/file.jpg` optionally followed by
/// `?param=WxH`. Drop any existing param and ask for the requested square.
fn rewrite_netease_param_with(url: &str, param: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let mut out = base.to_string();
    out.push_str("?param=");
    out.push_str(param);
    out
}

/// Fetches the display-quality cover for `url`: walks the provider's size
/// ladder largest-first and returns the biggest variant that actually serves
/// pixels, falling back to the original thumbnail when every rung misses.
/// Failed rungs are remembered for the session so later retrieves for the
/// same cover skip straight to the winning size. Both winners and misses go
/// through the shared disk cache, so grid elements fetching the original
/// thumbnail still share entries with this path.
pub async fn fetch_display_cover_bytes(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let mut last_error = None;
    for rung in display_cover_candidates(url) {
        if variant_missed(&rung) {
            continue;
        }
        match crate::media::http_source::http_cover_bytes_cached(&rung).await {
            Ok(Some(bytes)) => {
                if rung != url {
                    info!(
                        target: "backdrop",
                        bytes = bytes.len(),
                        url = %rung,
                        "display cover resolved to a large variant"
                    );
                }
                return Ok(Some(bytes));
            }
            Ok(None) => note_variant_miss(&rung),
            Err(error) => {
                note_variant_miss(&rung);
                warn!(target: "backdrop", %error, url = %rung, "display cover variant fetch failed");
                last_error = Some(error);
            }
        }
    }
    match last_error {
        Some(error) => Err(error).with_context(|| format!("fetching cover {url}")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Largest rung of the ladder — what the old single-shot rewrite did.
    fn largest_variant(url: &str) -> String {
        display_cover_candidates(url).swap_remove(0)
    }

    #[test]
    fn kugou_stdmusic_bumps_to_display_size() {
        let url = "http://imge.kugou.com/stdmusic/256/20260306/20260306201650909877.jpg";
        assert_eq!(
            largest_variant(url),
            "http://imge.kugou.com/stdmusic/4096/20260306/20260306201650909877.jpg"
        );
        // Already-large URLs still bump to the top rung (2048 → 4096).
        assert_eq!(
            largest_variant("https://imgessl.kugou.com/stdmusic/2048/20200101/abcdef.jpg"),
            "https://imgessl.kugou.com/stdmusic/4096/20200101/abcdef.jpg"
        );
    }

    #[test]
    fn kugou_ladder_walks_largest_to_original() {
        let url = "http://imge.kugou.com/stdmusic/256/20260306/x.jpg";
        let candidates = display_cover_candidates(url);
        assert_eq!(
            candidates,
            vec![
                "http://imge.kugou.com/stdmusic/4096/20260306/x.jpg".to_string(),
                "http://imge.kugou.com/stdmusic/2048/20260306/x.jpg".to_string(),
                "http://imge.kugou.com/stdmusic/1280/20260306/x.jpg".to_string(),
                "http://imge.kugou.com/stdmusic/480/20260306/x.jpg".to_string(),
                url.to_string(),
            ]
        );
    }

    #[test]
    fn netease_gets_large_param() {
        let plain = "http://p3.music.126.net/AbCdEf/12345.jpg";
        assert_eq!(
            largest_variant(plain),
            "http://p3.music.126.net/AbCdEf/12345.jpg?param=2048y2048"
        );
        let with_param = "http://p1.music.126.net/AbCdEf/12345.jpg?param=300y300";
        assert_eq!(
            largest_variant(with_param),
            "http://p1.music.126.net/AbCdEf/12345.jpg?param=2048y2048"
        );
        let ladder = display_cover_candidates(with_param);
        assert_eq!(ladder.len(), 4);
        assert!(ladder[0].ends_with("?param=2048y2048"));
        assert!(ladder[1].ends_with("?param=1024y1024"));
        assert!(ladder[2].ends_with("?param=512y512"));
        // The original URL (with whatever param it carried) is the last rung.
        assert_eq!(ladder[3], with_param);
    }

    #[test]
    fn unknown_shapes_pass_through() {
        let url = "https://example.com/cover/art.png";
        assert_eq!(largest_variant(url), url);
        assert_eq!(display_cover_candidates(url), vec![url.to_string()]);
        // Non-numeric size segments are left alone.
        let weird = "http://imge.kugou.com/stdmusic/mid/20200101/x.jpg";
        assert_eq!(largest_variant(weird), weird);
    }
}
