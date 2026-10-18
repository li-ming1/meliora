use std::cmp::Reverse;
use std::path::Path;

#[cfg(target_os = "macos")]
use std::{ffi::CStr, mem::MaybeUninit};

use camino::Utf8PathBuf;
use rustc_hash::FxHashMap;
use sysinfo::Disks;

pub(crate) type DiskGroups = (
    Vec<Vec<Utf8PathBuf>>,
    Vec<Utf8PathBuf>,
    FxHashMap<Utf8PathBuf, usize>,
);

/// 按物理磁盘给库路径分组，供慢盘并行 I/O 使用。
/// 返回 (分组, 按长度降序的挂载点, 挂载点到通道)。未知挂载点用通道 0。
pub(crate) fn group_paths_by_disk(paths: &[Utf8PathBuf]) -> DiskGroups {
    if paths.is_empty() {
        return (Vec::new(), Vec::new(), FxHashMap::default());
    }

    let disks = Disks::new_with_refreshed_list();

    // 长的排前面，这样嵌套挂载点先于其父挂载点匹配上
    let mut mounts: Vec<&Path> = disks.iter().map(|d| d.mount_point()).collect();
    mounts.sort_by_key(|mount| Reverse(mount.as_os_str().as_encoded_bytes().len()));

    let mount_to_physical: Vec<(Utf8PathBuf, String)> = mounts
        .iter()
        .filter_map(|m| {
            let mount = Utf8PathBuf::from_path_buf(m.to_path_buf()).ok()?;
            let device_id = physical_device_id(m).unwrap_or_default();
            Some((mount, device_id))
        })
        .collect();

    let mount_points: Vec<Utf8PathBuf> = mount_to_physical.iter().map(|(m, _)| m.clone()).collect();

    // 按发现顺序分配通道，这样它们与路由器对得上
    let mut groups: Vec<Vec<Utf8PathBuf>> = Vec::new();
    let mut physical_to_channel: FxHashMap<String, usize> = FxHashMap::default();

    for path in paths {
        let canonical = path.canonicalize_utf8().unwrap_or(path.clone());
        let device_id = mount_to_physical
            .iter()
            .find(|(m, _)| path_is_under_mount(canonical.as_std_path(), m.as_std_path()))
            .map(|(_, id)| id.clone())
            .unwrap_or_default();

        let channel = *physical_to_channel.entry(device_id).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[channel].push(path.clone());
    }

    let mount_to_channel: FxHashMap<Utf8PathBuf, usize> = mount_to_physical
        .iter()
        .filter_map(|(mount, dev_id)| {
            physical_to_channel
                .get(dev_id)
                .copied()
                .map(|channel| (mount.clone(), channel))
        })
        .collect();

    (groups, mount_points, mount_to_channel)
}

#[cfg(not(windows))]
fn path_is_under_mount(path: &Path, mount: &Path) -> bool {
    path.starts_with(mount)
}

#[cfg(windows)]
fn path_is_under_mount(path: &Path, mount: &Path) -> bool {
    if path.starts_with(mount) {
        return true;
    }
    let Some(mount_str) = mount.to_str() else {
        return false;
    };
    if mount_str.starts_with(r"\\?\") {
        return false;
    }
    let verbatim = if let Some(share) = mount_str.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{share}")
    } else {
        format!(r"\\?\{mount_str}")
    };
    path.starts_with(Path::new(&verbatim))
}

