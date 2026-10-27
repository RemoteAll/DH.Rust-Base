//! 自升级管线（**应用层**）：识别 → 静置 → 自检 → 原子替换 → 交接重启。
//!
//! 下沉自三套各自实现的升级逻辑（口径统一，能力取并集）：
//! - **Pek.RAgent**：`{exe}.new` + `Update/` 双通道、静置 10s、影子自检 `-selftest`、
//!   原子替换、Unix `execv` 原地接管 / Windows SCM 失败恢复 / 重启助手兜底、外部替换检测；
//! - **Pek.RPanlServer**：`Data/SelfUpgrade/` 版本化暂存、可选 Ed25519 签名、`-ApplyUpdate` 助手；
//! - **HLZX.EIP**：`Update/` 暂存、五类包识别、资源目录热更（`index.html` 最后替换）、助手兜底。
//!
//! # 分层
//! - **入口约定（本模块）**：`{exe}.new` 优先，其次 `Update/` 目录里最新的一个；其余候选标记
//!   `.skipped`（一次只升一个，只重启一轮）；失败留证 `<文件>.failed`。
//! - **应用（本模块）**：静置稳定 → 可执行魔数 → 自检 → 原子替换（Windows 改名让位 + 重试）。
//! - **交接（本模块）**：Unix `execv` 原地接管（PID 不变、无服务空窗）；Windows 交由重启助手
//!   （等待旧进程退出 → 覆盖/拉起），启用 `service` 特性的项目可改走 SCM 失败恢复。
//! - **发行源检查 / 增量补丁**（`catalog.json` + `dhrust::patch`）不在本模块——见 `patch`、
//!   `plugin` 与各项目自己的 feed 实现（后续可继续下沉）。
//!
//! # 与磁盘的关系
//! 只做「文件级」动作：不碰数据库、不解析业务包。资源目录热更用 [`sync_tree`]，
//! 程序本体用 [`apply`] + [`handoff`]。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 重启助手参数名（新进程被拉起时带此参数 → 由 [`run_helper_from_args`] 接管）。
pub const HELPER_FLAG: &str = "--restart-helper";

// ————— 识别与校验 —————

/// 可执行文件魔数（ELF / PE）。用于拦掉「上传了半截文件 / 传了个 zip 当程序」。
pub fn is_executable(data: &[u8]) -> bool {
    data.starts_with(b"\x7FELF") || (data.len() > 2 && &data[..2] == b"MZ")
}

/// 暂存文件是否**已上传完成**（静置足够久）。
///
/// 判定：存在 → 是文件 → 大小 > 0 → 修改时间距今 ≥ `quiet`。
/// 上传中的文件（大小还在变）不满足，避免拿到半成品。
pub fn is_stable(path: &Path, quiet: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(mtime) {
        Ok(age) => age >= quiet,
        // 时钟回拨（mtime 在未来）：按「刚改动」处理，等下一轮
        Err(_) => false,
    }
}

/// 暂存候选（按优先级）：`{exe}.new` 在前，其后是 `Update/` 目录内的文件（新→旧）。
///
/// 跳过内部后缀（`.skipped` / `.failed` / `.download-tmp` / `.old`）与子目录；
/// 调用方通常只取第一个应用，其余用 [`skip_candidates`] 标记，避免多轮重启。
pub fn staged_candidates(exe: &Path, update_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dot_new = PathBuf::from(format!("{}.new", exe.display()));
    if dot_new.is_file() {
        out.push(dot_new);
    }
    let mut dir_items: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(update_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if [".skipped", ".failed", ".download-tmp", ".old"]
                .iter()
                .any(|s| name.ends_with(s))
            {
                continue;
            }
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            dir_items.push((mtime, p));
        }
    }
    dir_items.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    out.extend(dir_items.into_iter().map(|(_, p)| p));
    out
}

/// 把未被采用的候选标记为 `<名>.skipped`（保留现场、不再参与后续轮次）。
pub fn skip_candidates(paths: &[PathBuf]) {
    for p in paths {
        let skipped = PathBuf::from(format!("{}.skipped", p.display()));
        let _ = std::fs::remove_file(&skipped);
        let _ = std::fs::rename(p, &skipped);
    }
}

/// 升级失败留证：候选改名 `<名>.failed` 供排查（当前程序不受影响）。
pub fn mark_failed(path: &Path) -> Option<PathBuf> {
    let failed = PathBuf::from(format!("{}.failed", path.display()));
    let _ = std::fs::remove_file(&failed);
    std::fs::rename(path, &failed).ok().map(|_| failed)
}

/// 备份目标文件到 `<名>.bak-<时间戳>`（替换前保留旧程序；失败返回原因）。
pub fn backup_file(path: &Path) -> Result<PathBuf, String> {
    if !path.is_file() {
        return Err(format!("要备份的文件不存在：{}", path.display()));
    }
    let stamp = crate::times::now_local_ms_text().replace([' ', ':', '-', '.'], "");
    let bak = PathBuf::from(format!("{}.bak-{stamp}", path.display()));
    std::fs::copy(path, &bak).map_err(|e| format!("备份失败（{}）：{e}", path.display()))?;
    Ok(bak)
}

/// 星尘影子模式判定（**纯函数**，便于测试）：程序路径里带 `shadow/` → 返回部署基目录。
///
/// 影子模式（Pek.RAgent 托管）下运行的是 `shadow/` 里的副本，**源程序文件不被占用**，
/// 换完文件直接退出即可让星尘重建影子并拉起新版本——不需要助手、不需要等旧进程。
///
/// - `exe`：当前程序路径；
/// - `base_env`：部署基目录（各项目环境变量值，如 `PEK_RPANL_BASE` / `HLKTECH_IOT_BASE`）。
///
/// 环境变量缺失时**按路径推断**（`<实际目录>/shadow/<名称>-<hash>/<程序>` 上三级 = 实际目录）：
/// 星尘 CLI 注册的子服务不一定注入环境变量，只按环境变量判定会把影子部署误判成普通部署，
/// 从而走 `execv` 自重启（PID 不变、星尘仍认为原进程在跑，等于“重启没生效”）。
pub fn shadow_deploy_base(exe: &Path, base_env: Option<&str>) -> Option<PathBuf> {
    let normalized = exe.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    if !normalized.contains("/shadow/") {
        return None;
    }
    if let Some(base) = base_env.map(str::trim).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(base));
    }
    // shadow/<名称>-<hash>/<程序> → 上三级即实际目录
    exe.parent()?.parent()?.parent().map(Path::to_path_buf)
}

