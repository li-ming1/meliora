//! Download a NetEase online track to local disk: audio (highest quality the
//! account can fetch), `.lrc` + `.yrc` lyrics sidecars, and the cover + tags
//! embedded into the audio file.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use cntp_i18n::tr;
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::ItemKey,
    tag::{Accessor, ItemValue, Tag, TagExt, TagItem, TagType},
};
use serde_json::Value;

use crate::{
    media::http_source::http_cover_bytes,
    netease::NeteaseClient,
    toasts::{Toast, emit_toast},
    ui::netease::NeteaseTrackInfo,
};

use super::extract_song_url;

/// Quality ladder tried in order; the first tier that yields a full-track URL
/// wins. Trial clips (`freeTrialInfo`) are never saved.
const QUALITY_LADDER: [&str; 5] = ["hires", "lossless", "exhigh", "higher", "standard"];

/// Strips characters that are illegal in Windows file names.
fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "track".into()
    } else {
        trimmed.to_string()
    }
}

/// Fetches `url` into memory (audio files are a few tens of MB, fine to hold).
async fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let client = zed_reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response.bytes().await.map(|bytes| bytes.to_vec()).map_err(|err| err.to_string())
}

/// Best-quality full-track URL for `track` (hires -> ... -> standard),
/// rejecting trial clips so downloads never save the ~30s preview.
async fn resolve_best_url(
    client: &NeteaseClient,
    track: &NeteaseTrackInfo,
) -> Result<(String, &'static str), String> {
    for level in QUALITY_LADDER {
        if let Ok(resp) = client.song_url(track.id, level).await
            && let Some((url, trial)) = extract_song_url(&resp.body)
            && !trial
        {
            return Ok((url, level));
        }
    }
    Err(tr!(
        "NETEASE_DOWNLOAD_NO_URL",
        "No full-track URL for this song (higher tiers usually need VIP)"
    )
    .to_string())
}

