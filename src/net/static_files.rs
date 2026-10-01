//! 静态文件服务（默认约定 `wwwroot/` 目录）：MIME、默认文档、路径穿越防护、嵌入资源。
//!
//! 对齐 C#/NewLife 的静态资源与 `MapEmbedded` 惯例：
//! - **目录模式**：[`StaticFiles::new`]（或 [`StaticFiles::default`] = `wwwroot`），
//!   按请求读盘（开发友好，改文件即生效）；
//! - **嵌入模式**：[`StaticFiles::embed`] 注册编译期资源（`include_bytes!`/`include_str!`），
//!   与目录叠加（嵌入优先），适合单文件部署；
//! - **约定**：路径 `/` 或目录尾斜杠命中 `index.html` 默认文档；
//!   `..`/反斜杠/盘符/空段等危险路径直接拒绝（防目录穿越）；
//! - **用法**：通常挂到路由 fallback——命中返回文件，未命中 `None` 由调用方决定 404：
//!
//! ```no_run
//! use dhrust::net::http::{HttpOutcome, HttpResponse};
//! use dhrust::net::router::{route, Router};
//! use dhrust::net::static_files::StaticFiles;
//!
//! let statics = StaticFiles::default(); // wwwroot/
//! let mut router = Router::new();
//! router.fallback(route(move |ctx| {
//!     let statics = statics.clone();
//!     async move {
//!         statics
//!             .try_serve(&ctx.req.path)
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
}

impl StaticFiles {
    /// 以目录创建（目录不存在时全部未命中，不报错）。
    pub fn new(root: impl Into<PathBuf>) -> StaticFiles {
        StaticFiles {
            root: Some(root.into()),
            embedded: HashMap::new(),
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

    /// 尝试服务请求路径；未命中（含被拒绝的危险路径）返回 `None`。
    pub fn try_serve(&self, path: &str) -> Option<HttpResponse> {
        let relative = safe_relative(path)?;

        // 嵌入资源优先（部署形态稳定）
        if let Some((data, content_type)) = self.embedded.get(&relative) {
            return Some(HttpResponse::bytes(200, content_type, *data));
        }

        // 磁盘目录
        let root = self.root.as_ref()?;
        let full = root.join(&relative);
        let meta = std::fs::metadata(&full).ok()?;
        if !meta.is_file() {
            return None;
        }
        let data = std::fs::read(&full).ok()?;
        Some(HttpResponse::bytes(200, mime_type(&relative), data))
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
}
