//! In-app update check, download and install against GitHub Releases.
//!
//! Pipeline: check (`/releases/latest`, tag vs `CARGO_PKG_VERSION`) →
//! optional automatic download of the bare binary asset (uploaded by the
//! release workflow next to the zip/tar.gz packages) with sha256 verification
//! against the release's `checksums.txt` → rename-swap install
//! (`meliora.exe → meliora.exe.old`, `meliora.exe.new → meliora.exe`) →
//! explicit user restart. The swap works while the old process runs: Windows
//! forbids overwriting or deleting a mapped image but allows renaming it,
//! which is what makes a helper-free single-file update possible.
//!
//! Degradation paths never trap the user: old releases without a bare asset,
//! read-only install directories and failed verifications all leave the
//! release page reachable (or the check re-runnable), and a failed download
//! can never damage the running installation — the swap is two renames with
//! a rollback.

use std::{cmp::Ordering, path::Path, path::PathBuf, sync::LazyLock, time::Duration};

use cntp_i18n::tr;
use gpui::{App, AppContext, Entity, Global};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::{
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
};

const GITHUB_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/li-ming1/meliora/releases/latest";

/// Metadata + checksum fetches are tiny; a hard cap turns an unreachable API
/// into a `Failed` status instead of a hanging check.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// The bare binary is tens of MB on a possibly slow route; the cap only
/// guards against a stalled stream, not normal download duration.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Toast lifetime for update notices — an update prompt is easy to miss and
/// cheap to keep up compared to the default 5 s.
const UPDATE_TOAST_DURATION: Duration = Duration::from_secs(12);

/// The automatic check waits out the startup rush (first scan, audio init)
/// so its single request never competes with first-frame work.
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(8);

// `static`, not `const`: LazyLock is interior-mutable, and a `const` would
// inline a fresh lazily-initialized client at every use site (clippy::
// declare_interior_mutable_const).
static UPDATE_CLIENT: LazyLock<zed_reqwest::Client> = LazyLock::new(|| {
    zed_reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // No client-wide timeout: the download streams for minutes. Each
        // request sets its own cap instead.
        .user_agent(concat!("Meliora/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| zed_reqwest::Client::new())
});

/// Release asset identifier matching the bare binaries uploaded by
/// `.github/workflows/release.yml` (`meliora-vX.Y.Z-<platform>.<ext>`).
const PLATFORM_ID: &str = if cfg!(target_os = "windows") {
    if cfg!(target_arch = "x86_64") {
        "windows-x64"
    } else if cfg!(target_arch = "aarch64") {
        "windows-arm64"
    } else {
        "unsupported"
    }
} else if cfg!(target_os = "macos") {
    if cfg!(target_arch = "x86_64") {
        "macos-x64"
    } else if cfg!(target_arch = "aarch64") {
        "macos-arm64"
    } else {
        "unsupported"
    }
} else if cfg!(target_os = "linux") {
    if cfg!(target_arch = "x86_64") {
        "linux-x64"
    } else if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "unsupported"
    }
} else {
    "unsupported"
};

#[cfg(target_os = "windows")]
const BINARY_ASSET_EXT: &str = "exe";
#[cfg(not(target_os = "windows"))]
const BINARY_ASSET_EXT: &str = "bin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSource {
    /// The delayed launch-time check: stays silent unless an update is found.
    Startup,
    /// The settings-page button: feedback lives inline on the page.
    Manual,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    /// `url` is the release page (manual fallback when the bare asset is
    /// missing or the install directory is unwritable); `binary_url` is
    /// `None` for releases published before the bare binaries existed.
    Available {
        version: String,
        url: String,
        binary_url: Option<String>,
        checksums_url: Option<String>,
    },
    Downloading {
        version: String,
        received: u64,
        total: u64,
    },
    /// The swap already happened; only a restart separates the user from the
    /// new build. Normal quitting also applies it on the next launch.
    ReadyToRestart {
        version: String,
    },
    Failed,
}

#[derive(Debug)]
pub struct UpdateState {
    pub status: UpdateStatus,
}

pub struct UpdaterGlobal {
    pub state: Entity<UpdateState>,
}

impl Global for UpdaterGlobal {}

/// Subset of the GitHub Releases API response we consume.
#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Publish the updater state and scrub leftovers of a previous update
/// restart. Called once from the app bootstrap (`online` builds only).
pub fn init(cx: &mut App) {
    let state = cx.new(|_| UpdateState {
        status: UpdateStatus::Idle,
    });
    cx.set_global(UpdaterGlobal { state });

    // `meliora.exe.old` / `.new` next to the executable: normally the remains
    // of an update restart, where the previous process still holds the
    // renamed image for a moment — hence the delay and retries.
    crate::RUNTIME.spawn(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        cleanup_stale_install_files().await;
    });
}

