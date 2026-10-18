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

/// 把旧 `li-ming1/meliora` 命名遗留的数据目录与日志目录迁到当前 `meliora`
/// 名下。新目录已存在时重跑即空操作；任一源目录缺失则整段搬迁跳过。
/// 每棵搬迁失败的目录树各返回一条记录，供 `main` 在 logger 起来后统一
/// 打日志（本函数运行在 `logging::init` 之前）。
pub fn migrate_legacy_li_ming1_dirs() -> Vec<std::io::Error> {
    migrate_legacy_dirs_from("li-ming1", "meliora")
}

/// 把旧 `mailliw/hummingbird` 命名遗留的数据目录与日志目录迁到当前
/// `meliora` 名下。新目录已存在时重跑即空操作；任一源目录缺失则整段搬迁跳过。
/// 每棵搬迁失败的目录树各返回一条记录，供 `main` 在 logger 起来后统一
/// 打日志（本函数运行在 `logging::init` 之前）。
pub fn migrate_legacy_dirs() -> Vec<std::io::Error> {
    migrate_legacy_dirs_from("mailliw", "hummingbird")
}

/// 两个旧目录迁移共用的实现：把 `org/app` 的数据目录与本地数据目录搬到当前
/// `meliora` 名下。两棵树都会尝试，各自的失败分别返回。
fn migrate_legacy_dirs_from(org: &str, app: &str) -> Vec<std::io::Error> {
    let legacy = dirs_for(org, app);
    let mut failures = Vec::new();
    if let Err(err) = migrate_tree(legacy.data_dir(), &data_dir()) {
        failures.push(err);
    }
    if let Err(err) = migrate_tree(legacy.data_local_dir(), &log_dir_for_current()) {
        failures.push(err);
    }
    failures
}

fn log_dir_for_current() -> PathBuf {
    log_dir_in(project_dirs(), None)
}

fn migrate_tree(old: &Path, new: &Path) -> std::io::Result<()> {
    if !old.is_dir() || new.exists() {
        return Ok(());
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(old, new).is_err() {
        // 跨卷搬迁时回退到"先复制再删除"
        if let Err(err) = copy_recursive(old, new) {
            // 复制中途失败会留下半成品 `new`；删掉它，下次启动才会重试而非
            // 永久跳过搬迁。整体删除是安全的：上面的 `new.exists()` 门槛保证
            // 此处没有任何既有内容。
            let _ = std::fs::remove_dir_all(new);
            return Err(err);
        }
        let _ = std::fs::remove_dir_all(old);
    }
    Ok(())
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

    #[cfg(windows)]
    #[test]
    fn copy_fallback_failure_removes_partial_destination() {
        use std::os::windows::fs::OpenOptionsExt;

        let base = std::env::temp_dir().join("meliora-mig-test-partial");
        let old = base.join("old");
        let new = base.join("new");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(old.join("nested")).unwrap();
        std::fs::write(old.join("a.txt"), b"a").unwrap();
        let locked = old.join("nested").join("locked.bin");
        std::fs::write(&locked, b"locked").unwrap();

        // 以无共享标志打开并持有该文件，既拒绝 `old` 的改名（其下有打开的子孙），
        // 也拒绝该文件自身的复制，从而把 migrate_tree 逼进一次中途失败的
        // "先复制再删除"尝试
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();

        let err = migrate_tree(&old, &new).expect_err("mid-copy failure should propagate");
        drop(lock);
        assert!(
            !new.exists(),
            "half-built destination must be removed so the next start can \
             retry (copy error: {err})"
        );
        assert!(old.is_dir());

        // 锁释放后，下一次启动即可完成搬迁
        migrate_tree(&old, &new).unwrap();
        assert!(new.join("a.txt").exists());
        assert!(new.join("nested").join("locked.bin").exists());
        assert!(!old.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
