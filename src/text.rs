//! 通用文本小工具（多消费方收编：Pek.RAgent / Pek.RAdmin / DHDeploy.Agent.Rust 等）。

/// 按字符截断（附省略号"…"；不破坏 UTF-8 边界）。
///
/// 字符数不超过 `max` 时原样返回；否则取前 `max` 个字符并追加省略号。
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_and_clips_long() {
        assert_eq!(truncate_chars("ok", 5), "ok");
        assert_eq!(truncate_chars("中文测试", 2), "中文…");
        assert_eq!(truncate_chars("abcde", 5), "abcde");
        assert_eq!(truncate_chars("abcdef", 5), "abcde…");
        // UTF-8 边界安全（emoji 为多字节）
        assert_eq!(truncate_chars("🙂🙂🙂", 2), "🙂🙂…");
    }
}