/// 按**魔数**把升级包解到 `dest`：支持 `.zip` 与 `.tar.gz`（不看扩展名，改名也不会解错）。
///
/// - zip：`PK\x03\x04` → `crate::zip::extract_zip`（已防目录穿越 / 占用文件安全替换）；
/// - gzip：`1f 8b` → gunzip 后按 tar 解包（`tar` crate 会自动跳过绝对路径与带 `..` 的条目）。
#[cfg(feature = "zip-extract")]
pub fn extract_archive(path: &Path, dest: &Path) -> Result<(), String> {
    let mut head = [0u8; 4];
    let n = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .map_err(|e| format!("打开升级包失败（{}）：{e}", path.display()))?;
        f.read(&mut head)
            .map_err(|e| format!("读取升级包失败（{}）：{e}", path.display()))?
    };
    if n >= 4 && head[..4] == *b"PK\x03\x04" {
        return crate::zip::extract_zip(path, dest).map(|_| ());
    }
    if n >= 2 && head[0] == 0x1f && head[1] == 0x8b {
        let f = std::fs::File::open(path)
            .map_err(|e| format!("打开升级包失败（{}）：{e}", path.display()))?;
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
        ar.set_overwrite(true);
        ar.unpack(dest)
            .map_err(|e| format!("解包 tar.gz 失败（{}）：{e}", path.display()))?;
        return Ok(());
    }
    Err(format!(
        "不支持的升级包格式（{}）——只支持 .zip / .tar.gz",
        path.display()
    ))
}

/// 从升级包里解出程序文件：解包到 `dest` → 递归按文件名找 → 大小/魔数校验。
///
/// 下沉自 HlktechIoT「面板在线升级」：发布包可能是 `.zip` 也可能是 `.tar.gz`（集群发行包
/// 常用后者），按**魔数字节**自动分派，不靠扩展名。内含 `<程序名>`（可能在子目录里），
/// 找不到/不是可执行文件/体积过小都明确报错，不静默替换。
///
/// - `names`：候选文件名（按顺序匹配，如 `["app.exe", "app"]`）；
/// - `min_bytes`：最小体积（防「传了个配置/半截包」）。
#[cfg(feature = "zip-extract")]
pub fn extract_program(
    archive_path: &Path,
    dest: &Path,
    names: &[&str],
    min_bytes: usize,
) -> Result<PathBuf, String> {
    if names.is_empty() {
        return Err("未提供程序文件名".to_string());
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("创建升级目录失败：{e}"))?;
    extract_archive(archive_path, dest)?;
    let mut found = None;
    for name in names {
        if let Some(p) = find_file(dest, name, 2000) {
            found = Some(p);
            break;
        }
    }
    let Some(path) = found else {
        return Err(format!(
            "包里没有找到程序文件（{}）——请使用发布包 .zip / .tar.gz",
            names.join(" / ")
        ));
    };
    let meta = std::fs::metadata(&path).map_err(|e| format!("读取程序信息失败：{e}"))?;
    if (meta.len() as usize) < min_bytes {
        return Err(format!(
            "升级程序体积过小（{} 字节 < {min_bytes}），疑似不是完整发布包",
            meta.len()
        ));
    }
    let data = std::fs::read(&path).map_err(|e| format!("读取升级程序失败：{e}"))?;
    if !is_executable(&data) {
        return Err(format!(
            "程序格式不符（{} 不是 PE/ELF 可执行文件）",
            path.display()
        ));
    }
    Ok(path)
}

/// 递归查找文件名（最多 `limit` 个条目，避免畸形包拖死）。
#[cfg(feature = "zip-extract")]
fn find_file(dir: &Path, name: &str, limit: usize) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(cur) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > limit {
                return None;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(path);
            }
        }
    }
    None
}

// ————— 版本化升级包仓库 —————

/// 版本号是否合法（字母数字与 `.` `_` `+` `-`，最长 40 字符）。
pub fn valid_version(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty()
        && t.len() <= 40
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// 从文件名 / 路径里识别 `vX.Y.Z` 版本号（**至少三段数字**才认为命中，避免把 `v2` 当版本）。
///
/// 例：`pek-rpanlserver-v0.1.62-x86_64-unknown-linux-musl.tar.gz` → `0.1.62`。
pub fn version_from_name(name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        if (chars[i] == 'v' || chars[i] == 'V') && chars[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut out = String::new();
            let mut dots = 0;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                if chars[j] == '.' {
                    dots += 1;
                }
                out.push(chars[j]);
                j += 1;
            }
            let trimmed = out.trim_end_matches('.').to_string();
            if dots >= 2 && !trimmed.is_empty() {
                return Some(trimmed);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// 升级包元数据（落盘为 `<目录>/<文件名>.json`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    /// 存储文件名（`<前缀>-<版本>-<平台>`）
    pub file: String,
    /// 版本号（不含前导 `v`）
    pub version: String,
    /// 字节数
    pub size: u64,
    /// SHA-256（十六进制）
    pub sha256: String,
    /// 上传说明
    pub notes: String,
    /// 上传时间（`yyyy-MM-dd HH:mm:ss`）
    pub uploaded_at: String,
    /// 目标平台标签
    pub platform: String,
}

impl Package {
    /// 元数据 JSON（面板/接口直接用；不含 `current` / `prunable` 这类调用方语义）。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "file": self.file,
            "version": self.version,
            "size": self.size,
            "sha256": self.sha256,
            "notes": self.notes,
            "uploadedAt": self.uploaded_at,
            "platform": self.platform,
        })
    }
}

