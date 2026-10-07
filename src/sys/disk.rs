//! 磁盘挂载过滤（跨平台纯逻辑；无平台依赖）。
//!
//! 对齐 C# `DHDeploy.Agent.Jobs.DiskUsageJob.IsTemporaryVolume`：
//! 文件系统类型黑名单、引导分区、系统虚拟文件系统目录三类规则。

/// 临时/虚拟文件系统类型黑名单（对齐 C# `DiskUsageJob.IsTemporaryVolume`）。
const TEMP_FS_TYPES: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "devfs",
    "overlay",
    "ramfs",
    "squashfs",
    "aufs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "pstore",
    "debugfs",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "configfs",
    "securityfs",
    "binfmt_misc",
    "rpc_pipefs",
    "devpts",
];

/// 临时/虚拟文件系统类型黑名单（只读视图）。
pub fn temporary_fs_types() -> &'static [&'static str] {
    TEMP_FS_TYPES
}

/// 挂载点归一：去除尾部斜杠（根 `/` 除外），与 `/proc/mounts` 及 df 输出对齐。
pub fn mount_key(mount: &str) -> &str {
    if mount.len() > 1 {
        mount.trim_end_matches('/')
    } else {
        mount
    }
}

/// 判断挂载点是否为临时/虚拟文件系统：
/// ① 文件系统类型黑名单（tmpfs/overlay/squashfs 等；传 `None` 时跳过该规则）；
/// ② 引导分区 `/boot`、`/boot/efi`（精确匹配）；
/// ③ `/sys`、`/proc`、`/dev`、`/run`、`/snap` 前缀（含子路径）。
pub fn is_temporary_mount(mount: &str, fs_type: Option<&str>) -> bool {
    if let Some(fs) = fs_type {
        if TEMP_FS_TYPES.iter().any(|t| t.eq_ignore_ascii_case(fs)) {
            return true;
        }
    }

    let mount = mount_key(mount);

    if mount == "/boot" || mount == "/boot/efi" {
        return true;
    }

    if mount == "/sys"
        || mount == "/proc"
        || mount == "/dev"
        || mount == "/run"
        || mount == "/snap"
        || mount.starts_with("/sys/")
        || mount.starts_with("/proc/")
        || mount.starts_with("/dev/")
        || mount.starts_with("/run/")
        || mount.starts_with("/snap/")
    {
        return true;
    }

    false
}

/// 还原 `/proc/mounts` 的八进制转义（空格 `\040`、制表 `\011`、换行 `\012`、反斜杠 `\134`）。
///
/// 仅识别三位八进制数字（0-7）；其它 `\x` 序列原样保留。按字节安全检查，
/// 反斜杠后为非 ASCII 字符（多字节 UTF-8）时不会切坏字符边界（不 panic）。
pub fn unescape_mount(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('\\') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let bytes = after.as_bytes();
        if bytes.len() >= 3 && bytes[..3].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let code = u8::from_str_radix(&after[..3], 8).unwrap_or(0);
            // 转义只覆盖 ASCII 可打印字符；按字节还原（Latin-1 语义）
            out.push(code as char);
            rest = &after[3..];
            continue;
        }
        out.push('\\');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// 解析 `/proc/mounts`（Linux；其他平台返回空）：`(设备, 挂载点(原始未转义), 文件系统)`。
/// 调用方通常还需 [`unescape_mount`] 还原八进制转义再做过滤。
/// （2026-10-07 自 dhrust `sys::machine::disks` 与 DHDeploy 磁盘采集的重复解析下沉）
pub fn list_mounts() -> Vec<(String, String, String)> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return out;
        };
        for line in mounts.lines() {
            let mut cols = line.split_whitespace();
            let (Some(dev), Some(mp), Some(fs)) = (cols.next(), cols.next(), cols.next()) else {
                continue;
            };
            out.push((dev.to_string(), mp.to_string(), fs.to_string()));
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// 挂载点容量（`statvfs`）：`(总字节, 可用字节)`（块大小 `f_frsize` 回退 `f_bsize`，
/// 可用取非特权用户口径 `f_bavail`，与 df 一致）；失败（未挂载/无权限）返回 None。
/// 非 Unix 平台恒为 None。
pub fn usage_bytes(mount: &str) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let path = std::ffi::CString::new(mount).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let block = if stat.f_frsize > 0 {
            stat.f_frsize
        } else {
            stat.f_bsize
        } as u64;
        let total = (stat.f_blocks as u64).saturating_mul(block);
        let avail = (stat.f_bavail as u64).saturating_mul(block);
        Some((total, avail))
    }
    #[cfg(not(unix))]
    {
        let _ = mount;
        None
    }
}

#[cfg(test)]
mod mount_prims_tests {
    use super::*;

    #[test]
    fn list_mounts_platform_shape() {
        let mounts = list_mounts();
        #[cfg(target_os = "linux")]
        assert!(
            mounts.iter().any(|(d, _, _)| d.starts_with("/dev/")),
            "Linux 应至少枚举到一个 /dev 挂载"
        );
        #[cfg(not(target_os = "linux"))]
        assert!(mounts.is_empty(), "非 Linux 平台返回空列表");
    }

    #[test]
    fn usage_bytes_invalid_mount_is_none() {
        assert!(usage_bytes("/definitely/not/a/dir/pek-rpanl").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_mount_rules() {
        // 类型黑名单：任意挂载点的 tmpfs/overlay/squashfs 均过滤
        assert!(is_temporary_mount("/", Some("tmpfs")));
        assert!(is_temporary_mount("/www", Some("overlay")));
        assert!(is_temporary_mount("/mnt/x", Some("squashfs")));
        assert!(is_temporary_mount("/www/tmp", Some("TMPFS")));
        // 引导分区：精确匹配（/boot2 不算）
        assert!(is_temporary_mount("/boot", Some("ext4")));
        assert!(is_temporary_mount("/boot/", Some("ext4")));
        assert!(is_temporary_mount("/boot/efi", Some("vfat")));
        assert!(!is_temporary_mount("/boot2", Some("ext4")));
        // 系统虚拟目录前缀（fs 未知时也过滤）
        assert!(is_temporary_mount("/proc/1", None));
        assert!(is_temporary_mount("/sys/fs/cgroup", None));
        assert!(is_temporary_mount("/snap/core20/1974", None));
        assert!(is_temporary_mount("/run/user/0", None));
        assert!(is_temporary_mount("/dev/shm", None));
        // 普通挂载
        assert!(!is_temporary_mount("/", Some("ext4")));
        assert!(!is_temporary_mount("/www", Some("xfs")));
        assert!(!is_temporary_mount("/data", None));
    }

    #[test]
    fn mount_escape_restore() {
        assert_eq!(unescape_mount("/data\\040disk"), "/data disk");
        assert_eq!(unescape_mount("/normal"), "/normal");
        assert_eq!(unescape_mount("/a\\134b"), "/a\\b");
        // 非法序列（非八进制）原样保留
        assert_eq!(unescape_mount("/a\\999"), "/a\\999");
        // 反斜杠后为非 ASCII：不 panic、原样保留
        assert_eq!(unescape_mount("/a\\中文"), "/a\\中文");
        // 结尾孤立反斜杠
        assert_eq!(unescape_mount("/a\\"), "/a\\");
    }

    #[test]
    fn mount_key_trims_trailing_slash() {
        assert_eq!(mount_key("/boot/"), "/boot");
        assert_eq!(mount_key("/"), "/");
        assert_eq!(mount_key("/www"), "/www");
    }
}
