//! 插件包格式 + 插件源（catalog）协议——Pek.RAgent（安装端/发布工具）与 Pek.RPanlServer
//! （插件平台）共用（feature `plugin`，隐含 `zip-extract`）。
//!
//! ## 插件包（zip）
//! - 包根或唯一子目录放 `plugin.json`（字段：`id/name/description/version/icon/entry/app`，
//!   除 `entry` 默认 `index.html` 外均可缺省）；
//! - `id` 缺省时按回退名（子目录名 / 上传文件名 stem）取；平台侧要求 `version` 必填；
//! - 解压经 `dhrust::zip::extract_zip`（防目录穿越）。
//!
//! ## 插件源（catalog）
//! - `catalog.json`（`{"name":…,"plugins":[…],"updated":…}`）+ 同址 `.sig`（base64 Ed25519，
//!   对 catalog 原文签名）；
//! - 条目 `url` 必须为 https（仅 `127.0.0.1` 例外）、`sha256` 为包内容哈希（消费端强制校验）；
//! - 公钥为 32 字节裸 hex（也接受 44 字节 SPKI DER）；私钥为 32 字节 seed hex 文件。
//!
//! 来源：Pek.RAgent `plugins.rs` 与 Pek.RPanlServer `store.rs`/`plugins.rs` 的去重下沉（2026-10-05）。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};

/// 便捷再导出（消费方无需直接依赖 ed25519-dalek）。
pub use ed25519_dalek::{SigningKey, VerifyingKey};

/// 插件清单 / 插件包大小上限。
pub const MAX_CATALOG_SIZE: usize = 1024 * 1024;
pub const MAX_PACKAGE_SIZE: usize = 32 * 1024 * 1024;

// ————— 校验 —————

/// 插件 id（目录名）校验：1~64 位 `[A-Za-z0-9._-]`，且非 `.`/`..`。
pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err("插件 id 不合法（仅允许字母/数字/._-，≤64 位）".to_string())
    }
}

/// 版本号校验（`[A-Za-z0-9._-]`，1~50 位；同时用作文件名片段）。
pub fn validate_version(version: &str) -> Result<(), String> {
    let ok = !version.is_empty()
        && version.len() <= 50
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err("版本号不合法（仅允许字母/数字/._-，≤50 位）".to_string())
    }
}

/// URL 放行规则（插件源）：https 一律允许；http 仅 127.0.0.1（本地调试）。
pub fn is_allowed_store_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("https://")
        || u.starts_with("http://127.0.0.1:")
        || u.starts_with("http://127.0.0.1/")
}

// ————— 清单 —————

/// 插件清单（解析产物；字段均已修剪/缺省/钳位）。
#[derive(Clone, Debug)]
pub struct Manifest {
    /// 插件 id
    pub id: String,
    /// 显示名（缺省 = id）
    pub name: String,
    /// 描述
    pub description: String,
    /// 版本号（消费端可空；平台要求必填）
    pub version: String,
    /// 图标（缺省 🧩）
    pub icon: String,
    /// 关联子服务名
    pub app: String,
    /// 入口文件（相对路径，默认 index.html；已校验存在）
    pub entry: String,
}

impl Manifest {
    /// 面板列表输出（Pek.RAgent 既有字段集）。
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "version": self.version,
            "icon": self.icon,
            "entry": self.entry,
            "app": self.app,
        })
    }
}

/// 包检查参数。
#[derive(Clone, Debug)]
pub struct InspectOptions {
    /// 字节上限（0 = 不限；默认 [`MAX_PACKAGE_SIZE`]）
    pub max_bytes: usize,
    /// 扁平包（`plugin.json` 在包根）时的 id 回退名
    pub fallback_id: Option<String>,
    /// 是否要求 `version` 必填（平台侧 true；Agent 安装/发布 false）
    pub require_version: bool,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            max_bytes: MAX_PACKAGE_SIZE,
            fallback_id: None,
            require_version: false,
        }
    }
}

