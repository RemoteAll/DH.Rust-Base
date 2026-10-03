//! 静态文件服务（默认约定 `wwwroot/` 目录）：MIME、默认文档、路径穿越防护、嵌入资源。
//!
//! 对齐 C#/NewLife 的静态资源与 `MapEmbedded` 惯例：
//! - **目录模式**：[`StaticFiles::new`]（或 [`StaticFiles::default`] = `wwwroot`），
//!   按请求读盘（开发友好，改文件即生效）；
//! - **嵌入模式**：[`StaticFiles::embed`] / [`StaticFiles::embed_many`] 注册编译期资源
//!   （`include_bytes!`/`include_str!`），与目录叠加（嵌入优先），适合单文件部署；
//!   批量资源表一般由消费方 `build.rs` 扫描前端 `dist` 生成（见 `Doc/SPA与MVC一体化.md`）；
//! - **SPA 回退**：[`StaticFiles::spa_fallback`] 启用后，未命中的“前端路由”路径回退
//!   `index.html`（对齐 ASP.NET Core `MapFallbackToFile("index.html")`）；后端前缀
//!   （如 `/api`）用 [`StaticFiles::spa_excludes`] 排除，保持 JSON 404；
//! - **约定**：路径 `/` 或目录尾斜杠命中 `index.html` 默认文档；
//!   `..`/反斜杠/盘符/空段等危险路径直接拒绝（防目录穿越）；
//! - **用法**：通常挂到路由 fallback——命中返回文件，未命中 `None` 由调用方决定 404：
//!
//! ```no_run
//! use dhrust::net::http::{HttpOutcome, HttpResponse};
//! use dhrust::net::router::{route, Router};
//! use dhrust::net::static_files::StaticFiles;
//!
//! // SPA 一体化：嵌入构建产物 + 深链接回退；/api 前缀排除（保持 JSON 404）
//! let statics = StaticFiles::new("wwwroot") // 开发期可放磁盘目录（嵌入优先）
//!     .embed("/index.html", b"<html>spa</html>", "text/html; charset=utf-8")
//!     .spa_fallback(true)
//!     .spa_excludes(&["/api"]);
//!
//! let mut router = Router::new();
//! router.fallback(route(move |ctx| {
//!     let statics = statics.clone();
//!     async move {
//!         statics
//!             .try_serve_with_accept(&ctx.req.path, ctx.req.header("accept"))
//!             .map(HttpOutcome::Response)
//!             .unwrap_or_else(|| {
//!                 HttpOutcome::Response(HttpResponse::text(404, "Not Found"))
//!             })
//!     }
//! }));
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use super::http::HttpResponse;

/// 静态文件服务（目录 + 嵌入资源；`Clone` 可共享进闭包）。
#[derive(Clone)]
pub struct StaticFiles {
    /// 磁盘根目录（`None` = 仅嵌入资源）
    root: Option<PathBuf>,
    /// 嵌入资源（相对路径 → 数据 + Content-Type）
    embedded: HashMap<String, (&'static [u8], &'static str)>,
    /// SPA 回退：未命中的“前端路由”路径回退 index 文档（history 路由）
    spa: bool,
    /// SPA 排除前缀（已归一化：小写、前导 `/`、无尾 `/`）
    spa_excludes: Vec<String>,
}

impl StaticFiles {
    /// 以目录创建（目录不存在时全部未命中，不报错）。
    pub fn new(root: impl Into<PathBuf>) -> StaticFiles {
        StaticFiles {
            root: Some(root.into()),
            embedded: HashMap::new(),
            spa: false,
            spa_excludes: Vec::new(),
        }
    }

    /// 注册嵌入资源（编译期资源，如 `include_bytes!("webpanel/index.html")`）。
    ///
    /// 与目录资源叠加且**优先命中**（单文件部署不受运行目录影响）。
    pub fn embed(
        mut self,
        path: &str,
        data: &'static [u8],
        content_type: &'static str,
    ) -> StaticFiles {
        self.embedded
            .insert(normalize_key(path), (data, content_type));
        self
    }

