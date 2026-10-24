//! 证书与域名工具（零外部依赖）：PEM 拆包、最小 X.509 解析、域名通配符匹配。
//!
//! 用途（Pek.RAgent ↔ Pek.RPanlServer 证书下发/部署）：
//! - 平台上传证书时解析域名（SAN/CN）、有效期与指纹；
//! - Agent 盘点站点现有证书、判断证书是否覆盖站点域名；
//! - 通配符规则与浏览器一致：`*.example.com` 只匹配**一级**子域，`example.com` 不匹配。
//!
//! 设计取舍：
//! - 只做「读元数据」，不做签名链校验、私钥数学校验（部署安全由 `nginx -t` / IIS 导入结果兜底）；
//! - DER 解析器容错：任何畸形输入返回 `None`，绝不 panic（网络输入）；
//! - 时间按 RFC 5280（UTCTime 50 年分界 / GeneralizedTime），只接受 `Z` 结尾的 UTC 形态。

use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};

/// 一个 PEM 块（保留原文与解码后的 DER）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    /// 标签（`CERTIFICATE` / `PRIVATE KEY` / `RSA PRIVATE KEY` / `EC PRIVATE KEY` …）
    pub label: String,
    /// 原始 PEM 文本（含首尾行）
    pub pem: String,
    /// base64 解码后的 DER（解码失败时为空）
    pub der: Vec<u8>,
}

/// 证书元数据（从 X.509 解析）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertInfo {
    /// Subject CN（可能为空）
    pub subject_cn: String,
    /// SAN 中的 DNS 名称（按出现顺序）
    pub dns_names: Vec<String>,
    /// SAN 中的 IP 地址
    pub ip_addresses: Vec<String>,
    /// 生效时间（Unix 秒，UTC）
    pub not_before: i64,
    /// 到期时间（Unix 秒，UTC）
    pub not_after: i64,
    /// 序列号（十六进制，小写，无前导 0）
    pub serial_hex: String,
    /// SHA-1 指纹（小写十六进制；IIS `netsh http` 绑定用，展示时转大写）
    pub sha1_hex: String,
    /// SHA-256 指纹（小写十六进制）
    pub sha256_hex: String,
    /// 是否 CA 证书（部署到站点会被浏览器/客户端拒绝）
    pub is_ca: bool,
}

impl CertInfo {
    /// 证书声明的域名集合：SAN DNS 优先；SAN 为空时回退 CN。
    ///
    /// 返回小写、去重、保持顺序（叶子证书通常首个 SAN 为主域名）。
    pub fn domains(&self) -> Vec<String> {
        fn push(out: &mut Vec<String>, d: &str) {
            let d = d.trim().trim_end_matches('.').to_ascii_lowercase();
            if !d.is_empty() && !out.iter().any(|x| x == &d) {
                out.push(d);
            }
        }
        let mut out: Vec<String> = Vec::new();
        for d in &self.dns_names {
            push(&mut out, d);
        }
        if out.is_empty() {
            push(&mut out, &self.subject_cn);
        }
        out
    }

    /// 证书是否覆盖指定主机名（大小写不敏感、忽略结尾点）。
    pub fn covers_host(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.');
        if host.is_empty() {
            return false;
        }
        self.domains().iter().any(|p| domain_matches(p, host)) || self.covers_ip(host)
    }

    /// 证书 SAN 中是否包含指定 IP（客户端按 IP 访问时需要）。
    pub fn covers_ip(&self, host: &str) -> bool {
        let host = host.trim();
        self.ip_addresses.iter().any(|ip| ip == host)
    }

    /// `not_after` 距今剩余天数（向上取整；已过期返回负数）。
    pub fn days_left(&self, now: i64) -> i64 {
        let secs = self.not_after - now;
        if secs >= 0 {
            (secs + 86_399) / 86_400
        } else {
            secs / 86_400
        }
    }

    /// 是否已过期（相对给定 Unix 秒）。
    pub fn expired(&self, now: i64) -> bool {
        now >= self.not_after
    }

