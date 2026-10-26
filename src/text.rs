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

/// 中国大陆手机号：忽略首尾空白，11 位且全为数字、以 `1` 开头。
///
/// （HLZX.EIP 的账号体系下沉；只做格式判断，不校验号段真实性。）
pub fn is_mobile_cn(text: &str) -> bool {
    let t = text.trim();
    t.len() == 11 && t.chars().all(|c| c.is_ascii_digit()) && t.starts_with('1')
}

/// 手机号脱敏（日志/提示用）：11 位时保留前 3 后 4（`183****5593`），其它原样返回。
pub fn mask_mobile(text: &str) -> String {
    let t = text.trim();
    if t.len() == 11 && t.chars().all(|c| c.is_ascii_digit()) {
        // 已确认是 11 位 ASCII 数字，按字节切安全
        format!("{}****{}", &t[..3], &t[7..])
    } else {
        t.to_string()
    }
}

/// 邮箱脱敏：本地部分保留前 2 位（`ab***@example.com`）；本地部分过短或无 `@` 时原样返回。
pub fn mask_email(text: &str) -> String {
    let t = text.trim();
    match t.split_once('@') {
        Some((user, domain)) if user.chars().count() > 2 => {
            let head: String = user.chars().take(2).collect();
            format!("{head}***@{domain}")
        }
        _ => t.to_string(),
    }
}