/// Schedule the once-per-launch automatic check (gated by the
/// `update.auto_check` setting).
pub fn schedule_startup_check(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(STARTUP_CHECK_DELAY).await;
        let auto_check = cx.update(|cx| {
            cx.has_global::<SettingsGlobal>()
                && cx
                    .global::<SettingsGlobal>()
                    .model
                    .read(cx)
                    .update
                    .auto_check
        });
        if auto_check {
            cx.update(|cx| check_for_updates(cx, CheckSource::Startup));
        }
    })
    .detach();
}

/// Run one check. Re-entrant calls while a check or download is in flight
/// are ignored; the status machine is only ever mutated on the UI thread.
pub fn check_for_updates(cx: &mut App, source: CheckSource) {
    if !cx.has_global::<UpdaterGlobal>() {
        return;
    }
    let state = cx.global::<UpdaterGlobal>().state.clone();
    if matches!(
        state.read(cx).status,
        UpdateStatus::Checking | UpdateStatus::Downloading { .. }
    ) {
        return;
    }
    state.update(cx, |state, cx| {
        state.status = UpdateStatus::Checking;
        cx.notify();
    });

    cx.spawn(async move |cx| {
        let fetched = crate::RUNTIME.spawn(fetch_latest_release()).await;
        cx.update(|cx| match fetched {
            Ok(Ok(release)) => apply_check_result(cx, release, source),
            Ok(Err(error)) => mark_check_failed(cx, error),
            Err(error) => mark_check_failed(cx, format!("update check task failed: {error}")),
        });
    })
    .detach();
}

fn mark_check_failed(cx: &mut App, error: String) {
    warn!(error = %error, "updater: check failed");
    let state = cx.global::<UpdaterGlobal>().state.clone();
    state.update(cx, |state, cx| {
        state.status = UpdateStatus::Failed;
        cx.notify();
    });
}

fn apply_check_result(cx: &mut App, release: LatestRelease, source: CheckSource) {
    let Some(Ordering::Greater) = compare_versions(env!("CARGO_PKG_VERSION"), &release.tag_name)
    else {
        // Equal or older, or a tag that doesn't parse into three numeric
        // components — "cannot judge" must never offer an update.
        info!(tag = %release.tag_name, "updater: no update over the running version");
        let state = cx.global::<UpdaterGlobal>().state.clone();
        state.update(cx, |state, cx| {
            state.status = UpdateStatus::UpToDate;
            cx.notify();
        });
        return;
    };

    info!(version = %release.tag_name, "updater: update available");
    let (binary_url, checksums_url) = find_update_assets(&release);
    let state = cx.global::<UpdaterGlobal>().state.clone();
    state.update(cx, |state, cx| {
        state.status = UpdateStatus::Available {
            version: release.tag_name.clone(),
            url: release.html_url.clone(),
            binary_url: binary_url.clone(),
            checksums_url: checksums_url.clone(),
        };
        cx.notify();
    });

    if source == CheckSource::Startup {
        let page_url = release.html_url.clone();
        emit_toast(
            Toast::info(tr!(
                "UPDATE_AVAILABLE_TOAST",
                "Meliora {{version}} is available",
                version = release.tag_name.clone()
            ))
            .with_duration(UPDATE_TOAST_DURATION)
            .with_action(tr!("UPDATE_OPEN_RELEASE"), move |cx| cx.open_url(&page_url)),
        );
    }

    // The toggle is the user's standing instruction; the check origin only
    // decides whether a toast accompanied the offer.
    let auto_download = cx.has_global::<SettingsGlobal>()
        && cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .update
            .auto_download;
    if auto_download && binary_url.is_some() {
        start_download(cx);
    }
}