/// 校验插件包（zip 字节）并读取清单（临时目录解压检查，不落任何持久文件）。
pub fn inspect_package(data: &[u8], opts: &InspectOptions) -> Result<Manifest, String> {
    if data.is_empty() {
        return Err("插件包为空".to_string());
    }
    if opts.max_bytes > 0 && data.len() > opts.max_bytes {
        return Err(format!(
            "插件包过大（上限 {}MB）",
            opts.max_bytes / 1024 / 1024
        ));
    }
    if !data.starts_with(b"PK") {
        return Err("文件格式校验失败：不是 zip 包".to_string());
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "dh-plugin-inspect-{}-{}",
        std::process::id(),
        nonce
    ));
    let zip_path = dir.with_extension("inspect.zip");
    fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    fs::write(&zip_path, data).map_err(|e| format!("写入临时文件失败：{e}"))?;
    let result = (|| -> Result<Manifest, String> {
        crate::zip::extract_zip(&zip_path, &dir)?;
        let (src, folder) = locate_plugin_root(&dir)?;
        let fallback = folder.or_else(|| opts.fallback_id.clone());
        read_manifest_at(&src, fallback.as_deref(), opts.require_version)
    })();
    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&dir);
    result
}

/// 定位解压目录中的插件根（根含 `plugin.json`，或唯一子目录含之；
/// 跳过隐藏目录与 `__MACOSX`；多候选报错）。
pub fn locate_plugin_root(staging: &Path) -> Result<(PathBuf, Option<String>), String> {
    if staging.join("plugin.json").is_file() {
        return Ok((staging.to_path_buf(), None));
    }
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir(staging) {
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.eq_ignore_ascii_case("__MACOSX") {
                continue;
            }
            if path.join("plugin.json").is_file() {
                found.push((path, name));
            }
        }
    }
    match found.len() {
        1 => {
            let (path, name) = found.remove(0);
            Ok((path, Some(name)))
        }
        0 => Err("压缩包中未找到 plugin.json（可置于包根或唯一子目录）".to_string()),
        _ => Err("压缩包含多个插件目录，请单个插件单独打包".to_string()),
    }
}

/// 读取并校验插件清单（`dir` = 插件根）。
///
/// id 取 `plugin.json.id`，缺省用 `fallback_id`；`version` 非空时按 [`validate_version`]
/// 校验，`require_version` = true 时空版本报错。
pub fn read_manifest_at(
    dir: &Path,
    fallback_id: Option<&str>,
    require_version: bool,
) -> Result<Manifest, String> {
    let json = read_manifest_json(dir)?;
    let id = {
        let v = field_of(&json, "id");
        if v.is_empty() {
            fallback_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "无法确定插件 id：plugin.json 缺 id 字段且无回退名".to_string()
                })?
                .to_string()
        } else {
            v
        }
    };
    validate_id(&id)?;
    build_manifest(dir, &json, &id, require_version, true)
}

/// 读取“已安装目录”的清单（`id` 由目录名给定、**优先生效**——在线安装落库后
/// `plugin.json` 的 id 字段不参与判断；版本宽松校验，保证历史安装不因版本串被拒）。
pub fn read_installed_manifest(dir: &Path, id: &str) -> Result<Manifest, String> {
    validate_id(id)?;
    let json = read_manifest_json(dir)?;
    build_manifest(dir, &json, id, false, false)
}

/// 读取 `plugin.json`（须存在且为 JSON 对象）。
fn read_manifest_json(dir: &Path) -> Result<Json, String> {
    let text = fs::read_to_string(dir.join("plugin.json"))
        .map_err(|_| "缺少 plugin.json".to_string())?;
    let man: Json =
        serde_json::from_str(&text).map_err(|e| format!("plugin.json 解析失败：{e}"))?;
    if !man.is_object() {
        return Err("plugin.json 需为 JSON 对象".to_string());
    }
    Ok(man)
}

