//! IIS 站点/应用池解析与本地控制（Windows；非 Windows 返回空或明确错误）。
//!
//! 由 Pek.RAgent 的 `src/iis.rs` 下沉（2026-10-22）：把 IIS 的“读配置、解析站点/
//! 应用/物理路径、w3wp ↔ 应用池映射、appcmd 调用、站点/应用池状态”抽成公共能力，
//! 供 Pek.RAgent（面板/流量）与 DHDeploy.Agent.Rust（IIS 部署）共同复用。
//!
//! 设计要点：
//! - `applicationHost.config` 支持共享配置转发（`redirection.config` → `configFile`）；
//! - 应用池按 IIS 继承规则解析：`<application applicationPool>` 优先，其次站点级
//!   `<applicationDefaults>`，再次全局 `<sites><applicationDefaults>`；
//! - `physicalPath` 展开 `%VAR%` 环境变量，路径比较统一斜杠、忽略大小写（Windows）；
//! - `appcmd` 一律直接传参（不经 shell），附带超时；输出优先解析 `/xml`，避免本地化文本；
//! - 停/启站点、停/启应用池、查询 worker PID 均为幂等操作；
//! - 本模块只做 IIS 原语，不包含部署事务（备份/回滚/维护页/进度）——那些属于调用方。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::process;

/// IIS 应用（站点 + 应用路径 + 物理路径）。保留给面板/进程展示使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisSite {
    /// 站点名
    pub site: String,
    /// 应用路径（`/` 表示站点根）
    pub path: String,
    /// 物理路径
    pub physical_path: String,
}

/// 一个 w3wp 工作进程的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisWorker {
    /// 应用池名
    pub app_pool: String,
    /// 该应用池承载的应用
    pub sites: Vec<IisSite>,
}

/// IIS 应用完整信息（配置解析结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisApplication {
    /// 站点名
    pub site: String,
    /// 站点 Id
    pub site_id: String,
    /// 应用路径（`/` 或 `/api` 等）
    pub path: String,
    /// 应用池
    pub application_pool: String,
    /// 物理路径（已展开环境变量）
    pub physical_path: String,
}

/// `applicationHost.config` 解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IisHostConfig {
    /// 全部应用（站点/应用/物理路径/应用池）
    pub applications: Vec<IisApplication>,
}

/// 部署目录反查出的 IIS 目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IisTarget {
    /// 站点名
    pub site: String,
    /// 应用路径
    pub app_path: String,
    /// 应用池
    pub application_pool: String,
    /// 应用物理路径
    pub physical_path: String,
    /// 应用池是否被多个站点共享（停止会波及其它站点）
    pub shared_pool: bool,
}

impl IisHostConfig {
    /// 从 `applicationHost.config` XML 解析（纯函数，可单测）。
    pub fn from_xml(xml: &str) -> IisHostConfig {
        let Ok(value) = crate::config::read_xml_to_json(xml) else {
            return IisHostConfig::default();
        };
        let root = value.get("configuration").unwrap_or(&value);
        let Some(host) = root.get("system.applicationHost") else {
            return IisHostConfig::default();
        };
        let Some(sites) = host.get("sites") else {
            return IisHostConfig::default();
        };

        // 全局应用池默认值：<sites><applicationDefaults applicationPool="..." />
        let global_pool = first_application_pool(sites.get("applicationDefaults"));
        let mut applications = Vec::new();
        for site in as_values_opt(sites.get("site")) {
            let site_name = str_attr(site, "name");
            if site_name.is_empty() {
                continue;
            }
            let site_id = str_attr(site, "id");
            // 站点级默认值（部分 IIS 版本支持）
            let site_pool = first_application_pool(site.get("applicationDefaults"))
                .or_else(|| global_pool.clone());
            let Some(apps) = site.get("application") else {
                continue;
            };
            for app in as_values(apps) {
                let path = {
                    let p = str_attr(app, "path");
                    if p.is_empty() {
                        "/".to_string()
                    } else {
                        p
                    }
                };
                let pool = {
                    let p = str_attr(app, "applicationPool");
                    if p.is_empty() {
                        site_pool
                            .clone()
                            .unwrap_or_else(|| "DefaultAppPool".to_string())
                    } else {
                        p
                    }
                };
                applications.push(IisApplication {
                    site: site_name.clone(),
                    site_id: site_id.clone(),
                    path,
                    application_pool: pool,
                    physical_path: expand_env_vars(&app_physical_path(app)),
                });
            }
        }
        IisHostConfig { applications }
    }

    /// 应用池 → 应用列表（键为小写池名）。
    pub fn by_pool(&self) -> HashMap<String, Vec<IisSite>> {
        let mut out: HashMap<String, Vec<IisSite>> = HashMap::new();
        for app in &self.applications {
            out.entry(app.application_pool.to_ascii_lowercase())
                .or_default()
                .push(IisSite {
                    site: app.site.clone(),
                    path: app.path.clone(),
                    physical_path: app.physical_path.clone(),
                });
        }
        out
    }