    /// 生效/到期时间的可读形式（UTC）。
    pub fn validity_text(&self) -> String {
        format!(
            "{} ~ {}",
            format_unix_utc(self.not_before),
            format_unix_utc(self.not_after)
        )
    }
}

/// 证书包（证书链 + 私钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertBundle {
    /// 证书链 PEM（全部 `CERTIFICATE` 块，按原顺序）
    pub certs_pem: String,
    /// 私钥 PEM（首个私钥块；可能为空——仅上传证书时）
    pub key_pem: String,
    /// 解析成功的证书元数据（首个通常是叶子证书）
    pub certs: Vec<CertInfo>,
}

/// 是否为私钥标签（PKCS#8 / PKCS#1 / EC / 加密私钥）。
pub fn is_private_key_label(label: &str) -> bool {
    let l = label.trim().to_ascii_uppercase();
    l.contains("PRIVATE KEY") // PRIVATE KEY / RSA PRIVATE KEY / EC PRIVATE KEY / ENCRYPTED PRIVATE KEY
}

/// 是否为证书标签。
pub fn is_certificate_label(label: &str) -> bool {
    label.trim().eq_ignore_ascii_case("CERTIFICATE")
}

/// 提取文本中的全部 PEM 块（忽略块外内容；base64 解码失败时 `der` 为空）。
pub fn pem_blocks(text: &str) -> Vec<PemBlock> {
    let mut out = Vec::new();
    let mut label: Option<String> = None;
    let mut body = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            if let Some(end) = rest.strip_suffix("-----") {
                label = Some(end.trim().to_string());
                body.clear();
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("-----END ") {
            if let Some(end) = rest.strip_suffix("-----") {
                let end_label = end.trim().to_string();
                if let Some(l) = label.take() {
                    // 标签不匹配也接受（按开始标签归档），但 DER 可能解码失败
                    let clean: String = body
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>();
                    let der = crate::sign::base64_decode(&clean).unwrap_or_default();
                    let pem = format!(
                        "-----BEGIN {l}-----\n{}\n-----END {l}-----\n",
                        wrap64(&clean)
                    );
                    out.push(PemBlock { label: l, pem, der });
                }
                let _ = end_label;
                body.clear();
                continue;
            }
        }
        if label.is_some() && !line.is_empty() && !line.starts_with("-----") {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// 按 64 列换行（标准 PEM 排版）。
fn wrap64(s: &str) -> String {
    s.as_bytes()
        .chunks(64)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 拆分证书包：证书链 + 私钥（缺证书 = 错误；无私钥 = 允许，`key_pem` 为空）。
pub fn split_bundle(text: &str) -> Result<CertBundle, String> {
    let blocks = pem_blocks(text);
    if blocks.is_empty() {
        return Err("未找到 PEM 内容（需要 -----BEGIN CERTIFICATE----- 块）".to_string());
    }
    let mut certs_pem = String::new();
    let mut key_pem = String::new();
    let mut certs = Vec::new();
    for b in &blocks {
        if is_certificate_label(&b.label) {
            if b.der.is_empty() {
                return Err(format!("证书 PEM base64 解码失败（块 {}）", certs.len() + 1));
            }
            certs_pem.push_str(&b.pem);
            match parse_certificate_der(&b.der) {
                Some(info) => certs.push(info),
                None => return Err("证书 DER 解析失败（不是有效的 X.509 证书）".to_string()),
            }
        } else if is_private_key_label(&b.label) {
            if b.der.is_empty() {
                return Err("私钥 PEM base64 解码失败".to_string());
            }
            if key_pem.is_empty() {
                key_pem = b.pem.clone();
            }
        }
    }
    if certs.is_empty() {
        return Err("未找到 CERTIFICATE 块".to_string());
    }
    Ok(CertBundle {
        certs_pem,
        key_pem,
        certs,
    })
}

/// 解析首个证书 PEM。
pub fn parse_certificate_pem(text: &str) -> Option<CertInfo> {
    let blocks = pem_blocks(text);
    blocks
        .iter()
        .filter(|b| is_certificate_label(&b.label))
        .find_map(|b| parse_certificate_der(&b.der))
}

/// 解析 X.509 证书 DER（失败返回 `None`）。
pub fn parse_certificate_der(der: &[u8]) -> Option<CertInfo> {
    let mut pos = 0usize;
    let outer = read_tlv(der, &mut pos)?;
    if outer.tag != 0x30 {
        return None;
    }
    let parts = children(outer.value);
    let tbs = parts.first()?;
    if tbs.tag != 0x30 {
        return None;
    }
    let fields = children(tbs.value);
    let mut i = 0usize;
    if fields.first().map(|t| t.tag) == Some(0xA0) {
        i = 1; // version [0] EXPLICIT
    }
    let serial = fields.get(i)?;
    if serial.tag != 0x02 {
        return None;
    }
    i += 1;
    let _sig_alg = fields.get(i)?;
    i += 1;
    let _issuer = fields.get(i)?;
    i += 1;
    let validity = fields.get(i)?;
    i += 1;
    let subject = fields.get(i)?;

    let subject_cn = subject_cn(subject.value);
    let (not_before, not_after) = parse_validity(validity.value)?;

    let mut dns_names = Vec::new();
    let mut ip_addresses = Vec::new();
    let mut is_ca = false;
    for f in &fields {
        if f.tag != 0xA3 {
            continue; // extensions [3] EXPLICIT
        }
        let mut p = 0usize;
        let Some(seq) = read_tlv(f.value, &mut p) else {
            continue;
        };
        for ext in children(seq.value) {
            let parts = children(ext.value);
            let Some(oid) = parts.first() else {
                continue;
            };
            let Some(value) = parts.last() else {
                continue;
            };
            if oid.value == [0x55, 0x1D, 0x11] {
                parse_san(value.value, &mut dns_names, &mut ip_addresses);
            } else if oid.value == [0x55, 0x1D, 0x13] {
                let mut p = 0usize;
                if let Some(bc) = read_tlv(value.value, &mut p) {
                    for item in children(bc.value) {
                        if item.tag == 0x01 {
                            is_ca = item.value.iter().any(|b| *b != 0);
                        }
                    }
                }
            }
        }
    }

    let serial_hex = hex_trim(serial.value);
    Some(CertInfo {
        subject_cn,
        dns_names,
        ip_addresses,
        not_before,
        not_after,
        serial_hex,
        sha1_hex: hex_lower(&sha1_bytes(der)),
        sha256_hex: crate::sign::sha256_hex(der),
        is_ca,
    })
}

/// 通配符域名匹配（与浏览器规则一致）：
/// - `*.example.com` 只匹配 `a.example.com`（一级、非空、非点）；
/// - `example.com` 精确匹配（含忽略大小写/结尾点）；
/// - 不支持 `a.*.example.com` 这类中间通配符（按字面比较，必然不匹配）。
pub fn domain_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if p.is_empty() || h.is_empty() {
        return false;
    }
    if let Some(rest) = p.strip_prefix("*.") {
        let Some((first, tail)) = h.split_once('.') else {
            return false;
        };
        !first.is_empty() && !tail.is_empty() && tail == rest
    } else {
        p == h
    }
}

/// 任一域名模式覆盖主机名。
pub fn any_matches<'a, I>(patterns: I, host: &str) -> bool
where
    I: IntoIterator<Item = &'a String>,
{
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    patterns.into_iter().any(|p| domain_matches(p, host))
}

/// Unix 秒 → `YYYY-MM-DD HH:MM:SS UTC`。
pub fn format_unix_utc(ts: i64) -> String {
    match Utc.timestamp_opt(ts, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => String::from("-"),
    }
}

// ———————————————— DER 基础设施 ————————————————

#[derive(Debug, Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
}

fn read_tlv<'a>(data: &'a [u8], pos: &mut usize) -> Option<Tlv<'a>> {
    if *pos >= data.len() {
        return None;
    }
    let tag = data[*pos];
    *pos += 1;
    if *pos >= data.len() {
        return None;
    }
    let first = data[*pos];
    *pos += 1;
    let len = if first & 0x80 == 0 {
        first as usize
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 8 || *pos + n > data.len() {
            return None;
        }
        let mut v = 0usize;
        for i in 0..n {
            v = (v << 8) | data[*pos + i] as usize;
        }
        *pos += n;
        v
    };
    if *pos + len > data.len() {
        return None;
    }
    let value = &data[*pos..*pos + len];
    *pos += len;
    Some(Tlv { tag, value })
}

