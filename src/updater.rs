//! 针对 GitHub Releases 的应用内更新检查、下载与安装。
//!
//! 流水线：检查（`/releases/latest`，tag 对比 `CARGO_PKG_VERSION`）→ 可选地自动
//! 下载裸二进制资产（由 release 工作流与 zip/tar.gz 包一同上传），并按 release 的
//! `checksums.txt` 校验 sha256 → 改名交换式安装
//! （`meliora.exe → meliora.exe.old`、`meliora.exe.new → meliora.exe`）→ 由用户
//! 显式重启。旧进程还在跑时交换也能成立：Windows 禁止覆写或删除已映射的镜像，
//! 但允许改名，正是这一点让"无需辅助程序的单文件更新"成为可能。
//!
//! 降级路径绝不会把用户困住：没有裸资产的旧 release、只读的安装目录、校验失败，
//! 全都仍然能让 release 页面可打开（或让检查可重跑）；而下载失败绝不可能损坏正在
//! 运行的安装——交换只是两次改名，带一次回滚。

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

/// 元数据 + 校验和的抓取都很小；设硬上限能把不可达的 API 变成一个 `Failed` 状态，
/// 而不是一次挂死的检查。
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// 裸二进制有几十 MB，且可能走一条很慢的链路；这个上限只防流卡住，不管正常下载
/// 耗时。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 更新提示 toast 的存活时长——相比默认 5 s，更新提示很容易错过，而让它多留一会儿
/// 成本极低。
const UPDATE_TOAST_DURATION: Duration = Duration::from_secs(12);

/// 自动检查会等过启动高峰（首次扫描、音频初始化），让它那一次请求不与首帧工作
/// 抢资源。
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(8);

// 用 `static` 而不是 `const`：LazyLock 内部可变，写成 `const` 会在每个使用点内联
// 一份全新的、惰性初始化的客户端（clippy::declare_interior_mutable_const）。
static UPDATE_CLIENT: LazyLock<zed_reqwest::Client> = LazyLock::new(|| {
    zed_reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // 不设客户端级超时：下载要流式跑几分钟。每个请求各自设自己的上限。
        .user_agent(concat!("Meliora/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| zed_reqwest::Client::new())
});

/// 与 `.github/workflows/release.yml` 上传的裸二进制资产相匹配的标识
/// （`meliora-vX.Y.Z-<platform>.<ext>`）。
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
    /// 延迟到启动时的自动检查：除非发现更新，否则保持静默。
    Startup,
    /// 设置页上的按钮：反馈就地显示在该页面上。
    Manual,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    /// `url` 是 release 页面（裸资产缺失或安装目录不可写时的人工兜底）；
    /// `binary_url` 在裸二进制还不存在时发布的 release 上是 `None`。
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
    /// 交换已经完成，用户与新构建之间只差一次重启。正常退出时也会在下次启动应用
    /// 这次交换。
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

/// 我们实际消费的 GitHub Releases API 响应子集。
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

/// 发布更新器状态，并清扫上一次更新重启留下的残留。由应用启动引导调用一次
/// （仅 `online` 构建）。
pub fn init(cx: &mut App) {
    let state = cx.new(|_| UpdateState {
        status: UpdateStatus::Idle,
    });
    cx.set_global(UpdaterGlobal { state });

    // 可执行文件旁边的 `meliora.exe.old` / `.new`：通常是上一次更新重启的残留，
    // 此时前一个进程还短暂持有已改名的镜像——所以才需要延迟与重试。
    crate::RUNTIME.spawn(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        cleanup_stale_install_files().await;
    });
}

/// 安排每次启动只跑一次的自动检查（受 `update.auto_check` 设置门控）。
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

/// 非 progress 状态变更的统一入口：换入新状态并唤醒 UI。
fn set_status(cx: &mut App, status: UpdateStatus) {
    let state = cx.global::<UpdaterGlobal>().state.clone();
    state.update(cx, |state, cx| {
        state.status = status;
        cx.notify();
    });
}

/// 执行一次检查。检查或下载已在途时的重入调用会被忽略；状态机只在 UI 线程上变更。
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
    set_status(cx, UpdateStatus::Checking);

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
    set_status(cx, UpdateStatus::Failed);
}

