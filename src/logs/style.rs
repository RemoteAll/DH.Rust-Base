//! 终端样式：ANSI 着色与颜色门控的统一入口。
//!
//! 门控策略：标准输出为终端且未设置 `NO_COLOR` 环境变量时启用；
//! 输出被重定向到文件/管道时自动退化为纯文本（`[`ConsoleLog::new`](super::ConsoleLog::new)` 与本模块同源）。

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

/// 颜色开关未探测
const UNKNOWN: u8 = 0;
/// 启用
const ON: u8 = 1;
/// 禁用
const OFF: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

/// 颜色是否可用（首次调用时探测并缓存；探测规则：stdout 为终端且未设置 `NO_COLOR`）。
pub fn color_enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => {
            let enabled = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            set_color_enabled(enabled);
            enabled
        }
    }
}

/// 强制指定颜色开关（如按启动参数/测试场景注入）。
pub fn set_color_enabled(enabled: bool) {
    STATE.store(if enabled { ON } else { OFF }, Ordering::Relaxed);
}

/// 用 ANSI 颜色码着色（颜色不可用时原样返回）。
pub fn paint(s: &str, code: u8) -> String {
    paint_sgr(s, &code.to_string())
}

/// 用复合 SGR 参数着色（如 `2;34` 表示暗淡蓝；颜色不可用时原样返回）。
pub fn paint_sgr(s: &str, sgr: &str) -> String {
    if color_enabled() {
        format!("\x1b[{sgr}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// 暗灰（时间戳等次要信息）
pub fn dim(s: &str) -> String {
    paint(s, 90)
}

/// 亮蓝（原始报文预览等高频行：与 SCAN 同亮度等级，色相区分）
pub fn bright_blue(s: &str) -> String {
    paint(s, 94)
}

/// 亮洋红（周期保活行，如设备心跳：与时间戳、信息青、连接绿均可区分）
pub fn bright_magenta(s: &str) -> String {
    paint(s, 95)
}

/// 红色（错误）
pub fn red(s: &str) -> String {
    paint(s, 31)
}

/// 绿色（连接/正常事件）
pub fn green(s: &str) -> String {
    paint(s, 32)
}

/// 黄色（警告）
pub fn yellow(s: &str) -> String {
    paint(s, 33)
}

/// 青色（常规信息）
pub fn cyan(s: &str) -> String {
    paint(s, 36)
}

/// 亮绿（重点数据，如扫码）
pub fn bright_green(s: &str) -> String {
    paint(s, 92)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_respects_switch() {
        set_color_enabled(false);
        assert_eq!(red("x"), "x");
        assert_eq!(bright_blue("x"), "x");
        assert_eq!(bright_magenta("x"), "x");
        assert!(!color_enabled());

        set_color_enabled(true);
        assert_eq!(red("x"), "\x1b[31mx\x1b[0m");
        assert_eq!(bright_blue("x"), "\x1b[94mx\x1b[0m");
        assert_eq!(bright_magenta("x"), "\x1b[95mx\x1b[0m");
        assert!(color_enabled());
    }
}