    /// 批量注册嵌入资源（MIME 按扩展名自动推断；适合整张前端构建产物表）。
    ///
    /// 资源表通常由消费方 `build.rs` 扫描前端 `dist` 目录生成
    /// （生成器模板见 `Doc/SPA与MVC一体化.md`），形如：
    ///
    /// ```ignore
    /// include!(concat!(env!("OUT_DIR"), "/spa_files.rs")); // pub static SPA_FILES: &[(&str, &[u8])]
    /// let statics = StaticFiles::new("wwwroot").embed_many(SPA_FILES);
    /// ```
    pub fn embed_many(mut self, files: &[(&'static str, &'static [u8])]) -> StaticFiles {
        for (path, data) in files {
            self.embedded
                .insert(normalize_key(path), (*data, mime_type(path)));
        }
        self
    }

    /// 启用 SPA 回退：未命中的“前端路由”路径返回 `index.html`
    /// （对齐 ASP.NET Core `MapFallbackToFile("index.html")`）。
    ///
    /// 回退条件（任一，见 [`spa_route_like`]）：
    /// - 路径最后一段不含 `.`（如 `/dashboard`、`/user/42`、目录结尾）；
    /// - 浏览器导航（`Accept` 含 `text/html`，如 history 深链硬刷新）。
    ///
    /// 带扩展名的未命中资源（如 `/assets/missing.js`）不回退，保持 404；
    /// 后端前缀（`/api` 等）先用 [`StaticFiles::spa_excludes`] 排除。
    pub fn spa_fallback(mut self, enable: bool) -> StaticFiles {
        self.spa = enable;
        self
    }

    /// 设置 SPA 排除前缀：该前缀（含其子路径）下的未命中路径不回退 `index.html`。
    ///
    /// 例如 `&["/api", "/star"]`——后端命名空间的 404 保持 JSON 语义。
    /// 空串忽略；大小写不敏感；`/apiary` 不算 `/api` 的子路径（按段边界匹配）。
    pub fn spa_excludes(mut self, prefixes: &[&str]) -> StaticFiles {
        self.spa_excludes = prefixes.iter().filter_map(|p| normalize_prefix(p)).collect();
        self
    }

    /// 尝试服务请求路径（文件 + 已启用时的 SPA 回退；无 `Accept` 信息，回退按扩展名启发判断）。
    /// 未命中（含被拒绝的危险路径）返回 `None`。
    pub fn try_serve(&self, path: &str) -> Option<HttpResponse> {
        self.try_serve_with_accept(path, None)
    }

    /// 仅按文件服务（含默认文档与嵌入资源；**不做** SPA 回退）。
    ///
    /// 适合非 GET/HEAD 请求：只允许命中真实文件，未知路径仍交还调用方 404。
    pub fn try_serve_file(&self, path: &str) -> Option<HttpResponse> {
        let relative = safe_relative(path)?;
        self.fetch(&relative)
    }

    /// 尝试服务（带 `Accept` 头）：文件优先；已启用 SPA 回退且路径“像前端路由”时
    /// 返回 `index.html`（浏览器导航由 `Accept: text/html` 精确识别）。
    pub fn try_serve_with_accept(&self, path: &str, accept: Option<&str>) -> Option<HttpResponse> {
        let relative = safe_relative(path)?;
        if let Some(response) = self.fetch(&relative) {
            return Some(response);
        }
        if !self.spa || self.is_spa_excluded(path) || !spa_route_like(path, accept) {
            return None;
        }
        self.fetch("index.html")
    }

    /// 查嵌入资源（优先）→ 磁盘文件。
    fn fetch(&self, relative: &str) -> Option<HttpResponse> {
        if let Some((data, content_type)) = self.embedded.get(relative) {
            return Some(HttpResponse::bytes(200, content_type, *data));
        }

        let root = self.root.as_ref()?;
        let full = root.join(relative);
        let meta = std::fs::metadata(&full).ok()?;
        if !meta.is_file() {
            return None;
        }
        let data = std::fs::read(&full).ok()?;
        Some(HttpResponse::bytes(200, mime_type(relative), data))
    }

    /// 路径是否落在 SPA 排除前缀内（段边界匹配，大小写不敏感）。
    fn is_spa_excluded(&self, path: &str) -> bool {
        let plain = path
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        self.spa_excludes
            .iter()
            .any(|prefix| plain == *prefix || plain.starts_with(&format!("{prefix}/")))
    }
}

impl Default for StaticFiles {
    fn default() -> Self {
        // 默认约定：项目根下的 `wwwroot` 目录
        StaticFiles::new("wwwroot")
    }
}