/// 挂载点背后的物理磁盘，好让同一块盘上的分区共享一个通道。macOS：BSD 名，
/// Linux：sysfs 块设备，Windows：IOCTL（否则用盘符/UNC）。
fn physical_device_id(mount_point: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut stat = MaybeUninit::<libc::statfs>::uninit();
        let cpath = std::ffi::CString::new(mount_point.as_os_str().as_encoded_bytes()).ok()?;
        if unsafe { libc::statfs(cpath.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return None;
        }
        let stat = unsafe { stat.assume_init() };

        let bsd_name = unsafe { CStr::from_ptr(stat.f_mntfromname.as_ptr()) }
            .to_bytes()
            .strip_prefix(b"/dev/")
            .and_then(|name| std::str::from_utf8(name).ok())?;

        // "disk0s1" -> "disk0"，"disk0s1s2" -> "disk0s1"（同一块盘上的 APFS 卷）
        Some(
            bsd_name
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .trim_end_matches('s')
                .to_string(),
        )
    }

    #[cfg(target_os = "linux")]
    {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let mount_str = mount_point.to_str()?;
        let device = mountinfo
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let _id = parts.next()?;
                let _parent = parts.next()?;
                let dev = parts.next()?;
                let _root = parts.next()?;
                let mp = parts.next()?;
                Some((mp, dev))
            })
            .find(|(mp, _)| *mp == mount_str)
            .map(|(_, dev)| dev.to_string())?;

        linux_physical_device_id(&device)
    }

    #[cfg(target_os = "windows")]
    {
        let mount = mount_point.to_str()?;

        if let Some(share) = mount.strip_prefix(r"\\") {
            let share = share.trim_end_matches('\\');
            if share.is_empty() {
                return None;
            }
            return Some(format!("unc_{}", share.to_lowercase()));
        }

        let mut chars = mount.chars();
        let letter = chars.next()?.to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() || chars.next() != Some(':') {
            return None;
        }

        Some(windows_physical_drive_id(letter).unwrap_or_else(|| format!("drive_{letter}")))
    }
}

#[cfg(target_os = "linux")]
fn linux_physical_device_id(device: &str) -> Option<String> {
    let sysfs_device = std::fs::canonicalize(Path::new("/sys/dev/block").join(device)).ok();

    let device_name = sysfs_device.and_then(|mut path| {
        // 用父磁盘的名字，这样它的所有分区共享一个通道
        if path.join("partition").exists() {
            path = path.parent()?.to_path_buf();
        }
        path.file_name()?.to_str().map(str::to_owned)
    });

    // 设备解析不出来时，用它的完整标识符，而不是只按主号分组
    Some(
        device_name
            .map(|name| format!("linux:{name}"))
            .unwrap_or_else(|| format!("linux:{device}")),
    )
}

/// 经 IOCTL 取物理盘号。卷打不开或跨多块盘时为 None。
#[cfg(target_os = "windows")]
fn windows_physical_drive_id(letter: char) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};
    use windows::core::HSTRING;

    let volume = HSTRING::from(format!(r"\\.\{letter}:"));
    // access 0 允许元数据 IOCTL 而无需管理员权限
    let handle = unsafe {
        CreateFileW(
            &volume,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;

    let mut number = STORAGE_DEVICE_NUMBER::default();
    let mut bytes_returned = 0u32;

    let result = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some((&raw mut number).cast()),
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }

    // 跨多块盘的卷会报 DeviceNumber = 0xFFFFFFFF
    result.ok().filter(|_| number.DeviceNumber != u32::MAX)?;
    Some(format!("physicaldrive_{}", number.DeviceNumber))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use std::path::Path;

    #[test]
    fn group_paths_by_disk_single_disk() {
        let dir = TestDir::new("disk-group-test");
        let p1 = dir.utf8_join("music");
        let p2 = dir.utf8_join("podcasts");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&p2).unwrap();

        let (groups, mounts, mount_to_channel) = group_paths_by_disk(&[p1, p2]);

        assert_eq!(groups.len(), 1, "expected 1 group, got {groups:?}");
        let paths = groups.into_iter().next().unwrap();
        assert_eq!(paths.len(), 2);

        assert!(
            !mount_to_channel.is_empty(),
            "expected non-empty mount_to_channel"
        );
        // sysinfo 可能列出没有分配任何路径的挂载点
        assert!(mount_to_channel.len() <= mounts.len());
    }

    #[test]
    fn physical_device_id_returns_disk_for_root() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let id = physical_device_id(Path::new(root));
        assert!(
            id.is_some(),
            "root filesystem should have a physical device ID"
        );
        let id = id.unwrap();
        assert!(!id.is_empty(), "physical device ID should not be empty");
        if cfg!(target_os = "macos") {
            assert!(
                id.len() > 4,
                "physical_device_id('/') = '{id}' on macOS. expected 'diskN' with a digit, \
                 got '{id}' which is stripped too far. Fix the stripping logic."
            );
        }
    }

    #[test]
    fn group_paths_by_disk_fallback_on_nonexistent_path() {
        let dir = TestDir::new("disk-group-test");
        let nonexistent = dir.utf8_join("does-not-exist");

        let (groups, _mounts, _mount_to_channel) = group_paths_by_disk(&[nonexistent]);

        // 不存在的路径也照样得到一个分组（回退键 ""）
        assert_eq!(groups.len(), 1);
    }
}