    /// 按部署目录反查 IIS 应用：取物理路径最长（最具体）的祖先匹配。
    pub fn resolve_by_physical_path(&self, deploy_path: &Path) -> Option<IisTarget> {
        let deploy = normalize_for_compare(&deploy_path.to_string_lossy());
        if deploy.is_empty() {
            return None;
        }
        let mut best: Option<(&IisApplication, usize)> = None;
        for app in &self.applications {
            let base = normalize_for_compare(&app.physical_path);
            if base.is_empty() {
                continue;
            }
            let matched = deploy == base || deploy.starts_with(&format!("{base}/"));
            if !matched {
                continue;
            }
            let len = base.len();
            if best.map(|(_, l)| len > l).unwrap_or(true) {
                best = Some((app, len));
            }
        }
        best.map(|(app, _)| self.target_of(app))
    }

    /// 按站点名反查站点根应用（`path="/"`），没有根应用时取第一个。
    pub fn resolve_by_site(&self, site: &str) -> Option<IisTarget> {
        let site = site.trim();
        if site.is_empty() {
            return None;
        }
        let mut fallback: Option<&IisApplication> = None;
        for app in &self.applications {
            if !app.site.eq_ignore_ascii_case(site) {
                continue;
            }
            if app.path == "/" || app.path.is_empty() {
                return Some(self.target_of(app));
            }
            fallback.get_or_insert(app);
        }
        fallback.map(|app| self.target_of(app))
    }

    fn target_of(&self, app: &IisApplication) -> IisTarget {
        let mut sites: HashSet<&str> = HashSet::new();
        for other in &self.applications {
            if other
                .application_pool
                .eq_ignore_ascii_case(&app.application_pool)
            {
                sites.insert(other.site.as_str());
            }
        }
        IisTarget {
            site: app.site.clone(),
            app_path: app.path.clone(),
            application_pool: app.application_pool.clone(),
            physical_path: app.physical_path.clone(),
            shared_pool: sites.len() > 1,
        }
    }
}

/// IIS 是否可用（Windows 且能找到 appcmd.exe）。
pub fn available() -> bool {
    cfg!(windows) && appcmd_path().is_some()
}

/// 读取实际生效的 `applicationHost.config`（含共享配置转发），返回路径与内容。
pub fn read_host_config_with_path() -> Option<(PathBuf, String)> {
    #[cfg(windows)]
    {
        let root = std::env::var_os("windir")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let dir = root.join("System32").join("inetsrv").join("config");
        let local = dir.join("applicationHost.config");
        let text = std::fs::read_to_string(&local).ok()?;
        if text.contains("system.applicationHost") {
            return Some((local, text));
        }
        // IIS 共享配置：本地 applicationHost.config 只是转发文件，
        // 真正的站点配置由 redirection.config 指向（常见于多机统一管理）。
        let redirect = std::fs::read_to_string(dir.join("redirection.config")).ok()?;
        let value = crate::config::read_xml_to_json(&redirect).ok()?;
        let shared = shared_config_path(&value)?;
        let path = PathBuf::from(expand_env_vars(&shared));
        let text = std::fs::read_to_string(&path).ok()?;
        Some((path, text))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 读取并解析 `applicationHost.config`（每次重新读取，部署前用）。
pub fn read_host_config() -> Result<IisHostConfig, String> {
    let Some((_, text)) = read_host_config_with_path() else {
        return Err("读取 IIS 配置失败（applicationHost.config）".to_string());
    };
    Ok(IisHostConfig::from_xml(&text))
}

/// 读取带 60 秒缓存的配置（面板类高频查询用）。
pub fn read_host_config_cached() -> IisHostConfig {
    let mut slot = CONFIG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, config)) = slot.as_ref() {
        if at.elapsed() < CONFIG_TTL {
            return config.clone();
        }
    }
    let config = read_host_config().unwrap_or_default();
    *slot = Some((Instant::now(), config.clone()));
    config
}

/// 按部署目录反查 IIS 目标（每次读取最新配置）。
pub fn resolve_by_physical_path(deploy_path: &Path) -> Option<IisTarget> {
    read_host_config()
        .ok()
        .and_then(|config| config.resolve_by_physical_path(deploy_path))
}

/// 按站点名反查 IIS 目标（每次读取最新配置）。
pub fn resolve_by_site(site: &str) -> Option<IisTarget> {
    read_host_config()
        .ok()
        .and_then(|config| config.resolve_by_site(site))
}