fn children(value: &[u8]) -> Vec<Tlv<'_>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < value.len() {
        match read_tlv(value, &mut pos) {
            Some(t) => out.push(t),
            None => break,
        }
    }
    out
}

fn subject_cn(name_value: &[u8]) -> String {
    // RDNSequence ::= SEQUENCE OF RelativeDistinguishedName(SET OF AttributeTypeAndValue)
    for rdn in children(name_value) {
        for attr in children(rdn.value) {
            let kv = children(attr.value);
            if kv.len() < 2 {
                continue;
            }
            // OID 2.5.4.3 = CN
            if kv[0].value == [0x55, 0x04, 0x03] {
                return String::from_utf8_lossy(kv[1].value).trim().to_string();
            }
        }
    }
    String::new()
}

fn parse_validity(value: &[u8]) -> Option<(i64, i64)> {
    let mut pos = 0usize;
    let nb = read_tlv(value, &mut pos)?;
    let na = read_tlv(value, &mut pos)?;
    Some((parse_time(&nb)?, parse_time(&na)?))
}

fn parse_time(t: &Tlv<'_>) -> Option<i64> {
    let raw = std::str::from_utf8(t.value).ok()?.trim();
    let s = raw.strip_suffix('Z').unwrap_or(raw).trim();
    let dt: NaiveDateTime = match t.tag {
        0x17 => {
            // UTCTime：YYMMDDHHMMSSZ（部分实现省略秒）
            if s.len() < 10 {
                return None;
            }
            let yy: i32 = s.get(0..2)?.parse().ok()?;
            let year = if yy < 50 { 2000 + yy } else { 1900 + yy };
            let rest = &s[2..];
            let fmt = if rest.len() >= 10 {
                "%m%d%H%M%S"
            } else {
                "%m%d%H%M"
            };
            let md = NaiveDateTime::parse_from_str(
                &format!("{year:04}{rest}"),
                &format!("%Y{fmt}"),
            )
            .ok()?;
            md
        }
        0x18 => {
            // GeneralizedTime：YYYYMMDDHHMMSSZ
            if s.len() < 12 {
                return None;
            }
            let fmt = if s.len() >= 14 {
                "%Y%m%d%H%M%S"
            } else {
                "%Y%m%d%H%M"
            };
            NaiveDateTime::parse_from_str(s, fmt).ok()?
        }
        _ => return None,
    };
    Some(dt.and_utc().timestamp())
}

