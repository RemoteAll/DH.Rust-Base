//! 口令散列：**与 C# `NewLife.Security.SaltPasswordProvider` 字节级互通**。
//!
//! # 为什么按 C# 来实现
//! 数据库里的口令列是**跨语言共享的资产**：同一套账号库，C#（DH.NCore / DH.NCode
//! 的 `IManageProvider`）与 Rust 服务都要能验证。因此算法、盐值字符集、编码大小写、
//! 拼接顺序都必须逐字节对齐 C#，否则"换语言就登不上"或"改密码后 C# 端失效"。
//!
//! # 存储格式（C# 原样）
//! ```text
//! $算法$盐值$散列
//! 例：$sha512$abCD12./xyZ9012$Xw9m0k...==
//! ```
//! 用 `$` 分段：第 0 段为空、第 1 段算法名、第 2 段盐值、第 3 段散列（Base64 或十六进制）。
//!
//! # 支持的算法（与 C# 同名）
//! | 算法 | 散列算法（C# 原式） | 说明 |
//! |---|---|---|
//! | `md5` | `MD5( MD5(密码).ToUpperHex + 盐 )` 十六进制大写 | 两道 MD5 + 字符串盐 |
//! | `sha1` | `Base64(HMACSHA1(key=盐, msg=密码UTF8))` | 盐作 HMAC 密钥 |
//! | `sha512` | `Base64(HMACSHA512(key=盐, msg=密码UTF8))` | **C# 默认算法** |
//! | `md5+sha1` | `Base64(HMACSHA1(key=盐, msg=MD5原始16字节))` | 先 MD5 再 HMAC-SHA1 |
//! | `md5+sha512` | `Base64(HMACSHA512(key=盐, msg=MD5原始16字节))` | 先 MD5 再 HMAC-SHA512 |
//!
//! 注意 C# 的 `SHA1(bytes, salt)` 实际是 **HMAC-SHA1（盐为密钥）**，不是"拼接后散列"，
//! 这一点最容易实现错；本模块以 C# 为准（见 `DH.NCore/DH.NCore/Security/SecurityHelper.cs`）。
//!
//! # 旧式口令（兼容 + 自动升级）
//! 不含 `$` 的历史值按 C# `Verify` 的老值分支处理：命中**明文**或**MD5 十六进制**
//! （大小写不敏感 —— PHP 端存小写、C# 存大写，两边都要认）。登录成功后调用
//! [`verify_and_upgrade`] 即把它替换成新格式（对齐 C# `LoginCore` 里
//! `if (!user.Password.Contains("$")) user.Password = prv.Hash(password);`）。
//!
//! # 盐值
//! - 默认 16 位随机字符，字符集 `a-zA-Z0-9./`（与 C# `_cs` 完全一致）；
//! - `salt_time > 0` 时改用 **Unix 秒** 作盐，并在验证时校验时间差（C# `SaltTime`，
//!   用于防重放的临时口令传输，建议 30 秒）。
//!
//! # 用法
//! ```no_run
//! use dhrust::sign::password;
//!
//! // 存库
//! let stored = password::hash("New#life");
//! // 校验 + 顺带升级旧格式
//! let (ok, upgraded) = password::verify_and_upgrade("New#life", &stored);
//! assert!(ok);
//! assert!(upgraded.is_none()); // 已是新格式
//! ```

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha512;

use super::{base64_encode, fixed_time_eq, hex_decode, md5_bytes};

/// 默认算法（对齐 C# `SaltPasswordProvider.Algorithm` 缺省值）。
pub const DEFAULT_ALGORITHM: &str = "sha512";

/// 支持的算法名（与 C# `switch` 分支一致）。
pub const ALGORITHMS: [&str; 5] = ["md5", "sha1", "sha512", "md5+sha1", "md5+sha512"];

/// 盐值字符集（与 C# `SaltPasswordProvider._cs` 逐字符一致，含末尾 `./`）。
pub const SALT_CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789./";

/// 随机盐长度（C# 固定 16）。
pub const SALT_LENGTH: usize = 16;

