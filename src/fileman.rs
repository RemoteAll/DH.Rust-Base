//! 文件管理（Web 面板「文件管理」页的服务端能力）：浏览、上传/下载、在线编辑、
//! 新建、删除、重命名、复制/移动、压缩/解压、权限修改与搜索。
//!
//! HTTP 适配层（鉴权、审计、参数解析、日志）留在各消费方：
//! - Pek.RAgent：`/star/file*`（面板「🗂 文件管理」）；
//! - Pek.RPanlServer：`/api/file*`（面板「🗂 文件管理」，需 `files` 权限）。
//!
//! 安全模型（对齐宝塔文件管理）：
//! - 路径词法归一化（相对路径按 `base` 解析），不访问文件系统即可消除 `.`/`..`；
//! - **删除/重命名/移动**对"根""一级目录（如 `/etc`、`C:\Windows`）""程序目录本身"
//!   设保护（目录内部仍可操作）——防止误操作损毁系统；
//! - 上传文件名与压缩包条目名做目录穿越拦截（`..`、绝对路径、分隔符）；
//! - 可选 [`Guard`] 拒绝名单（如平台签名私钥 `Config/plugin-store.key`）：读、写、下载一律拒绝；
//! - 下载上限 256MB、在线编辑上限 10MB（二进制拒读），大文件请用 SFTP。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use chrono::{DateTime, Local};
use serde_json::{json, Value as Json};

/// 在线编辑的文件大小上限（10MB；需经 JSON 传输并由浏览器编辑框渲染，过大易卡死）。
pub const MAX_EDIT_SIZE: u64 = 10 * 1024 * 1024;
/// 面板下载的文件大小上限（256MB）。
pub const MAX_DOWNLOAD_SIZE: u64 = 256 * 1024 * 1024;
/// 批量操作（删除/复制/移动/压缩）单次条目上限。
pub const MAX_BATCH: usize = 100;
/// 单文件上传上限（2GB；分片上传——服务端只按片接收，内存不随文件大小增长）。
pub const MAX_UPLOAD_SIZE: u64 = 2 * 1024 * 1024 * 1024;
/// 分片上传的临时文件前缀（目录列表隐藏；`upload_abort` / 空闲清理按此前缀识别）。
pub const UPLOAD_TMP_PREFIX: &str = ".pek-upload-";
/// 搜索返回条数上限。
const SEARCH_MAX_RESULTS: usize = 200;
/// 搜索扫描条数上限。
const SEARCH_MAX_SCAN: usize = 20_000;
/// 目录大小统计节点数上限（超出返回部分值，防超大目录拖死请求）。
const DIR_SIZE_MAX_NODES: usize = 200_000;
/// 目录大小统计时间上限（毫秒）。
const DIR_SIZE_MAX_MS: u128 = 10_000;

// ————— 路径与保护 —————

/// 归一化用户提交的路径（相对路径按 `base` 解析；不访问文件系统）。
pub fn resolve_path(base: &Path, input: &str) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("缺少路径参数".to_string());
    }
    let p = Path::new(trimmed);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    Ok(crate::io::lexical_normalize(&joined))
}

/// 是否受保护路径（禁止删除/重命名/移动**其本身**；目录内部不禁止）。
pub fn is_protected(path: &Path, base: &Path) -> bool {
    let p = crate::io::lexical_normalize(path);
    let Some(parent) = p.parent() else {
        return true; // 根 / 盘符根
    };
    if parent.parent().is_none() {
        return true; // 一级目录
    }
    p == crate::io::lexical_normalize(base)
}

/// 修改类操作的保护校验。
pub fn ensure_modifiable(path: &Path, base: &Path) -> Result<(), String> {
    if is_protected(path, base) {
        return Err(format!(
            "该路径受保护，禁止删除/重命名/移动：{}",
            path.display()
        ));
    }
    Ok(())
}

/// 校验文件/目录名称（无分隔符、非 `.`/`..`、长度合理）。
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名称不能为空".to_string());
    }
    if name == "." || name == ".." {
        return Err("名称不合法".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("名称不能包含路径分隔符".to_string());
    }
    if name.len() > 255 {
        return Err("名称过长".to_string());
    }
    Ok(())
}

/// 取文件/目录的显示名。
pub fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 快捷入口（前端"常用位置"）：Linux 常见目录 / Windows 盘符 + 程序目录。
pub fn quick_roots(base: &Path) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    #[cfg(unix)]
    {
        for p in ["/", "/www", "/opt", "/var/log", "/tmp"] {
            if Path::new(p).is_dir() {
                roots.push(p.to_string());
            }
        }
    }
    #[cfg(windows)]
    {
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).exists() {
                roots.push(root);
            }
        }
    }
    let base_text = base.display().to_string();
    if !roots.iter().any(|r| r == &base_text) {
        roots.push(base_text);
    }
    roots
}

/// 访问护栏：基础目录 + 拒绝名单（读/写/下载一律拒绝）。
///
/// 拒绝名单按**路径前缀**匹配（同组件边界），因此既可以禁单个文件
/// （`Config/plugin-store.key`），也可以禁整棵子树。
#[derive(Clone, Debug)]
pub struct Guard {
    base: PathBuf,
    deny: Vec<PathBuf>,
}

impl Guard {
    /// 以程序基础目录创建护栏（无拒绝名单）。
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: crate::io::lexical_normalize(&base.into()),
            deny: Vec::new(),
        }
    }

    /// 追加拒绝路径（相对路径按基础目录解析）。
    pub fn deny(mut self, path: impl AsRef<Path>) -> Self {
        let p = path.as_ref();
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base.join(p)
        };
        self.deny.push(crate::io::lexical_normalize(&abs));
        self
    }

    /// 基础目录。
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// 归一化路径并校验不在拒绝名单内。
    pub fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        let p = resolve_path(&self.base, input)?;
        self.ensure_readable(&p)?;
        Ok(p)
    }

    /// 是否命中拒绝名单。
    pub fn is_denied(&self, path: &Path) -> bool {
        let p = crate::io::lexical_normalize(path);
        self.deny.iter().any(|d| p == *d || p.starts_with(d))
    }

    /// 读/下载/列表校验。
    pub fn ensure_readable(&self, path: &Path) -> Result<(), String> {
        if self.is_denied(path) {
            return Err(format!("该路径受保护，禁止访问：{}", path.display()));
        }
        Ok(())
    }

    /// 写/删除/重命名校验（拒绝名单 + 根/一级目录/程序目录保护）。
    pub fn ensure_writable(&self, path: &Path) -> Result<(), String> {
        self.ensure_readable(path)?;
        ensure_modifiable(path, &self.base)
    }

    /// 拒绝名单（只读）。
    pub fn denied_paths(&self) -> &[PathBuf] {
        &self.deny
    }
}

// ————— 元数据展示 —————