fn apply_check_result(cx: &mut App, release: LatestRelease, source: CheckSource) {
    let Some(Ordering::Less) = compare_versions(env!("CARGO_PKG_VERSION"), &release.tag_name)
    else {
        // 等于或旧于运行中的版本，或者 tag 解析不出三个数字段——"无法判断"绝不能
        // 提供更新。
        info!(tag = %release.tag_name, "updater: no update over the running version");
        set_status(cx, UpdateStatus::UpToDate);
        return;
    };

    info!(version = %release.tag_name, "updater: update available");
    let (binary_url, checksums_url) = find_update_assets(&release);
    set_status(
        cx,
        UpdateStatus::Available {
            version: release.tag_name.clone(),
            url: release.html_url.clone(),
            binary_url: binary_url.clone(),
            checksums_url,
        },
    );

    if source == CheckSource::Startup {
        let page_url = release.html_url.clone();
        emit_toast(
            Toast::info(tr!(
                "UPDATE_AVAILABLE_TOAST",
                "Meliora {{version}} is available",
                version = release.tag_name
            ))
            .with_duration(UPDATE_TOAST_DURATION)
            .with_action(tr!("UPDATE_OPEN_RELEASE"), move |cx| cx.open_url(&page_url)),
        );
    }

    // 这个开关是用户长期有效的指令；检查来源只决定这次提示有没有配 toast。
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

/// 下载裸二进制、校验并交换到位。除非状态机正停在带匹配资产的 `Available`，
/// 否则是空操作。
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

    set_status(
        cx,
        UpdateStatus::Downloading {
            version: version.clone(),
            received: 0,
            total: 0,
        },
    );

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

        // 把节流后的进度转发给 UI 状态，直到 worker 结束或丢掉自己的发送端。
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
                set_status(
                    cx,
                    UpdateStatus::ReadyToRestart {
                        version: version.clone(),
                    },
                );
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
                set_status(cx, UpdateStatus::Failed);
            }
        });
    })
    .detach();
}

/// 启动刚装好的新二进制，并走 gpui 退出，让 `on_app_quit` 的落盘（统计、存储）
/// 仍然执行。
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

/// 下载 → sha256 → 交换。全程跑在 tokio runtime 上；watch 发送端把节流后的
/// `(received, total)` 进度带给 UI。
async fn download_and_swap(
    binary_url: String,
    checksums_url: Option<String>,
    exe_path: PathBuf,
    staged_path: PathBuf,
    progress: tokio::sync::watch::Sender<(u64, u64)>,
) -> Result<(), String> {
    // 失败即关闭：校验和清单是这条流程唯一的完整性锚点（没有代码签名、没有钉死
    // 的哈希）。清单或其条目缺失，就意味着我们无法区分损坏的下载与被篡改的下载，
    // 因此拒绝自动安装并把用户指向 release 页面——两条早退都发生在下载之前，
    // 校验用的哈希因此不可能被跳过。
    let checksums = checksums_url.as_ref().ok_or_else(|| {
        "updater: release has no checksums.txt; refusing to install an unverified binary — \
         download it manually from the release page instead"
            .to_string()
    })?;
    let manifest = http_get_text(checksums).await?;
    let asset_name = binary_url.rsplit('/').next().unwrap_or_default();
    let expected = lookup_checksum(&manifest, asset_name).ok_or_else(|| {
        format!(
            "updater: checksums.txt has no entry for {asset_name}; refusing to install an \
             unverified binary — download it manually from the release page instead"
        )
    })?;

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
    // 约 2% 一档（大小未知时 1 MB）——按 chunk 更新只会白烧重绘。
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
    file.sync_all()
        .await
        .map_err(|error| format!("syncing staged update failed: {error}"))?;
    drop(file);
    let _ = progress.send((received, total));

    let actual = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = tokio::fs::remove_file(&staged_path).await;
        return Err(format!(
            "checksum mismatch for staged update: expected {expected}, got {actual}"
        ));
    }

    swap_install(&exe_path, &staged_path)?;
    Ok(())
}

/// 带滚动的两次改名交换；为什么在 Windows 上给正在运行的镜像改名是安全的，见模块
/// 文档。
fn swap_install(exe_path: &Path, staged_path: &Path) -> Result<(), String> {
    let old_path = append_suffix(exe_path, ".old");
    // 上一次更新被打断留下的陈旧备份文件不能挡住我们。
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(exe_path, &old_path)
        .map_err(|error| format!("cannot back up the current executable: {error}"))?;
    if let Err(error) = std::fs::rename(staged_path, exe_path) {
        // 把备份滚回去，让安装保持可运行。
        let _ = std::fs::rename(&old_path, exe_path);
        return Err(format!("cannot move the staged update into place: {error}"));
    }
    Ok(())
}

/// `(正在运行的可执行文件路径, 暂存的下载路径)`——暂存文件放在可执行文件旁边，
/// 好让最终的改名留在同一个卷内。
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
                // 刚被替换掉的进程在更新重启后可能还短暂持有该镜像；稍作重试。
                Err(_) if attempt < 4 => tokio::time::sleep(Duration::from_secs(2)).await,
                Err(error) => {
                    warn!(path = %path.display(), error = %error, "updater: could not remove stale update file");
                    break;
                }
            }
        }
    }
}

/// 本平台对应的 `(裸二进制资产 URL, checksums.txt 资产 URL)`。
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

/// `sha256sum` 的行格式：`<hex> <空白> <文件名>`，文件名前可有可选的 `*` 二进制
/// 模式标记。哈希不是 64 个 hex 字符的条目会被忽略（防清单被弄乱）。
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

/// "v1.2.3" / "1.2.3" → `[1, 2, 3]`。其他形态（rc 后缀、段数缺失或多出）都解析
/// 不了——见 [`compare_versions`]。
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

/// `Some(Greater)` 表示运行中的版本比 tag 更新（即这是个旧 release）；`Some(Less)`
/// 表示 tag 是比运行版本更新的 release；`None` 表示任一侧解析不了，不得下任何结论。
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
