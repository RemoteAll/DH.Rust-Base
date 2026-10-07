//! 星尘（StarAgent）配置注册：把本程序注册为星尘守护代理（Pek.RAgent / C# StarAgent）的子服务，
//! 实现“安装即被星尘守护且自动拉起”。
//!
//! 星尘配置位于 `<星尘程序目录>/Config/StarAgent.config`，条目为属性形式的
//! `<ServiceInfo Name="..." FileName="..." ... />`（对齐 C# `Stardust.Models.ServiceInfo`，
//! Rust 版 Pek.RAgent 与 C# 版均可识别）。
//!
//! **固定规则（组织部署标准，有意写死、不提供选项）**：
//! - 条目固定采用 **Shadow 模式（Mode=11）**：星尘将程序复制到 `../shadow/` 运行，
//!   实际目录仅存配置与数据——升级时**直接覆盖实际目录中的二进制即自动重启生效**，
//!   运行中的文件不受占用（Windows/Linux 通用）；
//! - 固定 `ReloadOnChange="true"`：文件变动自动重启（影子升级的触发机制）。
//!
//! 编辑采用保守的“整行替换/插入”方式：除目标行外**逐字节保留**原文件
//! （兼容混合行尾、BOM、末尾无换行等情形）；写回前先在同目录留一份 `.bak`。
//!
//! 调用方（各服务 CLI 的 `-RegisterAgent` / `-UnregisterAgent`）只需
//! 提供 [`Entry`] 规格；核心逻辑由本模块统一承担（2026-10-07 自 Pek.RPanlServer
//! 与 HlktechIoT MQTT 的重复实现下沉）。

use std::path::{Path, PathBuf};

/// 星尘条目规格（渲染 `<ServiceInfo .../>` 所需的应用信息）。
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    /// 条目名（全局唯一标识，如 `PekRPanlServer`；大小写不敏感匹配已有条目）。
    pub name: &'a str,
    /// 注入的数据目录环境变量名（如 `Some("PEK_RPANL_BASE")`）→ `Environments="<名>=<实际目录>"`；
    /// `None` 表示不注入（照常输出空的 `Environments=""`，对齐 C# 属性序）。
    pub base_env: Option<&'a str>,
    /// 健康检查地址（如 `http://127.0.0.1:5502/api/ping`）；空串表示不配置。
    pub health_check: &'a str,
}

/// 注册（upsert）服务条目：同名条目整行替换，不存在则插入 `<Services>` 段内。
///
/// 程序路径取 `current_exe()`，工作目录取其父目录（星尘以工作目录为实际目录）。
pub fn register(config_path: &Path, entry: &Entry) -> Result<String, String> {
    let text = read_config(config_path)?;
    let exe = current_exe()?;
    let work = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let eol = detect_eol(&text);
    let line = render_entry(entry, &exe, &work);
    let updated = upsert_entry(&text, &line, entry.name, eol)?;
    write_config(config_path, &updated)?;

    let mut msg = format!(
        "已注册到星尘：{}\n  名称：{}\n  程序：{}\n  工作目录：{}",
        config_path.display(),
        entry.name,
        exe.display(),
        work.display(),
    );
    let health = entry.health_check.trim();
    if !health.is_empty() {
        msg.push_str(&format!("\n  健康检查：{health}"));
    }
    if let Some(env) = entry.base_env {
        msg.push_str(&format!("\n  数据目录变量：{env}={}", work.display()));
    }
    msg.push_str(
        "\n  模式：Shadow（Mode=11，影子目录运行，程序文件不被占用）+ 文件变动自动重启\n  升级方式：直接覆盖实际目录中的本程序即自动重启生效（无需停服）\n提示：注册已写入星尘配置；随后请求星尘重载并启动即可（install.sh 会自动执行），也可在星尘面板「子服务」页手动启动。",
    );
    Ok(msg)
}

/// 注销：从星尘配置移除服务条目（幂等，条目不存在也算成功）。
pub fn unregister(config_path: &Path, name: &str) -> Result<String, String> {
    let text = read_config(config_path)?;
    let (updated, removed) = remove_entry(&text, name)?;
    if removed {
        write_config(config_path, &updated)?;
        Ok(format!(
            "已从星尘注销：{}（条目 {} 已移除）",
            config_path.display(),
            name
        ))
    } else {
        Ok(format!(
            "星尘配置中没有条目 {}（无需注销）：{}",
            name,
            config_path.display()
        ))
    }
}

// ———— 内部实现 ————

fn read_config(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!("找不到星尘配置文件：{}", path.display()));
    }
    std::fs::read_to_string(path).map_err(|e| format!("读取星尘配置失败：{e}"))
}