/// 由清单 JSON 组装 [`Manifest`]（入口校验 + 缺省 + 钳位 + 版本策略）。
fn build_manifest(
    dir: &Path,
    man: &Json,
    id: &str,
    require_version: bool,
    strict_version: bool,
) -> Result<Manifest, String> {
    let entry = {
        let e = field_of(man, "entry");
        if e.is_empty() {
            "index.html".to_string()
        } else {
            e
        }
    };
    if entry
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == ".." || s.contains('\\') || s.contains(':'))
    {
        return Err("entry 路径不合法".to_string());
    }
    if !dir.join(&entry).is_file() {
        return Err(format!("入口文件不存在：{entry}"));
    }
    let version = field_of(man, "version");
    if require_version && version.is_empty() {
        return Err("plugin.json 缺少 version（平台按版本管理插件包）".to_string());
    }
    if strict_version && !version.is_empty() {
        validate_version(&version)?;
    }
    let name = {
        let n = field_of(man, "name");
        if n.is_empty() {
            id.to_string()
        } else {
            n
        }
    };
    let icon = {
        let i = field_of(man, "icon");
        if i.is_empty() {
            "🧩".to_string()
        } else {
            i
        }
    };
    Ok(Manifest {
        id: id.to_string(),
        name: clamp_text(&name, 100),
        description: clamp_text(&field_of(man, "description"), 500),
        version,
        icon: clamp_text(&icon, 16),
        app: clamp_text(&field_of(man, "app"), 100),
        entry,
    })
}

/// 读取字符串字段（修剪；缺省空串）。
fn field_of(man: &Json, key: &str) -> String {
    man.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 截断到指定字符数（防字段超长）。
fn clamp_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

// ————— 密钥与签名 —————

/// hex 编码（小写）。
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// hex 解码（奇数长度或非法字符返回 `None`）。
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        out.push((val(bytes[i])? << 4) | val(bytes[i + 1])?);
        i += 2;
    }
    Some(out)
}

/// 生成 Ed25519 私钥 seed（32 字节 OS 熵 → 64 位 hex；写入 `plugin-store.key`）。
pub fn generate_signing_key() -> String {
    hex_encode(&crate::random::bytes(32))
}

/// 由 32 字节 seed hex 解析签名私钥。
pub fn signing_key_from_hex(hex_text: &str) -> Result<SigningKey, String> {
    let bytes = hex_decode(hex_text.trim()).ok_or_else(|| "私钥不是有效的 hex 文本".to_string())?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| "私钥长度应为 32 字节".to_string())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// 读取私钥文件（hex seed；文本可为多行，取修剪后内容）。
pub fn signing_key_from_file(path: &Path) -> Result<SigningKey, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("读取私钥失败（{}）：{e}", path.display()))?;
    signing_key_from_hex(&text)
}

/// 公钥 hex（32 字节裸公钥；写入配置 `PluginStorePubKey`）。
pub fn pubkey_hex(key: &SigningKey) -> String {
    hex_encode(&key.verifying_key().to_bytes())
}

/// 解析十六进制公钥（32 字节裸公钥，或 44 字节 Ed25519 SPKI DER）。
pub fn parse_pubkey(pubkey_hex: &str) -> Result<VerifyingKey, String> {
    let clean = pubkey_hex.trim().trim_start_matches("0x").trim();
    let bytes = hex_decode(clean).ok_or_else(|| "公钥不是有效的 hex".to_string())?;
    // Ed25519 SPKI DER 前缀 302a300506032b6570032100（12 字节）+ 32 字节裸公钥
    const SPKI: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let raw: [u8; 32] = if bytes.len() == 32 {
        bytes.as_slice().try_into().unwrap()
    } else if bytes.len() == 44 && bytes.starts_with(&SPKI) {
        bytes[12..].try_into().unwrap()
    } else {
        return Err("公钥长度应为 32 字节（或 44 字节 SPKI DER）".to_string());
    };
    VerifyingKey::from_bytes(&raw).map_err(|e| format!("公钥无效：{e}"))
}

/// 对数据签名，返回 base64（Ed25519，64 字节签名）。
pub fn sign_base64(key: &SigningKey, data: &[u8]) -> String {
    use ed25519_dalek::Signer;
    crate::sign::base64_encode(&key.sign(data).to_bytes())
}

