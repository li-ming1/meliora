//! Shared "refresh a stale online stream URL" plumbing.
//!
//! KuGou / NetEase direct-link URLs are signed and expire (HTTP 403), but
//! playlist items persist them. Both callers re-fetch a fresh URL from the
//! provider identity: the session-restore pass in `app.rs` and the playback
//! thread's one-shot retry when opening a persisted URL fails.

use crate::playback::queue::OnlineIdentity;

/// Re-exposed for the playback thread's expired-URL retry: resolves a KuGou
/// stream URL to its remembered track (by the mixsongid embedded in the URL),
/// which is how playlist items, which persist only the URL, recover their
/// provider identity.
#[cfg(feature = "kugou")]
pub use crate::ui::kugou::online_track_matching_path;

/// NetEase counterpart of the KuGou re-export above, aliased so both provider
/// features can be enabled at once without an ambiguous name.
#[cfg(feature = "netease")]
pub use crate::ui::netease::online_track_matching_path as netease_online_track_matching_path;

/// Display metadata shape shared by both providers:
/// `(name, artist, duration, cover_url)`.
pub type OnlineDisplay = (
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// Re-fetches a fresh stream URL for an online identity and re-records it in
/// the provider's stream registry (lyrics / like / download resolve by that
/// registry, so a refreshed URL must be re-registered or those break).
///
/// Quality arguments are feature-gated to match the call sites, which only
/// pass the settings of the providers compiled in.
///
/// Returns `None` when the provider can no longer produce a playable URL.
#[allow(unused_variables)]
pub async fn refresh_online_url(
    identity: &OnlineIdentity,
    #[cfg(feature = "kugou")] kugou_quality: &str,
    #[cfg(feature = "netease")] netease_quality: &str,
    display: OnlineDisplay,
) -> Option<String> {
    match identity {
        #[cfg(feature = "kugou")]
        OnlineIdentity::Kugou {
            hash,
            mix_song_id,
            album_id,
        } => {
            let client = crate::kugou::shared_client();
            let hash = hash.clone();
            let mix_song_id = *mix_song_id;
            let album_id = *album_id;
            let kugou_quality = kugou_quality.to_string();
            let url = {
                let hash = hash.clone();
                crate::RUNTIME
                    .spawn(async move {
                        crate::ui::kugou::fetch_stream_url(
                            &client,
                            &hash,
                            mix_song_id,
                            album_id,
                            &kugou_quality,
                        )
                        .await
                    })
                    .await
                    .ok()
                    .flatten()
            }?;

            let (name, artist, duration, cover) = display;
            crate::ui::kugou::remember_online_track(
                url.clone(),
                crate::ui::kugou::KugouTrackInfo {
                    title: name.unwrap_or_default().into(),
                    artist: artist.unwrap_or_default().into(),
                    album: gpui::SharedString::default(),
                    duration: duration.unwrap_or(0),
                    hash,
                    mix_song_id,
                    album_id,
                    cover_url: cover.unwrap_or_default().into(),
                },
            );
            Some(url)
        }
        #[cfg(feature = "netease")]
        OnlineIdentity::Netease { id } => {
            crate::ui::netease::refresh_restored_url(
                *id,
                netease_quality,
                display.0,
                display.1,
                display.2,
                display.3,
            )
            .await
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}