/// 查询 w3wp 的 IIS 归属（进程面板用）。
pub fn lookup_worker(pid: u32, process_name: &str, user: &str) -> Option<IisWorker> {
    if !is_w3wp(process_name) {
        return None;
    }
    let apps = read_host_config_cached().by_pool();
    let pool = process::command_line(pid)
        .and_then(|cmd| parse_app_pool(&cmd))
        .or_else(|| pool_from_user(user, &apps))?;
    let sites = apps
        .get(&pool.to_ascii_lowercase())
        .cloned()
        .unwrap_or_default();
    Some(IisWorker {
        app_pool: pool,
        sites,
    })
}

/// 从 w3wp 命令行解析 `-ap "应用池"`（也兼容不带引号）。
pub fn parse_app_pool(command_line: &str) -> Option<String> {
    let lower = command_line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("-ap") {
        let abs = from + pos;
        let before_ok = cmd_char_before(command_line, abs).is_none_or(|c| c.is_whitespace());
        let rest = &command_line[abs + 3..];
        let after_ok = rest
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '"');
        if before_ok && after_ok {
            let rest = rest.trim_start();
            let value = match rest.strip_prefix('"') {
                Some(stripped) => stripped.split('"').next().unwrap_or(""),
                None => rest.split_whitespace().next().unwrap_or(""),
            };
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        from = abs + 3;
    }
    None
}

/// 是否 w3wp 进程名。
pub fn is_w3wp(name: &str) -> bool {
    name.trim()
        .trim_end_matches(".exe")
        .eq_ignore_ascii_case("w3wp")
}

/// 查询 IIS 站点运行状态（`Started` / `Stopped`；未知/非 Windows 返回空）。
pub fn site_states() -> HashMap<String, String> {
    #[cfg(windows)]
    {
        {
            let slot = STATE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, map)) = slot.as_ref() {
                if at.elapsed() < STATE_TTL {
                    return map.clone();
                }
            }
        }
        let map = query_site_states();
        let mut slot = STATE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some((Instant::now(), map.clone()));
        map
    }
    #[cfg(not(windows))]
    {
        HashMap::new()
    }
}

/// 查询 IIS 应用池运行状态（`Started` / `Stopped`）。
pub fn app_pool_states() -> HashMap<String, String> {
    #[cfg(windows)]
    {
        let Some(out) = appcmd(&["list", "apppools", "/xml"], APPCMD_TIMEOUT).ok() else {
            return HashMap::new();
        };
        parse_named_states_xml(&out, "APP_POOL", &["APP_POOL.NAME", "APPPOOL.NAME", "name"])
    }
    #[cfg(not(windows))]
    {
        HashMap::new()
    }
}

/// 查询应用池对应的 w3wp 进程 PID（Web 园可能有多个）。
///
/// 优先用**进程快照 + 命令行 `-ap "池名"` 归属**判定：`appcmd list wps` 在「没有
/// worker」（应用池已停，部署时的常态）时会返回退出码 1 + 空的 `<appcmd></appcmd>`，
/// 容易被误判为失败；快照方案不依赖该语义。快照未命中时再用 appcmd 交叉验证。
pub fn workers_of_pool(pool: &str) -> Result<Vec<u32>, String> {
    let pool = pool.trim();
    if pool.is_empty() {
        return Ok(Vec::new());
    }
    let mut pids = Vec::new();
    for pid in super::monitor::pids_by_name("w3wp") {
        if let Some(cmd) = process::command_line(pid) {
            if let Some(name) = parse_app_pool(&cmd) {
                if name.eq_ignore_ascii_case(pool) {
                    pids.push(pid);
                }
            }
        }
    }
    if !pids.is_empty() {
        pids.sort_unstable();
        pids.dedup();
        return Ok(pids);
    }
    #[cfg(windows)]
    if let Some(pids) = appcmd_workers(pool) {
        return Ok(pids);
    }
    Ok(Vec::new())
}

/// `appcmd list wps` 的 worker PID（None 表示查询不可用/出错，调用方应以快照为准）。
#[cfg(windows)]
fn appcmd_workers(pool: &str) -> Option<Vec<u32>> {
    let filter = format!("/apppool.name:{pool}");
    let args = ["list", "wps", filter.as_str(), "/xml"];
    let (code, stdout, stderr) = appcmd_raw(&args, APPCMD_TIMEOUT).ok()?;
    if code != 0 {
        // 「无匹配对象」= 空文档 + 无错误输出 → 没有 worker（正常）
        return if stderr.trim().is_empty() && is_empty_appcmd_doc(&stdout) {
            Some(Vec::new())
        } else {
            None
        };
    }
    let mut pids = Vec::new();
    if let Ok(value) = crate::config::read_xml_to_json(&stdout) {
        collect_process_ids(&value, &mut pids);
    }
    pids.sort_unstable();
    pids.dedup();
    Some(pids)
}