/// 嵌入资源键归一化（去前导斜杠）。
fn normalize_key(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

/// SPA 排除前缀归一化：去空白、补前导 `/`、去尾 `/`、小写；空串返回 `None`。
fn normalize_prefix(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::new();
    if !trimmed.starts_with('/') {
        out.push('/');
    }
    out.push_str(trimmed);
    Some(out.to_ascii_lowercase())
}

/// SPA 回退判定：路径是否“像前端路由”。
///
/// - 浏览器导航（`Accept` 含 `text/html`）→ 视为路由（history 深链硬刷新）；
/// - 否则：最后一段不含 `.` → 视为路由（`/dashboard`、`/user/42`、目录结尾）。
fn spa_route_like(path: &str, accept: Option<&str>) -> bool {
    if let Some(accept) = accept {
        if accept.to_ascii_lowercase().contains("text/html") {
            return true;
        }
    }
    let plain = path.split(['?', '#']).next().unwrap_or("");
    let last = plain.rsplit('/').next().unwrap_or("");
    !last.contains('.')
}

/// 归一化请求路径为安全相对路径。
///
/// - 去查询/锚点、去前导 `/`；
/// - 空路径或目录结尾 → 追加 `index.html` 默认文档；
/// - 逐段安全检查：拒绝 `..`/`.`/空段、反斜杠、冒号（盘符）、百分号（编码穿越变体）。
fn safe_relative(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    let trimmed = path.trim_start_matches('/');
    let mut relative = if trimmed.is_empty() {
        "index.html".to_string()
    } else {
        trimmed.to_string()
    };
    if relative.ends_with('/') {
        relative.push_str("index.html");
    }

    for segment in relative.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment.contains(':')
            || segment.contains('%')
        {
            return None;
        }
    }
    Some(relative)
}

