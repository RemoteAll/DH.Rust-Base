//! Linux init 系统探测与脚本模板（纯逻辑：无副作用、可在任意平台编译与单测）。
//!
//! 覆盖三类部署环境：
//! - **systemd**：主流发行版（含麒麟/统信/openEuler）；
//! - **procd**：OpenWrt（路由器/IoT 网关）；
//! - **SysVinit / OpenRC**：`/etc/init.d` 体系（老发行版 / Alpine 等）。

use super::ServiceManager;

/// init 系统种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitKind {
    /// systemd（systemctl）
    Systemd,
    /// OpenWrt procd（rc.common）
    Procd,
    /// SysVinit / OpenRC（init.d + update-rc.d/chkconfig/rc-update）
    Sysv,
    /// 未知
    Unknown,
}

impl InitKind {
    /// 中文文本。
    pub fn text(self) -> &'static str {
        match self {
            InitKind::Systemd => "systemd",
            InitKind::Procd => "procd（OpenWrt）",
            InitKind::Sysv => "SysVinit/OpenRC",
            InitKind::Unknown => "未知",
        }
    }
}

/// 探测决策（纯函数，便于单测）。
///
/// 优先级：
/// 1. 环境变量 `PEK_RAGENT_INIT`（systemd/procd/sysv；大小写不敏感，无效值忽略）；
/// 2. systemd 已启动（`/run/systemd/system` 存在，sd_booted 语义）；
/// 3. OpenWrt 特征（`/etc/rc.common` 或 `/sbin/procd` 存在）→ procd；
/// 4. `/etc/init.d` 存在 → SysVinit/OpenRC；
/// 5. 都不满足 → Unknown。
pub fn decide(
    override_kind: Option<&str>,
    systemd_booted: bool,
    rc_common: bool,
    procd_bin: bool,
    initd_dir: bool,
) -> InitKind {
    if let Some(v) = override_kind {
        match v.trim().to_ascii_lowercase().as_str() {
            "systemd" => return InitKind::Systemd,
            "procd" => return InitKind::Procd,
            "sysv" | "openrc" | "initd" => return InitKind::Sysv,
            _ => {} // 无效值：忽略，按自动探测
        }
    }

    if systemd_booted {
        return InitKind::Systemd;
    }
    if rc_common || procd_bin {
        return InitKind::Procd;
    }
    if initd_dir {
        return InitKind::Sysv;
    }
    InitKind::Unknown
}

/// systemd 单元文件内容。
///
/// **保守原则**（真实发行版首验后收紧）：仅保留最标准的指令与 ASCII 内容；
/// `KillMode=process` 必须保留（停止时不误杀子应用，对齐 C# StarAgent 语义）。
///
/// **坑（真实首验踩过）**：`WorkingDirectory=` 的值**不能加引号**——systemd 不剥离引号，
/// 会把 `"/www/Agent"` 当作路径本体并判为“不是绝对路径”（fatal error，整个单元拒启）；
/// 而 `ExecStart=` 支持引号且路径含空格时需要引号（按需加）。
pub fn systemd_unit_text(mgr: &ServiceManager) -> String {
    format!(
        "[Unit]\n\
Description={name}\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={workdir}\n\
ExecStart={exe} -s\n\
Restart=always\n\
RestartSec=5\n\
KillMode=process\n\
LimitNOFILE=65535\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        name = mgr.name,
        workdir = mgr.base.display(), // 裸写（WorkingDirectory 不能引号）
        exe = quote_path(&mgr.exe.display().to_string()),
    )
}

/// 路径引号（仅含空白时加引号）。
///
/// 仅适用于**支持引号剥离**的指令（如 `ExecStart=`）；
/// `WorkingDirectory=` 不能加引号（systemd 会把引号当作路径本体 → fatal）。
pub fn quote_path(path: &str) -> String {
    if path.chars().any(|c| c.is_whitespace()) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// 控制字符可视化（排查单元文件里“看不见的问题”）：`\r`/NUL/BOM/其它控制符转义显示。
pub fn visualize_invisibles(text: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\u{feff}' => out.push_str("\\u{feff}(BOM)"),
            c if c.is_control() && c != '\n' => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
        if out.len() >= max {
            out.push_str("\n…（截断）");
            break;
        }
    }
    out
}

/// 截断到约 `max` 字节（按字符边界回退），超出追加省略标记。
pub fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…（截断）", &text[..end])
}

/// 取文本尾部 `n` 行。
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// 解析 systemd 单元文件文本中的 `ExecStart=` 程序路径（与 [`systemd_unit_text`] 生成格式对称）。
///
/// 支持 `ExecStart="/path/exe" -s` 与 `ExecStart=/path/exe -s` 两种形态。
pub fn parse_exec_start(text: &str) -> Option<std::path::PathBuf> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("ExecStart=") else {
            continue;
        };
        let rest = rest.trim();
        let path = if let Some(stripped) = rest.strip_prefix('"') {
            stripped.split('"').next().unwrap_or("")
        } else {
            rest.split_whitespace().next().unwrap_or("")
        };
        if !path.is_empty() {
            return Some(std::path::PathBuf::from(path));
        }
    }
    None
}