/// 等待应用池的 w3wp 全部退出（超时返回 false）。
pub fn wait_pool_stopped(pool: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if workers_of_pool(pool).map(|p| p.is_empty()).unwrap_or(false) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 停站点（幂等）。
pub fn stop_site(site: &str, timeout: Duration) -> Result<(), String> {
    change_state("site", site, "Stopped", timeout)
}

/// 启站点（幂等）。
pub fn start_site(site: &str, timeout: Duration) -> Result<(), String> {
    change_state("site", site, "Started", timeout)
}

/// 停应用池（幂等）。共享池会波及其它站点，调用方需自行提示。
pub fn stop_app_pool(pool: &str, timeout: Duration) -> Result<(), String> {
    change_state("apppool", pool, "Stopped", timeout)
}

/// 启应用池（幂等）。
pub fn start_app_pool(pool: &str, timeout: Duration) -> Result<(), String> {
    change_state("apppool", pool, "Started", timeout)
}

/// 实时查询站点状态（`Started`/`Stopped`；未安装 IIS 或查询失败返回 None）。
///
/// 与 [`site_states`]（批量 + 缓存）不同：本函数每次真实调用 appcmd，用于停/启后的裁决。
pub fn site_state(site: &str) -> Option<String> {
    current_state("site", site.trim())
}

/// 实时查询应用池状态（`Started`/`Stopped`；未安装 IIS 或查询失败返回 None）。
pub fn app_pool_state(pool: &str) -> Option<String> {
    current_state("apppool", pool.trim())
}

/// 站点/应用池状态切换（幂等）。
///
/// 重要：`appcmd stop/start` 对「目标已是期望状态」会返回非 0 退出码且输出空的
/// `<?xml?><appcmd></appcmd>`（例如启动一个已启动的应用池），因此命令失败时一律用
/// **实时单对象查询**复核，达到期望状态即视为成功，避免部署被误判为失败。
fn change_state(kind: &str, name: &str, want: &str, timeout: Duration) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(if kind == "site" {
            "站点名为空".to_string()
        } else {
            "应用池名为空".to_string()
        });
    }
    let verb = if want.eq_ignore_ascii_case("Stopped") {
        "stop"
    } else {
        "start"
    };
    if current_state(kind, name).is_some_and(|s| s.eq_ignore_ascii_case(want)) {
        return Ok(());
    }
    match appcmd(&[verb, kind, name], timeout) {
        Ok(_) => {
            invalidate_cache();
            Ok(())
        }
        Err(e) => {
            invalidate_cache();
            let state = current_state(kind, name);
            if state
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(want))
            {
                return Ok(());
            }
            Err(match &state {
                Some(s) => format!("{e}（当前状态：{s}）"),
                None => format!("{e}（未能查询到当前状态）"),
            })
        }
    }
}

/// 实时查询状态（每次真实调用 appcmd，不读缓存）。
fn current_state(kind: &str, name: &str) -> Option<String> {
    #[cfg(windows)]
    {
        if name.is_empty() {
            return None;
        }
        let out = appcmd(&["list", kind, name], APPCMD_TIMEOUT).ok()?;
        parse_state_text(&out, name)
    }
    #[cfg(not(windows))]
    {
        let _ = (kind, name);
        None
    }
}