/// 版本化升级包仓库：`<dir>/<前缀>-<版本>-<平台>` + 同名 `.json` 元数据。
///
/// 下沉自 Pek.RPanlServer 的 `Data/SelfUpgrade/`（面板上传 → 列出 → 删除 → 清理旧包），
/// **落盘格式与既有完全兼容**（老包不用迁移），可直接给其它项目做「上传发布包 + 一键回退/清理」。
///
/// 目录由调用方给（各项目自己的 Data 目录），本类型只管「文件级」动作：不解析业务包、
/// 不校验签名（签名策略属调用方）。
#[derive(Clone, Debug)]
pub struct PackageStore {
    dir: PathBuf,
    prefix: String,
}

impl PackageStore {
    /// `prefix` = 程序名（包文件名前缀，如 `pek-rpanlserver`）。
    pub fn new(dir: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            prefix: prefix.into(),
        }
    }

    /// 仓库目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 包文件名前缀（程序名）。
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// 存储文件名白名单校验（防路径穿越）：必须以前缀开头，只允许字母数字与 `.` `_` `-` `+`。
    pub fn safe_file_name(&self, name: &str) -> bool {
        let n = name.trim();
        !n.is_empty()
            && n.len() <= 120
            && n.starts_with(&self.prefix)
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
    }

    /// 列出全部升级包（新 → 旧，按上传时间倒序）。
    pub fn list(&self) -> Vec<Package> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Package> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|e| {
                let text = std::fs::read_to_string(e.path()).ok()?;
                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                Some(Package {
                    file: v.get("file")?.as_str()?.to_string(),
                    version: v.get("version")?.as_str()?.to_string(),
                    size: v.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
                    sha256: v
                        .get("sha256")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    notes: v
                        .get("notes")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    uploaded_at: v
                        .get("uploadedAt")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    platform: v
                        .get("platform")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect();
        out.sort_by(|a, b| b.uploaded_at.cmp(&a.uploaded_at));
        out
    }

    /// 按存储文件名查找包元数据。
    pub fn find(&self, file: &str) -> Option<Package> {
        let file = file.trim();
        self.list().into_iter().find(|p| p.file == file)
    }

    /// 保存升级包（同版本覆盖）：写二进制 + 元数据，返回包信息。
    ///
    /// 版本号非法、写盘失败都返回可读错误；体积上限等策略由调用方在调用前判断。
    pub fn save(
        &self,
        version: &str,
        platform: &str,
        data: &[u8],
        notes: &str,
    ) -> Result<Package, String> {
        let version = version.trim().trim_start_matches('v');
        if !valid_version(version) {
            return Err(format!(
                "版本号无效（示例：0.1.25；只能包含字母数字与 . _ + -）：{version}"
            ));
        }
        if data.is_empty() {
            return Err("升级包为空".to_string());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("创建升级包目录失败：{e}"))?;
        let platform = platform.trim();
        let file = if platform.is_empty() {
            format!("{}-{}", self.prefix, version)
        } else {
            format!("{}-{}-{}", self.prefix, version, platform)
        };
        let bin = self.dir.join(&file);
        std::fs::write(&bin, data).map_err(|e| format!("写入升级包失败：{e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
        }
        let pkg = Package {
            file: file.clone(),
            version: version.to_string(),
            size: data.len() as u64,
            sha256: crate::sign::sha256_hex(data),
            notes: notes.trim().chars().take(200).collect(),
            uploaded_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            platform: platform.to_string(),
        };
        std::fs::write(
            self.dir.join(format!("{file}.json")),
            serde_json::to_vec_pretty(&pkg.to_json()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("写入元数据失败：{e}"))?;
        Ok(pkg)
    }

    /// 删除升级包（二进制与元数据一并删除）。
    pub fn delete(&self, file: &str) -> Result<(), String> {
        let file = file.trim();
        if !self.safe_file_name(file) {
            return Err("文件名无效".to_string());
        }
        let bin = self.dir.join(file);
        if !bin.is_file() {
            return Err("升级包不存在".to_string());
        }
        let _ = std::fs::remove_file(&bin);
        let _ = std::fs::remove_file(self.dir.join(format!("{file}.json")));
        Ok(())
    }

    /// 读取升级包二进制内容（文件名同样过白名单校验）。
    pub fn read(&self, file: &str) -> Result<Vec<u8>, String> {
        let file = file.trim();
        if !self.safe_file_name(file) {
            return Err("文件名无效".to_string());
        }
        let bin = self.dir.join(file);
        std::fs::read(&bin).map_err(|e| format!("读取升级包失败：{e}"))
    }

    /// 清理计划：**保留**版本高于 `current` 的包（还没升级到）与不高于 `current` 里最新
    /// `keep_recent` 个（当前版本 + 上一版本，供回退），返回其余可清理的包（新 → 旧）。
    pub fn plan_prune(&self, current: &str, keep_recent: usize) -> Vec<Package> {
        let mut older: Vec<Package> = self
            .list()
            .into_iter()
            .filter(|p| !crate::version::is_newer(&p.version, current))
            .collect();
        older.sort_by(|a, b| crate::version::compare_numeric(&b.version, &a.version));
        older.into_iter().skip(keep_recent).collect()
    }

    /// 某包是否会被清理（面板「可清理」标记用）。
    pub fn is_prunable(&self, current: &str, keep_recent: usize, file: &str) -> bool {
        self.plan_prune(current, keep_recent)
            .iter()
            .any(|p| p.file == file)
    }

    /// 执行清理（口径见 [`Self::plan_prune`]），返回被删除的包（按删除顺序）。
    pub fn prune(&self, current: &str, keep_recent: usize) -> Vec<Package> {
        let mut removed = Vec::new();
        for pkg in self.plan_prune(current, keep_recent) {
            if self.delete(&pkg.file).is_ok() {
                removed.push(pkg);
            }
        }
        removed
    }
}

// ————— 替换 —————

/// 原子替换 `src` → `dst`（`dst` 可在运行中；Windows 先改名让位再写入）。
///
/// - Unix：`rename` 覆盖（运行中的 ELF 可被重命名，不能就地覆盖）；
/// - Windows：`dst` 先改名 `dst.old-<时间戳>`（运行中的 exe 允许改名），再 `rename(src, dst)`；
///   瞬态占用（杀毒/索引）按 `retries` 次、`interval` 间隔重试，全部失败则**回滚**改名。
pub fn replace_file(src: &Path, dst: &Path) -> Result<(), String> {
    replace_file_with(src, dst, 20, Duration::from_millis(500))
}

/// 同 [`replace_file`]，可指定重试次数与间隔（测试与快速失败场景）。
pub fn replace_file_with(
    src: &Path,
    dst: &Path,
    retries: u32,
    interval: Duration,
) -> Result<(), String> {
    if !src.is_file() {
        return Err(format!("待替换文件不存在：{}", src.display()));
    }
    #[cfg(unix)]
    {
        let _ = (retries, interval);
        std::fs::rename(src, dst)
            .map_err(|e| format!("替换程序文件失败（{}）：{e}", dst.display()))?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        // 目标先改名让位（运行中的 exe 不能删除/覆盖，但可以改名）
        let mut backup: Option<PathBuf> = None;
        if dst.exists() {
            let stamp = crate::times::now_local_ms_text().replace([' ', ':', '-', '.'], "");
            let old = PathBuf::from(format!("{}.old-{stamp}", dst.display()));
            std::fs::rename(dst, &old)
                .map_err(|e| format!("旧程序改名让位失败（{}）：{e}", dst.display()))?;
            backup = Some(old);
        }
        let mut last = String::new();
        for i in 0..=retries {
            match std::fs::rename(src, dst) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = e.to_string();
                    if i < retries {
                        std::thread::sleep(interval);
                    }
                }
            }
        }
        // 失败回滚：把旧文件改回来，避免「目标位置空着」
        if let Some(old) = backup {
            let _ = std::fs::rename(&old, dst);
        }
        Err(format!(
            "替换程序文件失败（{}，已重试 {retries} 次）：{last}",
            dst.display()
        ))
    }
}

// ————— 自检 —————

/// 自检参数（从暂存位置跑一遍新程序，确认能启动）。
#[derive(Clone, Debug)]
pub struct SelftestSpec {
    /// 自检参数（如 `-selftest`）
    pub args: Vec<String>,
    /// 超时（超时视为失败并杀进程）
    pub timeout: Duration,
    /// 额外环境变量（如影子自检必须带 `PEK_RAGENT_BASE=<真实安装目录>`，
    /// 否则自检进程会把暂存位置当程序目录、在其下生成 Log/Config）
    pub envs: Vec<(String, String)>,
}

impl SelftestSpec {
    /// 常用形态：`[flag]` + 30 秒超时。
    pub fn new(flag: &str) -> Self {
        Self {
            args: vec![flag.to_string()],
            timeout: Duration::from_secs(30),
            envs: Vec::new(),
        }
    }

    /// 追加一个自检进程的环境变量（链式）。
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }
}

