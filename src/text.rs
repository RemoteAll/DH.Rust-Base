//! 通用文本小工具（多消费方收编：Pek.RAgent / Pek.RAdmin / DHDeploy.Agent.Rust 等）。

use std::collections::HashSet;

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

/// 手机号拆分：`,` `;` `|` `，` `；` 分隔，去空白、去重（保持首次出现顺序）。
pub fn split_phone_numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in text.split([',', ';', '|', '，', '；']) {
        let number = part.trim();
        if number.is_empty() {
            continue;
        }
        let owned = number.to_string();
        if seen.insert(owned.clone()) {
            out.push(owned);
        }
    }
    out
}

/// 按 `{%key%}` 风格占位符单次扫描替换（未命中的占位符原样保留）。
///
/// `open`/`close` 由调用方指定（如 `"{%"` / `"%}"`）；`pairs` 保持调用方顺序，
/// 冲突时以先出现者为准。相比“逐个 `String::replace`”，本实现只扫描一遍正文，
/// 参数多、正文长时分配和扫描次数更少。
pub fn replace_named<K, V>(text: &str, pairs: &[(K, V)], open: &str, close: &str) -> String
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    if pairs.is_empty() || open.is_empty() || close.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after_open[..end];
        if let Some((_, value)) = pairs.iter().find(|(k, _)| k.as_ref() == key) {
            out.push_str(value.as_ref());
        } else {
            out.push_str(&rest[start..start + open.len() + end + close.len()]);
        }
        rest = &after_open[end + close.len()..];
    }
    out.push_str(rest);
    out
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

    #[test]
    fn phone_numbers_split_and_dedupe_in_order() {
        assert_eq!(
            split_phone_numbers("13800138000, 13800138000；13900139000|13800138000"),
            vec!["13800138000", "13900139000"]
        );
        assert!(split_phone_numbers("  ; , ").is_empty());
    }

    #[test]
    fn replace_named_single_pass_and_keeps_unknown() {
        let pairs = [("code", "123456"), ("minutes", "5")];
        assert_eq!(
            replace_named(
                "验证码 {%code%}，{%minutes%} 分钟；{%unknown%}",
                &pairs,
                "{%",
                "%}"
            ),
            "验证码 123456，5 分钟；{%unknown%}"
        );
        assert_eq!(
            replace_named("没有占位符", &pairs, "{%", "%}"),
            "没有占位符"
        );
        // 没有闭合符时原样保留剩余文本
        assert_eq!(replace_named("前 {%code", &pairs, "{%", "%}"), "前 {%code");
    }
}