/// procd（OpenWrt）init 脚本内容。
pub fn procd_script_text(mgr: &ServiceManager) -> String {
    PROCD_TEMPLATE.replace("@@EXE@@", &mgr.exe.display().to_string())
}

/// SysVinit / OpenRC init 脚本内容（LSB 头 + start/stop/restart/status）。
pub fn sysv_script_text(mgr: &ServiceManager) -> String {
    SYSV_TEMPLATE
        .replace("@@NAME@@", &mgr.name)
        .replace("@@DISPLAY@@", &mgr.display)
        .replace("@@EXE@@", &mgr.exe.display().to_string())
        .replace("@@BASE@@", &mgr.base.display().to_string())
}

/// procd 脚本模板（占位符：`@@EXE@@`）。
const PROCD_TEMPLATE: &str = r#"#!/bin/sh /etc/rc.common

START=95
STOP=95
USE_PROCD=1

start_service() {
    procd_open_instance
    procd_set_param command "@@EXE@@" -s
    procd_set_param respawn
    procd_set_param stdout 1
    procd_set_param stderr 1
    procd_close_instance
}
"#;

/// SysVinit / OpenRC 脚本模板（占位符：`@@NAME@@`/`@@DISPLAY@@`/`@@EXE@@`/`@@BASE@@`）。
const SYSV_TEMPLATE: &str = r#"#!/bin/sh
### BEGIN INIT INFO
# Provides:          @@NAME@@
# Required-Start:    $network
# Required-Stop:     $network
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: @@DISPLAY@@
### END INIT INFO

NAME="@@NAME@@"
DAEMON="@@EXE@@"
DAEMON_ARGS="-s"
PIDFILE="/var/run/$NAME.pid"