/// 强制结束应用池的 w3wp（调用前应先停池，避免 WAS 自动拉起）。返回结束的进程数。
pub fn kill_pool_workers(pool: &str) -> Result<usize, String> {
    let pids = workers_of_pool(pool)?;
    for pid in &pids {
        process::signal_force(*pid);
    }
    if !pids.is_empty() {
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(pids.len())
}

/// 执行 appcmd（直接传参、隐藏窗口、带超时）。
pub fn appcmd(args: &[&str], timeout: Duration) -> Result<String, String> {
    #[cfg(windows)]
    {
        let (code, stdout, stderr) = appcmd_raw(args, timeout)?;
        if code != 0 {
            return Err(appcmd_failure(code, args, &stdout, &stderr));
        }
        Ok(stdout)
    }
    #[cfg(not(windows))]
    {
        let _ = (args, timeout);
        Err("仅 Windows/IIS 支持此操作".to_string())
    }
}

/// appcmd 原始执行结果（退出码 / 标准输出 / 标准错误）。
///
/// 与 [`appcmd`] 的区别：不把非 0 退出码直接当错误——`appcmd` 对「没有匹配对象」
/// 会返回退出码 1 并输出空的 `<appcmd></appcmd>`（如查询无 worker 的应用池），
/// 调用方可据此判定为「空结果」。
#[cfg(windows)]
fn appcmd_raw(args: &[&str], timeout: Duration) -> Result<(i32, String, String), String> {
    let exe = appcmd_path().ok_or_else(|| "未找到 appcmd.exe（IIS 未安装？）".to_string())?;
    let timeout = timeout.max(Duration::from_millis(200));
    let r = process::run_with_timeout(&exe.to_string_lossy(), args, timeout)
        .map_err(|e| format!("启动 appcmd 失败：{e}"))?;
    if r.timed_out {
        return Err(format!(
            "appcmd 执行超时（{} 秒）：{}",
            timeout.as_secs(),
            args.join(" ")
        ));
    }
    Ok((r.exit_code, r.stdout, r.stderr))
}

/// 组装 appcmd 失败消息（带命令与输出）。
#[cfg_attr(not(windows), allow(dead_code))]
fn appcmd_failure(code: i32, args: &[&str], stdout: &str, stderr: &str) -> String {
    let msg = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let detail = if msg.trim().is_empty() {
        "(无输出)".to_string()
    } else {
        msg.trim().to_string()
    };
    format!(
        "appcmd 执行失败（退出码 {}，命令：appcmd {}）：{}",
        code,
        args.join(" "),
        detail
    )
}

/// appcmd 输出是否为「空对象文档」（`<?xml ...?><appcmd></appcmd>`、`<appcmd/>` 或空串）。
///
/// appcmd 在没有匹配对象时正是这种输出（配合退出码 1），此时应视为空结果而非错误。
#[cfg_attr(not(windows), allow(dead_code))]
fn is_empty_appcmd_doc(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    if lower.contains("<error") || lower.contains("error (") {
        return false;
    }
    let mut body = t;
    if let Some(pos) = lower.find("<appcmd") {
        body = &t[pos..];
    }
    if let Some(start) = body.find('>') {
        body = &body[start + 1..];
    }
    let lower_body = body.to_ascii_lowercase();
    if let Some(end) = lower_body.rfind("</appcmd") {
        body = &body[..end];
    }
    body.trim().is_empty()
}

/// appcmd.exe 路径（32 位进程优先 Sysnative，规避 System32 重定向）。
pub fn appcmd_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let root = std::env::var_os("windir")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut candidates = Vec::new();
        if cfg!(target_pointer_width = "32") {
            candidates.push(root.join("Sysnative").join("inetsrv").join("appcmd.exe"));
        }
        candidates.push(root.join("System32").join("inetsrv").join("appcmd.exe"));
        candidates.into_iter().find(|p| p.is_file())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 清除配置/站点状态缓存（IIS 配置或站点状态变化后调用）。
pub fn invalidate_cache() {
    *CONFIG_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    #[cfg(windows)]
    {
        *STATE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// 展开 `%NAME%` 环境变量（未知名原样保留）。
pub fn expand_env_vars(text: &str) -> String {
    expand_env_vars_with(text, |name| std::env::var(name).ok())
}

/// 展开 `%NAME%` 环境变量（自定义查找函数，便于测试）。
pub fn expand_env_vars_with<F>(text: &str, lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        if ch != '%' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some(rel) = text[i + 1..].find('%') else {
            out.push('%');
            i += 1;
            continue;
        };
        let end = i + 1 + rel;
        let name = &text[i + 1..end];
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '(' || c == ')');
        if let (true, Some(value)) = (valid, lookup(name)) {
            out.push_str(&value);
            i = end + 1;
        } else {
            out.push('%');
            i += 1;
        }
    }
    out
}

// ————— 内部实现 —————

const CONFIG_TTL: Duration = Duration::from_secs(60);
#[cfg(windows)]
const STATE_TTL: Duration = Duration::from_secs(15);
#[cfg(windows)]
const APPCMD_TIMEOUT: Duration = Duration::from_secs(30);

static CONFIG_CACHE: Mutex<Option<(Instant, IisHostConfig)>> = Mutex::new(None);
#[cfg(windows)]
static STATE_CACHE: Mutex<Option<(Instant, HashMap<String, String>)>> = Mutex::new(None);

#[cfg(windows)]
fn query_site_states() -> HashMap<String, String> {
    let Some(out) = appcmd(&["list", "sites", "/xml"], APPCMD_TIMEOUT).ok() else {
        return HashMap::new();
    };
    let map = parse_named_states_xml(&out, "SITE", &["SITE.NAME", "site.name", "name"]);
    if !map.is_empty() {
        return map;
    }
    parse_site_states_text(&out)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_named_states_xml(
    xml: &str,
    element: &str,
    name_attrs: &[&str],
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(value) = crate::config::read_xml_to_json(xml) else {
        return out;
    };
    let root = value.get("appcmd").unwrap_or(&value);
    let Some(list) = root.get(element).or_else(|| {
        // XML 解析器可能保留原始大小写；按不区分大小写再找一次
        root.as_object().and_then(|obj| {
            obj.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(element))
                .map(|(_, v)| v)
        })
    }) else {
        return out;
    };
    for item in as_values(list) {
        let name = attr_ci(item, name_attrs);
        let state = attr_ci(item, &["state"]);
        if !name.is_empty() && !state.is_empty() {
            out.insert(name, state);
        }
    }
    out
}

/// 解析 `appcmd list sites` 文本（XML 不可用时的回退）。
#[cfg(windows)]
fn parse_site_states_text(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        if let Some((name, state)) = parse_state_line(line) {
            out.insert(name, state);
        }
    }
    out
}