/// 时间格式化（本地时间）。
pub fn fmt_time(t: SystemTime) -> String {
    let dt: DateTime<Local> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 权限文本（Unix 八进制；Windows 为空）。
#[cfg(unix)]
pub fn perm_text(md: &fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    format!("{:o}", md.permissions().mode() & 0o7777)
}

#[cfg(windows)]
pub fn perm_text(_md: &fs::Metadata) -> String {
    String::new()
}

/// 目录递归统计（文件数, 字节数）。
pub fn dir_stats(path: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let Ok(rd) = fs::read_dir(path) else {
        return (files, bytes);
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            let (f, b) = dir_stats(&p);
            files += f;
            bytes += b;
        } else {
            files += 1;
            bytes += md.len();
        }
    }
    (files, bytes)
}

/// 递归统计目录（字节数, 文件数, 子目录数, 是否截断）；不跟随符号链接（防循环）。
pub fn size_recursive(root: &Path) -> (u64, u64, u64, bool) {
    let started = Instant::now();
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut nodes = 0usize;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            nodes += 1;
            if nodes > DIR_SIZE_MAX_NODES || started.elapsed().as_millis() > DIR_SIZE_MAX_MS {
                return (bytes, files, dirs, true);
            }
            let Ok(lm) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if lm.file_type().is_symlink() {
                continue;
            }
            if lm.is_dir() {
                dirs += 1;
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += lm.len();
            }
        }
    }
    (bytes, files, dirs, false)
}

/// 目录条目 JSON（列表接口单条）。
pub fn entry_json(name: String, path: &Path, md: &fs::Metadata, is_link: bool) -> Json {
    // 符号链接按跟随后的类型展示（可进入指向的目录）
    let is_dir = if is_link {
        fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    } else {
        md.is_dir()
    };
    json!({
        "name": name,
        "isDir": is_dir,
        "isLink": is_link,
        "size": if is_dir { 0 } else { md.len() },
        "mtime": md.modified().map(fmt_time).unwrap_or_default(),
        "perm": perm_text(md),
    })
}

/// 列出目录内容（目录在前，再按名称不区分大小写排序）。
pub fn list_dir(dir: &Path) -> Result<Vec<Json>, String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}：{}", dir.display(), e))?;
    let mut items: Vec<(bool, String, Json)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // 分片上传的临时分片不对外展示（否则用户会在面板看到 .pek-upload-xxx.part）
        if is_upload_temp(&name) {
            continue;
        }
        // DirEntry::metadata 不跟随符号链接（链接本身元数据）
        let Ok(md) = entry.metadata() else { continue };
        let is_link = md.file_type().is_symlink();
        let item = entry_json(name.clone(), &entry.path(), &md, is_link);
        let is_dir = item.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false);
        items.push((is_dir, name.to_lowercase(), item));
    }
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(items.into_iter().map(|(_, _, v)| v).collect())
}

// ————— 文件系统操作 —————

/// 删除文件或目录（符号链接只删链接本身）；返回（文件数, 字节数）。
pub fn remove_path(path: &Path) -> Result<(u64, u64), String> {
    let md =
        fs::symlink_metadata(path).map_err(|e| format!("无法访问 {}：{}", path.display(), e))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        // 文件链接与目录链接（含 Windows junction）统一处理
        if fs::remove_dir(path).is_err() {
            fs::remove_file(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        }
        Ok((1, 0))
    } else if ft.is_dir() {
        let stats = dir_stats(path);
        fs::remove_dir_all(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        Ok(stats)
    } else {
        let size = md.len();
        fs::remove_file(path).map_err(|e| format!("删除失败 {}：{}", path.display(), e))?;
        Ok((1, size))
    }
}

/// 递归复制（符号链接按普通文件复制其内容；指向目录的链接跳过）。
pub fn copy_recursive(src: &Path, dst: &Path) -> Result<(u64, u64), String> {
    let lm = fs::symlink_metadata(src).map_err(|e| format!("无法访问 {}：{}", src.display(), e))?;
    if lm.file_type().is_symlink() {
        let follow = fs::metadata(src);
        if follow.map(|m| m.is_dir()).unwrap_or(false) {
            return Ok((0, 0)); // 指向目录的链接：跳过（防循环）
        }
        let size = lm.len();
        fs::copy(src, dst).map_err(|e| format!("复制失败 {}：{}", src.display(), e))?;
        return Ok((1, size));
    }
    if lm.is_dir() {
        fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 {}：{}", dst.display(), e))?;
        let mut files = 0u64;
        let mut bytes = 0u64;
        let rd = fs::read_dir(src).map_err(|e| format!("无法读取目录 {}：{}", src.display(), e))?;
        for entry in rd.flatten() {
            let (f, b) = copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
            files += f;
            bytes += b;
        }
        Ok((files, bytes))
    } else {
        let size = lm.len();
        fs::copy(src, dst).map_err(|e| format!("复制失败 {}：{}", src.display(), e))?;
        Ok((1, size))
    }
}

/// 统计路径的文件数/字节（移动成功后对源路径统计会失败——此函数用于移动前的近似统计）。
pub fn dir_or_file_stats(path: &Path) -> (u64, u64) {
    let Ok(md) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if md.is_dir() {
        dir_stats(path)
    } else {
        (1, md.len())
    }
}

/// 修改权限（仅 Unix；`mode` 为八进制值）。
#[cfg(unix)]
pub fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(mode & 0o7777);
    fs::set_permissions(path, perms).map_err(|e| format!("设置权限失败 {}：{}", path.display(), e))
}

#[cfg(windows)]
pub fn chmod(_path: &Path, _mode: u32) -> Result<(), String> {
    Err("修改权限仅支持 Linux/macOS".to_string())
}

/// 新建目录或空文件（已存在则报错）。
pub fn create_entry(parent: &Path, name: &str, is_dir: bool) -> Result<PathBuf, String> {
    validate_name(name)?;
    if !parent.is_dir() {
        return Err(format!("目录不存在：{}", parent.display()));
    }
    let dst = parent.join(name.trim());
    if dst.exists() {
        return Err(format!("已存在：{}", dst.display()));
    }
    let result = if is_dir {
        fs::create_dir(&dst)
    } else {
        fs::File::create(&dst).map(|_| ())
    };
    result.map_err(|e| format!("创建失败：{e}"))?;
    Ok(dst)
}

/// 重命名（同目录内改文件名；目标已存在则报错）。
pub fn rename_entry(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    validate_name(new_name)?;
    if fs::symlink_metadata(path).is_err() {
        return Err(format!("不存在：{}", path.display()));
    }
    let Some(parent) = path.parent() else {
        return Err("无法重命名根目录".to_string());
    };
    let dst = parent.join(new_name.trim());
    if dst.exists() {
        return Err(format!("目标已存在：{}", dst.display()));
    }
    fs::rename(path, &dst).map_err(|e| format!("重命名失败：{e}"))?;
    Ok(dst)
}