/// 常见 MIME 类型（按扩展名；未知回退 `application/octet-stream`）。
pub fn mime_type(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dhrust-static-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn serve_from_disk_with_default_document() {
        let dir = temp_dir("disk");
        std::fs::write(dir.join("index.html"), "<h1>首页</h1>").unwrap();
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(dir.join("app").join("index.html"), "app home").unwrap();
        std::fs::write(dir.join("style.css"), "body{}").unwrap();

        let statics = StaticFiles::new(&dir);

        // 根路径与目录结尾 → index.html
        let home = statics.try_serve("/").expect("应命中 index.html");
        assert!(String::from_utf8_lossy(&home.body).contains("首页"));
        assert_eq!(home.headers[0].1, "text/html; charset=utf-8");

        let app = statics.try_serve("/app/").expect("应命中子目录 index.html");
        assert_eq!(String::from_utf8_lossy(&app.body), "app home");

        // 普通文件 + MIME
        let css = statics.try_serve("/style.css").expect("应命中 css");
        assert_eq!(css.headers[0].1, "text/css; charset=utf-8");

        // 未命中
        assert!(statics.try_serve("/missing.js").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedded_resources_take_priority() {
        let statics = StaticFiles::new("不存在的目录")
            .embed("/index.html", b"<b>embedded</b>", "text/html; charset=utf-8")
            .embed("favicon.ico", &[0u8, 1, 2], "image/x-icon");

        let home = statics.try_serve("/").expect("应命中嵌入 index.html");
        assert_eq!(String::from_utf8_lossy(&home.body), "<b>embedded</b>");

        let icon = statics.try_serve("/favicon.ico").expect("应命中嵌入图标");
        assert_eq!(icon.headers[0].1, "image/x-icon");
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = temp_dir("safe");
        std::fs::write(dir.join("index.html"), "ok").unwrap();
        let statics = StaticFiles::new(&dir);

        for path in [
            "/../secret.txt",
            "/a/../../b",
            "/..\\windows\\x",
            "/C:/windows/x",
            "/..%2fsecret",
            "//etc/passwd",
        ] {
            assert!(statics.try_serve(path).is_none(), "应拒绝: {path}");
        }
        // 正常路径仍可用
        assert!(statics.try_serve("/").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mime_types() {
        assert!(mime_type("index.html").starts_with("text/html"));
        assert_eq!(mime_type("a.woff2"), "font/woff2");
        assert_eq!(mime_type("noext"), "application/octet-stream");
        assert_eq!(mime_type("a.unknownext"), "application/octet-stream");
    }

    #[test]
    fn embed_many_registers_with_auto_mime() {
        let statics = StaticFiles::new("不存在的目录").embed_many(&[
            ("index.html", b"<html>spa</html>"),
            ("assets/app-1.js", b"console.log(1)"),
            ("assets/app-1.css", b"body{}"),
        ]);

        let home = statics.try_serve("/").expect("根路径应命中嵌入 index");
        assert_eq!(home.headers[0].1, "text/html; charset=utf-8");
        let js = statics.try_serve("/assets/app-1.js").expect("应命中 js");
        assert_eq!(js.headers[0].1, "text/javascript; charset=utf-8");
        let css = statics.try_serve("/assets/app-1.css").expect("应命中 css");
        assert_eq!(css.headers[0].1, "text/css; charset=utf-8");
    }

    #[test]
    fn spa_fallback_serves_index_for_route_like_paths() {
        let statics = StaticFiles::new("不存在的目录")
            .embed("/index.html", b"<html>index</html>", "text/html; charset=utf-8")
            .embed("/assets/app.js", b"js", "text/javascript; charset=utf-8")
            .spa_fallback(true);

        // 无扩展名的前端路由 → index（深链接）
        let page = statics.try_serve("/dashboard").expect("深链接应回退 index");
        assert_eq!(String::from_utf8_lossy(&page.body), "<html>index</html>");
        // 目录形态路径同样回退
        assert!(statics.try_serve("/user/42/").is_some());
        // 命中真实资源则直出
        assert_eq!(
            String::from_utf8_lossy(&statics.try_serve("/assets/app.js").unwrap().body),
            "js"
        );
        // 带扩展名的未命中资源 → 404（不回退）
        assert!(statics.try_serve("/assets/missing.js").is_none());
        // 浏览器导航（Accept: text/html）即便带扩展名也回退
        assert!(statics
            .try_serve_with_accept("/legacy/page.html", Some("text/html,application/xhtml+xml"))
            .is_some());
        // 非 HTML 请求（如图片）不回退
        assert!(statics
            .try_serve_with_accept("/logo.png", Some("image/png"))
            .is_none());
        // 仅文件模式（try_serve_file）不做回退
        assert!(statics.try_serve_file("/dashboard").is_none());
    }

    #[test]
    fn spa_excludes_keep_backend_namespaces_404() {
        let statics = StaticFiles::new("不存在的目录")
            .embed("/index.html", b"index", "text/html; charset=utf-8")
            .spa_fallback(true)
            .spa_excludes(&["api", "/star/", ""]);

        // 排除前缀（含子路径、前缀本身）不回退
        assert!(statics.try_serve("/api/users").is_none());
        assert!(statics.try_serve("/API/Users").is_none()); // 大小写不敏感
        assert!(statics.try_serve("/star/machine").is_none());
        assert!(statics.try_serve("/api").is_none());
        // 段边界：/apiary 不是 /api 的子路径 → 回退
        assert!(statics.try_serve("/apiary").is_some());
        // 非排除路径 → 回退
        assert!(statics.try_serve("/dashboard").is_some());
    }

    #[test]
    fn spa_fallback_disabled_by_default_and_dangerous_paths_rejected() {
        let statics = StaticFiles::new("不存在的目录").embed(
            "/index.html",
            b"index",
            "text/html; charset=utf-8",
        );
        // 默认关闭：深链接不回退
        assert!(statics.try_serve("/dashboard").is_none());

        // 开启后，危险路径（目录穿越变体）仍被拒绝，不触发回退
        let spa = statics.spa_fallback(true);
        assert!(spa.try_serve("/../index.html").is_none());
        assert!(spa.try_serve("/..%2findex.html").is_none());
        assert!(spa.try_serve("/a\\b").is_none());
        assert!(spa.try_serve("/C:/windows/x").is_none());
    }

    #[test]
    fn spa_fallback_uses_disk_index_too() {
        let dir = temp_dir("spa-disk");
        std::fs::write(dir.join("index.html"), "disk-spa").unwrap();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets").join("app.js"), "js").unwrap();

        let statics = StaticFiles::new(&dir).spa_fallback(true);
        // 深链接 → 磁盘 index.html
        let page = statics.try_serve("/route/a").expect("应回退磁盘 index");
        assert_eq!(String::from_utf8_lossy(&page.body), "disk-spa");
        // 真实资源直出；缺失资源 404
        assert!(statics.try_serve("/assets/app.js").is_some());
        assert!(statics.try_serve("/assets/missing.js").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