/// 从 appcmd 文本输出中解析指定对象的状态（`SITE "x" (id:1,...,state:Started)`）。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_state_text(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((n, s)) = parse_state_line(line) {
            if n.eq_ignore_ascii_case(name) {
                return Some(s);
            }
        }
    }
    None
}

/// 解析 `PREFIX "name" (...,state:Started)` 一行（站点与应用池输出通用，`state:` 不区分大小写）。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_state_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let open = line.find('"')?;
    let rest = &line[open + 1..];
    let close = rest.find('"')?;
    let name = rest[..close].trim();
    if name.is_empty() {
        return None;
    }
    let tail = &rest[close + 1..];
    let lower = tail.to_ascii_lowercase();
    let pos = lower.rfind("state:")?;
    let state = tail[pos + 6..]
        .split([',', ')'])
        .next()
        .unwrap_or("")
        .trim();
    if state.is_empty() {
        return None;
    }
    Some((name.to_string(), state.to_string()))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn collect_process_ids(value: &Value, out: &mut Vec<u32>) {
    if let Some(obj) = value.as_object() {
        if let Ok(pid) = attr_ci(value, &["processId", "processid", "pid"]).parse::<u32>() {
            out.push(pid);
        }
        for child in obj.values() {
            collect_process_ids(child, out);
        }
    } else if let Some(list) = value.as_array() {
        for child in list {
            collect_process_ids(child, out);
        }
    }
}

fn first_application_pool(value: Option<&Value>) -> Option<String> {
    let value = value?;
    for item in as_values(value) {
        let pool = str_attr(item, "applicationPool");
        if !pool.is_empty() {
            return Some(pool);
        }
    }
    None
}

fn app_physical_path(app: &Value) -> String {
    let Some(value) = app.get("virtualDirectory") else {
        return String::new();
    };
    let list = as_values(value);
    let root = list.iter().find(|vd| {
        let p = str_attr(vd, "path");
        p.is_empty() || p == "/"
    });
    root.or_else(|| list.first())
        .map(|vd| str_attr(vd, "physicalPath"))
        .unwrap_or_default()
}

fn as_values(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .map(|list| list.iter().collect())
        .unwrap_or_else(|| vec![value])
}

fn as_values_opt(value: Option<&Value>) -> Vec<&Value> {
    value.map(as_values).unwrap_or_default()
}

fn str_attr(value: &Value, key: &str) -> String {
    value.get(key).map(value_text).unwrap_or_default()
}