fn parse_san(value: &[u8], dns: &mut Vec<String>, ips: &mut Vec<String>) {
    let mut pos = 0usize;
    let Some(seq) = read_tlv(value, &mut pos) else {
        return;
    };
    for gn in children(seq.value) {
        match gn.tag {
            // dNSName [2] IA5String（隐式标签）
            0x82 => {
                let name = String::from_utf8_lossy(gn.value).trim().to_string();
                if !name.is_empty() {
                    dns.push(name);
                }
            }
            // iPAddress [7] OCTET STRING（4/16 字节）
            0x87 => {
                let parsed = match gn.value.len() {
                    4 => {
                        let mut b = [0u8; 4];
                        b.copy_from_slice(gn.value);
                        Some(std::net::IpAddr::from(b))
                    }
                    16 => {
                        let mut b = [0u8; 16];
                        b.copy_from_slice(gn.value);
                        Some(std::net::IpAddr::from(b))
                    }
                    _ => None,
                };
                if let Some(ip) = parsed {
                    ips.push(ip.to_string());
                }
            }
            _ => {}
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    crate::sign::hex_encode(bytes)
}

/// INTEGER 内容 → 十六进制（去掉前导 0，保留符号位语义外的常规表示）。
fn hex_trim(bytes: &[u8]) -> String {
    let mut i = 0usize;
    while i + 1 < bytes.len() && bytes[i] == 0 {
        i += 1;
    }
    crate::sign::hex_encode(&bytes[i..])
}

fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(data);
    let out = h.finalize();
    let mut b = [0u8; 20];
    b.copy_from_slice(&out);
    b
}

/// 内部日期工具（测试/消费方需要把 `not_after` 换成日历日期时可用）。
pub fn unix_to_date(ts: i64) -> Option<String> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.date_naive().to_string())
}