/// 从指定路径运行自检，成功条件：在 `timeout` 内正常退出且退出码为 0。
///
/// 从**暂存位置**执行（而非目标位置）——这样「新程序起不来」时当前程序毫发无损。
pub fn selftest(program: &Path, spec: &SelftestSpec) -> Result<(), String> {
    if !program.is_file() {
        return Err(format!("自检程序不存在：{}", program.display()));
    }
    let mut cmd = std::process::Command::new(program);
    cmd.args(&spec.args);
    for (k, v) in &spec.envs {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 不弹窗（服务/后台进程）
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("自检启动失败（{}）：{e}", program.display()))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("自检未通过（退出码 {status}）"))
                };
            }
            Ok(None) => {
                if started.elapsed() >= spec.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("自检超时（{} 秒）", spec.timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("自检等待失败：{e}")),
        }
    }
}

// ————— 文件身份（外部替换检测）—————

/// 程序文件身份指纹：用于发现「文件被别人直接替换了」（先删后传 / mv / 改名替换）。
///
/// - Unix：设备号 + inode（`touch`/改属性不影响）；
/// - Windows：大小 + 创建时间（运行中的 exe 不能被覆盖写，改名替换会刷新创建时间）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileIdentity {
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    created: Option<SystemTime>,
    #[cfg(not(any(unix, windows)))]
    mtime: Option<SystemTime>,
}

impl FileIdentity {
    /// 与另一份指纹比较：`true` = 文件已被替换/改写。
    ///
    /// 注意 Windows 下「同一秒内的等长改写」可能识别不到——外部替换检测本就是兜底，
    /// 主通道仍是 `{exe}.new` / `Update/`。
    pub fn changed(&self, other: &Self) -> bool {
        self != other
    }
}

/// 读取文件身份指纹（文件不存在/无权限 → `None`）。
pub fn identity_of(path: &Path) -> Option<FileIdentity> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            len: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
    #[cfg(windows)]
    {
        Some(FileIdentity {
            len: meta.len(),
            created: meta.created().ok(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some(FileIdentity {
            len: meta.len(),
            mtime: meta.modified().ok(),
        })
    }
}

// ————— 资源目录热更 —————

/// 同步目录树 `src` → `dst`：复制新增/变更的文件，删除 `dst` 里多余的旧文件。
///
/// - `index.html` **最后**写入：避免中途刷新拿到「新 html 引用旧资源」；
/// - 返回 `(复制数, 删除数)`；`dst` 不存在会自动创建。
pub fn sync_tree(src: &Path, dst: &Path) -> Result<(usize, usize), String> {
    if !src.is_dir() {
        return Err(format!("资源目录不存在：{}", src.display()));
    }
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目标目录失败（{}）：{e}", dst.display()))?;
    let mut copied = 0usize;
    let mut deferred: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut wanted: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(v) => v,
            Err(e) => return Err(format!("读取目录失败（{}）：{e}", dir.display())),
        };
        for e in rd.flatten() {
            let from = e.path();
            let rel = match from.strip_prefix(src) {
                Ok(v) => v.to_path_buf(),
                Err(_) => continue,
            };
            if from.is_dir() {
                stack.push(from);
                continue;
            }
            wanted.insert(rel.clone());
            let to = dst.join(&rel);
            if to.is_file() && same_content(&from, &to) {
                continue;
            }
            if let Some(p) = to.parent() {
                if !p.exists() {
                    std::fs::create_dir_all(p)
                        .map_err(|e| format!("创建目录失败（{}）：{e}", p.display()))?;
                }
            }
            // index.html 最后落盘
            if rel.to_string_lossy().eq_ignore_ascii_case("index.html") {
                deferred.push((from, to));
                continue;
            }
            write_atomic(&from, &to)?;
            copied += 1;
        }
    }

    for (from, to) in deferred {
        write_atomic(&from, &to)?;
        copied += 1;
    }

    // 清理目标里源没有的旧文件（按内容寻址的旧哈希资源就靠这一步淘汰）
    let mut removed = 0usize;
    let mut stack = vec![dst.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(dst) else {
                continue;
            };
            if wanted.contains(rel) {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    Ok((copied, removed))
}

/// 逐文件同步：先写同目录临时文件，再走 [`crate::io::safe_replace_file`]（占用时改名让位）。
fn write_atomic(from: &Path, to: &Path) -> Result<(), String> {
    let data =
        std::fs::read(from).map_err(|e| format!("读取失败（{}）：{e}", from.display()))?;
    let name = to
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = to.with_file_name(format!(".{name}.upgrade-tmp"));
    std::fs::write(&tmp, &data).map_err(|e| format!("写入临时文件失败（{}）：{e}", tmp.display()))?;
    crate::io::safe_replace_file(&tmp, to).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换失败（{}）：{e}", to.display())
    })
}