fn value_text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        s.trim().to_string()
    } else if let Some(n) = value.as_i64() {
        n.to_string()
    } else if let Some(b) = value.as_bool() {
        b.to_string()
    } else {
        String::new()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn attr_ci(value: &Value, names: &[&str]) -> String {
    let Some(obj) = value.as_object() else {
        return String::new();
    };
    for name in names {
        if let Some(v) = obj.get(*name) {
            let text = value_text(v);
            if !text.is_empty() {
                return text;
            }
        }
    }
    for (key, v) in obj {
        let key_lower = key.to_ascii_lowercase();
        for name in names {
            let name_lower = name.to_ascii_lowercase();
            if key_lower == name_lower || key_lower.ends_with(&format!(".{name_lower}")) {
                let text = value_text(v);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    String::new()
}

fn cmd_char_before(text: &str, byte_index: usize) -> Option<char> {
    text.get(..byte_index).and_then(|s| s.chars().next_back())
}

fn pool_from_user(user: &str, apps: &HashMap<String, Vec<IisSite>>) -> Option<String> {
    let candidate = user.rsplit(['\\', '/']).next().unwrap_or(user).trim();
    if candidate.is_empty() {
        return None;
    }
    apps.get(&candidate.to_ascii_lowercase())
        .map(|_| candidate.to_string())
}

/// 从 `redirection.config` 提取共享配置路径（`<redirection><configFile path="..."/>`）。
#[cfg(windows)]
fn shared_config_path(value: &Value) -> Option<String> {
    let root = value.get("configuration").unwrap_or(value);
    let file = root
        .get("redirection")
        .and_then(|r| r.get("configFile"))
        .or_else(|| root.get("configFile"))?;
    for item in as_values(file) {
        let path = str_attr(item, "path");
        if !path.is_empty() {
            return Some(path);
        }
        if let Some(text) = item.as_str() {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
        if let Some(text) = item.get("#text").and_then(|v| v.as_str()) {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

/// 路径比较形态：展开环境变量、统一 `/`、去尾斜杠、Windows 忽略大小写。
fn normalize_for_compare(text: &str) -> String {
    let expanded = expand_env_vars(text.trim());
    let stripped = expanded.strip_prefix(r"\\?\").unwrap_or(&expanded);
    let mut s = stripped.replace('\\', "/");
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<configuration>
  <system.applicationHost>
    <sites>
      <applicationDefaults applicationPool="DefaultAppPool" />
      <site name="gicisky.net" id="1">
        <application path="/" applicationPool="gicisky.net">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\gicisky.net" />
        </application>
        <application path="/api" applicationPool="gicisky.net">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\gicisky-api" />
        </application>
      </site>
      <site name="site2" id="2">
        <application path="/">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\site2" />
        </application>
      </site>
      <site name="site3" id="3">
        <applicationDefaults applicationPool="Site3Pool" />
        <application path="/">
          <virtualDirectory path="/" physicalPath="D:\wwwroot\site3" />
        </application>
      </site>
    </sites>
  </system.applicationHost>
</configuration>"#;

    #[test]
    fn parses_pool_inheritance() {
        let config = IisHostConfig::from_xml(SAMPLE);
        let apps = &config.applications;
        // 显式 applicationPool 优先
        assert_eq!(
            apps.iter()
                .find(|a| a.site == "gicisky.net" && a.path == "/")
                .unwrap()
                .application_pool,
            "gicisky.net"
        );
        // 站点未写 applicationPool → 继承全局 applicationDefaults
        assert_eq!(
            apps.iter()
                .find(|a| a.site == "site2")
                .unwrap()
                .application_pool,
            "DefaultAppPool"
        );
        // 站点级 applicationDefaults 覆盖全局
        assert_eq!(
            apps.iter()
                .find(|a| a.site == "site3")
                .unwrap()
                .application_pool,
            "Site3Pool"
        );
    }

    #[test]
    fn resolve_picks_longest_physical_path() {
        let config = IisHostConfig::from_xml(SAMPLE);
        // 嵌套应用优先
        let target = config
            .resolve_by_physical_path(Path::new(r"D:\wwwroot\gicisky-api\bin"))
            .expect("nested app");
        assert_eq!(target.site, "gicisky.net");
        assert_eq!(target.app_path, "/api");
        assert_eq!(target.application_pool, "gicisky.net");
        // 站点根
        let target = config
            .resolve_by_physical_path(Path::new(r"D:\wwwroot\gicisky.net\wwwroot"))
            .expect("site root");
        assert_eq!(target.app_path, "/");
        // 非 IIS 目录
        assert!(config
            .resolve_by_physical_path(Path::new(r"E:\other"))
            .is_none());
    }

    #[test]
    fn resolves_site_root_and_shared_pool() {
        let config = IisHostConfig::from_xml(SAMPLE);
        let target = config.resolve_by_site("gicisky.net").expect("site");
        assert_eq!(target.app_path, "/");
        assert!(!target.shared_pool);
        // gicisky.net 池被同一站点的两个应用使用，但只算一个站点 → 不算共享
        let mut xml = SAMPLE.replace(
            r#"<site name="site2" id="2">"#,
            r#"<site name="site2" id="2"><application path="/x" applicationPool="gicisky.net"><virtualDirectory path="/" physicalPath="D:\wwwroot\x" /></application>"#,
        );
        if !xml.contains("site2") {
            xml = SAMPLE.to_string();
        }
        let config = IisHostConfig::from_xml(&xml);
        let target = config.resolve_by_site("gicisky.net").expect("site");
        assert!(target.shared_pool, "跨站点共用同一应用池应标记 shared_pool");
    }

    #[test]
    fn parses_app_pool_from_command_line() {
        let cmd = r#""C:\Windows\System32\inetsrv\w3wp.exe" -ap "gicisky.net" -v "v4.0""#;
        assert_eq!(parse_app_pool(cmd).as_deref(), Some("gicisky.net"));
        assert_eq!(
            parse_app_pool(r#"w3wp.exe -ap DefaultAppPool -v v4.0"#).as_deref(),
            Some("DefaultAppPool")
        );
        assert_eq!(parse_app_pool(r#"w3wp.exe -v v4.0"#), None);
    }

    #[test]
    fn parses_site_states_xml() {
        let xml = r#"<appcmd>
            <SITE SITE.NAME="a" state="Started" />
            <SITE SITE.NAME="b" state="Stopped" />
        </appcmd>"#;
        let map = parse_named_states_xml(xml, "SITE", &["SITE.NAME"]);
        assert_eq!(map.get("a").map(String::as_str), Some("Started"));
        assert_eq!(map.get("b").map(String::as_str), Some("Stopped"));
    }

    #[test]
    fn parses_app_pool_states_xml() {
        let xml = r#"<appcmd>
            <APP_POOL APP_POOL.NAME="DefaultAppPool" state="Started" />
        </appcmd>"#;
        let map = parse_named_states_xml(xml, "APP_POOL", &["APP_POOL.NAME"]);
        assert_eq!(
            map.get("DefaultAppPool").map(String::as_str),
            Some("Started")
        );
    }

    #[test]
    fn parses_worker_process_ids() {
        let xml = r#"<appcmd>
            <WP WP.NAME="1234" processId="1234" />
            <WP WP.NAME="5678" processId="5678" />
        </appcmd>"#;
        let value = crate::config::read_xml_to_json(xml).unwrap();
        let mut pids = Vec::new();
        collect_process_ids(&value, &mut pids);
        pids.sort_unstable();
        assert_eq!(pids, vec![1234, 5678]);
    }

    #[test]
    fn expands_env_vars() {
        assert_eq!(
            expand_env_vars_with(r"%DRIVE%\logs\%SITE%", |name| match name {
                "DRIVE" => Some("D:".to_string()),
                "SITE" => Some("shop".to_string()),
                _ => None,
            }),
            r"D:\logs\shop"
        );
        assert_eq!(expand_env_vars_with("100% %NOPE%", |_| None), "100% %NOPE%");
    }

    #[test]
    fn normalizes_paths_for_comparison() {
        assert_eq!(
            normalize_for_compare(r"D:\WWW\Site\"),
            normalize_for_compare(r"D:\WWW\Site")
        );
        assert_eq!(
            normalize_for_compare(r"D:/WWW/Site"),
            normalize_for_compare(r"D:\WWW\Site")
        );
    }

    #[test]
    fn parses_state_line_for_site_and_app_pool() {
        // 站点：appcmd list site 的文本格式（小写 state:）
        let site =
            r#"SITE "kf.hlktech.com" (id:2,bindings:http/*:80:kf.hlktech.com,state:Started)"#;
        assert_eq!(
            parse_state_line(site),
            Some(("kf.hlktech.com".to_string(), "Started".to_string()))
        );
        // 应用池：appcmd list apppool 的文本格式（大写 State:）
        let pool = r#"APPPOOL "kf.hlktech.com" (State:Started)"#;
        assert_eq!(
            parse_state_line(pool),
            Some(("kf.hlktech.com".to_string(), "Started".to_string()))
        );
        // 中文站点名（非 ASCII）也能正确取状态
        let zh = r#"SITE "海凌科 AI客服" (id:3,bindings:http/*:80:,state:Stopped)"#;
        assert_eq!(
            parse_state_line(zh),
            Some(("海凌科 AI客服".to_string(), "Stopped".to_string()))
        );
        // 错误输出不带状态
        assert_eq!(
            parse_state_line(r#"ERROR ( message:Cannot find SITE object with identifier "x". )"#),
            None
        );
        assert_eq!(parse_state_line("some other line"), None);
    }

    #[test]
    fn parses_single_state_text() {
        let text = "SITE \"a\" (id:1,state:Started)\nSITE \"b\" (id:2,state:Stopped)\n";
        assert_eq!(parse_state_text(text, "A").as_deref(), Some("Started"));
        assert_eq!(parse_state_text(text, "b").as_deref(), Some("Stopped"));
        assert_eq!(parse_state_text(text, "c"), None);
        assert_eq!(parse_state_text("", "a"), None);
    }

    #[test]
    fn detects_empty_appcmd_document() {
        // 无匹配对象时 appcmd 的真实输出（配退出码 1）
        assert!(is_empty_appcmd_doc(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\n<appcmd>\n\n</appcmd>"
        ));
        assert!(is_empty_appcmd_doc(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><appcmd></appcmd>"
        ));
        assert!(is_empty_appcmd_doc("<appcmd/>"));
        assert!(is_empty_appcmd_doc(""));
        assert!(is_empty_appcmd_doc("   \r\n  "));
        // 有对象或错误信息 → 不是空文档
        assert!(!is_empty_appcmd_doc(
            r#"<?xml version="1.0"?><appcmd><WP WP.NAME="123" processId="123"/></appcmd>"#
        ));
        assert!(!is_empty_appcmd_doc(
            r#"<?xml version="1.0"?><appcmd><ERROR message="Cannot find APP"/></appcmd>"#
        ));
        assert!(!is_empty_appcmd_doc(
            r#"ERROR ( message:Cannot find APPPOOL object with identifier "x". )"#
        ));
    }
}