/// 校验 base64 签名（`pubkey_hex` 接受 32 字节裸公钥或 44 字节 SPKI DER）。
pub fn verify_base64(pubkey_hex: &str, data: &[u8], sig_base64: &str) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier};
    let key = parse_pubkey(pubkey_hex)?;
    let sig_bytes = crate::sign::base64_decode(sig_base64.trim())
        .ok_or_else(|| "签名文件不是有效的 base64".to_string())?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "签名长度不是 64 字节".to_string())?;
    let sig = Signature::from_bytes(&sig_arr);
    key.verify(data, &sig)
        .map_err(|_| "插件源签名校验失败（目录可能被篡改）".to_string())
}

/// 对目录文件（catalog.json）签名，产出同目录 `{文件名}.sig`（base64，Ed25519）。
pub fn sign_catalog_file(catalog: &Path, key_file: &Path) -> Result<(), String> {
    let key = signing_key_from_file(key_file)?;
    let data = fs::read(catalog).map_err(|e| format!("读取 {} 失败：{e}", catalog.display()))?;
    let sig_path = PathBuf::from(format!("{}.sig", catalog.display()));
    fs::write(&sig_path, sign_base64(&key, &data)).map_err(|e| format!("写入签名失败：{e}"))
}

// ————— 插件源条目（catalog.json） —————

/// 在线插件源条目（`catalog.json` 中的一项）。
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    /// 开发商/上传者（可选字段；平台侧为归属账号名）
    pub author: String,
    pub version: String,
    pub icon: String,
    pub app: String,
    pub url: String,
    pub sha256: String,
}