/// Download the bare binary, verify it, and swap it into place. No-op unless
/// the state machine is sitting on `Available` with a matching asset.
pub fn start_download(cx: &mut App) {
    if !cx.has_global::<UpdaterGlobal>() {
        return;
    }
    let state = cx.global::<UpdaterGlobal>().state.clone();
    let (version, binary_url, checksums_url) = match &state.read(cx).status {
        UpdateStatus::Available {
            version,
            binary_url: Some(binary_url),
            checksums_url,
            ..
        } => (version.clone(), binary_url.clone(), checksums_url.clone()),
        _ => return,
    };
    let Some((exe_path, staged_path)) = install_paths() else {
        warn!("updater: cannot locate the running executable; download aborted");
        return;
    };

    state.update(cx, |state, cx| {
        state.status = UpdateStatus::Downloading {
            version: version.clone(),
            received: 0,
            total: 0,
        };
        cx.notify();
    });

    cx.spawn(async move |cx| {
        let (progress_tx, mut progress_rx) = tokio::sync::watch::channel((0u64, 0u64));
        let mut task = crate::RUNTIME.spawn(async move {
            download_and_swap(
                binary_url,
                checksums_url,
                exe_path,
                staged_path,
                progress_tx,
            )
            .await
        });

        // Forward throttled progress to the UI state until the worker either
        // finishes or drops its sender.
        let mut sender_alive = true;
        let result = loop {
            if !sender_alive {
                break task
                    .await
                    .map_err(|error| format!("download task failed: {error}"))
                    .and_then(|result| result);
            }
            tokio::select! {
                changed = progress_rx.changed() => {
                    if changed.is_err() {
                        sender_alive = false;
                    } else {
                        let (received, total) = *progress_rx.borrow();
                        cx.update(|cx| {
                            state.update(cx, |state, cx| {
                                let next = UpdateStatus::Downloading {
                                    version: version.clone(),
                                    received,
                                    total,
                                };
                                if state.status != next {
                                    state.status = next;
                                    cx.notify();
                                }
                            });
                        });
                    }
                }
                done = &mut task => {
                    break done
                        .map_err(|error| format!("download task failed: {error}"))
                        .and_then(|result| result);
                }
            }
        };

        cx.update(|cx| match result {
            Ok(()) => {
                info!(version = %version, "updater: staged update installed, restart to apply");
                let state = cx.global::<UpdaterGlobal>().state.clone();
                state.update(cx, |state, cx| {
                    state.status = UpdateStatus::ReadyToRestart {
                        version: version.clone(),
                    };
                    cx.notify();
                });
                emit_toast(
                    Toast::success(tr!(
                        "UPDATE_READY_TOAST",
                        "Meliora {{version}} downloaded — restart to apply",
                        version = version.clone()
                    ))
                    .with_duration(UPDATE_TOAST_DURATION)
                    .with_action(tr!("UPDATE_RESTART_NOW"), restart_to_apply),
                );
            }
            Err(error) => {
                warn!(error = %error, "updater: download/install failed");
                let state = cx.global::<UpdaterGlobal>().state.clone();
                state.update(cx, |state, cx| {
                    state.status = UpdateStatus::Failed;
                    cx.notify();
                });
            }
        });
    })
    .detach();
}

/// Spawn the freshly installed binary and quit through gpui so the
/// `on_app_quit` flushes (stats, storage) still run.
pub fn restart_to_apply(cx: &mut App) {
    if crate::spawn_replacement() {
        cx.quit();
    } else {
        emit_toast(
            Toast::error(tr!(
                "UPDATE_RESTART_FAILED_TOAST",
                "Failed to restart — please restart manually"
            ))
            .with_duration(UPDATE_TOAST_DURATION),
        );
    }
}

async fn fetch_latest_release() -> Result<LatestRelease, String> {
    let response = UPDATE_CLIENT
        .get(GITHUB_LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .timeout(CHECK_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub API returned {}", response.status()));
    }
    response
        .json::<LatestRelease>()
        .await
        .map_err(|error| format!("unexpected response: {error}"))
}

async fn http_get_text(url: &str) -> Result<String, String> {
    let response = UPDATE_CLIENT
        .get(url)
        .timeout(CHECK_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("request to {url} failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url} returned {}", response.status()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("reading {url} failed: {error}"))
}

/// Download → sha256 → swap. Runs entirely on the tokio runtime; the watch
/// sender carries throttled `(received, total)` progress to the UI.
async fn download_and_swap(
    binary_url: String,
    checksums_url: Option<String>,
    exe_path: PathBuf,
    staged_path: PathBuf,
    progress: tokio::sync::watch::Sender<(u64, u64)>,
) -> Result<(), String> {
    // checksums.txt ships with every release that also has bare binaries; a
    // missing manifest (or entry) degrades to HTTPS-only integrity, loudly.
    let expected_hash = match &checksums_url {
        Some(url) => {
            let manifest = http_get_text(url).await?;
            let asset_name = binary_url.rsplit('/').next().unwrap_or_default();
            match lookup_checksum(&manifest, asset_name) {
                Some(hash) => Some(hash),
                None => {
                    warn!(asset = %asset_name, "updater: no checksum entry, skipping verification");
                    None
                }
            }
        }
        None => {
            warn!("updater: release has no checksums.txt, skipping verification");
            None
        }
    };

    let response = UPDATE_CLIENT
        .get(&binary_url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("download request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("download returned {}", response.status()));
    }
    let total = response.content_length().unwrap_or(0);
    let mut response = response;
    let mut file = tokio::fs::File::create(&staged_path)
        .await
        .map_err(|error| {
            format!(
                "cannot stage update in {}: {error} (is the install directory writable?)",
                staged_path.display()
            )
        })?;
    use tokio::io::AsyncWriteExt;

    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut last_sent: u64 = 0;
    // ~2% steps (or 1 MB when the size is unknown) — chunk-level updates
    // would only burn renders.
    let step: u64 = if total >= 100 {
        total / 50
    } else {
        1024 * 1024
    };
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("download interrupted: {error}"))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("writing staged update failed: {error}"))?;
        received += chunk.len() as u64;
        if received - last_sent >= step {
            last_sent = received;
            let _ = progress.send((received, total));
        }
    }
    file.flush()
        .await
        .map_err(|error| format!("flushing staged update failed: {error}"))?;
    drop(file);
    let _ = progress.send((received, total));

    if let Some(expected) = &expected_hash {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(&staged_path).await;
            return Err(format!(
                "checksum mismatch for staged update: expected {expected}, got {actual}"
            ));
        }
    }

    swap_install(&exe_path, &staged_path)?;
    Ok(())
}