/// 批量复制/移动（`target` 必须是目录；跨卷移动退化为复制 + 删除）；返回（文件数, 字节数）。
pub fn transfer_many(paths: &[PathBuf], target: &Path, copy: bool) -> Result<(u64, u64), String> {
    if !target.is_dir() {
        return Err(format!("目标目录不存在：{}", target.display()));
    }
    // 预检
    for p in paths {
        if !p.exists() && fs::symlink_metadata(p).is_err() {
            return Err(format!("不存在：{}", p.display()));
        }
        if p.starts_with(target) {
            return Err(if copy {
                "目标目录在选中项内部，无法复制".to_string()
            } else {
                "目标目录在选中项内部，无法移动".to_string()
            });
        }
        let dst = target.join(file_label(p));
        if dst == *p {
            return Err("目标与源相同".to_string());
        }
        if dst.exists() {
            return Err(format!("目标已存在：{}", dst.display()));
        }
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    for p in paths {
        let dst = target.join(file_label(p));
        // 统计须在移动前取（rename 后源路径已不存在）
        let stats = dir_or_file_stats(p);
        if copy {
            let (f, b) = copy_recursive(p, dst.as_path())?;
            files += f;
            bytes += b;
        } else if fs::rename(p, &dst).is_err() {
            // 跨卷：复制 + 删除
            let (f, b) = copy_recursive(p, dst.as_path())?;
            remove_path(p)?;
            files += f;
            bytes += b;
        } else {
            files += stats.0;
            bytes += stats.1;
        }
    }
    Ok((files, bytes))
}

/// 保存上传内容（先写临时文件再原子替换，覆盖同名文件）。
pub fn save_upload(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf, String> {
    validate_name(name)?;
    if !dir.is_dir() {
        return Err(format!("目录不存在：{}", dir.display()));
    }
    if data.is_empty() {
        return Err("上传内容为空".to_string());
    }
    let dst = dir.join(name.trim());
    if dst.is_dir() {
        return Err("目标是目录，无法覆盖".to_string());
    }
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".pek-upload-{}-{}.tmp", std::process::id(), nonce));
    fs::write(&tmp, data).map_err(|e| format!("写入临时文件失败：{e}"))?;
    match crate::io::safe_replace_file(&tmp, &dst) {
        Ok(()) => Ok(dst),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("保存失败：{e}"))
        }
    }
}

/// 读取待编辑文本（上限 10MB；二进制拒读）。
pub fn read_edit(path: &Path) -> Result<String, String> {
    let md = fs::metadata(path).map_err(|e| format!("无法访问 {}：{}", path.display(), e))?;
    if md.is_dir() {
        return Err("目标是目录，无法编辑".to_string());
    }
    if md.len() > MAX_EDIT_SIZE {
        return Err("文件过大（上限 10MB），请使用下载/其他编辑器".to_string());
    }
    let data = fs::read(path).map_err(|e| format!("读取失败：{e}"))?;
    if data.iter().take(8192).any(|b| *b == 0) {
        return Err("疑似二进制文件，无法在线编辑".to_string());
    }
    String::from_utf8(data).map_err(|_| "不是 UTF-8 文本，无法在线编辑".to_string())
}

/// 写入文本（父目录必须存在）。
pub fn write_text(path: &Path, content: &str) -> Result<usize, String> {
    if let Some(parent) = path.parent() {
        if !parent.is_dir() {
            return Err(format!("上级目录不存在：{}", parent.display()));
        }
    }
    crate::io::write_all_text(path, content).map_err(|e| format!("保存失败：{e}"))?;
    Ok(content.len())
}

/// 下载信息（大小 + `Content-Disposition`）；超限/目录报错。
pub fn download_info(path: &Path) -> Result<(u64, String), String> {
    let md = fs::metadata(path).map_err(|e| format!("文件不存在：{}（{e}）", path.display()))?;
    if md.is_dir() {
        return Err("目标是目录，无法下载（可先压缩）".to_string());
    }
    if md.len() > MAX_DOWNLOAD_SIZE {
        return Err("文件过大（上限 256MB），请使用 SFTP 下载".to_string());
    }
    let name = file_label(path);
    Ok((md.len(), content_disposition(&name)))
}

/// `Content-Disposition`（RFC 5987；中文文件名兼容）。
pub fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii,
        percent_encode(name)
    )
}

// ————— ZIP 压缩 / 解压 —————

/// ZIP 写入选项（Deflate 压缩）。
fn zip_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644)
}

/// 目录递归加入 zip（空目录也保留条目）。
fn zip_add_dir(
    zip: &mut zip::ZipWriter<fs::File>,
    dir: &Path,
    prefix: &str,
    skip: &Path,
    files: &mut u64,
) -> Result<(), String> {
    zip.add_directory(format!("{prefix}/"), zip_options())
        .map_err(|e| format!("写入压缩包失败：{e}"))?;
    let rd = fs::read_dir(dir).map_err(|e| format!("无法读取目录 {}：{}", dir.display(), e))?;
    for entry in rd.flatten() {
        let p = entry.path();
        if p == skip {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = format!("{prefix}/{name}");
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            zip_add_dir(zip, &p, &rel, skip, files)?;
        } else {
            let Ok(data) = fs::read(&p) else { continue };
            zip.start_file(rel, zip_options())
                .map_err(|e| format!("写入压缩包失败：{e}"))?;
            zip.write_all(&data)
                .map_err(|e| format!("写入压缩包失败：{e}"))?;
            *files += 1;
        }
    }
    Ok(())
}