/// 由 `YYYY-MM-DD` 得到当日 00:00:00 UTC 的 Unix 秒（测试辅助）。
pub fn date_to_unix(date: &str) -> Option<i64> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自签通配符证书（openssl 生成；CN=*.example.com，SAN=*.example.com,example.com）
    const SAMPLE_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDODCCAiCgAwIBAgIULevWOt43E+eXlYLIePaOJnedt3MwDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNKi5leGFtcGxlLmNvbTAeFw0yNjEwMjQwMDAxMzJaFw0z
NjEwMjEwMDAxMzJaMBgxFjAUBgNVBAMMDSouZXhhbXBsZS5jb20wggEiMA0GCSqG
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQDHONi49KHYNrSeQg4WnbWp/nP+yjRJ5wAa
HAWBJGoQ8cmNFsK23I+dyp9D3RL42B6Bn1mgLnurHn0YW0OlQmKAGuTNVSyFtDUG
pzSUJ/RL3/7IimVDK+qxOehPaxLtlX7OPXJh2DXA5SLZ0TpiblqcL2/pssUqyd2J
G5MzNqbeO+Akt4LSGy4dW8gZ99Peyztb1FS68lrXZSMuvdsTWrs0yKTnQ4B92hSX
rzR+zhQJ+Ua/mubQdFvuCurhm5SJLuUSyR070PM4tSVSH7GrVBJ6nYKDvk12Gmm3
PdtWvzaRpYz1hsCiWu2oM7wriUx9TkhFWwzpnzysrUFCTGRHbUujAgMBAAGjejB4
MB0GA1UdDgQWBBR1Fmw9rVifiDL9t7+N0RIx1pgq1DAfBgNVHSMEGDAWgBR1Fmw9
rVifiDL9t7+N0RIx1pgq1DAPBgNVHRMBAf8EBTADAQH/MCUGA1UdEQQeMByCDSou
ZXhhbXBsZS5jb22CC2V4YW1wbGUuY29tMA0GCSqGSIb3DQEBCwUAA4IBAQBa+a6V
Gzzi4DNRz1mAZB4w+FtAtQsQoua0sQlWJTBxRMFT6+VufuX4NQLLLqwADt7fAZSB
e3O11LWfOhQhzt1PU/nJpIORtIfnNp0jL8pZvJdT0HZdpdlt8qHi5h++Km8d4CWj
q6UXkt5F53SqUvYAMwnhHNC0ibLw2YFZvYSHMwreiB0zMII1w/Kn3We3iCuRLFMv
zhjxVY6PyEs3kKQyylOGyBXhugF1ovMazCbRR0dbzpKmvdJo5LMtAL++Kh3d0K6C
UGlDw1oAha64J73sXXoUwIyzvglj7BlOtDlWsLAIN6MlDyMX6a2j1YQcshiZim0a
jMNPZqSIqfUqwb4d
-----END CERTIFICATE-----
";

    const SAMPLE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDHONi49KHYNrSe
