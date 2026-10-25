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

/// 邮箱格式校验（对齐 C# `ValidFormatHelper.IsEmail` 的正则语义：
/// `\w[-\w.+]*@([A-Za-z0-9][-A-Za-z0-9]+\.)+[A-Za-z]{2,14}`）。
///
/// 与 C# 版本的差异：本实现要求**整个字符串**是一个邮箱（C# 用 `Regex.Matches`，
/// 会接受 `"张三 <a@b.com>"` 这类含前后缀文本的输入）；不接受首尾空白。
/// 本地部分允许 Unicode 字母/数字/下划线开头，其余允许 `-` `.` `+` 与字母数字下划线；
/// 域名每级至少 2 个字符、首字符为字母数字，顶级域为 2~14 个字母。
pub fn is_email(email: &str) -> bool {
    if email.is_empty() || email.trim() != email {
        return false;
    }
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || local.contains('@') {
        return false;
    }

    // 本地部分：首字符为 \w；其余为 \w 或 - . +
    let mut local_chars = local.chars();
    let Some(first) = local_chars.next() else {
        return false;
    };
    if !(first.is_alphanumeric() || first == '_') {
        return false;
    }
    if !local_chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+')) {
        return false;
    }

    // 域名：至少两级；除最后一级外，每级 ≥2 字符且仅字母数字/连字符（首字符为字母数字）；
    // 最后一级为 2~14 个字母。
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let (tld, rest) = labels.split_last().expect("至少两级");
    if tld.len() < 2 || tld.len() > 14 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    rest.iter().all(|label| {
        label.len() >= 2
            && label
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// 取邮箱后缀（`@` 之后的部分），对齐 C# `MailHelper.GetEmailSuffix`。
///
/// 与 C# 版本的差异：输入为空、无 `@`、`@` 在末尾、或本地部分为空时返回 `None`
/// （C# 版对前三者抛 `ArgumentException`，对空本地部分不校验）。
pub fn email_suffix(email: &str) -> Option<&str> {
    let (local, domain) = email.rsplit_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    Some(domain)
}

/// 拆分邮件地址串：按 `,` / `;` 分割并去除首尾空白（对齐 C# `EmailSenderBase.PaserMailAddress`）。
///
/// 空项自动丢弃；返回的每个元素都是非空字符串切片。
pub fn split_mail_addresses(addresses: &str) -> Vec<&str> {
    addresses
        .split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
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

    #[test]
    fn email_validation_matches_csharp_regex() {
        for ok in [
            "a@example.com",
            "user.name+tag@sub.example.com",
            "user_name-1@my-domain.cn",
            "nerd@qq.com",
        ] {
            assert!(is_email(ok), "应通过：{ok}");
        }
        for bad in [
            "",
            " plain@example.com",
            "plain@example.com ",
            "plain.example.com",
            "@example.com",
            "user@",
            "user@com",
            "user@example.c",
            "user@example.toolongtld123",
            "user@-example.com",
            "user@@example.com",
            "用户 name@example.com",
        ] {
            assert!(!is_email(bad), "应拒绝：{bad}");
        }
    }

    #[test]
    fn email_suffix_splits_after_last_at() {
        assert_eq!(email_suffix("a@example.com"), Some("example.com"));
        assert_eq!(email_suffix("weird@local@example.com"), Some("example.com"));
        assert_eq!(email_suffix("a@"), None);
        assert_eq!(email_suffix("@example.com"), None);
        assert_eq!(email_suffix("no-at"), None);
    }

    #[test]
    fn mail_addresses_split_by_comma_and_semicolon() {
        assert_eq!(
            split_mail_addresses("a@x.com; b@x.com,  c@x.com ,,"),
            vec!["a@x.com", "b@x.com", "c@x.com"]
        );
        assert!(split_mail_addresses("  ;  ").is_empty());
    }
}