/// 压缩多个路径到 zip 文件；返回（文件数, 压缩包字节数）。
pub fn compress_zip(dst: &Path, sources: &[PathBuf]) -> Result<(u64, u64), String> {
    let file = fs::File::create(dst).map_err(|e| format!("创建压缩包失败：{e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let mut files = 0u64;
    for src in sources {
        if src == dst {
            continue;
        }
        let name = file_label(src);
        if name.is_empty() {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(src) else {
            continue;
        };
        if md.is_dir() {
            zip_add_dir(&mut zip, src, &name, dst, &mut files)?;
        } else if md.is_file() {
            let Ok(data) = fs::read(src) else { continue };
            zip.start_file(name, zip_options())
                .map_err(|e| format!("写入压缩包失败：{e}"))?;
            zip.write_all(&data)
                .map_err(|e| format!("写入压缩包失败：{e}"))?;
            files += 1;
        }
    }
    zip.finish().map_err(|e| format!("完成压缩包失败：{e}"))?;
    let size = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
    Ok((files, size))
}

/// 解压 zip 到目标目录（目录穿越条目跳过）；返回（文件数, 字节数）。
pub fn extract_zip_file(src: &Path, target: &Path) -> Result<(u64, u64), String> {
    let file = fs::File::open(src).map_err(|e| format!("无法打开 {}：{}", src.display(), e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("不是有效的 zip 文件（{}）：{e}", src.display()))?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        // enclosed_name：拒绝 `..`、绝对路径等穿越条目
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = target.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| format!("创建目录失败：{e}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
        let mut f =
            fs::File::create(&out).map_err(|e| format!("写入失败 {}：{}", out.display(), e))?;
        std::io::copy(&mut entry, &mut f)
            .map_err(|e| format!("写入失败 {}：{}", out.display(), e))?;
        files += 1;
        bytes += entry.size();
    }
    Ok((files, bytes))
}

/// 压缩入口（校验目标/名称；返回 zip 路径与统计）。
pub fn compress_entry(
    target_dir: &Path,
    sources: &[PathBuf],
    name: &str,
) -> Result<(PathBuf, u64, u64), String> {
    if !target_dir.is_dir() {
        return Err(format!("目标目录不存在：{}", target_dir.display()));
    }
    let mut name = name.trim().to_string();
    if name.is_empty() {
        name = "archive.zip".to_string();
    }
    validate_name(&name)?;
    if !name.to_ascii_lowercase().ends_with(".zip") {
        name.push_str(".zip");
    }
    let dst = target_dir.join(&name);
    if dst.exists() {
        return Err(format!("已存在：{}", dst.display()));
    }
    let (files, size) = compress_zip(&dst, sources)?;
    Ok((dst, files, size))
}

// ————— 分片上传（大文件走分片：单片 ≤ 调用方切片大小，服务端内存恒定）—————

/// 是否为上传临时文件（目录列表过滤用）。
pub fn is_upload_temp(name: &str) -> bool {
    name.starts_with(UPLOAD_TMP_PREFIX)
}

/// 分片上传进度。
#[derive(Debug, Clone)]
pub struct UploadProgress {
    /// 服务端已接收字节数
    pub received: u64,
    /// 文件总大小
    pub total: u64,
    /// 是否已收齐并落盘
    pub done: bool,
    /// 落盘后的最终路径（`done=true` 时有效）
    pub path: Option<PathBuf>,
}

fn upload_part_path(dir: &Path, upload_id: &str) -> Result<PathBuf, String> {
    let id = upload_id.trim();
    let valid = (8..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err("上传会话标识无效（应为 8~64 位字母/数字/连字符）".to_string());
    }
    Ok(dir.join(format!("{UPLOAD_TMP_PREFIX}{id}.part")))
}

/// 查询某会话已接收字节数（0 = 未开始或已清理；供断点续传）。
pub fn upload_received(dir: &Path, upload_id: &str) -> Result<u64, String> {
    let part = upload_part_path(dir, upload_id)?;
    Ok(match fs::metadata(&part) {
        Ok(md) if md.is_file() => md.len(),
        _ => 0,
    })
}

/// 写入一个分片；返回进度（`done=true` 表示已落盘到目标文件名）。
///
/// - `offset` 必须等于服务端已接收字节数（防乱序/重复；断点续传时先查 `upload_received`）；
/// - `overwrite=false` 时：首片即检查目标是否存在（不白传 2GB 再失败），末片再检查一次；
/// - 收齐前只写 `{dir}/.pek-upload-{id}.part`，收齐后原子替换到目标（占用时安全让位）。
pub fn upload_chunk(
    dir: &Path,
    name: &str,
    upload_id: &str,
    offset: u64,
    total: u64,
    data: &[u8],
    overwrite: bool,
) -> Result<UploadProgress, String> {
    validate_name(name)?;
    if !dir.is_dir() {
        return Err(format!("目录不存在：{}", dir.display()));
    }
    let dst = dir.join(name.trim());
    if dst.is_dir() {
        return Err("目标是目录，无法覆盖".to_string());
    }
    if total == 0 {
        return Err("文件为空（0 字节），无需分片上传".to_string());
    }
    if total > MAX_UPLOAD_SIZE {
        return Err(format!("文件过大（上限 2GB）：{} 字节", total));
    }
    let part = upload_part_path(dir, upload_id)?;
    let received = upload_received(dir, upload_id)?;
    if received != offset {
        return Err(format!(
            "分片位置不匹配（服务端已收 {received} 字节，本次 offset={offset}）：请查询已收字节后续传"
        ));
    }
    if !overwrite && received == 0 && dst.exists() {
        return Err(format!(
            "目标已存在：{}（确认覆盖后可继续上传）",
            dst.display()
        ));
    }
    if data.is_empty() {
        return Err("分片内容为空".to_string());
    }
    let next = received
        .checked_add(data.len() as u64)
        .ok_or_else(|| "分片长度溢出".to_string())?;
    if next > total {
        return Err(format!("分片超出文件大小（{next} > {total}）"));
    }
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .map_err(|e| format!("写入临时分片失败（{}）：{e}", part.display()))?;
        f.write_all(data)
            .map_err(|e| format!("写入临时分片失败（{}）：{e}", part.display()))?;
        if let Err(e) = f.flush() {
            return Err(format!("刷新临时分片失败（{}）：{e}", part.display()));
        }
    }
    if next < total {
        return Ok(UploadProgress {
            received: next,
            total,
            done: false,
            path: None,
        });
    }
    if !overwrite && dst.exists() {
        return Err(format!(
            "目标已存在：{}（确认覆盖后重发最后一片即可）",
            dst.display()
        ));
    }
    crate::io::safe_replace_file(&part, &dst).map_err(|e| format!("保存失败：{e}"))?;
    Ok(UploadProgress {
        received: next,
        total,
        done: true,
        path: Some(dst),
    })
}

/// 放弃上传会话（删除临时分片）。
pub fn upload_abort(dir: &Path, upload_id: &str) -> Result<(), String> {
    let part = upload_part_path(dir, upload_id)?;
    let _ = fs::remove_file(&part);
    Ok(())
}

// ————— 多格式解压（zip / 7z / tar[.gz|.bz2|.xz|.zst] / 单文件 gz|bz2|xz|zst）—————

/// 支持的压缩包格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    SevenZip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl ArchiveFormat {
    /// 机器可读代码（前端展示 / 日志用）。
    pub fn code(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZip => "7z",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarBz2 => "tar.bz2",
            Self::TarXz => "tar.xz",
            Self::TarZst => "tar.zst",
            Self::Gzip => "gz",
            Self::Bzip2 => "bz2",
            Self::Xz => "xz",
            Self::Zstd => "zst",
        }
    }

    /// 展示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::SevenZip => "7Z",
            Self::Tar => "TAR",
            Self::TarGz => "TAR.GZ",
            Self::TarBz2 => "TAR.BZ2",
            Self::TarXz => "TAR.XZ",
            Self::TarZst => "TAR.ZST",
            Self::Gzip => "GZ",
            Self::Bzip2 => "BZ2",
            Self::Xz => "XZ",
            Self::Zstd => "ZST",
        }
    }

    /// 是否支持密码（仅 zip / 7z）。
    pub fn supports_password(self) -> bool {
        matches!(self, Self::Zip | Self::SevenZip)
    }
}

/// 解压统计。
#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    /// 识别出的格式（识别失败时为 `None`）
    pub format: Option<ArchiveFormat>,
    /// 写出的文件数
    pub files: u64,
    /// 写出的字节数
    pub bytes: u64,
    /// 被跳过的条目数（目录穿越 / 符号链接 / 循环链接 / 损坏条目）
    pub skipped: u64,
    /// 覆盖的同名文件数
    pub overridden: u64,
}