impl CatalogEntry {
    /// 解析校验单个清单条目（无效即错误；调用方可选择忽略并记录日志）。
    pub fn from_json(item: &Json) -> Result<Self, String> {
        let field = |k: &str| {
            item.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("")
                .to_string()
        };
        let id = field("id");
        validate_id(&id)?;
        let url = field("url");
        if !is_allowed_store_url(&url) {
            return Err(format!("{id}：url 仅允许 https（127.0.0.1 例外）"));
        }
        let sha256 = field("sha256").to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{id}：sha256 缺失或格式错误"));
        }
        let name = {
            let n = field("name");
            if n.is_empty() { id.clone() } else { n }
        };
        let icon = {
            let i = field("icon");
            if i.is_empty() {
                "🧩".to_string()
            } else {
                i
            }
        };
        Ok(Self {
            id,
            name,
            description: field("description"),
            author: field("author"),
            version: field("version"),
            icon,
            app: field("app"),
            url,
            sha256,
        })
    }

    /// 序列化（平台侧生成 catalog 用；字段集与 [`CatalogEntry::from_json`] 对齐）。
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "author": self.author,
            "version": self.version,
            "icon": self.icon,
            "app": self.app,
            "url": self.url,
            "sha256": self.sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dh-plugin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = crate::zip::ZipWriter::new();
        for (name, data) in files {
            zip.add_file(name, data.as_bytes());
        }
        zip.finish()
    }

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    #[test]
    fn validate_id_and_version_rules() {
        assert!(validate_id("demo").is_ok());
        assert!(validate_id("my.plugin_1").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", "中 文", "a:b"] {
            assert!(validate_id(bad).is_err(), "{bad} 应被拒绝");
        }
        assert!(validate_id(&"a".repeat(65)).is_err());
        assert!(validate_version("1.0.0").is_ok());
        assert!(validate_version("1.0 beta").is_err());
        assert!(validate_version(&"1".repeat(51)).is_err());
    }

    #[test]
    fn hex_and_pubkey_parsing() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(hex_encode(&[0x00, 0xff, 0x10]), "00ff10");
        assert!(hex_decode("0").is_none());
        assert!(hex_decode("zz").is_none());
        let pub_hex = pubkey_hex(&test_key());
        assert!(parse_pubkey(&pub_hex).is_ok());
        // SPKI DER 形式（12 字节前缀 + 32 字节裸公钥）
        let mut spki = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        spki.extend_from_slice(test_key().verifying_key().as_bytes());
        assert!(parse_pubkey(&hex_encode(&spki)).is_ok());
        assert!(parse_pubkey("abcd").is_err());
    }

    #[test]
    fn inspect_folder_package_defaults_and_clamps() {
        let long_name = "名".repeat(150);
        let zip = build_zip(&[
            (
                "demo/plugin.json",
                &format!(
                    r#"{{"name":"{long_name}","version":"1.0.0","entry":"main.html","app":"svc"}}"#
                ),
            ),
            ("demo/main.html", "<html>ok</html>"),
            ("demo/js/app.js", "x"),
        ]);
        let m = inspect_package(&zip, &InspectOptions::default()).unwrap();
        assert_eq!(m.id, "demo");
        assert_eq!(m.name.chars().count(), 100);
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.entry, "main.html");
        assert_eq!(m.app, "svc");
        assert_eq!(m.icon, "🧩");
    }

    #[test]
    fn inspect_flat_package_id_resolution() {
        let flat = build_zip(&[
            ("plugin.json", r#"{"name":"扁平","version":"9"}"#),
            ("index.html", "x"),
        ]);
        // 无回退名 → 拒绝
        assert!(inspect_package(&flat, &InspectOptions::default()).is_err());
        // 回退名生效（Agent 上传文件名 stem / 在线目录 id）
        let opts = InspectOptions {
            fallback_id: Some("flatdemo".to_string()),
            ..Default::default()
        };
        let m = inspect_package(&flat, &opts).unwrap();
        assert_eq!(m.id, "flatdemo");
        assert_eq!(m.entry, "index.html");
        // 清单自带 id 优先于回退名
        let flat2 = build_zip(&[
            ("plugin.json", r#"{"id":"inner","version":"9"}"#),
            ("index.html", "x"),
        ]);
        assert_eq!(inspect_package(&flat2, &opts).unwrap().id, "inner");
    }

    #[test]
    fn inspect_rejects_bad_packages() {
        let opts = InspectOptions::default();
        assert!(inspect_package(b"", &opts).unwrap_err().contains("空"));
        assert!(inspect_package(b"not a zip", &opts)
            .unwrap_err()
            .contains("zip"));
        let no_manifest = build_zip(&[("readme.txt", "x")]);
        assert!(inspect_package(&no_manifest, &opts)
            .unwrap_err()
            .contains("plugin.json"));
        // 多候选目录
        let multi = build_zip(&[
            ("a/plugin.json", r#"{"version":"1"}"#),
            ("a/index.html", "x"),
            ("b/plugin.json", r#"{"version":"1"}"#),
            ("b/index.html", "x"),
        ]);
        assert!(inspect_package(&multi, &opts).unwrap_err().contains("多个"));
        // 入口缺失
        let no_entry = build_zip(&[("p/plugin.json", r#"{"id":"p","version":"1"}"#)]);
        assert!(inspect_package(&no_entry, &opts)
            .unwrap_err()
            .contains("入口"));
        // 包过大
        let big_opts = InspectOptions {
            max_bytes: 8,
            ..Default::default()
        };
        let small = build_zip(&[("a.txt", "x")]);
        assert!(inspect_package(&small, &big_opts)
            .unwrap_err()
            .contains("过大"));
    }

    #[test]
    fn inspect_version_policy() {
        let zip = build_zip(&[
            ("p/plugin.json", r#"{"id":"p"}"#),
            ("p/index.html", "x"),
        ]);
        // 平台要求 version 必填
        let strict = InspectOptions {
            require_version: true,
            ..Default::default()
        };
        assert!(inspect_package(&zip, &strict)
            .unwrap_err()
            .contains("version"));
        // Agent 模式可空
        assert_eq!(inspect_package(&zip, &InspectOptions::default()).unwrap().version, "");
        // 版本号含非法字符 → 拒绝
        let bad = build_zip(&[
            ("p/plugin.json", r#"{"id":"p","version":"1.0 beta"}"#),
            ("p/index.html", "x"),
        ]);
        assert!(inspect_package(&bad, &InspectOptions::default())
            .unwrap_err()
            .contains("版本号"));
        // id 非法
        let badid = build_zip(&[
            ("p/plugin.json", r#"{"id":"a/b","version":"1"}"#),
            ("p/index.html", "x"),
        ]);
        assert!(inspect_package(&badid, &InspectOptions::default()).is_err());
    }

    #[test]
    fn installed_manifest_authoritative_id_and_lenient_version() {
        let dir = temp_dir("installed");
        fs::write(
            dir.join("plugin.json"),
            r#"{"id":"other","name":"n","version":"1.0 beta"}"#,
        )
        .unwrap();
        fs::write(dir.join("index.html"), "x").unwrap();
        let m = read_installed_manifest(&dir, "folder-name").unwrap();
        assert_eq!(m.id, "folder-name");
        assert_eq!(m.version, "1.0 beta");
        assert!(read_installed_manifest(&dir, "bad/id").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keygen_sign_verify_roundtrip_and_tamper() {
        let seed_hex = generate_signing_key();
        assert_eq!(seed_hex.len(), 64);
        let key = signing_key_from_hex(&seed_hex).unwrap();
        let pub_hex = pubkey_hex(&key);
        let data = b"catalog bytes".as_slice();
        let sig = sign_base64(&key, data);
        assert!(verify_base64(&pub_hex, data, &sig).is_ok());
        assert!(verify_base64(&pub_hex, b"tampered", &sig).is_err());
        assert!(verify_base64(&pub_hex, data, "not-base64!!!").is_err());
        // 他人重签 → 拒绝
        let other = SigningKey::from_bytes(&[7u8; 32]);
        assert!(verify_base64(&pub_hex, data, &sign_base64(&other, data)).is_err());
        // 私钥文本错误
        assert!(signing_key_from_hex("zz").is_err());
        assert!(signing_key_from_hex("abcd").is_err());
    }

    #[test]
    fn sign_catalog_file_writes_sig() {
        let dir = temp_dir("signfile");
        let catalog = dir.join("catalog.json");
        let key_file = dir.join("plugin-store.key");
        fs::write(&catalog, r#"{"name":"测试源","plugins":[]}"#).unwrap();
        fs::write(&key_file, hex_encode(&[42u8; 32])).unwrap();
        sign_catalog_file(&catalog, &key_file).unwrap();
        let sig = fs::read_to_string(dir.join("catalog.json.sig")).unwrap();
        let pub_hex = pubkey_hex(&test_key());
        assert!(verify_base64(&pub_hex, &fs::read(&catalog).unwrap(), &sig).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_entry_roundtrip_and_validation() {
        let item = json!({
            "id": "demo",
            "name": "演示",
            "description": "d",
            "author": "dev1",
            "version": "1.0.0",
            "icon": "X",
            "app": "svc",
            "url": "https://example.com/demo-1.0.0.zip",
            "sha256": "AABB".to_string() + &"0".repeat(60),
        });
        let e = CatalogEntry::from_json(&item).unwrap();
        assert_eq!(e.id, "demo");
        assert_eq!(e.sha256.len(), 64);
        assert!(e.sha256.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        let back = CatalogEntry::from_json(&e.to_json()).unwrap();
        assert_eq!(back.id, "demo");
        assert_eq!(back.url, e.url);
        // 非法：http 外链 / sha256 缺失 / id 非法
        let mut bad = item.clone();
        bad["url"] = json!("http://evil.com/x.zip");
        assert!(CatalogEntry::from_json(&bad).is_err());
        let mut bad = item.clone();
        bad["sha256"] = json!("abc");
        assert!(CatalogEntry::from_json(&bad).is_err());
        let mut bad = item.clone();
        bad["id"] = json!("a/b");
        assert!(CatalogEntry::from_json(&bad).is_err());
    }

    #[test]
    fn allowed_store_url_rules() {
        assert!(is_allowed_store_url("https://example.com/catalog.json"));
        assert!(is_allowed_store_url("http://127.0.0.1:5590/store/catalog.json"));
        assert!(is_allowed_store_url("http://127.0.0.1/x"));
        assert!(!is_allowed_store_url("http://example.com/x"));
        assert!(!is_allowed_store_url("ftp://example.com/x"));
    }
}
