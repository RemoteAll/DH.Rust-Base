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
pub fn workers_of_pool(pool: &str) -> Result<Vec<u32>, String> {
    #[cfg(windows)]
    {
        let pool = pool.trim();
        if pool.is_empty() {
            return Ok(Vec::new());
        }
        let filter = format!("/apppool.name:{pool}");
        let out = appcmd(&["list", "wps", &filter, "/xml"], APPCMD_TIMEOUT)?;
        let mut pids = Vec::new();
        if let Ok(value) = crate::config::read_xml_to_json(&out) {
            collect_process_ids(&value, &mut pids);
        }
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }
    #[cfg(not(windows))]
    {
        let _ = pool;
        Err("仅 Windows/IIS 支持此操作".to_string())
    }
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
    let site = site.trim();
    if site.is_empty() {
        return Err("站点名为空".to_string());
    }
    if state_is(&site_states(), site, "Stopped") {
        return Ok(());
    }
    let result = appcmd(&["stop", "site", site], timeout).map(|_| ());
    invalidate_cache();
    if result.is_err() && state_is(&site_states(), site, "Stopped") {
        return Ok(());
    }
    result
}

/// 启站点（幂等）。
pub fn start_site(site: &str, timeout: Duration) -> Result<(), String> {
    let site = site.trim();
    if site.is_empty() {
        return Err("站点名为空".to_string());
    }
    if state_is(&site_states(), site, "Started") {
        return Ok(());
    }
    let result = appcmd(&["start", "site", site], timeout).map(|_| ());
    invalidate_cache();
    if result.is_err() && state_is(&site_states(), site, "Started") {
        return Ok(());
    }
    result
}

/// 停应用池（幂等）。共享池会波及其它站点，调用方需自行提示。
pub fn stop_app_pool(pool: &str, timeout: Duration) -> Result<(), String> {
    let pool = pool.trim();
    if pool.is_empty() {
        return Err("应用池名为空".to_string());
    }
    if state_is(&app_pool_states(), pool, "Stopped") {
        return Ok(());
    }
    let result = appcmd(&["stop", "apppool", pool], timeout).map(|_| ());
    invalidate_cache();
    if result.is_err() && state_is(&app_pool_states(), pool, "Stopped") {
        return Ok(());
    }
    result
}

/// 启应用池（幂等）。
pub fn start_app_pool(pool: &str, timeout: Duration) -> Result<(), String> {
    let pool = pool.trim();
    if pool.is_empty() {
        return Err("应用池名为空".to_string());
    }
    if state_is(&app_pool_states(), pool, "Started") {
        return Ok(());
    }
    let result = appcmd(&["start", "apppool", pool], timeout).map(|_| ());
    invalidate_cache();
    if result.is_err() && state_is(&app_pool_states(), pool, "Started") {
        return Ok(());
    }
    result
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
        if r.exit_code != 0 {
            let msg = if r.stderr.trim().is_empty() {
                r.stdout
            } else {
                r.stderr
            };
            return Err(format!(
                "appcmd 执行失败（退出码 {}）：{}",
                r.exit_code,
                msg.trim()
            ));
        }
        Ok(r.stdout)
    }
    #[cfg(not(windows))]
    {
        let _ = (args, timeout);
        Err("仅 Windows/IIS 支持此操作".to_string())
    }
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
        let line = line.trim();
        let Some(rest) = line.strip_prefix("SITE ") else {
            continue;
        };
        let Some(open) = rest.find('"') else {
            continue;
        };
        let rest = &rest[open + 1..];
        let Some(close) = rest.find('"') else {
            continue;
        };
        let name = &rest[..close];
        let tail = &rest[close + 1..];
        let Some(pos) = tail.rfind("state:") else {
            continue;
        };
        let state = tail[pos + 6..]
            .split([',', ')'])
            .next()
            .unwrap_or("")
            .trim();
        if !name.is_empty() && !state.is_empty() {
            out.insert(name.to_string(), state.to_string());
        }
    }
    out
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

fn state_is(states: &HashMap<String, String>, name: &str, want: &str) -> bool {
    states
        .get(name)
        .map(|s| s.eq_ignore_ascii_case(want))
        .unwrap_or(false)
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
}