/// 压缩包同名主干名（`a.tar.gz` → `a`；`a.zip` → `a`）。
pub fn archive_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.zstd", ".tgz", ".tbz2", ".tbz", ".txz",
        ".tzst", ".tar", ".zip", ".7z", ".gz", ".bz2", ".xz", ".zst", ".zstd",
    ] {
        if lower.ends_with(suffix) {
            let base = &name[..name.len() - suffix.len()];
            if !base.is_empty() {
                return base.to_string();
            }
        }
    }
    name.to_string()
}

/// 解压默认目标目录：压缩包同级同名子目录（`a.zip` → 同目录 `a/`）。
pub fn default_extract_target(src: &Path) -> PathBuf {
    let name = file_label(src);
    let stem = archive_stem(&name);
    let dir = if stem.is_empty() || stem == "." || stem == ".." {
        format!("{name}.out")
    } else {
        stem
    };
    src.parent()
        .map(|p| p.join(&dir))
        .unwrap_or_else(|| PathBuf::from(dir))
}

fn read_head(path: &Path, n: usize) -> Result<Vec<u8>, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("无法打开 {}：{}", path.display(), e))?;
    let mut buf = vec![0u8; n];
    let got = f
        .read(&mut buf)
        .map_err(|e| format!("读取失败（{}）：{e}", path.display()))?;
    buf.truncate(got);
    Ok(buf)
}

fn open_decoder(path: &Path, fmt: ArchiveFormat) -> Result<Box<dyn Read>, String> {
    let f = fs::File::open(path).map_err(|e| format!("无法打开 {}：{}", path.display(), e))?;
    Ok(match fmt {
        ArchiveFormat::Gzip | ArchiveFormat::TarGz => {
            Box::new(flate2::read::MultiGzDecoder::new(f))
        }
        ArchiveFormat::Bzip2 | ArchiveFormat::TarBz2 => Box::new(bzip2::read::BzDecoder::new(f)),
        ArchiveFormat::Xz | ArchiveFormat::TarXz => Box::new(lzma_rust2::XzReader::new(f, false)),
        ArchiveFormat::Zstd | ArchiveFormat::TarZst => Box::new(
            zstd::stream::read::Decoder::new(f)
                .map_err(|e| format!("zstd 解码初始化失败（{}）：{e}", path.display()))?,
        ),
        _ => return Err("该格式不是单流压缩".to_string()),
    })
}

/// 单流压缩包内部是否为 tar（读解压后的前 512 字节看 `ustar` 魔数）。
fn inner_is_tar(path: &Path, fmt: ArchiveFormat) -> Result<bool, String> {
    let mut dec = open_decoder(path, fmt)?;
    let mut buf = [0u8; 512];
    let mut got = 0usize;
    while got < buf.len() {
        match dec.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            // 解不开就别在这里下结论，交给正式解压时报具体错误
            Err(_) => return Ok(false),
        }
    }
    Ok(got >= 262 && &buf[257..262] == b"ustar")
}

/// 识别压缩包格式（魔数优先、扩展名兜底；rar 明确报不支持）。
pub fn detect_archive(path: &Path) -> Result<ArchiveFormat, String> {
    let md = fs::metadata(path).map_err(|e| format!("文件不存在：{}（{e}）", path.display()))?;
    if md.is_dir() {
        return Err("目标是目录，无法解压".to_string());
    }
    let name = file_label(path).to_ascii_lowercase();
    if name.ends_with(".rar") {
        return Err(
            "RAR 格式暂不支持（无纯 Rust 解压实现）：请先转换为 ZIP 或 7Z 再解压".to_string(),
        );
    }
    let head = read_head(path, 512)?;
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") || head.starts_with(b"PK\x07\x08")
    {
        return Ok(ArchiveFormat::Zip);
    }
    if head.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return Ok(ArchiveFormat::SevenZip);
    }
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Ok(ArchiveFormat::Tar);
    }
    let stream = if head.starts_with(b"\x1f\x8b") {
        Some((ArchiveFormat::Gzip, ArchiveFormat::TarGz))
    } else if head.starts_with(b"BZh") {
        Some((ArchiveFormat::Bzip2, ArchiveFormat::TarBz2))
    } else if head.starts_with(b"\xfd7zXZ\x00") {
        Some((ArchiveFormat::Xz, ArchiveFormat::TarXz))
    } else if head.starts_with(b"\x28\xb5\x2f\xfd") {
        Some((ArchiveFormat::Zstd, ArchiveFormat::TarZst))
    } else {
        None
    };
    if let Some((plain, tarball)) = stream {
        return Ok(if inner_is_tar(path, plain)? {
            tarball
        } else {
            plain
        });
    }
    // 魔数不认识：按扩展名兜底（让解压器给出更具体的错误）
    if name.ends_with(".zip") {
        return Ok(ArchiveFormat::Zip);
    }
    if name.ends_with(".7z") {
        return Ok(ArchiveFormat::SevenZip);
    }
    if name.ends_with(".tar") {
        return Ok(ArchiveFormat::Tar);
    }
    Err(format!(
        "无法识别的压缩包格式：{}（支持 zip / 7z / tar / tar.gz / tar.bz2 / tar.xz / tar.zst / gz / bz2 / xz / zst）",
        file_label(path)
    ))
}

/// 把条目相对路径拼到目标目录（拒绝绝对路径、盘符、`..`、空名）。
fn safe_join(target: &Path, rel: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = target.to_path_buf();
    let mut pushed = false;
    for comp in rel.components() {
        match comp {
            Component::Normal(seg) => {
                let text = seg.to_string_lossy();
                if text.is_empty() || text == "." {
                    continue;
                }
                out.push(seg);
                pushed = true;
            }
            Component::CurDir => {}
            // RootDir / Prefix（C:）/ ParentDir 一律拒绝
            _ => return None,
        }
    }
    if pushed { Some(out) } else { None }
}