/// The two-rename swap with rollback; see the module docs for why renaming a
/// running image is safe on Windows.
fn swap_install(exe_path: &Path, staged_path: &Path) -> Result<(), String> {
    let old_path = append_suffix(exe_path, ".old");
    // A stale backup from an interrupted earlier update must not block us.
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(exe_path, &old_path)
        .map_err(|error| format!("cannot back up the current executable: {error}"))?;
    if let Err(error) = std::fs::rename(staged_path, exe_path) {
        // Roll the backup back so the installation stays runnable.
        let _ = std::fs::rename(&old_path, exe_path);
        return Err(format!("cannot move the staged update into place: {error}"));
    }
    Ok(())
}

/// `(running executable path, staged download path)` — the staged file lives
/// next to the executable so the final rename stays within one volume.
fn install_paths() -> Option<(PathBuf, PathBuf)> {
    let exe_path = std::env::current_exe().ok()?;
    let staged_path = append_suffix(&exe_path, ".new");
    Some((exe_path, staged_path))
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

async fn cleanup_stale_install_files() {
    let Some((exe_path, staged_path)) = install_paths() else {
        return;
    };
    for path in [append_suffix(&exe_path, ".old"), staged_path] {
        for attempt in 0..5 {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    info!(path = %path.display(), "updater: removed stale update file");
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                // The just-replaced process may still hold the image for a
                // moment after an update restart; retry briefly.
                Err(_) if attempt < 4 => tokio::time::sleep(Duration::from_secs(2)).await,
                Err(error) => {
                    warn!(path = %path.display(), error = %error, "updater: could not remove stale update file");
                    break;
                }
            }
        }
    }
}

/// `(bare binary asset URL, checksums.txt asset URL)` for this platform.
fn find_update_assets(release: &LatestRelease) -> (Option<String>, Option<String>) {
    let binary_name = format!(
        "meliora-{}-{PLATFORM_ID}.{BINARY_ASSET_EXT}",
        release.tag_name
    );
    let mut binary_url = None;
    let mut checksums_url = None;
    for asset in &release.assets {
        if asset.name == binary_name {
            binary_url = Some(asset.browser_download_url.clone());
        } else if asset.name == "checksums.txt" {
            checksums_url = Some(asset.browser_download_url.clone());
        }
    }
    (binary_url, checksums_url)
}

/// `sha256sum` line format: `<hex> <whitespace> <filename>`, with the
/// optional `*` binary-mode marker before the filename. Entries whose hash
/// isn't 64 hex chars are ignored (protects against a mangled manifest).
fn lookup_checksum(manifest: &str, asset_name: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut tokens = line.split_whitespace();
        let hash = tokens.next()?;
        let name = tokens.next()?;
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != asset_name {
            return None;
        }
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(hash.to_ascii_lowercase())
    })
}

