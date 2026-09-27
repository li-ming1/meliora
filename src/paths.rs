use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use directories::ProjectDirs;

static PROJECT_DIRS: OnceLock<ProjectDirs> = OnceLock::new();

fn dirs_for(org: &str, app: &str) -> ProjectDirs {
    ProjectDirs::from("org", org, app).expect("couldn't generate project dirs")
}

pub fn project_dirs() -> &'static ProjectDirs {
    PROJECT_DIRS.get_or_init(|| dirs_for("", "meliora"))
}

pub fn data_dir() -> PathBuf {
    project_dirs().data_dir().to_path_buf()
}

pub fn log_dir() -> PathBuf {
    log_dir_in(
        project_dirs(),
        std::env::var_os("MELIORA_LOG_DIR").as_deref(),
    )
}

/// Moves the data and log directories left behind by the legacy
/// `li-ming1/meliora` naming under the current `meliora` one.
/// Rerunning is a no-op once the new directories exist, and the whole move is
/// skipped when either source is missing.
pub fn migrate_legacy_li_ming1_dirs() {
    migrate_legacy_dirs_from("li-ming1", "meliora");
}

/// Moves the data and log directories left behind by the legacy
/// `mailliw/hummingbird` naming under the current `meliora` one.
/// Rerunning is a no-op once the new directories exist, and the whole move is
/// skipped when either source is missing.
pub fn migrate_legacy_dirs() {
    migrate_legacy_dirs_from("mailliw", "hummingbird");
}

/// Shared body of the two legacy-directory migrations: moves `org/app`'s data
/// and local-data directories under the current `meliora` ones.
fn migrate_legacy_dirs_from(org: &str, app: &str) {
    let legacy = dirs_for(org, app);
    migrate_tree(legacy.data_dir(), &data_dir());
    migrate_tree(legacy.data_local_dir(), &log_dir_for_current());
}

fn log_dir_for_current() -> PathBuf {
    log_dir_in(project_dirs(), None)
}

fn migrate_tree(old: &Path, new: &Path) {
    if !old.is_dir() || new.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(old, new).is_err() {
        // fall back to copy-then-remove for cross-volume moves
        match copy_recursive(old, new) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(old);
            }
            Err(err) => tracing::warn!(%err, "failed to migrate legacy data dir"),
        }
    }
}

fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn log_dir_in(dirs: &ProjectDirs, override_dir: Option<&OsStr>) -> PathBuf {
    override_dir
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_log_dir(dirs))
}

fn default_log_dir(dirs: &ProjectDirs) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        dirs.state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf()
    }

    #[cfg(not(target_os = "linux"))]
    {
        dirs.data_local_dir().to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_dir_uses_platform_default() {
        let dirs = dirs_for("", "meliora");

        #[cfg(target_os = "linux")]
        assert_eq!(
            default_log_dir(&dirs),
            dirs.state_dir().unwrap().to_path_buf()
        );

        #[cfg(not(target_os = "linux"))]
        assert_eq!(default_log_dir(&dirs), dirs.data_local_dir().to_path_buf());
    }

    #[test]
    fn log_dir_prefers_environment_override() {
        let dirs = dirs_for("", "meliora");
        let override_dir = std::env::temp_dir().join("meliora-log-override");

        assert_eq!(
            log_dir_in(&dirs, Some(override_dir.as_os_str())),
            override_dir,
        );
    }

    #[test]
    fn empty_log_dir_override_is_ignored() {
        let dirs = dirs_for("", "meliora");

        assert_eq!(
            log_dir_in(&dirs, Some(OsStr::new(""))),
            default_log_dir(&dirs),
        );
    }
}