fn extract_zip_entries(
    src: &Path,
    target: &Path,
    password: Option<&str>,
    report: &mut ExtractReport,
) -> Result<(), String> {
    let file = fs::File::open(src).map_err(|e| format!("无法打开 {}：{}", src.display(), e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("不是有效的 ZIP 文件（{}）：{e}", src.display()))?;
    // 没给密码时先探测加密条目：直接给出「请输入密码」，而不是逐条静默跳过
    if password.is_none() {
        for i in 0..zip.len() {
            if let Ok(entry) = zip.by_index_raw(i) {
                if entry.encrypted() {
                    return Err("压缩包已加密：请输入密码后重试".to_string());
                }
            }
        }
    }
    let mut bad_password = false;
    for i in 0..zip.len() {
        let opened = match password {
            Some(pw) => zip.by_index_decrypt(i, pw.as_bytes()),
            None => zip.by_index(i),
        };
        let mut entry = match opened {
            Ok(e) => e,
            Err(zip::result::ZipError::InvalidPassword) => {
                bad_password = true;
                continue;
            }
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };
        let name = match entry.name() {
            Ok(n) => n.to_string(),
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };
        let Some(out) = safe_join(target, Path::new(&name)) else {
            report.skipped += 1;
            continue;
        };
        if entry.is_dir() {
            fs::create_dir_all(&out)
                .map_err(|e| format!("创建目录失败（{}）：{e}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败（{}）：{e}", parent.display()))?;
        }
        if out.exists() {
            report.overridden += 1;
        }
        let mut f =
            fs::File::create(&out).map_err(|e| format!("写入失败（{}）：{e}", out.display()))?;
        match std::io::copy(&mut entry, &mut f) {
            Ok(n) => {
                report.files += 1;
                report.bytes += n;
            }
            Err(e) => {
                // ZipCrypto 只有 1/256 概率在打开时校验出错误密码，读取阶段才暴露
                return Err(if password.is_some() {
                    format!("解密失败（密码错误或压缩包损坏）：{e}")
                } else {
                    format!("读取条目失败（{}）：{e}", out.display())
                });
            }
        }
    }
    if bad_password {
        return Err("密码错误：无法解密压缩包".to_string());
    }
    Ok(())
}

fn extract_7z_entries(
    src: &Path,
    target: &Path,
    password: Option<&str>,
    report: &mut ExtractReport,
) -> Result<(), String> {
    let file = fs::File::open(src).map_err(|e| format!("无法打开 {}：{}", src.display(), e))?;
    let pw = sevenz_rust2::Password::from(password.unwrap_or(""));
    let mut reader = sevenz_rust2::ArchiveReader::new(file, pw)
        .map_err(|e| format!("不是有效的 7Z 文件（{}）：{e}", src.display()))?;
    let mut fail: Option<String> = None;
    let walk = reader.for_each_entries(|entry, rd| {
        if fail.is_some() {
            return Ok(false);
        }
        let raw = entry.name().replace('\\', "/");
        let Some(out) = safe_join(target, Path::new(&raw)) else {
            report.skipped += 1;
            return Ok(true);
        };
        if entry.is_directory() {
            if let Err(e) = fs::create_dir_all(&out) {
                fail = Some(format!("创建目录失败（{}）：{e}", out.display()));
                return Ok(false);
            }
            return Ok(true);
        }
        if let Some(parent) = out.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                fail = Some(format!("创建目录失败（{}）：{e}", parent.display()));
                return Ok(false);
            }
        }
        if out.exists() {
            report.overridden += 1;
        }
        let mut f = match fs::File::create(&out) {
            Ok(f) => f,
            Err(e) => {
                fail = Some(format!("写入失败（{}）：{e}", out.display()));
                return Ok(false);
            }
        };
        match std::io::copy(rd, &mut f) {
            Ok(n) => {
                report.files += 1;
                report.bytes += n;
            }
            Err(e) => {
                fail = Some(if password.is_some() {
                    format!("解密失败（密码错误或压缩包损坏）：{e}")
                } else {
                    format!("读取条目失败（{}）：{e}", out.display())
                });
                return Ok(false);
            }
        }
        Ok(true)
    });
    if let Some(msg) = fail {
        return Err(msg);
    }
    walk.map_err(|e| format!("解压 7Z 失败（密码错误或压缩包损坏？）：{e}"))?;
    Ok(())
}

fn extract_tar_stream(
    reader: Box<dyn Read>,
    target: &Path,
    report: &mut ExtractReport,
) -> Result<(), String> {
    let mut ar = tar::Archive::new(reader);
    let entries = ar
        .entries()
        .map_err(|e| format!("读取 tar 失败（{}）：{e}", target.display()))?;
    for entry in entries {
        let mut entry = match entry {
            Ok(e) => e,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };
        let path = match entry.path() {
            Ok(p) => p.to_path_buf(),
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };
        let Some(out) = safe_join(target, &path) else {
            report.skipped += 1;
            continue;
        };
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&out)
                .map_err(|e| format!("创建目录失败（{}）：{e}", out.display()))?;
            continue;
        }
        if !kind.is_file() {
            // 符号链接 / 硬链接 / 设备节点：跳过（防借链接写到目标目录之外）
            report.skipped += 1;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败（{}）：{e}", parent.display()))?;
        }
        if out.exists() {
            report.overridden += 1;
        }
        let mut f =
            fs::File::create(&out).map_err(|e| format!("写入失败（{}）：{e}", out.display()))?;
        let n = std::io::copy(&mut entry, &mut f)
            .map_err(|e| format!("写入失败（{}）：{e}", out.display()))?;
        report.files += 1;
        report.bytes += n;
    }
    Ok(())
}

/// 多格式解压到 `target`（不存在则创建）；`password` 仅 zip / 7z 生效。
pub fn extract_archive(
    src: &Path,
    target: &Path,
    password: Option<&str>,
) -> Result<ExtractReport, String> {
    let format = detect_archive(src)?;
    if !target.is_dir() {
        fs::create_dir_all(target).map_err(|e| format!("创建目标目录失败：{e}"))?;
    }
    let pw = password.map(|p| p.trim()).filter(|p| !p.is_empty());
    let mut report = ExtractReport {
        format: Some(format),
        ..Default::default()
    };
    match format {
        ArchiveFormat::Zip => extract_zip_entries(src, target, pw, &mut report)?,
        ArchiveFormat::SevenZip => extract_7z_entries(src, target, pw, &mut report)?,
        ArchiveFormat::Tar => {
            let f =
                fs::File::open(src).map_err(|e| format!("无法打开 {}：{}", src.display(), e))?;
            extract_tar_stream(Box::new(f), target, &mut report)?;
        }
        ArchiveFormat::TarGz
        | ArchiveFormat::TarBz2
        | ArchiveFormat::TarXz
        | ArchiveFormat::TarZst => {
            let dec = open_decoder(src, format)?;
            extract_tar_stream(dec, target, &mut report)?;
        }
        ArchiveFormat::Gzip | ArchiveFormat::Bzip2 | ArchiveFormat::Xz | ArchiveFormat::Zstd => {
            if let Some(p) = pw {
                let _ = p; // 单流压缩格式不支持密码，忽略
            }
            let mut dec = open_decoder(src, format)?;
            let stem = archive_stem(&file_label(src));
            let out = target.join(if stem.is_empty() {
                format!("{}.out", file_label(src))
            } else {
                stem
            });
            if out.exists() {
                report.overridden += 1;
            }
            let mut f =
                fs::File::create(&out).map_err(|e| format!("写入失败（{}）：{e}", out.display()))?;
            let n = std::io::copy(&mut dec, &mut f)
                .map_err(|e| format!("解压失败（{}）：{e}", file_label(src)))?;
            report.files = 1;
            report.bytes = n;
        }
    }
    Ok(report)
}