Qg4WnbWp/nP+yjRJ5wAaHAWBJGoQ8cmNFsK23I+dyp9D3RL42B6Bn1mgLnurHn0Y
W0OlQmKAGuTNVSyFtDUG
-----END PRIVATE KEY-----
";

    #[test]
    fn parses_wildcard_certificate_metadata() {
        let info = parse_certificate_pem(SAMPLE_CERT).expect("应解析成功");
        assert_eq!(info.subject_cn, "*.example.com");
        assert_eq!(info.domains(), vec!["*.example.com", "example.com"]);
        assert_eq!(
            info.sha1_hex, "3c3de759ecbfcbd153ca38b874feefad58d654cf",
            "SHA-1 指纹（IIS 绑定用）应与 openssl 一致"
        );
        assert_eq!(
            info.sha256_hex, "db58f8aab23baad7ce07c3c220dda04b025d3439dea1a6995f523b80aadff0ec"
        );
        assert_eq!(unix_to_date(info.not_before).as_deref(), Some("2026-10-24"));
        assert_eq!(unix_to_date(info.not_after).as_deref(), Some("2036-10-21"));
        assert!(info.is_ca, "自签样本带 CA:TRUE（仅用于解析测试）");
        assert!(info.not_after > info.not_before);
        assert!(info.validity_text().contains("2036-10-21"));
    }

    #[test]
    fn wildcard_matching_follows_browser_rules() {
        assert!(domain_matches("*.example.com", "a.example.com"));
        assert!(domain_matches("*.example.com", "A.Example.COM"));
        assert!(!domain_matches("*.example.com", "a.b.example.com"));
        assert!(!domain_matches("*.example.com", "example.com"));
        assert!(!domain_matches("*.example.com", ".example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("example.com.", "example.com"));
        assert!(!domain_matches("example.com", "www.example.com"));
        assert!(!domain_matches("a.*.example.com", "a.b.example.com"));
    }

    #[test]
    fn bundle_split_keeps_chain_and_key() {
        let text = format!("说明文字\n{SAMPLE_CERT}\n{SAMPLE_KEY}\n");
        let b = split_bundle(&text).expect("应拆分成功");
        assert_eq!(b.certs.len(), 1);
        assert!(b.certs_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(b.certs_pem.ends_with("-----END CERTIFICATE-----\n"));
        assert!(b.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(b.key_pem.contains("-----END PRIVATE KEY-----"));
        assert_eq!(b.certs[0].subject_cn, "*.example.com");
    }

    #[test]
    fn bundle_rejects_missing_certificate() {
        let err = split_bundle(SAMPLE_KEY).unwrap_err();
        assert!(err.contains("CERTIFICATE"), "{err}");
        let err = split_bundle("hello").unwrap_err();
        assert!(err.contains("PEM"), "{err}");
    }

    #[test]
    fn covers_host_and_ip() {
        let info = parse_certificate_pem(SAMPLE_CERT).unwrap();
        assert!(info.covers_host("api.example.com"));
        assert!(info.covers_host("example.com"));
        assert!(!info.covers_host("a.b.example.com"));
        assert!(!info.covers_host("other.com"));
        assert!(!info.covers_host(""));
        assert!(!info.covers_ip("1.2.3.4"));
    }

    #[test]
    fn malformed_input_never_panics() {
        assert!(parse_certificate_der(&[]).is_none());
        assert!(parse_certificate_der(&[0x30, 0x82, 0xFF, 0xFF]).is_none());
        assert!(parse_certificate_der(&[0x30, 0x03, 0x30, 0x01, 0x00]).is_none());
        assert!(pem_blocks("-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n")[0]
            .der
            .is_empty());
    }

    #[test]
    fn date_helpers_roundtrip() {
        let ts = date_to_unix("2030-01-02").unwrap();
        assert_eq!(unix_to_date(ts).as_deref(), Some("2030-01-02"));
    }
}