/// 两个文件内容是否一致（先比长度，再逐字节/分块比）。
fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// ————— 应用编排 —————

/// 一次「应用升级」的计划。
#[derive(Clone, Debug)]
pub struct ApplyPlan {
    /// 待应用的新程序（暂存文件）
    pub staged: PathBuf,
    /// 目标程序（通常是自己 `current_exe`）
    pub target: PathBuf,
    /// 静置要求（上传完成判定）
    pub quiet: Duration,
    /// 自检（`None` = 不自检）
    pub selftest: Option<SelftestSpec>,
}

impl ApplyPlan {
    /// 常用形态：静置 10 秒 + `-selftest` 自检。
    pub fn new(staged: PathBuf, target: PathBuf) -> Self {
        Self {
            staged,
            target,
            quiet: Duration::from_secs(10),
            selftest: Some(SelftestSpec::new("-selftest")),
        }
    }
}

/// 应用结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 还在上传（静置不够）——下一轮再试，**不算失败**
    NotStable,
    /// 不是可执行程序（格式不对）
    NotExecutable,
    /// 已替换目标程序（调用方接着交接/重启）
    Applied,
}

/// 执行「应用」：静置 → 可执行魔数 → 自检 → 原子替换。
///
/// 失败语义：
/// - [`ApplyOutcome::NotStable`] 是**正常中间态**（上传未完成），不要记错误、不要标记 `.failed`；
/// - 其余 `Err` 由调用方决定是否 [`mark_failed`]。
pub fn apply(plan: &ApplyPlan) -> Result<ApplyOutcome, String> {
    if !plan.staged.is_file() {
        return Err(format!("待应用文件不存在：{}", plan.staged.display()));
    }
    if !is_stable(&plan.staged, plan.quiet) {
        return Ok(ApplyOutcome::NotStable);
    }
    let data = std::fs::read(&plan.staged)
        .map_err(|e| format!("读取待应用文件失败（{}）：{e}", plan.staged.display()))?;
    if !is_executable(&data) {
        return Ok(ApplyOutcome::NotExecutable);
    }
    drop(data);
    if let Some(spec) = &plan.selftest {
        selftest(&plan.staged, spec)?;
    }
    replace_file(&plan.staged, &plan.target)?;
    Ok(ApplyOutcome::Applied)
}

// ————— 交接（重启）—————

/// 交接给新版本（**不返回**）。
///
/// - Unix：`execv` 原地接管——同进程号、无服务空窗、日志连续；
/// - 其它平台：拉起**[`HELPER_FLAG`] 重启助手**（等本进程退出后由助手拉起新版本），然后退出。
///
/// 调用方应先 `crate::logs::flush()`；本函数内部也会 flush 一次。
///
/// **没有「外部拉起者」的假设**：除「Windows 已安装为系统服务、由 SCM 失败恢复拉起」这一条外，
/// 服务进程只能「自己拉起自己」——所以助手兜底不能省（RAgent / RPanlServer / HlktechIoT 都是如此）。
pub fn handoff(exe: &Path, args: &[String]) -> ! {
    crate::logs::flush();
    #[cfg(unix)]
    {
        match exec_takeover(exe, args) {
            Ok(()) => unreachable!("execv 成功不会返回"),
            Err(e) => {
                crate::error!("原地接管失败，改用重启助手：{e}");
            }
        }
    }
    // 兜底：重启助手（Windows 及 Unix exec 失败）
    match spawn_helper(exe, args, std::process::id()) {
        Ok(pid) => crate::info!("已启动重启助手（pid {pid}），等待本进程退出后拉起新版本"),
        Err(e) => crate::error!("重启助手启动失败：{e}（请手工重启程序）"),
    }
    crate::logs::flush();
    std::process::exit(0)
}

/// Unix `execv` 原地接管（`args` 为**业务参数**，不含程序名）。
#[cfg(unix)]
fn exec_takeover(exe: &Path, args: &[String]) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(exe.as_os_str().as_bytes())
        .map_err(|_| "程序路径含 NUL 字符".to_string())?;
    let mut argv: Vec<std::ffi::CString> = Vec::new();
    for a in std::iter::once(exe.to_string_lossy().to_string()).chain(args.iter().cloned()) {
        argv.push(
            std::ffi::CString::new(a.as_bytes())
                .map_err(|_| "命令行参数含 NUL 字符".to_string())?,
        );
    }
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    crate::info!("程序文件已替换：正在原地接管新版本（execv）…");
    crate::logs::flush();
    let rc = unsafe { libc::execv(path.as_ptr(), ptrs.as_ptr()) };
    Err(format!(
        "execv 失败（错误码 {rc}）：{}",
        std::io::Error::last_os_error()
    ))
}

/// 拉起重启助手：`<非 exe 路径>`——注意传的是**新程序路径**，助手会等 `wait_pid` 退出后拉起它。
///
/// 助手进程 = 新程序自己带 [`HELPER_FLAG`]；由 [`run_helper_from_args`] 在 `main` 早期处理。
pub fn spawn_helper(program: &Path, args: &[String], wait_pid: u32) -> Result<u32, String> {
    let mut cmd = std::process::Command::new(program);
    cmd.arg(HELPER_FLAG).arg(wait_pid.to_string());
    for a in args {
        cmd.arg(a);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008); // DETACHED_PROCESS：不随父进程退出
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动重启助手失败（{}）：{e}", program.display()))?;
    Ok(child.id())
}