/// 写入（先在同目录留一份 `.bak` 覆盖式备份，便于排障与回滚）。
fn write_config(path: &Path, text: &str) -> Result<(), String> {
    let bak = PathBuf::from(format!("{}.bak", path.display()));
    let _ = std::fs::copy(path, &bak);
    std::fs::write(path, text).map_err(|e| format!("写入星尘配置失败：{e}"))
}

/// 当前程序绝对路径（Windows 下去掉 canonicalize 产生的 `\\?\` 前缀）。
fn current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取程序路径：{e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let s = exe.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return Ok(PathBuf::from(rest));
    }
    Ok(exe)
}

fn detect_eol(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// 生成 ServiceInfo 条目（单行；属性序对齐 C# 声明序）。
///
/// `Mode="11"`（Shadow）：运行在影子目录，实际目录文件可随时覆盖替换；
/// `Environments` 传入调用方指定的数据目录变量，确保影子运行时 base 仍为实际目录
/// （否则影子模式下配置/数据会落到影子目录）。
fn render_entry(entry: &Entry, exe: &Path, work: &Path) -> String {
    let env_value = match entry.base_env {
        Some(name) => format!("{name}={}", work.display()),
        None => String::new(),
    };
    format!(
        "<ServiceInfo Priority=\"Normal\" Name=\"{name}\" FileName=\"{file}\" Arguments=\"\" WorkingDirectory=\"{work}\" UserName=\"\" Enable=\"true\" Mode=\"11\" AllowMultiple=\"false\" Environments=\"{env}\" AutoStop=\"false\" ReloadOnChange=\"true\" MaxMemory=\"0\" OomScoreAdjust=\"0\" HealthCheck=\"{health}\" Overwrite=\"\" Debug=\"false\" />",
        name = entry.name,
        file = escape_attr(&exe.display().to_string()),
        work = escape_attr(&work.display().to_string()),
        env = escape_attr(&env_value),
        health = escape_attr(entry.health_check.trim()),
    )
}

fn escape_attr(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 行首缩进（空格/制表符）。
fn indent_of(line: &str) -> String {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// 该行是否包含指定 Name 的 ServiceInfo 条目（属性值大小写不敏感）。
fn entry_line_matches(line: &str, name: &str) -> bool {
    if !line.contains("<ServiceInfo") {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    lower.contains(&format!("name=\"{n}\"")) || lower.contains(&format!("name = \"{n}\""))
}

/// 行内容（去掉行尾 CR/LF）。
fn line_core(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

/// 行的原始行尾（`\r\n` / `\n` / 空）。
fn line_ending(line: &str) -> &str {
    &line[line_core(line).len()..]
}

/// upsert：找到 `<Services>` 段，将同名条目整行替换；不存在则插入。
///
/// 采用“按行切片（保留各自行尾）+ 拼接”方式：除目标行外**逐字节保留**原文件。
fn upsert_entry(text: &str, entry: &str, name: &str, eol: &str) -> Result<String, String> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(String::from).collect();
    let (open_idx, close_idx) = find_services(&lines)?;

    if open_idx == close_idx {
        // 单行段（`<Services></Services>`）：展开为多行，保留原行尾与行尾后缀
        let core = line_core(&lines[open_idx]).to_string();
        let ending = line_ending(&lines[open_idx]).to_string();
        let indent = indent_of(&core);
        let after = core
            .find("</Services>")
            .map(|i| core[i + "</Services>".len()..].to_string())
            .unwrap_or_default();
        let pieces = vec![
            format!("{indent}<Services>{eol}"),
            format!("{indent}  {entry}{eol}"),
            format!("{indent}</Services>{after}{ending}"),
        ];
        lines.splice(open_idx..=open_idx, pieces);
    } else {
        let mut target: Option<usize> = None;
        let mut first_svc: Option<usize> = None;
        for i in (open_idx + 1)..close_idx {
            if line_core(&lines[i]).contains("<ServiceInfo") {
                if first_svc.is_none() {
                    first_svc = Some(i);
                }
                if entry_line_matches(&lines[i], name) {
                    target = Some(i);
                    break;
                }
            }
        }
        match target {
            Some(i) => {
                // 整行替换（保留缩进与原有行尾）
                let indent = indent_of(line_core(&lines[i]));
                let ending = line_ending(&lines[i]);
                let ending = if ending.is_empty() { eol } else { ending };
                lines[i] = format!("{indent}{entry}{ending}");
            }
            None => {
                // 插入到 </Services> 之前（缩进继承段内首条目，空段则基于 <Services> 缩进 +2）
                let indent = match first_svc {
                    Some(fi) => indent_of(line_core(&lines[fi])).to_string(),
                    None => format!("{}  ", indent_of(line_core(&lines[open_idx]))),
                };
                lines.insert(close_idx, format!("{indent}{entry}{eol}"));
            }
        }
    }

    Ok(lines.concat())
}

/// 移除同名条目；返回（新文本, 是否移除）。除目标行外逐字节保留。
fn remove_entry(text: &str, name: &str) -> Result<(String, bool), String> {
    let lines: Vec<String> = text.split_inclusive('\n').map(String::from).collect();
    let (open_idx, close_idx) = find_services(&lines)?;
    if open_idx == close_idx {
        return Ok((text.to_string(), false));
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut removed = false;
    for (i, line) in lines.iter().enumerate() {
        if !removed && i > open_idx && i < close_idx && entry_line_matches(line, name) {
            removed = true;
            continue;
        }
        out.push(line.clone());
    }
    if removed {
        Ok((out.concat(), true))
    } else {
        Ok((text.to_string(), false))
    }
}

/// 定位 `<Services>` 与 `</Services>` 所在行；单行空段时两者相同。
fn find_services(lines: &[String]) -> Result<(usize, usize), String> {
    let mut open_idx: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if open_idx.is_none() {
            if t.starts_with("<Services") {
                open_idx = Some(i);
                if t.contains("</Services>") {
                    return Ok((i, i));
                }
            }
        } else if t.starts_with("</Services") {
            return Ok((open_idx.unwrap(), i));
        }
    }
    match open_idx {
        Some(i) => Err(format!("星尘配置第 {} 行有 <Services> 但缺少 </Services>", i + 1)),
        None => Err("星尘配置中找不到 <Services> 段".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<StarAgent>\n  <Debug>false</Debug>\n  <Services>\n    <ServiceInfo Priority=\"High\" Name=\"test\" FileName=\"ping\" Enable=\"false\" />\n  </Services>\n  <ServiceName>StarAgentRust</ServiceName>\n</StarAgent>\n";

    fn spec() -> Entry<'static> {
        Entry {
            name: "DemoSvc",
            base_env: Some("DEMO_BASE"),
            health_check: "",
        }
    }

    /// 完整合法的条目（含 Name 属性），供替换/删除匹配用。
    fn sample_entry(file: &str) -> String {
        format!("<ServiceInfo Priority=\"Normal\" Name=\"DemoSvc\" FileName=\"{file}\" Enable=\"true\" />")
    }

    #[test]
    fn append_after_existing_entry() {
        let e = sample_entry("x.exe");
        let out = upsert_entry(SAMPLE, &e, "DemoSvc", "\n").unwrap();
        assert!(out.contains("<ServiceInfo Priority=\"High\" Name=\"test\""));
        assert_eq!(out.matches("x.exe").count(), 1);
        // 新条目在 </Services> 之前、test 条目之后
        let a = out.find("Name=\"test\"").unwrap();
        let b = out.find("x.exe").unwrap();
        let c = out.find("</Services>").unwrap();
        assert!(a < b && b < c, "顺序错误：\n{out}");
        // 其余内容不受影响
        assert!(out.contains("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(out.contains("<ServiceName>StarAgentRust</ServiceName>"));
    }

    #[test]
    fn replace_existing_same_name_is_idempotent() {
        let a = sample_entry("old.exe");
        let b = sample_entry("new.exe");
        let one = upsert_entry(SAMPLE, &a, "DemoSvc", "\n").unwrap();
        let two = upsert_entry(&one, &b, "DemoSvc", "\n").unwrap();
        assert_eq!(two.matches("new.exe").count(), 1);
        assert_eq!(two.matches("old.exe").count(), 0);
        assert_eq!(two.matches("<ServiceInfo").count(), 2, "应只有 test + 服务两条：\n{two}");
    }

    #[test]
    fn empty_single_line_expands() {
        let text = "<Root>\n  <Services></Services>\n</Root>";
        let out = upsert_entry(text, "ENTRY-X", "DemoSvc", "\n").unwrap();
        assert!(out.contains("  <Services>\n    ENTRY-X\n  </Services>"), "{out}");
    }

    #[test]
    fn empty_multiline_inserts() {
        let text = "<Root>\n  <Services>\n  </Services>\n</Root>";
        let out = upsert_entry(text, "ENTRY-X", "DemoSvc", "\n").unwrap();
        assert!(out.contains("  <Services>\n    ENTRY-X\n  </Services>"), "{out}");
    }

    #[test]
    fn case_insensitive_name_match() {
        let text = "<Root>\n  <Services>\n    <ServiceInfo Name=\"demosvc\" FileName=\"old\" />\n  </Services>\n</Root>";
        let e = sample_entry("new.exe");
        let out = upsert_entry(text, &e, "DemoSvc", "\n").unwrap();
        assert_eq!(out.matches("<ServiceInfo").count(), 1);
        assert!(out.contains("new.exe"));
        assert!(!out.contains("FileName=\"old\""));
    }

    #[test]
    fn remove_keeps_others() {
        let e = sample_entry("x.exe");
        let one = upsert_entry(SAMPLE, &e, "DemoSvc", "\n").unwrap();
        let (two, removed) = remove_entry(&one, "DemoSvc").unwrap();
        assert!(removed);
        assert_eq!(two.matches("<ServiceInfo").count(), 1);
        assert!(two.contains("Name=\"test\""));
        assert_eq!(two, SAMPLE, "注销后应与原始文本逐字节一致");
        // 幂等：再删一次
        let (three, removed2) = remove_entry(&two, "DemoSvc").unwrap();
        assert!(!removed2);
        assert_eq!(three, two);
    }

    #[test]
    fn crlf_style_preserved() {
        let text = SAMPLE.replace('\n', "\r\n");
        let e = sample_entry("x.exe");
        let out = upsert_entry(&text, &e, "DemoSvc", "\r\n").unwrap();
        assert!(out.contains("\r\n"));
        let lone_lf = out.replace("\r\n", "").matches('\n').count();
        assert_eq!(lone_lf, 0, "不应出现孤立 LF：\n{out:?}");
        let (removed_text, yes) = remove_entry(&out, "DemoSvc").unwrap();
        assert!(yes);
        assert_eq!(removed_text, text, "注销后应与原始文本逐字节一致");
    }

    #[test]
    fn unregister_restores_bytes_with_mixed_eol() {
        // 模拟真实环境：文件混有 LF/CRLF（星尘多次改写后可能出现），未动行必须逐字节保留
        let text = "<?xml version=\"1.0\"?>\n<Root>\r\n  <Services>\n    <ServiceInfo Name=\"keep\" FileName=\"k\" />\r\n  </Services>\n</Root>";
        let e = sample_entry("x.exe");
        let one = upsert_entry(text, &e, "DemoSvc", "\n").unwrap();
        assert!(one.contains("x.exe"));
        let (two, removed) = remove_entry(&one, "DemoSvc").unwrap();
        assert!(removed);
        assert_eq!(two, text, "注销后应与原始文本逐字节一致（混合行尾）");
    }

    #[test]
    fn missing_services_section_fails() {
        let err = upsert_entry("<Root>\n  <Debug>false</Debug>\n</Root>", "E", "DemoSvc", "\n").unwrap_err();
        assert!(err.contains("找不到"), "{err}");
        let err2 = remove_entry("<Root/>", "DemoSvc").unwrap_err();
        assert!(err2.contains("找不到"), "{err2}");
    }

    #[test]
    fn render_entry_fixed_shadow_rule_and_escaping() {
        let e = render_entry(
            &spec(),
            Path::new(r"C:\a&b\demo.exe"),
            Path::new(r"C:\a&b"),
        );
        assert!(e.contains("Name=\"DemoSvc\""));
        assert!(e.contains("FileName=\"C:\\a&amp;b\\demo.exe\""));
        assert!(e.contains("Mode=\"11\""), "固定影子模式");
        assert!(e.contains("Environments=\"DEMO_BASE=C:\\a&amp;b\""), "固定注入数据目录环境变量");
        assert!(e.contains("ReloadOnChange=\"true\""), "固定文件变动自动重启");
        assert!(e.contains("HealthCheck=\"\""), "未配置健康检查时为空串");
    }

    #[test]
    fn render_entry_with_health_check() {
        let with_health = Entry {
            name: "PekDemo",
            base_env: Some("DEMO_BASE"),
            health_check: "http://127.0.0.1:5502/api/ping",
        };
        let e = render_entry(&with_health, Path::new("/opt/demo"), Path::new("/opt/demo"));
        assert!(e.contains("HealthCheck=\"http://127.0.0.1:5502/api/ping\""));
        assert!(e.contains("Environments=\"DEMO_BASE=/opt/demo\""));
        // 不注入环境变量时输出空属性（保持属性序）
        let no_env = Entry {
            name: "PekDemo",
            base_env: None,
            health_check: "",
        };
        let e2 = render_entry(&no_env, Path::new("/opt/demo"), Path::new("/opt/demo"));
        assert!(e2.contains("Environments=\"\""));
    }
}
