use std::fs::exists;

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissingFolderPolicy {
    #[default]
    Ask,
    KeepInLibrary,
    DeleteFromLibrary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanSettings {
    // 默认值刻意是纯函数之外的：缺 `paths` 键的 settings.json 会在这里向 OS 探测
    // 标准音乐文件夹（WinRT StorageLibrary + canonicalize，见
    // `retrieve_default_paths`）。只有该键缺失时才运行，失败时返回空表也无害。
    #[serde(default = "retrieve_default_paths")]
    pub paths: Vec<Utf8PathBuf>,
    #[serde(default, deserialize_with = "crate::settings::deserialize_lenient")]
    pub missing_folder_policy: MissingFolderPolicy,
    #[serde(default)]
    pub slow_disk_mode: bool,
    #[serde(default = "default_watch_for_changes")]
    pub watch_for_changes: bool,
}

fn default_watch_for_changes() -> bool {
    true
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            paths: retrieve_default_paths(),
            missing_folder_policy: MissingFolderPolicy::default(),
            slow_disk_mode: false,
            watch_for_changes: true,
        }
    }
}

fn retrieve_default_paths() -> Vec<Utf8PathBuf> {
    #[cfg(target_os = "windows")]
    {
        use windows::{
            Storage::{KnownLibraryId, StorageLibrary},
            Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
        };

        // WinRT 要求当前线程有套间，而本函数跑在 tokio blocking 线程上（全仓没有
        // 其它初始化点）：不初始化会返回 CO_E_NOTINITIALIZED，首启的默认扫描
        // 目录直接为空。用 MTA 而不是 STA——`join()` 阻塞等异步完成，STA 没有
        // 消息泵会死锁。`S_FALSE`（已是 MTA）也算成功、同样需要配对反初始化；
        // `RPC_E_CHANGED_MODE`（线程已是 STA）初始化失败，此时不配对。
        let initialized = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();

        let folders = StorageLibrary::GetLibraryAsync(KnownLibraryId::Music)
            .and_then(|operation| operation.join())
            .and_then(|library| library.Folders());

        let mut paths: Vec<Utf8PathBuf> = match folders {
            Ok(folders) => folders
                .into_iter()
                .filter_map(|folder| match folder.Path() {
                    Ok(path) if !path.is_empty() => Some(Utf8PathBuf::from(path.to_string())),
                    Ok(_) => {
                        warn!("A Music library folder has no filesystem path: skipping it.");
                        None
                    }
                    Err(e) => {
                        warn!(
                            "Couldn't get the path of a Music library folder ({e}): skipping it."
                        );
                        None
                    }
                })
                .filter_map(|path| match path.canonicalize_utf8() {
                    Ok(path) => Some(path),
                    Err(e) => {
                        warn!(
                            "Couldn't canonicalize a Music library folder path ({e}): skipping it."
                        );
                        None
                    }
                })
                .collect(),
            Err(e) => {
                warn!(
                    "Couldn't retrieve the Music library ({e}): falling back to the user audio directory."
                );
                Vec::new()
            }
        };

        if initialized {
            unsafe { RoUninitialize() };
        }

        // 首启绝不空表：WinRT 不可用（权限/服务异常）时退回用户音频目录
        if paths.is_empty() {
            paths = user_audio_dir_paths();
        }
        paths
    }

    #[cfg(not(target_os = "windows"))]
    {
        user_audio_dir_paths()
    }
}

/// 用户音频目录探测（非 Windows 的主路径，也是 Windows 上 WinRT 不可用时的回退）。
fn user_audio_dir_paths() -> Vec<Utf8PathBuf> {
    let Some(user_directories) = directories::UserDirs::new() else {
        return default_paths_failure("couldn't find your home directory");
    };

    let dir = user_directories
        .audio_dir()
        .map(ToOwned::to_owned)
        .or_else(|| {
            warn!("Music directory couldn't be discovered normally, using $HOME/Music.");
            Some(user_directories.home_dir().join("Music"))
        });
    let Some(dir) = dir.filter(|dir| exists(dir).unwrap_or(false)) else {
        return default_paths_failure("the Music directory doesn't exist");
    };

    match Utf8PathBuf::from_path_buf(dir) {
        Ok(path) => vec![path],
        Err(_) => default_paths_failure("the Music directory path isn't valid UTF-8"),
    }
}

fn default_paths_failure(reason: &str) -> Vec<Utf8PathBuf> {
    warn!(
        "Could not find a usable Music directory ({reason}); nothing will be scanned by default."
    );
    Vec::new()
}