/// 助手任务：等 `wait_pid` 退出后，以原参数拉起程序。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperJob {
    /// 要等待退出的旧进程号
    pub wait_pid: u32,
    /// 业务参数（不含 `HELPER_FLAG` 与 pid）
    pub args: Vec<String>,
}

/// 从命令行参数里识别「这次是被重启助手拉起的」；是则返回任务（调用方应立即执行并退出）。
///
/// 形态：`<程序> --restart-helper <wait_pid> [原参数…]`——`--restart-helper=<pid>` 也认。
pub fn helper_job(args: &[String]) -> Option<HelperJob> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.trim();
        let rest = if a == HELPER_FLAG {
            it.next().map(|s| s.trim().to_string())
        } else {
            a.strip_prefix(HELPER_FLAG)
                .map(|r| r.trim_start_matches('=').trim().to_string())
        };
        let Some(pid_text) = rest else {
            continue;
        };
        let Ok(wait_pid) = pid_text.parse::<u32>() else {
            return None;
        };
        let mut rest_args: Vec<String> = Vec::new();
        let mut skipping = false;
        for x in args {
            let x = x.trim();
            if skipping {
                // 已消费 pid，后续全部是业务参数
                rest_args.push(x.to_string());
                continue;
            }
            if x == HELPER_FLAG || x.starts_with(HELPER_FLAG) {
                skipping = true;
                if x == HELPER_FLAG {
                    continue;
                }
                continue;
            }
            rest_args.push(x.to_string());
        }
        // 去掉可能残留的 pid 本身（`--restart-helper <pid>` 形态）
        if rest_args.first().map(|s| s.parse::<u32>().is_ok()) == Some(true) {
            rest_args.remove(0);
        }
        return Some(HelperJob {
            wait_pid,
            args: rest_args,
        });
    }
    None
}

