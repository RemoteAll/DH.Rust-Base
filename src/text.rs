//! 通用文本小工具（多消费方收编：Pek.RAgent / Pek.RAdmin / DHDeploy.Agent.Rust 等）。

use std::collections::HashSet;

/// 常量时间字节比较（长度不等直接 `false`；等长时逐字节异或累加，不提前返回）。
///
/// 用于口令 / 令牌 / 签名比对：避免「提前返回」泄露前缀匹配长度（时序侧信道）。
///
/// 下沉自 Pek.Proxy `security::constant_time_eq`（Pek.RDbProxy `proxy.rs`、Pek.ROutGuard `config.rs`
/// 各有一份同款实现）。**不依赖加密库**，所以放在这里而不是 `secret`（后者要开 `secret` 特性）。
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

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

/// 前缀匹配（ASCII 大小写不敏感，**零分配**）。
///
/// 替代 `s.to_ascii_lowercase().starts_with(p)`——后者每条都分配一个临时 String，
/// 高频路径（逐消息解析协议头/主题）应改用本函数。
pub fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// 子串包含判断（ASCII 大小写不敏感，**零分配**）。
///
/// 替代 `s.to_ascii_lowercase().contains(p)`。空 `needle` 返回 `true`（与 `String::contains` 一致）。
///
/// 实现要点（实测驱动）：先走大小写敏感 `str::contains`（memchr/SIMD 优化）快路径，
/// 未命中才逐候选位置做整段比较。协议主题/请求头这类"大小写本就一致"的场景
/// 稳定性与速度都优于 `to_ascii_lowercase()` + `contains`（后者每次分配一个 String）。
pub fn contains_ignore_ascii_case(text: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    // 快路径：大小写完全一致时直接命中（`str::contains` 由编译器向量化）
    if text.contains(needle) {
        return true;
    }
    let h = text.as_bytes();
    if h.len() < n.len() {
        return false;
    }
    // 慢路径：只在实际可能是首字节的位置做整段比较，避免每个窗口都比一遍
    let first_lo = n[0].to_ascii_lowercase();
    let first_up = n[0].to_ascii_uppercase();
    let mut i = 0;
    while i + n.len() <= h.len() {
        let b = h[i];
        if b == first_lo || b == first_up {
            if h[i..i + n.len()].eq_ignore_ascii_case(n) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 字节序列是否为空或全为空白（对齐 C# `String.IsNullOrWhiteSpace`，**通常零分配**）。
///
/// 纯 ASCII 走字节快路径；一旦遇到非 ASCII 字节则回退到完整 UTF-8 转换
/// （`Trim()` 语义需按 Unicode 空白判定，如全角空格 U+3000）。
pub fn is_blank_bytes(bytes: &[u8]) -> bool {
    for &b in bytes {
        if b >= 0x80 {
            return String::from_utf8_lossy(bytes).trim().is_empty();
        }
        if !(b as char).is_ascii_whitespace() {
            return false;
        }
    }
    true
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

/// 敏感值掩码：保留首 `head` 个与尾 `tail` 个字符，中间固定为 `****`。
///
/// - 空串返回空串（调用方据此判断「没有值」）；
/// - 长度不足以同时保留首尾（`len <= head + tail`）时**整体遮成 `********`**，
///   避免「很短的令牌反而露出大半」；
/// - 按**字符**计数（多字节安全），不按字节。
///
/// 收编自 Pek.DeepSeekProxy 面板的 API Key 掩码（原先名为 `mask_key`）。
///
/// ```
/// use dhrust::text::mask_secret;
///
/// assert_eq!(mask_secret("sk-abcdef123456", 7, 4), "sk-abcd****3456");
/// assert_eq!(mask_secret("short", 7, 4), "********");
/// assert_eq!(mask_secret("", 7, 4), "");
/// ```
pub fn mask_secret(value: &str, head: usize, tail: usize) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }

    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= head + tail {
        return "********".to_string();
    }

    let front: String = chars.iter().take(head).collect();
    let back: String = chars.iter().skip(chars.len() - tail).collect();
    format!("{front}****{back}")
}

/// 解析智能布尔文本（大小写不敏感）：
/// `true/1/yes/on/是` → `true`，`false/0/no/off/否` → `false`，其余 `None`。
///
/// 供 JSON 宽松取值（`JsonValue::to_bool` / `value_to_bool`）与内存数据库字段值共用。
pub fn parse_bool(text: &str) -> Option<bool> {    let text = text.trim();
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

/// 连接串归一化比较（判等）：忽略大小写、首尾空白、段顺序与 `ShowSql` 段。
///
/// 用于「数据源是否真的换了」这类判定（面板保存新连接串要不要标迁移、启动时要不要搬数据）：
/// 同一个库换个字段顺序、加个 `ShowSql` 不算换库，否则会白搬一次数据。
///
/// 只做字符串归一，不解析 Provider；空串与全空白等价。
/// 调用频次低（配置切换/启动一次），用朴素实现换取可读性与零依赖。
pub fn conn_same(a: &str, b: &str) -> bool {
    fn norm(conn: &str) -> String {
        let mut parts: Vec<String> = conn
            .split(';')
            .map(|part| part.trim().to_ascii_lowercase())
            .filter(|part| !part.is_empty() && !part.starts_with("showsql"))
            .collect();
        parts.sort();
        parts.join(";")
    }

    norm(a) == norm(b)
}

/// 取字节序列前 `max` 字节做 **UTF-8 宽松**预览（合法时**零拷贝借用**，不合法才替换坏字节）。
///
/// 为什么不用「先整体 `from_utf8` 失败再 lossy」：那样在合法路径上也要先扫一遍再拷一份。
/// 这里的语义是「尽量多看，坏字节替换成 `\u{FFFD}` 后继续」，因此**适合安全检测**
/// （二进制/混合编码的 payload 不会被整段跳过）；若只是想截一段给人看，用
/// [`truncate_chars`] 更合适（按字符数、附省略号）。
///
/// 收编自 Pek.RWaf 的 body 预览（原先每个扫描器各写一份 `from_utf8` + lossy 回退）。
pub fn utf8_lossy_prefix(bytes: &[u8], max: usize) -> std::borrow::Cow<'_, str> {
    if max == 0 || bytes.is_empty() {
        return std::borrow::Cow::Borrowed("");
    }
    let cap = bytes.len().min(max);
    let head = &bytes[..cap];
    match std::str::from_utf8(head) {
        // 合法 UTF-8（绝大多数文本 body）：直接借用，不拷贝
        Ok(text) => std::borrow::Cow::Borrowed(text),
        // 有坏字节（二进制 / 截断点在多字节字符中间）：替换坏字节后继续
        Err(_) => std::borrow::Cow::Owned(String::from_utf8_lossy(head).into_owned()),
    }
}

/// 折叠空白 + 转小写（特征匹配/搜索归一化）。
///
/// 语义：连续空白折叠成一个空格、去掉首尾空白（首部不会有前导空格）、ASCII 字母转小写；
/// 非 ASCII 字符**不做**大小写变换（与 `str::to_lowercase` 不同，这里只求稳定可比）。
///
/// 多段版本把各段视为**直接相连**（分隔符由调用方作为独立段给出），因此可以
/// 零拼接地规范化「路径 + `?` + 查询串 + 空格 + body」这种组合——
/// 收编自 Pek.RWaf 的攻击特征扫描（原先用 `format!` 拼一整份正文再处理）。
pub fn normalize_lower_parts(parts: &[&str]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    normalize_lower_bytes_into(parts, &mut out);
    String::from_utf8(out).expect("规范化输出必定是合法 UTF-8")
}

/// 折叠空白 + 转小写，写进**已有字节缓冲**（复用容量、零分配）。
///
/// 热路径用（每请求都要规范化路径/查询串）：调用方持有 `Vec<u8>` 反复使用，
/// 只需 `std::str::from_utf8(&buf)` 就能拿去扫描/比较。语义同 [`normalize_lower_parts`]。
///
/// # 参数
/// - `parts`：按顺序直接相连的多段文本（分隔符由调用方作为独立段给出）
/// - `out`：输出缓冲（**会先清空**；容量保留）
pub fn normalize_lower_bytes_into(parts: &[&str], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(parts.iter().map(|p| p.len()).sum::<usize>());
    let mut last_space = true; // 起始不留前导空格
    if parts.iter().all(|s| s.is_ascii()) {
        // 全 ASCII 走字节路径（不逐 `char` 解码再编码）
        for seg in parts {
            for &b in seg.as_bytes() {
                if b.is_ascii_whitespace() {
                    if !last_space {
                        out.push(b' ');
                        last_space = true;
                    }
                } else {
                    out.push(b.to_ascii_lowercase());
                    last_space = false;
                }
            }
        }
        return;
    }
    // 含非 ASCII：逐字符判定空白，UTF-8 手工写字节。
    // **不要**用 `encode_utf8(..).as_bytes()` + `extend_from_slice`：实测 16 KB 中文正文
    // 规范化 20 µs → 56 µs（慢 2.8 倍，多了临时切片与 memcpy 调用），`push` 才是直写备用容量。
    for seg in parts {
        for c in seg.chars() {
            if c.is_whitespace() {
                if !last_space {
                    out.push(b' ');
                    last_space = true;
                }
            } else {
                push_char_lower(out, c);
                last_space = false;
            }
        }
    }
}

/// 把字符（ASCII 转小写、非 ASCII 原样）按 UTF-8 写进字节缓冲。
///
/// `char` 不可能是代理项，所以手工编码的结果必定是合法 UTF-8。
fn push_char_lower(out: &mut Vec<u8>, c: char) {
    let code = c.to_ascii_lowercase() as u32;
    if code < 0x80 {
        out.push(code as u8);
    } else if code < 0x800 {
        out.push(0xC0 | (code >> 6) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else if code < 0x1_0000 {
        out.push(0xE0 | (code >> 12) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else {
        out.push(0xF0 | (code >> 18) as u8);
        out.push(0x80 | ((code >> 12) & 0x3F) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    }
}

/// 单段版本的 [`normalize_lower_parts`]。
pub fn normalize_lower(text: &str) -> String {
    normalize_lower_parts(&[text])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"token-123", b"token-123"));
        assert!(!constant_time_eq(b"token-123", b"token-124"));
        // 长度不同直接 false（不 panic、不越界）
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
        // 中文 / 非 ASCII 按字节比较
        assert!(constant_time_eq("口令".as_bytes(), "口令".as_bytes()));
        assert!(!constant_time_eq("口令".as_bytes(), "口另".as_bytes()));
    }

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
    fn contains_and_starts_ignore_ascii_case() {
        assert!(contains_ignore_ascii_case(
            "sys/ABC/thing/event/ping/post",
            "THING/EVENT/PING/POST"
        ));
        assert!(!contains_ignore_ascii_case(
            "sys/abc/x",
            "thing/event/ping/post"
        ));
        assert!(
            contains_ignore_ascii_case("abc", ""),
            "空 needle 与 String::contains 一致"
        );
        assert!(
            !contains_ignore_ascii_case("ab", "abc"),
            "haystack 短于 needle"
        );
        // 长度相等/相邻边界
        assert!(contains_ignore_ascii_case("ABCD", "bcd"));
        assert!(!contains_ignore_ascii_case("ABCD", "bce"));
        // 慢路径（大小写完全反向，快路径必然 miss）与命中位置在末尾
        assert!(contains_ignore_ascii_case(
            "SYS/ABC/THING/EVENT/PING/POST",
            "thing/event/ping/post"
        ));
        assert!(!contains_ignore_ascii_case(
            "SYS/ABC/THING/EVENT/PING/POSX",
            "thing/event/ping/post"
        ));

        assert!(starts_with_ignore_ascii_case(
            "Content-Type",
            "content-type"
        ));
        assert!(!starts_with_ignore_ascii_case("Content", "content-type"));
        assert!(starts_with_ignore_ascii_case("x", ""), "空前缀恒真");
    }

    #[test]
    fn is_blank_bytes_matches_csharp_is_null_or_white_space() {
        assert!(is_blank_bytes(b""));
        assert!(is_blank_bytes(b" \t\r\n"));
        assert!(!is_blank_bytes(b" a "));
        // 非 ASCII 回退路径：全角空格（U+3000）在 C# 中算空白
        assert!(is_blank_bytes("　".as_bytes()));
        // 非 ASCII 且非空白
        assert!(!is_blank_bytes("中".as_bytes()));
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

    #[test]
    fn mask_secret_keeps_short_values_fully_hidden() {
        assert_eq!(mask_secret("sk-abcdef123456", 7, 4), "sk-abcd****3456");
        // 长度不足以同时留首尾 → 全遮
        assert_eq!(mask_secret("sk-abc", 7, 4), "********");
        assert_eq!(mask_secret("12345678901", 7, 4), "********");
        // 刚好够（12 > 11）才露出首尾
        assert_eq!(mask_secret("123456789012", 7, 4), "1234567****9012");
        // 空串/空白
        assert_eq!(mask_secret("", 7, 4), "");
        assert_eq!(mask_secret("   ", 7, 4), "");
        // 多字节按字符计
        assert_eq!(mask_secret("口令口令口令口令", 2, 2), "口令****口令");
    }

    #[test]
    fn conn_same_ignores_case_order_and_showsql() {
        // 大小写 / 段顺序 / 空白 / ShowSql 都不算换库
        assert!(conn_same(
            "Server=a;Port=3306;Database=DH",
            "database=dh; server=A ;port=3306;ShowSql=true"
        ));
        // 空串与全空白等价
        assert!(conn_same("", " ShowSql=1 "));
        // 值变了就不算同一个库
        assert!(!conn_same("Server=a;Database=DH", "Server=b;Database=DH"));
        // 少一个真实段也不算同一个库（ShowSql 是唯一被忽略的段）
        assert!(!conn_same("Server=a", "Server=a;Pooling=false"));
    }

    #[test]
    fn normalize_lower_matches_char_collect() {
        // 中文 / 全角空格 / 表情 / 大小写混杂 / 连续空白
        let cases = [
            "Hello   World",
            "  招商银行\t经典  \n白金卡 ",
            "ＡＢＣ　全角空格",
            "emoji 😀 混排",
            "a\n\tb\r\nc",
            "",
        ];
        for case in cases {
            let expect: String = {
                let mut s = String::new();
                let mut last_space = true;
                for c in case.chars() {
                    if c.is_whitespace() {
                        if !last_space {
                            s.push(' ');
                            last_space = true;
                        }
                    } else {
                        s.push(c.to_ascii_lowercase());
                        last_space = false;
                    }
                }
                s
            };
            assert_eq!(normalize_lower(case), expect, "case={case:?}");
        }
    }

    /// 基准：非 ASCII 规范化三种实现的同进程对比（避免跨次测量被机器负载带偏）。
    ///
    /// 手动跑：`cargo test --release --lib bench::normalize_non_ascii_variants -- --ignored --nocapture`
    #[test]
    #[ignore = "bench（手动运行）"]
    fn bench_normalize_non_ascii_variants() {
        use std::hint::black_box;
        use std::time::Instant;

        const N: usize = 3_000;
        let unit = "{\"role\":\"user\",\"content\":\"请帮我总结这份招标文件里的技术要求与评分办法\"},";
        let mut text = String::from("{\"messages\":[");
        while text.len() < 16 * 1024 {
            text.push_str(unit);
        }
        text.push_str("]}");

        let measure = |mut f: Box<dyn FnMut()>| -> f64 {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let started = Instant::now();
                for _ in 0..N {
                    f();
                }
                best = best.min(started.elapsed().as_nanos() as f64 / N as f64);
            }
            best
        };

        let mut buf = Vec::new();
        let bytes_impl = measure(Box::new(|| {
            normalize_lower_bytes_into(&[&text], &mut buf);
            black_box(&buf);
        }));
        let string_impl = measure(Box::new(|| {
            // 旧实现（String + push(char)）
            let mut out = String::with_capacity(text.len());
            let mut last_space = true;
            for c in text.chars() {
                if c.is_whitespace() {
                    if !last_space {
                        out.push(' ');
                        last_space = true;
                    }
                } else {
                    out.push(c.to_ascii_lowercase());
                    last_space = false;
                }
            }
            black_box(&out);
        }));
        let encode_utf8_impl = measure(Box::new(|| {
            // 上一版实现（encode_utf8 + extend_from_slice）
            let mut out: Vec<u8> = Vec::with_capacity(text.len());
            let mut scratch = [0u8; 4];
            let mut last_space = true;
            for c in text.chars() {
                if c.is_whitespace() {
                    if !last_space {
                        out.push(b' ');
                        last_space = true;
                    }
                } else {
                    out.extend_from_slice(c.to_ascii_lowercase().encode_utf8(&mut scratch).as_bytes());
                    last_space = false;
                }
            }
            black_box(&out);
        }));
        println!(
            "16KB 中文正文：字节实现 {bytes_impl:.0} ns；String 实现 {string_impl:.0} ns；\
             encode_utf8 版 {encode_utf8_impl:.0} ns"
        );
        assert!(bytes_impl > 0.0);
    }
}