/// 口令提供者（对齐 C# `NewLife.Security.SaltPasswordProvider`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordProvider {
    algorithm: String,
    salt_time: i64,
}

impl Default for PasswordProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PasswordProvider {
    /// 默认提供者：`sha512` + 随机盐（C# `new SaltPasswordProvider()`）。
    pub fn new() -> Self {
        PasswordProvider {
            algorithm: DEFAULT_ALGORITHM.to_string(),
            salt_time: 0,
        }
    }

    /// 指定算法（`md5` / `sha1` / `sha512` / `md5+sha1` / `md5+sha512`）。
    pub fn with_algorithm(algorithm: impl Into<String>) -> Result<Self, String> {
        let algorithm = algorithm.into();
        let name = algorithm.trim().to_ascii_lowercase();
        if !ALGORITHMS.contains(&name.as_str()) {
            return Err(format!(
                "不支持的口令算法 [{algorithm}]（可选：{}）",
                ALGORITHMS.join(" / ")
            ));
        }
        Ok(PasswordProvider {
            algorithm: name,
            salt_time: 0,
        })
    }

    /// 时间盐：`seconds > 0` 时改用 Unix 秒作盐，并在验证时限制时间差（C# `SaltTime`）。
    pub fn with_salt_time(mut self, seconds: i64) -> Self {
        self.salt_time = seconds.max(0);
        self
    }

    /// 当前算法名。
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// 时间盐窗口（0 = 随机盐）。
    pub fn salt_time(&self) -> i64 {
        self.salt_time
    }

    /// 生成新口令散列（`$算法$盐$散列`）。
    pub fn hash(&self, password: &str) -> String {
        let salt = self.create_salt();
        self.hash_with_salt(password, &salt)
    }

    /// 用**指定盐**生成散列（确定性；互通校验、批量迁移、单元测试用）。
    pub fn hash_with_salt(&self, password: &str, salt: &str) -> String {
        let digest = compute(&self.algorithm, password, salt).unwrap_or_default();
        format!("${}${}${}", self.algorithm, salt, digest)
    }