/// 宽松邮箱校验：有 `@`、域名含点、无空白，总长 ≥6。
///
/// 用于「注册/登录」这类先放行、真伪靠验证邮件或人工审核的场景；
/// 需要严格口径（各级长度/字符集）时用 [`is_email`]。
pub fn is_email_lenient(text: &str) -> bool {
    let t = text.trim();
    if t.chars().count() < 6 || t.contains(char::is_whitespace) {
        return false;
    }
    match t.split_once('@') {
        Some((user, domain)) => {
            !user.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    }
}

/// 连接串脱敏（**日志/展示**用）：把 `pwd` / `password` / `user password` 的值替换成 `***`，
/// 其余键值原样保留（键名大小写不敏感）。
///
/// 与「白名单摘要」的区别：本函数**不丢弃**非敏感键，适合排查问题时打日志；
/// 需要只暴露少数键时（如列表展示）由调用方自行挑选键。
pub fn mask_connection_string(conn: &str) -> String {
    conn.split(';')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let key = p.split('=').next().unwrap_or_default().trim();
            if key.eq_ignore_ascii_case("pwd")
                || key.eq_ignore_ascii_case("password")
                || key.eq_ignore_ascii_case("user password")
            {
                format!("{key}=***")
            } else {
                p.trim().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// 解析智能布尔文本（大小写不敏感）：
/// `true/1/yes/on/是` → `true`，`false/0/no/off/否` → `false`，其余 `None`。
///
/// 供 JSON 宽松取值（`JsonValue::to_bool` / `value_to_bool`）与内存数据库字段值共用。
pub fn parse_bool(text: &str) -> Option<bool> {
    let text = text.trim();
    if text == "是" {
        return Some(true);
    }
    if text == "否" {
        return Some(false);
    }
    if ["true", "1", "yes", "on"]
        .iter()
        .any(|value| text.eq_ignore_ascii_case(value))
    {
        Some(true)
    } else if ["false", "0", "no", "off"]
        .iter()
        .any(|value| text.eq_ignore_ascii_case(value))
    {
        Some(false)
    } else {
        None
    }
}

/// CSV 单元格转义（含逗号/引号/换行时加引号并双写引号）。
///
/// **防公式注入**：Excel/WPS 会把以 `= + - @`、制表符/回车开头的单元格当公式执行，
/// 导出前统一加一个前导单引号（CSV 里显式为文本）。
///
/// 面板「导出 CSV」（审计、日志、列表）都应走它，避免各项目各写一份、漏掉注入防护。
pub fn csv_cell(value: &str) -> String {
    // 只跳过普通空格再判断（不能 `trim_start()`：它会把 `\t`、`\r` 一起吃掉，
    // 而制表符/回车开头的单元格同样会被 Excel 当公式执行）
    let probe = value.trim_start_matches(' ');
    let needs_formula_guard = probe.starts_with(['=', '+', '-', '@', '\t', '\r']);
    let text = if needs_formula_guard {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

/// 把一行字段拼成 CSV 行（每个字段走 [`csv_cell`]；行尾不加换行）。
pub fn csv_row<I, S>(fields: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    fields
        .into_iter()
        .map(|f| csv_cell(f.as_ref()))
        .collect::<Vec<_>>()
        .join(",")
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
    fn parse_bool_accepts_smart_values() {
        for yes in ["true", "TRUE", "1", "yes", "On", " 是 "] {
            assert_eq!(parse_bool(yes), Some(true), "应识别为 true：{yes}");
        }
        for no in ["false", "FALSE", "0", "no", "Off", " 否 "] {
            assert_eq!(parse_bool(no), Some(false), "应识别为 false：{no}");
        }
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
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

    /// 手机号/邮箱：格式判断 + 脱敏（HLZX.EIP 账号体系下沉）。
    #[test]
    fn mobile_and_email_helpers() {
        assert!(is_mobile_cn("18307555593"));
        assert!(is_mobile_cn(" 18307555593 "), "忽略首尾空白");
        assert!(!is_mobile_cn("1830755559"), "10 位不接受");
        assert!(!is_mobile_cn("28307555593"), "不以 1 开头");
        assert!(!is_mobile_cn("1830755559a"));

        assert_eq!(mask_mobile("18307555593"), "183****5593");
        assert_eq!(mask_mobile("12345"), "12345", "非 11 位原样");
        assert_eq!(mask_mobile(""), "");

        assert_eq!(mask_email("berin@hlktech.com"), "be***@hlktech.com");
        assert_eq!(mask_email("ab@x.com"), "ab@x.com", "本地部分过短原样");
        assert_eq!(mask_email("不是邮箱"), "不是邮箱");
        // 中文/Unicode 本地部分按字符截取（不能按字节切）
        assert_eq!(mask_email("张三李四@x.com"), "张三***@x.com");

        assert!(is_email_lenient("a@b.cn"), "宽松口径放行短域名");
        assert!(!is_email_lenient("a b@c.com"), "含空白拒绝");
        assert!(!is_email_lenient("ab@cd"), "域名无点拒绝");
        assert!(!is_email_lenient("@cd.com"));
        assert!(!is_email_lenient("ab@.com"), "域名以点开头拒绝");
    }

    /// 连接串脱敏：只遮蔽口令类键，其余原样（日志排查用）。
    #[test]
    fn connection_string_masking() {
        let conn = "Server=db.local;Database=erp;Uid=root;Pwd=secret123;Provider=MySQL";
        let masked = mask_connection_string(conn);
        assert!(!masked.contains("secret123"), "{masked}");
        assert!(masked.contains("Pwd=***"), "{masked}");
        assert!(masked.contains("Server=db.local"), "非敏感键保留：{masked}");
        assert!(masked.contains("Database=erp"), "{masked}");
        // 大小写不敏感 + `Password` / `User Password`
        let m2 = mask_connection_string("Password=x;user password=y;Host=z");
        assert_eq!(m2, "Password=***;user password=***;Host=z");
        // 空段忽略
        assert_eq!(mask_connection_string("A=1;;"), "A=1");
    }

    /// CSV 单元格：常规/引号/换行 + **公式注入防护**。
    #[test]
    fn csv_cell_escapes_and_guards_formulas() {
        assert_eq!(csv_cell("普通"), "普通");
        assert_eq!(csv_cell("含,逗号"), "\"含,逗号\"");
        assert_eq!(csv_cell("含\"引号"), "\"含\"\"引号\"");
        assert_eq!(csv_cell("两行\n文本"), "\"两行\n文本\"");
        // 公式注入：加前导单引号
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(csv_cell("+86"), "'+86");
        assert_eq!(csv_cell("-5"), "'-5");
        assert_eq!(csv_cell("@cmd"), "'@cmd");
        assert_eq!(csv_cell("\tx"), "'\tx");
        // 注入 + 需要引号时两者叠加
        assert_eq!(csv_cell("=a,b"), "\"'=a,b\"");
    }

    #[test]
    fn csv_row_joins_escaped_fields() {
        assert_eq!(csv_row(["a", "b,c", "=x"]), "a,\"b,c\",'=x");
        assert_eq!(csv_row(Vec::<String>::new()), "");
    }
}