/// 解压入口（目标为空 = 压缩包所在目录；不存在则创建）。
///
/// 兼容旧调用方：只返回（目标目录, 文件数, 字节数）；需要格式/跳过/覆盖统计时用
/// [`extract_archive`]。
pub fn extract_entry(src: &Path, target: Option<&Path>) -> Result<(PathBuf, u64, u64), String> {
    let md = fs::metadata(src).map_err(|e| format!("文件不存在：{}（{e}）", src.display()))?;
    if md.is_dir() {
        return Err("目标是目录，无法解压".to_string());
    }
    let target = match target {
        Some(t) => t.to_path_buf(),
        None => src
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let report = extract_archive(src, &target, None)?;
    Ok((target, report.files, report.bytes))
}

/// 解析八进制权限文本（`755` / `0o755`）。
pub fn parse_mode(text: &str) -> Result<u32, String> {
    let mode = u32::from_str_radix(text.trim().trim_start_matches("0o"), 8)
        .map_err(|_| "权限格式应为八进制（如 755）".to_string())?;
    if mode > 0o7777 {
        return Err("权限超出范围".to_string());
    }
    Ok(mode)
}

// ————— 搜索 —————

/// 按名称搜索（不区分大小写；目录深度优先，限量防重负载）。
pub fn search_dir(root: &Path, q: &str) -> Json {
    let needle = q.to_lowercase();
    let mut results: Vec<Json> = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    let mut stack = vec![root.to_path_buf()];
    'outer: while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            scanned += 1;
            if scanned > SEARCH_MAX_SCAN {
                truncated = true;
                break 'outer;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(md) = entry.metadata() else { continue };
            let path = entry.path();
            if name.to_lowercase().contains(&needle) {
                let item = entry_json(name, &path, &md, md.file_type().is_symlink());
                let mut obj = item;
                if let Some(m) = obj.as_object_mut() {
                    m.insert("path".to_string(), json!(path.display().to_string()));
                }
                results.push(obj);
                if results.len() >= SEARCH_MAX_RESULTS {
                    truncated = true;
                    break 'outer;
                }
            }
            if md.is_dir() {
                stack.push(path);
            }
        }
    }
    json!({ "items": results, "truncated": truncated, "scanned": scanned })
}