/// 在助手进程中执行任务：等待旧进程退出（最多 `max_wait`）→ 拉起程序 → 退出。
///
/// 由 `main` 早期调用：`if let Some(job) = upgrade::helper_job(&args) { upgrade::run_helper(&job, exe, max_wait) }`。
pub fn run_helper(job: &HelperJob, program: &Path, max_wait: Duration) -> ! {
    let started = std::time::Instant::now();
    while started.elapsed() < max_wait {
        if !crate::sys::process::is_alive(job.wait_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // 旧进程还没退（卡死）：不强起，避免双实例抢端口——留给运维/服务管理器
    if crate::sys::process::is_alive(job.wait_pid) {
        eprintln!(
            "重启助手：等待旧进程 {} 退出超时（{} 秒），已放弃拉起（请手工重启）",
            job.wait_pid,
            max_wait.as_secs()
        );
        std::process::exit(1);
    }
    let mut cmd = std::process::Command::new(program);
    cmd.args(&job.args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008);
    }
    match cmd.spawn() {
        Ok(child) => {
            println!("重启助手：旧进程已退出，已拉起新版本（pid {}）", child.id());
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("重启助手：拉起新版本失败：{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dhrust-up-{tag}-{}", crate::random::hex(4)));
        std::fs::create_dir_all(&p).expect("建临时目录");
        p
    }

    #[test]
    fn executable_magic_is_recognized() {
        assert!(is_executable(b"\x7FELF...."));
        assert!(is_executable(b"MZ\x90\x00"));
        assert!(!is_executable(b"PK\x03\x04"));
        assert!(!is_executable(b""));
        assert!(!is_executable(b"M"));
    }

    #[test]
    fn stability_requires_quiet_period() {
        let dir = tmp_dir("stable");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("写文件");
        // 刚写完：要求 30 秒静置 → 不稳定
        assert!(!is_stable(&f, Duration::from_secs(30)));
        // 要求 0 秒 → 稳定
        assert!(is_stable(&f, Duration::ZERO));
        // 空文件永远不算稳定
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").expect("写空文件");
        assert!(!is_stable(&empty, Duration::ZERO));
        // 不存在的文件
        assert!(!is_stable(&dir.join("nope"), Duration::ZERO));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_prefer_dot_new_then_newest_and_skip_internal() {
        let dir = tmp_dir("cand");
        let exe = dir.join("app.exe");
        std::fs::write(&exe, b"MZ").expect("写程序");
        let upd = dir.join("Update");
        std::fs::create_dir_all(&upd).expect("建 Update");
        let old = upd.join("v1.bin");
        std::fs::write(&old, b"1").expect("写旧");
        let new = upd.join("v2.bin");
        std::fs::write(&new, b"2").expect("写新");
        let skipped = upd.join("v0.bin.skipped");
        std::fs::write(&skipped, b"0").expect("写跳过");
        // 把旧文件的 mtime 设得更早（用改名 + 等待不可靠，直接比较顺序：新文件在后写）

        let list = staged_candidates(&exe, &upd);
        assert_eq!(list.len(), 2, "应排除 .skipped：{list:?}");
        assert!(list.iter().all(|p| !p.to_string_lossy().ends_with(".skipped")));

        // `{exe}.new` 优先
        let dot_new = PathBuf::from(format!("{}.new", exe.display()));
        std::fs::write(&dot_new, b"MZ").expect("写 .new");
        let list = staged_candidates(&exe, &upd);
        assert_eq!(list[0], dot_new);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skip_and_fail_mark_files_in_place() {
        let dir = tmp_dir("mark");
        let a = dir.join("a.bin");
        std::fs::write(&a, b"a").expect("写文件");
        skip_candidates(std::slice::from_ref(&a));
        assert!(!a.exists());
        assert!(dir.join("a.bin.skipped").is_file());

        let b = dir.join("b.bin");
        std::fs::write(&b, b"b").expect("写文件");
        let failed = mark_failed(&b).expect("标记失败");
        assert!(!b.exists());
        assert!(failed.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_file_moves_content_and_keeps_target_valid() {
        let dir = tmp_dir("replace");
        let src = dir.join("new.exe");
        let dst = dir.join("live.exe");
        std::fs::write(&src, b"MZ-new").expect("写新文件");
        std::fs::write(&dst, b"MZ-old").expect("写旧文件");
        replace_file_with(&src, &dst, 2, Duration::from_millis(10)).expect("替换成功");
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).expect("读目标"), b"MZ-new");
        // 源不存在 → Err
        assert!(replace_file(&src, &dst).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_changes_after_rewrite() {
        let dir = tmp_dir("ident");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"one").expect("写文件");
        let first = identity_of(&f).expect("指纹");
        assert!(!first.changed(&first));
        std::fs::write(&f, b"two-two").expect("改写");
        let second = identity_of(&f).expect("指纹");
        assert!(first.changed(&second), "改写后指纹应变化");
        assert!(identity_of(&dir.join("nope")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_tree_copies_updates_and_removes_stale() {
        let dir = tmp_dir("sync");
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(src.join("assets")).expect("建源目录");
        std::fs::write(src.join("index.html"), b"<html>new</html>").expect("写 html");
        std::fs::write(src.join("assets/app.js"), b"new-js").expect("写 js");
        std::fs::create_dir_all(dst.join("assets")).expect("建目标目录");
        std::fs::write(dst.join("index.html"), b"<html>old</html>").expect("写旧 html");
        std::fs::write(dst.join("assets/app-old.js"), b"stale").expect("写旧资源");

        let (copied, removed) = sync_tree(&src, &dst).expect("同步成功");
        assert_eq!(copied, 2, "index.html + app.js");
        assert_eq!(removed, 1, "旧哈希资源应被清理");
        assert_eq!(
            std::fs::read(dst.join("index.html")).expect("读 html"),
            b"<html>new</html>"
        );
        assert!(dst.join("assets/app.js").is_file());
        assert!(!dst.join("assets/app-old.js").exists());
        // 再同步一次：内容一致 → 不复制
        let (again, _) = sync_tree(&src, &dst).expect("二次同步");
        assert_eq!(again, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_requires_executable_and_quiet() {
        let dir = tmp_dir("apply");
        let staged = dir.join("stage.bin");
        let target = dir.join("live.exe");
        std::fs::write(&target, b"MZ-old").expect("写目标");
        std::fs::write(&staged, b"PK-not-exe").expect("写非程序");

        // 静置要求 30 秒 → 未稳定
        let plan = ApplyPlan {
            staged: staged.clone(),
            target: target.clone(),
            quiet: Duration::from_secs(30),
            selftest: None,
        };
        assert_eq!(apply(&plan).expect("应用"), ApplyOutcome::NotStable);

        // 允许立即应用 → 非可执行
        let plan = ApplyPlan {
            quiet: Duration::ZERO,
            ..plan
        };
        assert_eq!(apply(&plan).expect("应用"), ApplyOutcome::NotExecutable);

        // 换成可执行魔数 → 替换成功
        std::fs::write(&staged, b"MZ-new").expect("写程序");
        assert_eq!(apply(&plan).expect("应用"), ApplyOutcome::Applied);
        assert_eq!(std::fs::read(&target).expect("读目标"), b"MZ-new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_job_parses_flag_forms() {
        let flag = HELPER_FLAG;
        let a = vec![flag.to_string(), "1234".to_string(), "-c".to_string(), "x".to_string()];
        let job = helper_job(&a).expect("识别助手参数");
        assert_eq!(job.wait_pid, 1234);
        assert_eq!(job.args, vec!["-c".to_string(), "x".to_string()]);

        let b = vec![format!("{flag}=99"), "--http-addr".to_string(), ":8300".to_string()];
        let job = helper_job(&b).expect("识别 = 形态");
        assert_eq!(job.wait_pid, 99);
        assert_eq!(job.args, vec!["--http-addr".to_string(), ":8300".to_string()]);

        assert!(helper_job(&["-c".to_string()]).is_none());
    }

    /// 影子模式判定：路径含 `shadow/` 即算（环境变量缺失时按路径推断基目录）。
    #[test]
    fn shadow_mode_detection() {
        let shadowed = PathBuf::from("/opt/app/shadow/app-1234/app");
        assert_eq!(
            shadow_deploy_base(&shadowed, Some("/opt/app")).as_deref(),
            Some(Path::new("/opt/app"))
        );
        // 非影子目录
        assert!(shadow_deploy_base(Path::new("/opt/app/app"), Some("/opt/app")).is_none());
        // **环境变量缺失时按路径推断**（星尘 CLI 注册的子服务不注入环境变量，
        // 旧逻辑会误判成普通部署 → execv 自重启 → 星尘仍认为原进程在跑）
        assert_eq!(
            shadow_deploy_base(&shadowed, None).as_deref(),
            Some(Path::new("/opt/app"))
        );
        assert_eq!(
            shadow_deploy_base(&shadowed, Some("   ")).as_deref(),
            Some(Path::new("/opt/app"))
        );
        // 环境变量给了就以它为准（部署基目录可能与路径推断不同）
        assert_eq!(
            shadow_deploy_base(&shadowed, Some("/data/app")).as_deref(),
            Some(Path::new("/data/app"))
        );
        // Windows 风格路径也认
        let win = PathBuf::from(r"C:\App\shadow\app-1234\app.exe");
        assert!(shadow_deploy_base(&win, Some(r"C:\App")).is_some());
    }

    #[test]
    fn backup_file_keeps_copy() {
        let dir = tmp_dir("bak");
        let f = dir.join("app.exe");
        std::fs::write(&f, b"MZ-old").expect("写文件");
        let bak = backup_file(&f).expect("备份成功");
        assert!(bak.is_file());
        assert_eq!(std::fs::read(&bak).expect("读备份"), b"MZ-old");
        assert!(backup_file(&dir.join("nope")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 升级包格式：`.tar.gz` 按魔数识别并解出程序；不认识的格式明确报错。
    #[test]
    #[cfg(feature = "zip-extract")]
    fn extract_program_supports_tar_gz() {
        use std::io::Write;

        let root = tmp_dir("tgz");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(pkg.join("bin")).expect("建目录");
        // 假 ELF：魔数 + 填充（体积过 min_bytes 且能通过可执行文件校验）
        let mut exe = b"\x7fELF\x02\x01\x01\x00".to_vec();
        exe.extend(std::iter::repeat_n(0u8, 4096));
        std::fs::write(pkg.join("bin").join("demo-app"), &exe).expect("写程序");
        std::fs::write(pkg.join("README.md"), "hi").expect("写说明");

        let tgz = root.join("pkg.tar.gz");
        {
            let f = std::fs::File::create(&tgz).expect("建包");
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            b.append_dir_all("demo-1.0", &pkg).expect("打 tar");
            let enc = b.into_inner().expect("收尾");
            enc.finish().expect("压缩收尾").flush().expect("刷盘");
        }

        let found = extract_program(&tgz, &root.join("out"), &["demo-app"], 1024)
            .expect("应能解出 tar.gz 里的程序");
        assert!(found.ends_with("bin/demo-app"), "{}", found.display());

        // 不是 zip / tar.gz 的内容要明确报错，不能静默替换
        let bad = root.join("bad.bin");
        std::fs::write(&bad, b"hello world").expect("写垃圾");
        let err = extract_program(&bad, &root.join("out2"), &["demo-app"], 1).unwrap_err();
        assert!(err.contains("不支持的升级包格式"), "{err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 存量自检用例：环境变量透传（用 cmd/echo 造一个会回显环境变量的自检程序）。
    #[test]
    #[cfg(windows)]
    fn selftest_passes_env_to_child() {
        let dir = tmp_dir("selftest-env");
        let bat = dir.join("check.bat");
        std::fs::write(&bat, b"@echo off\r\nif \"%HLZX_PROBE%\"==\"ok\" exit /b 0\r\nexit /b 1\r\n")
            .expect("写批处理");
        let spec = SelftestSpec::new("")
            .with_env("HLZX_PROBE", "ok");
        let spec = SelftestSpec {
            args: Vec::new(),
            ..spec
        };
        selftest(&bat, &spec).expect("环境变量应透传，自检通过");
        let bad = SelftestSpec { args: Vec::new(), timeout: Duration::from_secs(5), envs: Vec::new() };
        assert!(selftest(&bat, &bad).is_err(), "未设置环境变量应自检失败");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 从 zip 里解出程序：子目录也能找到；非可执行/过小要报错。
    #[test]
    #[cfg(feature = "zip-extract")]
    fn extract_program_finds_binary_inside_zip() {
        let dir = tmp_dir("zip");
        let mut b = crate::zip::ZipWriter::new();
        b.add_file("readme.txt", b"hi");
        b.add_file("bin/app.exe", b"MZ-fake-program");
        let zip_path = dir.join("pkg.zip");
        std::fs::write(&zip_path, b.finish()).expect("写 zip");

        let out = dir.join("out");
        let found = extract_program(&zip_path, &out, &["app.exe"], 4).expect("应找到程序");
        assert_eq!(std::fs::read(&found).expect("读程序"), b"MZ-fake-program");

        // 体积门槛
        assert!(extract_program(&zip_path, &dir.join("o2"), &["app.exe"], 1024).is_err());
        // 名字不存在
        assert!(extract_program(&zip_path, &dir.join("o3"), &["nope.exe"], 4).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_from_name_reads_three_segments() {
        assert_eq!(
            version_from_name("pek-rpanlserver-v0.1.62-x86_64-unknown-linux-musl.tar.gz").as_deref(),
            Some("0.1.62")
        );
        assert_eq!(version_from_name("app/V1.2.3.zip").as_deref(), Some("1.2.3"));
        // 不足三段 → 不认（避免把 v2 当版本）
        assert_eq!(version_from_name("app-v2.zip"), None);
        assert_eq!(version_from_name("no-version.zip"), None);
        assert!(valid_version("0.1.62"));
        assert!(!valid_version("0.1 62"));
        assert!(!valid_version(""));
    }

    #[test]
    fn package_store_crud_and_prune_keeps_recent() {
        let dir = tmp_dir("pkgstore");
        let store = PackageStore::new(dir.clone(), "app");
        // 非法版本直接拒绝
        assert!(store.save("bad/version", "win", b"x", "").is_err());
        assert!(store.save("0.0.1", "win", b"", "").is_err());

        let a = store.save("0.0.1", "win", b"one", "首个版本").unwrap();
        assert_eq!(a.file, "app-0.0.1-win");
        assert_eq!(a.notes, "首个版本");
        let b = store.save("v0.0.2", "win", b"two", "").unwrap();
        assert_eq!(b.version, "0.0.2");
        let cur = store.save("0.0.3", "win", b"three", "").unwrap();
        let next = store.save("9.9.9", "win", b"next", "").unwrap();
        assert_eq!(store.list().len(), 4);
        assert_eq!(store.read(&a.file).unwrap(), b"one");
        assert_eq!(store.find(&b.file).map(|p| p.version), Some("0.0.2".to_string()));

        // 文件名白名单：路径穿越 / 非前缀一律拒绝
        assert!(store.read("../app-0.0.1-win").is_err());
        assert!(store.delete("other.exe").is_err());
        assert!(store.safe_file_name("app-0.0.1-win"));
        assert!(!store.safe_file_name("app-0.0.1-win/../x"));

        // 保留最近 2 个（当前 0.0.3 + 上一版 0.0.2）+ 更高的 9.9.9 → 只清 0.0.1
        assert!(store.is_prunable("0.0.3", 2, &a.file));
        assert!(!store.is_prunable("0.0.3", 2, &b.file));
        assert!(!store.is_prunable("0.0.3", 2, &cur.file));
        assert!(!store.is_prunable("0.0.3", 2, &next.file));

        let removed = store.prune("0.0.3", 2);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].file, a.file);
        // 幂等：保留区不动
        assert!(store.prune("0.0.3", 2).is_empty());
        let left: Vec<String> = store.list().into_iter().map(|p| p.version).collect();
        assert_eq!(left.len(), 3);
        assert!(left.contains(&cur.version));
        assert!(left.contains(&b.version));
        assert!(left.contains(&next.version));

        // 删除到空
        for p in store.list() {
            store.delete(&p.file).expect("删除");
        }
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