/// Fetches the lyrics for `track` from `lyric_new` in both formats: the plain
/// line-timed `.lrc` text and the word-level YRC karaoke text. Either may be
/// absent. Translations (`tlyric`) are folded into the `.lrc` sidecar text so
/// plain players see them too.
async fn fetch_lyrics(
    client: &NeteaseClient,
    track: &NeteaseTrackInfo,
) -> (Option<String>, Option<String>) {
    let Ok(resp) = client.lyric_new(track.id).await else {
        return (None, None);
    };
    let field = |key: &str| {
        resp.body
            .pointer(&format!("/{key}/lyric"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let lrc = field("lrc");
    let tlyric = field("tlyric");
    let yrc = field("yrc");

    let lrc = if lrc.is_empty() {
        None
    } else if !tlyric.is_empty() {
        // NetEase translated LRC is line-timed the same way; append each
        // translation line right after its original (the common bilingual
        // sidecar convention).
        let mut merged = String::new();
        let translations: Vec<(String, String)> = tlyric
            .lines()
            .filter_map(|line| {
                let (time, text) = line.split_once(']')?;
                Some((time.to_string(), text.to_string()))
            })
            .collect();
        for line in lrc.lines() {
            merged.push_str(line);
            merged.push('\n');
            if let Some((time, _)) = line.split_once(']')
                && let Some((_, text)) = translations.iter().find(|(t, _)| *t == time)
            {
                merged.push_str(time);
                merged.push(']');
                merged.push_str(text);
                merged.push('\n');
            }
        }
        Some(merged)
    } else {
        Some(lrc)
    };

    let yrc = (!yrc.is_empty()).then_some(yrc);
    (lrc, yrc)
}

/// Writes title/artist/album, lyrics and the cover into the downloaded file.
fn embed_tags(
    path: &Path,
    track: &NeteaseTrackInfo,
    cover: Option<Vec<u8>>,
    lrc: Option<&str>,
) -> Result<(), String> {
    let tag_type = match path.extension().and_then(|ext| ext.to_str()) {
        Some("flac") => TagType::VorbisComments,
        _ => TagType::Id3v2,
    };
    let mut tag = Tag::new(tag_type);
    if !track.title.is_empty() {
        tag.set_title(track.title.to_string());
    }
    if !track.artist.is_empty() {
        tag.set_artist(track.artist.to_string());
    }
    if !track.album.is_empty() {
        tag.set_album(track.album.to_string());
    }
    if let Some(lrc) = lrc {
        tag.insert(TagItem::new(
            ItemKey::UnsyncLyrics,
            ItemValue::Text(lrc.to_string()),
        ));
    }
    if let Some(bytes) = cover {
        let mime = if bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            MimeType::Jpeg
        } else {
            MimeType::Png
        };
        let picture = Picture::unchecked(bytes)
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build();
        tag.push_picture(picture);
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|err| err.to_string())
}

/// Downloads `track` into `dir` and returns the saved audio file path.
pub async fn download_track(
    client: &NeteaseClient,
    track: &NeteaseTrackInfo,
    dir: &Path,
) -> Result<PathBuf, String> {
    let (url, level) = resolve_best_url(client, track).await?;
    let ext = if level == "lossless" || level == "hires" {
        // the `type` field of the URL response is authoritative, but both
        // tiers are FLAC in practice
        "flac"
    } else {
        "mp3"
    };

    std::fs::create_dir_all(dir).map_err(|err| format!("create download dir: {err}"))?;

    let stem = sanitize_filename(&format!(
        "{}{}",
        if track.artist.is_empty() {
            String::new()
        } else {
            format!("{} - ", track.artist)
        },
        if track.title.is_empty() {
            format!("netease-{}", track.id)
        } else {
            track.title.to_string()
        }
    ));
    let audio_path = dir.join(format!("{stem}.{ext}"));

    let bytes = http_get_bytes(&url).await?;
    if bytes.len() < 1024 {
        return Err("downloaded file is empty".into());
    }
    std::fs::write(&audio_path, &bytes).map_err(|err| format!("write audio: {err}"))?;

    // Lyric sidecars + tags/cover are best-effort: the audio file is already saved.
    let (lrc, yrc) = fetch_lyrics(client, track).await;
    if let Some(yrc) = &yrc {
        let _ = std::fs::write(dir.join(format!("{stem}.yrc")), yrc);
    }
    if let Some(lrc) = &lrc {
        let _ = std::fs::write(dir.join(format!("{stem}.lrc")), lrc);
    }
    let cover = if track.cover_url.is_empty() {
        None
    } else {
        http_cover_bytes(&track.cover_url).await.ok().flatten()
    };
    let _ = embed_tags(&audio_path, track, cover, lrc.as_deref());

    Ok(audio_path)
}

/// Fire-and-forget entry point used by the UI: downloads on the Tokio runtime
/// and reports the outcome with a toast. Cheap, so it is safe to call from a
/// click handler.
pub fn download_track_ui(cx: &mut gpui::App, track: NeteaseTrackInfo) {
    let dir = cx
        .global::<crate::settings::SettingsGlobal>()
        .model
        .read(cx)
        .playback
        .effective_download_dir();
    cx.spawn(async move |cx| {
        let client = crate::netease::shared_client();
        let result = crate::RUNTIME
            .spawn(async move { download_track(&client, &track, &dir).await })
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| "download task panicked".to_string());
        cx.update(|_cx| match result {
            Ok(path) => emit_toast(Toast::success(tr!(
                "NETEASE_DOWNLOADED",
                "Downloaded to {{path}}",
                path = path.display().to_string()
            ))),
            Err(err) => emit_toast(Toast::error(tr!(
                "NETEASE_DOWNLOAD_FAILED",
                "Download failed: {{err}}",
                err = err
            ))),
        });
    })
    .detach();
}