/// "v1.2.3" / "1.2.3" → `[1, 2, 3]`. Anything else (rc suffix, missing or
/// extra components) is unparsable — see [`compare_versions`].
fn parse_version_tag(tag: &str) -> Option<[u64; 3]> {
    let trimmed = tag.trim();
    let digits = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    let mut parts = digits.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// `Some(Greater)` means the tag is a newer release than the running version;
/// `None` means either side didn't parse and no conclusion may be drawn.
fn compare_versions(current: &str, tag: &str) -> Option<Ordering> {
    Some(parse_version_tag(current)?.cmp(&parse_version_tag(tag)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with_assets(assets: Vec<ReleaseAsset>) -> LatestRelease {
        LatestRelease {
            tag_name: "v0.0.5".to_string(),
            html_url: "https://github.com/li-ming1/meliora/releases/tag/v0.0.5".to_string(),
            assets,
        }
    }

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn version_tags_compare_numerically() {
        assert_eq!(compare_versions("0.0.4", "v0.0.5"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.0.4", "v0.1.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.0.99", "v0.1.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.0.4", "v0.0.4"), Some(Ordering::Equal));
        assert_eq!(compare_versions("0.1.0", "v0.0.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.0.0", "v0.9.9"), Some(Ordering::Greater));
    }

    #[test]
    fn unparsable_tags_are_never_judged() {
        assert_eq!(parse_version_tag("v1.2"), None);
        assert_eq!(parse_version_tag("v1.2.3.4"), None);
        assert_eq!(parse_version_tag("v1.2.3-rc1"), None);
        assert_eq!(parse_version_tag("latest"), None);
        assert_eq!(parse_version_tag(""), None);
        assert_eq!(compare_versions("0.0.4", "v1.2.3-rc1"), None);
    }

    #[test]
    fn update_assets_match_platform_and_checksums() {
        let binary_name = format!("meliora-v0.0.5-{PLATFORM_ID}.{BINARY_ASSET_EXT}");
        let release = release_with_assets(vec![
            asset("checksums.txt"),
            asset(&format!("meliora-v0.0.5-{PLATFORM_ID}.zip")),
            asset(&binary_name),
            asset("meliora-v0.0.5-macos-arm64.bin"),
        ]);
        let (binary, checksums) = find_update_assets(&release);
        assert_eq!(
            binary.as_deref(),
            Some(format!("https://example.com/{binary_name}").as_str())
        );
        assert_eq!(
            checksums.as_deref(),
            Some("https://example.com/checksums.txt")
        );
    }

    #[test]
    fn old_releases_without_bare_binaries_degrade_to_none() {
        let release = release_with_assets(vec![
            asset("meliora-v0.0.5-windows-x64.zip"),
            asset("meliora-v0.0.5-windows-x64.tar.gz"),
        ]);
        let (binary, checksums) = find_update_assets(&release);
        assert!(binary.is_none());
        assert!(checksums.is_none());
    }

    #[test]
    fn checksums_manifest_lookup() {
        let manifest = concat!(
            "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789  meliora-v0.0.5-windows-x64.exe\n",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *meliora-v0.0.5-windows-x64.zip\n",
            "short  name-does-not-matter\n",
            "not-a-hash  meliora-v0.0.5-windows-x64.exe\n",
        );
        assert_eq!(
            lookup_checksum(manifest, "meliora-v0.0.5-windows-x64.exe"),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
        assert_eq!(
            lookup_checksum(manifest, "meliora-v0.0.5-windows-x64.zip"),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string())
        );
        assert_eq!(lookup_checksum(manifest, "missing"), None);
    }

    #[test]
    fn swap_install_moves_staged_binary_into_place() {
        let dir = crate::test_support::TestDir::new("meliora-updater-swap");
        let exe = dir.join("meliora.exe");
        let staged = append_suffix(&exe, ".new");
        std::fs::write(&exe, b"old-binary").unwrap();
        std::fs::write(&staged, b"new-binary").unwrap();

        swap_install(&exe, &staged).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary");
        assert_eq!(
            std::fs::read(append_suffix(&exe, ".old")).unwrap(),
            b"old-binary"
        );
        assert!(!staged.exists());
    }

    #[test]
    fn swap_install_overwrites_a_stale_backup() {
        let dir = crate::test_support::TestDir::new("meliora-updater-swap-stale");
        let exe = dir.join("meliora.exe");
        let staged = append_suffix(&exe, ".new");
        std::fs::write(&exe, b"old-binary").unwrap();
        std::fs::write(&staged, b"new-binary").unwrap();
        std::fs::write(append_suffix(&exe, ".old"), b"stale-backup").unwrap();

        swap_install(&exe, &staged).unwrap();

        assert_eq!(
            std::fs::read(append_suffix(&exe, ".old")).unwrap(),
            b"old-binary"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary");
    }
}