/// RFC 5987 百分号编码（用于中文文件名）。
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for b in text.as_bytes() {
        let c = *b as char;
        let attr = c.is_ascii_alphanumeric()
            || matches!(
                c,
                '!' | '#' | '$' | '&' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~'
            );
        if attr {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dhrust-fileman-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolves_absolute_relative_and_dotdot_paths() {
        let base = Path::new("/opt/app");
        assert_eq!(
            resolve_path(base, "/var/log/../log/x").unwrap(),
            PathBuf::from("/var/log/x")
        );
        assert_eq!(
            resolve_path(base, "data").unwrap(),
            PathBuf::from("/opt/app/data")
        );
        assert!(resolve_path(base, "  ").is_err());
    }

    #[test]
    fn protects_root_first_level_and_base() {
        let base = Path::new("/opt/star");
        assert!(is_protected(Path::new("/"), base));
        assert!(is_protected(Path::new("/etc"), base));
        assert!(is_protected(Path::new("/usr"), base));
        assert!(!is_protected(Path::new("/etc/nginx"), base));
        assert!(!is_protected(Path::new("/var/log"), base));
        assert!(is_protected(base, base));
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("a.txt").is_ok());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn guard_denies_configured_path_and_its_subtree() {
        let base = temp_dir("guard");
        let secret = base.join("Config").join("plugin-store.key");
        fs::create_dir_all(secret.parent().unwrap()).unwrap();
        fs::write(&secret, b"secret").unwrap();
        let guard = Guard::new(&base).deny("Config/plugin-store.key");
        assert!(guard.resolve("Config/plugin-store.key").is_err());
        assert!(guard.resolve("config/../Config/plugin-store.key").is_err());
        assert!(guard.resolve("Config").is_ok());
        assert!(guard.resolve("Config/other.toml").is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_entries_sorted_dirs_first() {
        let dir = temp_dir("list");
        fs::create_dir(dir.join("zdir")).unwrap();
        fs::write(dir.join("b.txt"), b"hello").unwrap();
        fs::write(dir.join("A.txt"), b"world!").unwrap();
        let items = list_dir(&dir).unwrap();
        let names: Vec<String> = items
            .iter()
            .map(|v| v.get("name").unwrap().as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["zdir", "A.txt", "b.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compress_and_extract_roundtrip() {
        let dir = temp_dir("zip");
        let a = dir.join("a.txt");
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(&a, b"AAAA").unwrap();
        fs::write(sub.join("b.txt"), b"BBBBBB").unwrap();
        let (zip_path, files, size) = compress_entry(&dir, &[a, sub], "out").unwrap();
        assert_eq!(files, 2);
        assert!(size > 0);
        let out = dir.join("extract");
        let (target, efiles, _) = extract_entry(&zip_path, Some(&out)).unwrap();
        assert_eq!(efiles, 2);
        assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"AAAA");
        assert_eq!(fs::read(target.join("sub/b.txt")).unwrap(), b"BBBBBB");
        fs::remove_dir_all(&dir).unwrap();
    }

    // ————— 分片上传（2026-10-31）—————

    #[test]
    fn upload_chunks_assemble_and_reject_bad_offset() {
        let dir = temp_dir("upload");
        let id = "abcdefgh1234";
        let p = upload_chunk(&dir, "a.bin", id, 0, 5, b"ab", false).unwrap();
        assert!(!p.done);
        assert_eq!(p.received, 2);
        assert_eq!(upload_received(&dir, id).unwrap(), 2);
        // offset 与服务端已收字节不一致 → 拒绝（防乱序/重复）
        assert!(upload_chunk(&dir, "a.bin", id, 0, 5, b"x", false).is_err());
        // 超出总大小 → 拒绝
        assert!(upload_chunk(&dir, "a.bin", id, 2, 5, b"cdef", false).is_err());
        let p = upload_chunk(&dir, "a.bin", id, 2, 5, b"cde", false).unwrap();
        assert!(p.done);
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"abcde");
        // 临时分片已合入目标并被移除
        assert_eq!(upload_received(&dir, id).unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_respects_overwrite_flag() {
        let dir = temp_dir("upload-overwrite");
        fs::write(dir.join("a.bin"), b"old").unwrap();
        let id = "abcdefgh5678";
        let err = upload_chunk(&dir, "a.bin", id, 0, 3, b"new", false).unwrap_err();
        assert!(err.contains("已存在"), "{err}");
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"old");
        let p = upload_chunk(&dir, "a.bin", id, 0, 3, b"new", true).unwrap();
        assert!(p.done);
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_rejects_bad_id_and_oversize() {
        let dir = temp_dir("upload-bad");
        // 会话 id 非法（含路径分隔符 / 过短）→ 拒绝，防写到目录外
        assert!(upload_chunk(&dir, "a.bin", "../escape", 0, 1, b"x", true).is_err());
        assert!(upload_chunk(&dir, "a.bin", "short", 0, 1, b"x", true).is_err());
        // 超过 2GB 上限 → 首片即拒，不白传
        assert!(upload_chunk(&dir, "a.bin", "abcdefgh1", 0, MAX_UPLOAD_SIZE + 1, b"x", true).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_dir_hides_upload_temp_files() {
        let dir = temp_dir("upload-hidden");
        fs::write(dir.join("a.txt"), b"x").unwrap();
        fs::write(dir.join(format!("{UPLOAD_TMP_PREFIX}abcdefgh.part")), b"x").unwrap();
        let items = list_dir(&dir).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    // ————— 多格式解压（2026-10-31）—————

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn detects_and_extracts_tar_gz() {
        let dir = temp_dir("targz");
        let tar = tar_bytes(&[("sub/a.txt", b"hello")]);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tar).unwrap();
        let src = dir.join("pkg.tar.gz");
        fs::write(&src, enc.finish().unwrap()).unwrap();
        assert_eq!(detect_archive(&src).unwrap(), ArchiveFormat::TarGz);
        assert_eq!(default_extract_target(&src), dir.join("pkg"));
        let target = dir.join("out");
        let report = extract_archive(&src, &target, None).unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(report.format, Some(ArchiveFormat::TarGz));
        assert_eq!(fs::read(target.join("sub/a.txt")).unwrap(), b"hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_bare_tar_and_bz2_zst_streams() {
        let dir = temp_dir("tar-forms");
        let tar = tar_bytes(&[("x/y.txt", b"0123456789")]);
        // 裸 tar
        let bare = dir.join("pkg.tar");
        fs::write(&bare, &tar).unwrap();
        assert_eq!(detect_archive(&bare).unwrap(), ArchiveFormat::Tar);
        let r = extract_archive(&bare, &dir.join("o1"), None).unwrap();
        assert_eq!(r.files, 1);
        // tar.bz2
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
        bz.write_all(&tar).unwrap();
        let bz2 = dir.join("pkg.tar.bz2");
        fs::write(&bz2, bz.finish().unwrap()).unwrap();
        assert_eq!(detect_archive(&bz2).unwrap(), ArchiveFormat::TarBz2);
        let r = extract_archive(&bz2, &dir.join("o2"), None).unwrap();
        assert_eq!(r.files, 1);
        assert_eq!(fs::read(dir.join("o2/x/y.txt")).unwrap(), b"0123456789");
        // tar.zst
        let mut z = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        z.write_all(&tar).unwrap();
        let zst = dir.join("pkg.tar.zst");
        fs::write(&zst, z.finish().unwrap()).unwrap();
        assert_eq!(detect_archive(&zst).unwrap(), ArchiveFormat::TarZst);
        let r = extract_archive(&zst, &dir.join("o3"), None).unwrap();
        assert_eq!(r.files, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_single_stream_files() {
        let dir = temp_dir("single-stream");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(b"single payload").unwrap();
        let src = dir.join("note.txt.gz");
        fs::write(&src, enc.finish().unwrap()).unwrap();
        // 不是 tar → 识别为单文件 GZ
        assert_eq!(detect_archive(&src).unwrap(), ArchiveFormat::Gzip);
        let r = extract_archive(&src, &dir.join("out"), None).unwrap();
        assert_eq!(r.files, 1);
        assert_eq!(fs::read(dir.join("out/note.txt")).unwrap(), b"single payload");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_encrypted_zip_with_password() {
        let dir = temp_dir("zip-aes");
        let src = dir.join("sec.zip");
        {
            let f = fs::File::create(&src).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opt = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .with_aes_encryption(zip::AesMode::Aes256, "s3cret");
            w.start_file("a.txt", opt).unwrap();
            w.write_all(b"secret payload").unwrap();
            w.finish().unwrap();
        }
        // 未给密码 → 明确提示需要密码
        let err = extract_archive(&src, &dir.join("o1"), None).unwrap_err();
        assert!(err.contains("加密"), "{err}");
        // 错密码
        let err = extract_archive(&src, &dir.join("o2"), Some("bad")).unwrap_err();
        assert!(err.contains("密码"), "{err}");
        // 正确密码
        let r = extract_archive(&src, &dir.join("o3"), Some("s3cret")).unwrap();
        assert_eq!(r.files, 1);
        assert_eq!(fs::read(dir.join("o3/a.txt")).unwrap(), b"secret payload");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_7z_with_and_without_password() {
        let dir = temp_dir("sevenz");
        let inner = dir.join("in");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("a.txt"), b"seven zip payload").unwrap();
        // 无密码包
        let plain = dir.join("pkg.7z");
        sevenz_rust2::compress_to_path(&inner, &plain).unwrap();
        assert_eq!(detect_archive(&plain).unwrap(), ArchiveFormat::SevenZip);
        let r = extract_archive(&plain, &dir.join("o1"), None).unwrap();
        assert_eq!(r.files, 1);
        // 加密包
        let locked = dir.join("locked.7z");
        sevenz_rust2::compress_to_path_encrypted(&inner, &locked, "pw123456".into()).unwrap();
        let r = extract_archive(&locked, &dir.join("o2"), Some("pw123456")).unwrap();
        assert_eq!(r.files, 1);
        let found = dir.join("o2/a.txt");
        assert_eq!(fs::read(&found).unwrap(), b"seven zip payload");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_traversal_entries_and_rar() {
        let dir = temp_dir("traversal");
        // tar crate 自身就拒绝写入含 `..` 的条目（append_data 报错），
        // 所以这里直接测解压侧的路径闸门：任何 `..` / 绝对路径 / 盘符一律不出目标目录。
        let base = PathBuf::from("/tmp/target");
        assert!(safe_join(&base, Path::new("../evil.txt")).is_none());
        assert!(safe_join(&base, Path::new("a/../../evil")).is_none());
        assert!(safe_join(&base, Path::new("/etc/passwd")).is_none());
        assert!(safe_join(&base, Path::new("")).is_none());
        assert!(safe_join(&base, Path::new("./")).is_none());
        assert_eq!(
            safe_join(&base, Path::new("./a/b.txt")).unwrap(),
            base.join("a").join("b.txt")
        );
        #[cfg(windows)]
        assert!(safe_join(&base, Path::new("C:\\Windows\\x.txt")).is_none());
        // 正常 tar 仍可解（回归：闸门不能误杀普通条目）
        let src = dir.join("ok.tar");
        fs::write(&src, tar_bytes(&[("a/b.txt", b"ok")])).unwrap();
        let report = extract_archive(&src, &dir.join("out"), None).unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(report.skipped, 0);
        // rar 明确不支持
        let rar = dir.join("x.rar");
        fs::write(&rar, b"Rar!\x1a\x07\x00").unwrap();
        let err = detect_archive(&rar).unwrap_err();
        assert!(err.contains("RAR"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_extract_target_strips_known_suffixes() {
        assert_eq!(archive_stem("a.tar.gz"), "a");
        assert_eq!(archive_stem("a.tgz"), "a");
        assert_eq!(archive_stem("a.zip"), "a");
        assert_eq!(archive_stem("a.7z"), "a");
        assert_eq!(archive_stem("a.txt.gz"), "a.txt");
        assert_eq!(archive_stem("noext"), "noext");
        assert_eq!(
            default_extract_target(Path::new("/tmp/pkg/b.tar.xz")),
            PathBuf::from("/tmp/pkg/b")
        );
    }
}