    /// 验证口令（含旧式明文/MD5、以及"前端已算好 MD5 传输"的兼容分支）。
    pub fn verify(&self, password: &str, hash: &str) -> bool {
        let hash = hash.trim();
        if hash.is_empty() {
            return false;
        }

        let parts: Vec<&str> = hash.split('$').collect();

        // ———— 旧式：明文或 MD5 十六进制（大小写不敏感）————
        if parts.len() == 1 {
            return eq_text_ignore_case(hash, password)
                || eq_text_ignore_case(hash, &md5_hex_lower(password));
        }
        // 非 `$算法$盐$散列` 形态：C# 抛异常；这里按"验证失败"处理（不 panic）
        if parts.len() != 4 || !parts[0].is_empty() {
            return false;
        }

        let (alg, salt, want) = (parts[1], parts[2], parts[3]);

        // 时间盐：超出允许时间差直接失败
        if self.salt_time > 0 {
            let Ok(ts) = salt.parse::<i64>() else {
                return false;
            };
            let now = crate::times::gettimestamp() as i64;
            if (now - ts).abs() > self.salt_time {
                return false;
            }
        }

        // 1) 标准分支：口令明文
        if let Some(got) = compute(alg, password, salt) {
            if eq_text(want, &got) {
                return true;
            }
        }

        // 2) 兼容分支（对齐 C# Verify 的多路尝试）
        match alg {
            // 传输的是「前端算好的 MD5 十六进制」：`(password + salt).MD5()`
            "md5" => {
                // 十六进制散列：忽略大小写（C# 产大写，其它语言可能产小写）
                let got = md5_hex_upper(&format!("{password}{salt}"));
                if eq_text_ignore_case(want, &got) {
                    return true;
                }
                // 额外兼容：前端 JS 常见小写十六进制（C# 只认大写原样）
                if looks_like_md5_hex(password) {
                    let got = md5_hex_upper(&format!("{}{salt}", password.to_ascii_uppercase()));
                    if eq_text_ignore_case(want, &got) {
                        return true;
                    }
                }
                false
            }
            // 兼容"纯 SHA 散列"与"传输 MD5 十六进制"两种旧值
            "md5+sha1" | "md5+sha512" => {
                let password_bytes = password.as_bytes();
                let hmac = |data: &[u8]| -> String {
                    if alg == "md5+sha1" {
                        base64_encode(&hmac_sha1(salt.as_bytes(), data))
                    } else {
                        base64_encode(&hmac_sha512(salt.as_bytes(), data))
                    }
                };
                // 兼容纯 SHA 散列（旧版算法从 sha512 换成 md5+sha512 时的历史值）
                if eq_text(want, &hmac(password_bytes)) {
                    return true;
                }
                // 传输的是十六进制散列 → 先还原字节
                if let Some(raw) = hex_decode(password) {
                    if eq_text(want, &hmac(&raw)) {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// 该散列是否建议升级（旧式、或算法与当前配置不同）。
    pub fn needs_upgrade(&self, hash: &str) -> bool {
        let hash = hash.trim();
        // 空口令：C# 在有明文时直接写入新格式
        if hash.is_empty() {
            return true;
        }
        let parts: Vec<&str> = hash.split('$').collect();
        if parts.len() != 4 || !parts[0].is_empty() {
            return true;
        }
        parts[1] != self.algorithm
    }

    /// 验证并按需给出新散列：`(是否通过, 升级后的散列)`。
    ///
    /// 对齐 C# `LoginCore`：验证通过后若仍是旧格式，立即改写为新格式。
    /// 返回 `None` 表示无需升级（调用方不必写库）。
    pub fn verify_and_upgrade(&self, password: &str, hash: &str) -> (bool, Option<String>) {
        if !self.verify(password, hash) {
            return (false, None);
        }
        if self.needs_upgrade(hash) {
            (true, Some(self.hash(password)))
        } else {
            (true, None)
        }
    }

    /// 创建盐值（时间盐 或 16 位随机字符）。
    fn create_salt(&self) -> String {
        if self.salt_time > 0 {
            return (crate::times::gettimestamp() as i64).to_string();
        }
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut salt = String::with_capacity(SALT_LENGTH);
        for _ in 0..SALT_LENGTH {
            salt.push(SALT_CHARSET[rng.gen_range(0..SALT_CHARSET.len())] as char);
        }
        salt
    }
}

// ————— 便捷函数（默认提供者 = C# 默认 sha512）————

/// 用默认算法生成口令散列。
pub fn hash(password: &str) -> String {
    PasswordProvider::new().hash(password)
}

/// 用默认算法验证口令。
pub fn verify(password: &str, hash: &str) -> bool {
    PasswordProvider::new().verify(password, hash)
}

/// 该散列是否建议升级为当前算法格式。
pub fn needs_upgrade(hash: &str) -> bool {
    PasswordProvider::new().needs_upgrade(hash)
}

/// 验证 + 按需升级（返回新散列，`None` = 无需改写）。
pub fn verify_and_upgrade(password: &str, hash: &str) -> (bool, Option<String>) {
    PasswordProvider::new().verify_and_upgrade(password, hash)
}

/// 是否为旧式散列（不含 `$`，即明文/MD5；C# 升级判据 `!Password.Contains("$")`）。
pub fn is_legacy(hash: &str) -> bool {
    !hash.trim().contains('$')
}

/// 解析散列里的算法名（旧式/异常返回 `None`）。
pub fn algorithm_of(hash: &str) -> Option<&str> {
    let hash = hash.trim();
    let mut parts = hash.split('$');
    if parts.next() != Some("") {
        return None;
    }
    parts.next()
}

// ————— 内部实现（逐行对齐 C# SaltPasswordProvider）————

/// 按算法计算散列体（不含 `$` 前缀）。
fn compute(algorithm: &str, password: &str, salt: &str) -> Option<String> {
    let salt_bytes = salt.as_bytes();
    Some(match algorithm {
        // (password.MD5() + salt).MD5()；C# ToHex 为**大写**
        "md5" => md5_hex_upper(&format!("{}{salt}", md5_hex_upper(password))),
        // password.GetBytes().SHA1(salt.GetBytes()).ToBase64()
        "sha1" => base64_encode(&hmac_sha1(salt_bytes, password.as_bytes())),
        // password.GetBytes().SHA512(salt.GetBytes()).ToBase64()
        "sha512" => base64_encode(&hmac_sha512(salt_bytes, password.as_bytes())),
        // password.GetBytes().MD5().SHA1(salt.GetBytes()).ToBase64()
        "md5+sha1" => base64_encode(&hmac_sha1(salt_bytes, &md5_bytes(password.as_bytes()))),
        // password.GetBytes().MD5().SHA512(salt.GetBytes()).ToBase64()
        "md5+sha512" => base64_encode(&hmac_sha512(salt_bytes, &md5_bytes(password.as_bytes()))),
        _ => return None,
    })
}

/// HMAC-SHA1（对齐 C# `SecurityHelper.SHA1(data, key)` = `HMACSHA1(key).ComputeHash(data)`）。
fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// HMAC-SHA512（对齐 C# `SecurityHelper.SHA512(data, key)`）。
fn hmac_sha512(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha512>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// MD5 十六进制**大写**（NewLife `String.MD5()` 的 `ToHex()` 输出为大写）。
fn md5_hex_upper(text: &str) -> String {
    super::hex_encode(&md5_bytes(text.as_bytes())).to_ascii_uppercase()
}

/// MD5 十六进制小写（PHP 端 `md5()` 口径，旧值比对用）。
fn md5_hex_lower(text: &str) -> String {
    super::hex_encode(&md5_bytes(text.as_bytes()))
}

/// 文本比较（大小写不敏感；等长时按固定时间比较）。
fn eq_text(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.len() != b.len() {
        return false;
    }
    fixed_time_eq(a.as_bytes(), b.as_bytes())
}

/// 文本比较（**忽略大小写**；旧式明文/MD5 与十六进制散列用，对齐 C# `EqualIgnoreCase`）。
fn eq_text_ignore_case(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.len() != b.len() {
        return false;
    }
    fixed_time_eq(
        a.to_ascii_lowercase().as_bytes(),
        b.to_ascii_lowercase().as_bytes(),
    )
}

/// 是否形如 32 位十六进制（前端传来的 MD5）。
fn looks_like_md5_hex(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定盐（真实盐字符集范围内的样例）便于确定性断言。
    const SALT: &str = "abCD12./xyZ9012Q";
    const PASS: &str = "New#life";

    #[test]
    fn default_provider_matches_csharp_defaults() {
        let prv = PasswordProvider::new();
        assert_eq!(prv.algorithm(), "sha512");
        assert_eq!(prv.salt_time(), 0);
        assert_eq!(DEFAULT_ALGORITHM, prv.algorithm());
    }

    #[test]
    fn hash_format_is_dollar_delimited() {
        let hash = hash(PASS);
        let parts: Vec<&str> = hash.split('$').collect();
        assert_eq!(parts.len(), 4, "格式 $算法$盐$散列：{hash}");
        assert_eq!(parts[0], "");
        assert_eq!(parts[1], "sha512");
        assert_eq!(parts[2].len(), SALT_LENGTH);
        assert!(parts[2].bytes().all(|b| SALT_CHARSET.contains(&b)));
        assert!(!parts[3].is_empty());
    }

    #[test]
    fn every_algorithm_round_trips() {
        for alg in ALGORITHMS {
            let prv = PasswordProvider::with_algorithm(alg).unwrap();
            let stored = prv.hash(PASS);
            assert_eq!(algorithm_of(&stored), Some(alg));
            assert!(prv.verify(PASS, &stored), "算法 {alg} 应能验证通过");
            assert!(!prv.verify("wrong", &stored), "算法 {alg} 错误口令应失败");
            assert!(!prv.needs_upgrade(&stored), "算法 {alg} 新值无需升级");
        }
    }

    #[test]
    fn unknown_algorithm_is_rejected() {
        assert!(PasswordProvider::with_algorithm("bcrypt").is_err());
        assert!(
            PasswordProvider::with_algorithm("SHA512").is_ok(),
            "大小写不敏感"
        );
    }

    #[test]
    fn legacy_plaintext_and_md5_verify() {
        let prv = PasswordProvider::new();
        // 明文（PHP 端可能直接存过）
        assert!(prv.verify(PASS, PASS));
        // MD5 小写（PHP `md5()`）
        let lower = md5_hex_lower(PASS);
        assert_eq!(lower.len(), 32);
        assert!(prv.verify(PASS, &lower));
        assert!(prv.verify(PASS, &lower.to_ascii_uppercase()));
        // 错误口令/空散列
        assert!(!prv.verify("wrong", &lower));
        assert!(!prv.verify(PASS, ""));
    }

    #[test]
    fn legacy_hash_is_upgraded_on_verify() {
        let prv = PasswordProvider::new();
        let legacy = md5_hex_lower(PASS);
        let (ok, upgraded) = prv.verify_and_upgrade(PASS, &legacy);
        assert!(ok);
        let upgraded = upgraded.expect("旧值应升级");
        assert_eq!(algorithm_of(&upgraded), Some("sha512"));
        assert!(prv.verify(PASS, &upgraded), "升级后的口令仍可验证");

        // 已是新格式：不再升级
        let (ok, again) = prv.verify_and_upgrade(PASS, &upgraded);
        assert!(ok);
        assert!(again.is_none());
    }

    #[test]
    fn algorithm_change_triggers_upgrade() {
        let old = PasswordProvider::with_algorithm("md5").unwrap();
        let stored = old.hash(PASS);
        let new = PasswordProvider::with_algorithm("sha512").unwrap();
        assert!(new.verify(PASS, &stored), "跨算法验证必须通过");
        assert!(new.needs_upgrade(&stored));
        let (ok, upgraded) = new.verify_and_upgrade(PASS, &stored);
        assert!(ok);
        assert_eq!(algorithm_of(&upgraded.unwrap()), Some("sha512"));
    }

    #[test]
    fn md5_accepts_client_side_md5_hex() {
        // C# `Md5Test_Web`：数据库 md5+md5，客户端上传 MD5 也能通过
        let prv = PasswordProvider::with_algorithm("md5").unwrap();
        let stored = prv.hash(PASS);
        assert!(prv.verify(&md5_hex_upper(PASS), &stored), "大写十六进制");
        assert!(
            prv.verify(&md5_hex_lower(PASS), &stored),
            "小写十六进制（JS 常见）"
        );
    }

    #[test]
    fn md5_sha_algorithms_accept_md5_hex_and_plain_sha() {
        for alg in ["md5+sha1", "md5+sha512"] {
            let prv = PasswordProvider::with_algorithm(alg).unwrap();
            let stored = prv.hash(PASS);
            // 传明文
            assert!(prv.verify(PASS, &stored));
            // 传 MD5 十六进制（C# 支路 3）
            assert!(prv.verify(&md5_hex_upper(PASS), &stored), "{alg} 大写 MD5");
            assert!(prv.verify(&md5_hex_lower(PASS), &stored), "{alg} 小写 MD5");
            // 兼容"纯 SHA"分支：手工构造
            let pure = if alg == "md5+sha1" {
                base64_encode(&hmac_sha1(SALT.as_bytes(), PASS.as_bytes()))
            } else {
                base64_encode(&hmac_sha512(SALT.as_bytes(), PASS.as_bytes()))
            };
            let crafted = format!("${alg}${SALT}${pure}");
            assert!(prv.verify(PASS, &crafted), "{alg} 应兼容纯 SHA 散列");
        }
    }

    #[test]
    fn sha_alone_rejects_client_side_md5() {
        // C# `SHA512Test_Web`：纯 SHA 算法不接受客户端 MD5
        let prv = PasswordProvider::with_algorithm("sha512").unwrap();
        let stored = prv.hash(PASS);
        assert!(!prv.verify(&md5_hex_upper(PASS), &stored));
    }

    #[test]
    fn salt_time_window_is_enforced() {
        let prv = PasswordProvider::with_algorithm("md5")
            .unwrap()
            .with_salt_time(30);
        let stored = prv.hash(PASS);
        let parts: Vec<&str> = stored.split('$').collect();
        let salt: i64 = parts[2].parse().expect("时间盐应为 Unix 秒");
        assert!((crate::times::gettimestamp() as i64 - salt).abs() <= 2);
        assert!(prv.verify(PASS, &stored), "窗口内应通过");

        // 手工把盐改到 10 分钟前 → 必须拒绝
        let old_salt = (crate::times::gettimestamp() as i64 - 600).to_string();
        let stale = prv.hash_with_salt(PASS, &old_salt);
        assert!(!prv.verify(PASS, &stale), "超时盐必须拒绝");
    }

    #[test]
    fn malformed_hash_does_not_panic() {
        let prv = PasswordProvider::new();
        for bad in [
            "$",
            "$$",
            "$sha512",
            "$sha512$abc",
            "$sha512$abc$",
            "x$y$z",
            "$未知$s$h",
        ] {
            assert!(!prv.verify(PASS, bad), "异常值应判失败：{bad}");
        }
        assert!(prv.needs_upgrade("$未知$s$h"));
        assert!(is_legacy("abcdef"));
        assert!(!is_legacy("$sha512$s$h"));
    }

    /// C# 互通黄金样本：**由 C# `NewLife.Security.SaltPasswordProvider` 同款算法生成**
    /// （`DHRustDemo password-hash-fixed`，盐固定为 `abCD12./xyZ9012Q`）；
    /// 生成脚本 `scripts/password-interop.ps1`。任一侧改动算法都会在此报警。
    #[test]
    fn matches_csharp_golden_hashes() {
        for (alg, stored) in CSHARP_GOLDEN {
            let prv = PasswordProvider::with_algorithm(*alg).unwrap();
            assert_eq!(
                &prv.hash_with_salt(GOLDEN_PASSWORD, GOLDEN_SALT),
                stored,
                "算法 {alg} 散列与 C# 不一致"
            );
            assert!(
                prv.verify(GOLDEN_PASSWORD, stored),
                "C# 散列验证失败（{alg}）：{stored}"
            );
            assert!(!prv.needs_upgrade(stored), "同算法不应升级：{stored}");
        }
    }

    const GOLDEN_PASSWORD: &str = "New#life";
    const GOLDEN_SALT: &str = "abCD12./xyZ9012Q";
    const CSHARP_GOLDEN: &[(&str, &str)] = &[
        (
            "md5",
            "$md5$abCD12./xyZ9012Q$F66328434CFAEF3EC9B394358D6D9C36",
        ),
        (
            "sha1",
            "$sha1$abCD12./xyZ9012Q$fNkn6u3ykPZM4OPL/y6r6Jof2mY=",
        ),
        (
            "sha512",
            "$sha512$abCD12./xyZ9012Q$ctlLR2xz3b1a9ARbpCkOZEFLvc6A2pLG/BMTVq99TR1pr9JIMs70qKj/OMjohcBvU0+XAyRh6CsExEMfvybOKg==",
        ),
        (
            "md5+sha1",
            "$md5+sha1$abCD12./xyZ9012Q$FAXHeXyCpOSOPugOqcHia+ZVMOw=",
        ),
        (
            "md5+sha512",
            "$md5+sha512$abCD12./xyZ9012Q$pO5mQ5rFhoYeCn+a6s1GhcvwqSgzbgWOuK3fC2VSaNQ1SGk5TSErph0jK5ThlAfkAbzdr9UM6Pe/xQ17Zk24Kw==",
        ),
    ];
}