is_running() {
    [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null
}

case "$1" in
    start)
        if is_running; then
            echo "$NAME already running"
            exit 0
        fi
        echo "Starting $NAME ..."
        cd "@@BASE@@" || exit 1
        nohup "$DAEMON" $DAEMON_ARGS >/dev/null 2>&1 &
        echo $! > "$PIDFILE"
        ;;
    stop)
        if ! is_running; then
            echo "$NAME not running"
            exit 0
        fi
        echo "Stopping $NAME ..."
        kill "$(cat "$PIDFILE")" 2>/dev/null
        rm -f "$PIDFILE"
        ;;
    restart)
        "$0" stop
        sleep 1
        "$0" start
        ;;
    status)
        if is_running; then
            echo "$NAME running"
            exit 0
        else
            echo "$NAME not running"
            exit 3
        fi
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mgr() -> ServiceManager {
        ServiceManager {
            name: "staragent".to_string(),
            display: "星尘代理".to_string(),
            description: "测试".to_string(),
            exe: PathBuf::from("/opt/staragent/pek-ragent"),
            base: PathBuf::from("/opt/staragent"),
            legacy: Vec::new(),
        }
    }

    #[test]
    fn decide_auto_detect_order() {
        // systemd 已启动优先
        assert_eq!(decide(None, true, true, true, true), InitKind::Systemd);
        // OpenWrt 两个特征任一命中
        assert_eq!(decide(None, false, true, false, true), InitKind::Procd);
        assert_eq!(decide(None, false, false, true, true), InitKind::Procd);
        // init.d 兜底
        assert_eq!(decide(None, false, false, false, true), InitKind::Sysv);
        // 全无 → 未知
        assert_eq!(decide(None, false, false, false, false), InitKind::Unknown);
    }

    #[test]
    fn decide_override_wins() {
        assert_eq!(decide(Some("procd"), true, false, false, false), InitKind::Procd);
        assert_eq!(decide(Some(" SysV "), false, false, false, false), InitKind::Sysv);
        assert_eq!(decide(Some("systemd"), false, false, false, true), InitKind::Systemd);
        // 无效值 → 忽略并自动探测
        assert_eq!(decide(Some("bogus"), false, false, false, true), InitKind::Sysv);
    }

    #[test]
    fn procd_script_contains_key_items() {
        let text = procd_script_text(&mgr());
        assert!(text.starts_with("#!/bin/sh /etc/rc.common"));
        assert!(text.contains("USE_PROCD=1"));
        assert!(text.contains("procd_set_param command \"/opt/staragent/pek-ragent\" -s"));
        assert!(text.contains("procd_set_param respawn"));
    }

    #[test]
    fn sysv_script_contains_key_items() {
        let text = sysv_script_text(&mgr());
        assert!(text.starts_with("#!/bin/sh"));
        assert!(text.contains("### BEGIN INIT INFO"));
        assert!(text.contains("Provides:") && text.contains("staragent"));
        // PIDFILE 使用脚本内 $NAME 变量（运行时展开）
        assert!(text.contains("PIDFILE=\"/var/run/$NAME.pid\""));
        assert!(text.contains("DAEMON=\"/opt/staragent/pek-ragent\""));
        assert!(text.contains("start|stop|restart|status"));
        assert!(text.contains("cd \"/opt/staragent\""));
    }

    #[test]
    fn systemd_unit_contains_key_items() {
        let text = systemd_unit_text(&mgr());
        assert!(text.contains("[Unit]"));
        assert!(text.contains("ExecStart=/opt/staragent/pek-ragent -s"));
        assert!(text.contains("WorkingDirectory=/opt/staragent"));
        assert!(text.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn systemd_unit_full_text_is_stable() {
        // 全量断言：任何模板改动必须显式更新本测试（服务安装真实首验翻车过，格式变更应当被看见）
        let expected = "[Unit]\n\
Description=staragent\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory=/opt/staragent\n\
ExecStart=/opt/staragent/pek-ragent -s\n\
Restart=always\n\
RestartSec=5\n\
KillMode=process\n\
LimitNOFILE=65535\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n";
        assert_eq!(systemd_unit_text(&mgr()), expected);
        // 行尾必须是 LF，不得含隐式回车（systemd 对 \r 敏感）；且保持纯 ASCII（最大兼容）
        let text = systemd_unit_text(&mgr());
        assert!(!text.contains('\r'));
        assert!(text.is_ascii(), "单元文件应保持 ASCII：{text}");
    }

    #[test]
    fn quote_path_only_when_needed() {
        assert_eq!(quote_path("/opt/staragent/pek-ragent"), "/opt/staragent/pek-ragent");
        assert_eq!(quote_path("/opt/my agent/app"), "\"/opt/my agent/app\"");
    }

    #[test]
    fn unit_with_spaced_path_quotes_only_execstart() {
        // 真实首验教训：WorkingDirectory 不能加引号（会被判“不是绝对路径”）；
        // ExecStart 含空格时需要引号
        let mgr = ServiceManager {
            name: "staragent".to_string(),
            display: "星尘代理".to_string(),
            description: "测试".to_string(),
            exe: PathBuf::from("/opt/my agent/pek-ragent"),
            base: PathBuf::from("/opt/my agent"),
            legacy: Vec::new(),
        };
        let text = systemd_unit_text(&mgr);
        assert!(
            text.contains("WorkingDirectory=/opt/my agent\n"),
            "WorkingDirectory 必须裸写：{text}"
        );
        assert!(
            !text.contains("WorkingDirectory=\""),
            "WorkingDirectory 不得出现引号：{text}"
        );
        assert!(
            text.contains("ExecStart=\"/opt/my agent/pek-ragent\" -s"),
            "ExecStart 含空格路径需要引号：{text}"
        );
    }

    #[test]
    fn diagnose_helpers() {
        // 控制字符可视化：\r/ NUL / BOM 转义；换行保留
        let text = "A\r\nB\u{feff}C\0D";
        let out = visualize_invisibles(text, 1000);
        assert!(out.contains("A\\r"), "{out}");
        assert!(out.contains("B\\u{feff}(BOM)"), "{out}");
        assert!(out.contains("C\\0D"), "{out}");
        assert!(out.contains('\n'));

        // 截断（含 UTF-8 多字节边界安全）
        assert_eq!(clip("short", 100), "short");
        let c = clip(&"x".repeat(50), 20);
        assert!(c.starts_with(&"x".repeat(20)) && c.contains("截断"));
        let zh = "中".repeat(20);
        let c2 = clip(&zh, 10); // 10 非 3 的倍数 → 回退到 9 字节
        assert!(c2.starts_with(&"中".repeat(3)), "{c2}");

        // 尾部行
        let text = "1\n2\n3\n4\n5";
        assert_eq!(tail_lines(text, 2), "4\n5");
        assert_eq!(tail_lines(text, 99), text);
    }

    #[test]
    fn parse_exec_start_roundtrip() {
        // 与 unit 文本生成对称：生成后解析应还原程序路径
        let text = systemd_unit_text(&mgr());
        assert_eq!(
            parse_exec_start(&text),
            Some(std::path::PathBuf::from("/opt/staragent/pek-ragent"))
        );
        assert_eq!(
            parse_exec_start("ExecStart=/usr/local/bin/app -s\n"),
            Some(std::path::PathBuf::from("/usr/local/bin/app"))
        );
        assert_eq!(parse_exec_start("[Unit]\nDescription=x\n"), None);
    }

    #[test]
    fn kind_text_display() {
        assert_eq!(InitKind::Systemd.text(), "systemd");
        assert_eq!(InitKind::Procd.text(), "procd（OpenWrt）");
        assert_eq!(InitKind::Sysv.text(), "SysVinit/OpenRC");
        assert_eq!(InitKind::Unknown.text(), "未知");
    }
}
