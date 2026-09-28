//! Cover-art URL helpers for full-screen display.
//!
//! Search/queue payloads carry *thumbnail* cover URLs (KuGou serves
//! `stdmusic/256`, NetEase often no `?param=` at all). Stretching those
//! across the immersive backdrop magnifies them 4-8× and reads as mush. The
//! providers host larger variants of the same art at well-known URL shapes:
//! KuGou's `stdmusic` path takes a size segment (480 is the largest served
//! bucket), NetEase images accept a `?param=WxH` query. [`display_cover_url`]
//! rewrites a thumbnail URL to its large variant; [`fetch_display_cover_bytes`]
//! fetches that variant with a fallback to the original URL in case a large
//! variant was never generated for a particular cover.

use anyhow::Context as _;

/// KuGou's `stdmusic` host resizes on the fly to the requested bucket
/// (probed 2026-09-28: 480/1280/1920/2048/4096 all serve real pixels). 2048
/// matches the immersive backdrop's decode cap — a fullscreen backdrop gets
/// ~1:1 texels instead of a 256px thumbnail stretched 7x.
const KUGOU_LARGE_SIZE: &str = "2048";
/// NetEase images accept `?param=WxH`; 2048 matches the backdrop decode cap.
const NETEASE_LARGE_PARAM: &str = "2048y2048";

/// Rewrites a thumbnail cover URL to its large-display variant. URLs that
/// match no known provider shape come back unchanged.
pub fn display_cover_url(url: &str) -> String {
    if let Some(rewritten) = rewrite_kugou_stdmusic(url) {
        return rewritten;
    }
    if url.contains("music.126.net") {
        return rewrite_netease_param(url);
    }
    url.to_string()
}

/// KuGou album art lives at `…/stdmusic/{size}/{date}/{hash}.jpg` with size
/// buckets 150/240/256/400/480. Bump whatever bucket the thumbnail carries to
/// the largest one; anything after `stdmusic/` is left untouched.
fn rewrite_kugou_stdmusic(url: &str) -> Option<String> {
    let marker = "/stdmusic/";
    let pos = url.find(marker)? + marker.len();
    let rest = &url[pos..];
    let size_len = rest.find('/')?;
    if !rest[..size_len].bytes().all(|b| b.is_ascii_digit()) || size_len == 0 {
        return None;
    }
    let mut out = String::with_capacity(url.len() + 2);
    out.push_str(&url[..pos]);
    out.push_str(KUGOU_LARGE_SIZE);
    out.push_str(&rest[size_len..]);
    Some(out)
}

/// NetEase image URLs are `host/hash/file.jpg` optionally followed by
/// `?param=WxH`. Drop any existing param and ask for the large square.
fn rewrite_netease_param(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let mut out = base.to_string();
    out.push_str("?param=");
    out.push_str(NETEASE_LARGE_PARAM);
    out
}

/// Fetches the display-quality cover for `url`: tries the large variant
/// first (when the URL shape has one) and falls back to the original URL if
/// the large variant was never generated. Both go through the shared disk
/// cache, so grid elements fetching the original thumbnail still share
/// entries with this path.
pub async fn fetch_display_cover_bytes(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let large = display_cover_url(url);
    if large != url
        && let Ok(Some(bytes)) = crate::media::http_source::http_cover_bytes_cached(&large).await
    {
        return Ok(Some(bytes));
    }
    crate::media::http_source::http_cover_bytes_cached(url)
        .await
        .with_context(|| format!("fetching cover {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kugou_stdmusic_bumps_to_display_size() {
        let url = "http://imge.kugou.com/stdmusic/256/20260306/20260306201650909877.jpg";
        assert_eq!(
            display_cover_url(url),
            "http://imge.kugou.com/stdmusic/2048/20260306/20260306201650909877.jpg"
        );
        // Already-large URLs pass through unchanged.
        assert_eq!(
            display_cover_url("https://imgessl.kugou.com/stdmusic/2048/20200101/abcdef.jpg"),
            "https://imgessl.kugou.com/stdmusic/2048/20200101/abcdef.jpg"
        );
    }

    #[test]
    fn netease_gets_large_param() {
        let plain = "http://p3.music.126.net/AbCdEf/12345.jpg";
        assert_eq!(
            display_cover_url(plain),
            "http://p3.music.126.net/AbCdEf/12345.jpg?param=2048y2048"
        );
        let with_param = "http://p1.music.126.net/AbCdEf/12345.jpg?param=300y300";
        assert_eq!(
            display_cover_url(with_param),
            "http://p1.music.126.net/AbCdEf/12345.jpg?param=2048y2048"
        );
    }

    #[test]
    fn unknown_shapes_pass_through() {
        let url = "https://example.com/cover/art.png";
        assert_eq!(display_cover_url(url), url);
        // Non-numeric size segments are left alone.
        let weird = "http://imge.kugou.com/stdmusic/mid/20200101/x.jpg";
        assert_eq!(display_cover_url(weird), weird);
    }
}
