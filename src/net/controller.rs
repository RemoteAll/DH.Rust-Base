//! HTTP 控制器与视图约定（对齐 C#/NewLife `MapController<T>` 与 MVC 默认约定）。
//!
//! **控制器**：一组命名动作挂载到 `/{prefix}/{action}`（方法可精确或 `*` 任意），
//! 相比逐个 `router.map_*` 手写提供统一入口与分发（注册顺序无关、动作名大小写不敏感）。
//!
//! **参数绑定**（[`arg`]）：按 C# 惯例依次取 路由参数 → 查询串 → 表单 → JSON body 字段
//! （字段名大小写不敏感），动作代码无需关心参数来源。
//!
//! **统一响应**（[`json_result`]）：`{code, message?, data?}` 信封（对齐 NewLife 控制器 JSON 惯例；
//! `message` 为空、`data` 为 `None` 时省略字段，与 C# 匿名对象序列化行为一致）。
//!
//! **视图约定**（[`ActionResult::View`] + [`ViewRenderer`]）：动作返回视图结果时按 MVC 约定渲染
//! `Views/{Controller}/{Action}.cshtml`（Razor 引擎：布局/分区/Partial/缓存全部继承）；
//! 视图根默认 `Views`（可用 [`Controller::with_views_root`] 调整），目录/文件大小写宽容。
//!
//! # 示例
//!
//! ```no_run
//! use dhrust::net::controller::{arg, json_result, Controller};
//! use dhrust::net::router::Router;
//!
//! let mut router = Router::new();
//! Controller::new("api")
//!     .get("ping", |_ctx| json_result(0, "", None))
//!     .post("login", |ctx| {
//!         let user = arg(ctx, "user").unwrap_or_default();
//!         json_result(0, "", Some(serde_json::json!({ "user": user })))
//!     })
//!     .mount(&mut router);
//! ```

use std::collections::HashMap;
#[cfg(feature = "razor")]
use std::cell::RefCell;
use std::path::{Path, PathBuf};
#[cfg(feature = "razor")]
use std::rc::Rc;
use std::sync::Arc;

use serde_json::Value as Json;

#[cfg(feature = "razor")]
use crate::razor::value::Value as RazorValue;
#[cfg(feature = "razor")]
use crate::razor::view::{DirViewLoader, ViewEngine};

use super::http::{json_escape, HttpOutcome, HttpResponse};
use super::router::{route, Ctx, RouteHandler, Router};

/// 动作处理结果：普通响应，或按约定渲染视图。
pub enum ActionResult {
    /// 直接响应
    Response(HttpResponse),
    /// 视图渲染：`name` 缺省时用动作名；`model` 供模板 `@Model.*` 使用
    View {
        /// 视图名（缺省 = 当前动作名）
        name: Option<String>,
        /// 模型（Razor `@Model` 的数据源）
        model: Json,
    },
}

impl From<HttpResponse> for ActionResult {
    fn from(response: HttpResponse) -> Self {
        ActionResult::Response(response)
    }
}

/// 渲染视图（约定：`Views/{Controller}/{Action}.cshtml`）。
pub fn view(model: Json) -> ActionResult {
    ActionResult::View { name: None, model }
}

/// 渲染指定视图。
pub fn view_named(name: &str, model: Json) -> ActionResult {
    ActionResult::View {
        name: Some(name.to_string()),
        model,
    }
}

/// 动作处理器。
pub type ActionHandler = Arc<dyn Fn(&Ctx) -> ActionResult + Send + Sync>;

/// 注册的动作。
struct ActionEntry {
    /// 请求方法（大写；`*` = 任意）
    method: String,
    handler: ActionHandler,
}

/// 控制器：一组命名动作（`/{prefix}/{action}`）。
pub struct Controller {
    prefix: String,
    actions: HashMap<String, ActionEntry>,
    views: ViewRenderer,
}

impl Controller {
    /// 新建控制器（`prefix` 如 `"api"`；两侧 `/` 自动去除，前缀与动作名大小写不敏感）。
    pub fn new(prefix: &str) -> Controller {
        Controller {
            prefix: prefix.trim_matches('/').to_ascii_lowercase(),
            actions: HashMap::new(),
            views: ViewRenderer::new(),
        }
    }

    /// 注册动作（`method` 为 `GET`/`POST`/…，`*` 表示任意方法；动作名大小写不敏感）。
    pub fn map(
        mut self,
        method: &str,
        action: &str,
        handler: impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static,
    ) -> Self {
        self.actions.insert(
            action.to_ascii_lowercase(),
            ActionEntry {
                method: method.to_ascii_uppercase(),
                handler: Arc::new(handler),
            },
        );
        self
    }

    /// 注册 GET 动作。
    pub fn get(
        self,
        action: &str,
        handler: impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static,
    ) -> Self {
        self.map("GET", action, handler)
    }

    /// 注册 POST 动作。
    pub fn post(
        self,
        action: &str,
        handler: impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static,
    ) -> Self {
        self.map("POST", action, handler)
    }

    /// 注册 PUT 动作。
    pub fn put(
        self,
        action: &str,
        handler: impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static,
    ) -> Self {
        self.map("PUT", action, handler)
    }

    /// 注册 DELETE 动作。
    pub fn delete(
        self,
        action: &str,
        handler: impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static,
    ) -> Self {
        self.map("DELETE", action, handler)
    }

    /// 设置视图根目录（默认 `Views`）。
    pub fn with_views_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.views = ViewRenderer::with_root(root);
        self
    }

    /// 挂载：注册 `/{prefix}/{action}` 统一入口（匹配任意方法，内部按注册方法校验）。
    pub fn mount(self, router: &mut Router) {
        let pattern = format!("/{}/{{action}}", self.prefix);
        let actions = Arc::new(self.actions);
        let views = Arc::new(self.views);
        let controller = self.prefix.clone();
        let handler: RouteHandler = route(move |ctx| {
            let actions = actions.clone();
            let views = views.clone();
            let controller = controller.clone();
            async move { dispatch(&actions, &views, &controller, ctx) }
        });
        router.map("*", &pattern, handler);
    }
}

/// 分发：查动作 → 校验方法 → 执行（视图结果就地渲染）。
fn dispatch(
    actions: &HashMap<String, ActionEntry>,
    views: &ViewRenderer,
    controller: &str,
    ctx: Ctx,
) -> HttpOutcome {
    let action = ctx.param("action").unwrap_or("").to_ascii_lowercase();
    let Some(entry) = actions.get(action.as_str()) else {
        return HttpOutcome::Response(HttpResponse::json(
            404,
            format!(
                "{{\"code\":404,\"message\":\"未知动作: {}\"}}",
                json_escape(&action)
            ),
        ));
    };

    if entry.method != "*" && !entry.method.eq_ignore_ascii_case(&ctx.req.method) {
        return HttpOutcome::Response(HttpResponse::text(405, "Method Not Allowed"));
    }

    match (entry.handler)(&ctx) {
        ActionResult::Response(response) => HttpOutcome::Response(response),
        ActionResult::View { name, model } => {
            let view_name = name.unwrap_or_else(|| action.clone());
            match views.render(controller, &action, &view_name, &model) {
                Ok(html) => HttpOutcome::Response(HttpResponse::bytes(
                    200,
                    "text/html; charset=utf-8",
                    html.into_bytes(),
                )),
                Err(message) => HttpOutcome::Response(HttpResponse::json(
                    500,
                    format!(
                        "{{\"code\":500,\"message\":\"视图渲染失败: {}\"}}",
                        json_escape(&message)
                    ),
                )),
            }
        }
    }
}

// ————— 参数绑定与统一响应 —————

/// 动作参数取值（对齐 C# 参数绑定顺序）：路由参数 → 查询串 → 表单 → JSON body 字段；
/// 字段名大小写不敏感。取不到返回 `None`。
pub fn arg(ctx: &Ctx, name: &str) -> Option<String> {
    if let Some(value) = ctx.param(name) {
        return Some(value.to_string());
    }
    if let Some(value) = ctx.query_value(name) {
        return Some(value.to_string());
    }
    if let Some(value) = ctx.form_value(name) {
        return Some(value.to_string());
    }

    let body = json_body(ctx)?;
    let object = body.as_object()?;
    for (key, value) in object {
        if key.eq_ignore_ascii_case(name) {
            return json_scalar(value);
        }
    }
    None
}

/// 动作参数取值（整数）。
pub fn arg_i64(ctx: &Ctx, name: &str) -> Option<i64> {
    arg(ctx, name)?.trim().parse().ok()
}

/// 动作参数取值（布尔；`true`/`1`/`yes` 为真，不区分大小写）。
pub fn arg_bool(ctx: &Ctx, name: &str) -> Option<bool> {
    match arg(ctx, name)?.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// JSON 请求体（为空或非 JSON 返回 `None`）。
pub fn json_body(ctx: &Ctx) -> Option<Json> {
    if ctx.req.body.is_empty() {
        return None;
    }
    serde_json::from_slice(&ctx.req.body).ok()
}

/// JSON 标量转字符串（字符串原样、数字/布尔转文本、null 与复合类型返回 `None`）。
fn json_scalar(value: &Json) -> Option<String> {
    match value {
        Json::String(text) => Some(text.clone()),
        Json::Number(number) => Some(number.to_string()),
        Json::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// 统一 JSON 响应信封：`{code, message?, data?}`（`message` 空与 `data` 为 `None` 时省略字段，
/// 对齐 C# 匿名对象序列化行为）。
pub fn json_result(code: i64, message: &str, data: Option<Json>) -> ActionResult {
    let mut object = serde_json::Map::new();
    object.insert("code".to_string(), Json::from(code));
    if !message.is_empty() {
        object.insert("message".to_string(), Json::from(message));
    }
    if let Some(data) = data {
        object.insert("data".to_string(), data);
    }
    ActionResult::Response(HttpResponse::json(200, Json::Object(object).to_string()))
}

/// 统一 JSON 错误响应（HTTP 200 + 信封 `code`，对齐 NewLife 控制器错误惯例）。
pub fn json_error(code: i64, message: &str) -> ActionResult {
    json_result(code, message, None)
}

// ————— 视图渲染（MVC 约定） —————

// 线程本地引擎缓存（键 = 视图根目录；Razor 解析缓存命中时零文件读取）
#[cfg(feature = "razor")]
thread_local! {
    static ENGINES: RefCell<HashMap<PathBuf, Rc<ViewEngine>>> = RefCell::new(HashMap::new());
}

/// 视图渲染器：MVC 约定 `Views/{Controller}/{Action}.cshtml`。
///
/// 视图查找大小写宽容：先 Pascal 化（C# 约定目录，如 `Views/Api/Login.cshtml`），
/// 再按原样（如 `Views/api/login.cshtml`）。
pub struct ViewRenderer {
    root: PathBuf,
}

impl ViewRenderer {
    /// 默认约定：视图根 `Views`。
    pub fn new() -> ViewRenderer {
        ViewRenderer {
            root: PathBuf::from("Views"),
        }
    }

    /// 指定视图根目录。
    pub fn with_root(root: impl Into<PathBuf>) -> ViewRenderer {
        ViewRenderer { root: root.into() }
    }

    /// 视图根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 按约定渲染视图并返回 HTML。
    ///
    /// 需要启用 `razor` feature（视图引擎）；未启用时返回明确错误。
    pub fn render(
        &self,
        controller: &str,
        _action: &str,
        view: &str,
        model: &Json,
    ) -> Result<String, String> {
        let name = self.resolve(controller, view).ok_or_else(|| {
            format!(
                "视图不存在：{}/{}.cshtml（视图根 {}）",
                pascal(controller),
                pascal(view),
                self.root.display()
            )
        })?;

        #[cfg(feature = "razor")]
        {
            return ENGINES.with(|slot| {
                let mut engines = slot.borrow_mut();
                let engine = engines.entry(self.root.clone()).or_insert_with(|| {
                    Rc::new(ViewEngine::new(Box::new(DirViewLoader::new(
                        self.root.clone(),
                    ))))
                });
                let value = RazorValue::from(model.clone());
                engine.render(&name, &value).map_err(|e| format!("{e}"))
            });
        }

        #[cfg(not(feature = "razor"))]
        {
            let _ = (name, model);
            Err("视图渲染需要启用 dhrust 的 razor feature".to_string())
        }
    }

    /// 约定解析：返回相对视图名（`{Controller}/{View}.cshtml`）。
    fn resolve(&self, controller: &str, view: &str) -> Option<String> {
        let controllers = [pascal(controller), controller.to_string()];
        let views = [pascal(view), view.to_string()];
        for c in &controllers {
            for v in &views {
                // 返回不带扩展名的模板名（DirViewLoader 自行补 `.cshtml`）
                let name = format!("{c}/{v}");
                if self.root.join(format!("{name}.cshtml")).is_file() {
                    return Some(name);
                }
            }
        }
        None
    }
}

impl Default for ViewRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// 首字母大写（C# PascalCase 约定目录名）。
fn pascal(text: &str) -> String {
    match text.chars().next() {
        Some(first) => {
            let mut out = String::with_capacity(text.len());
            out.extend(first.to_uppercase());
            out.push_str(&text[first.len_utf8()..]);
            out
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::http::HttpRequest;

    /// 构造测试请求。
    fn request(method: &str, path: &str, query: &str, body: &str) -> Ctx {
        Ctx::build(HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            headers: Vec::new(),
            body: body.to_string().into(),
            remote_addr: None,
        })
    }

    /// 取出响应体文本。
    fn body_of(outcome: HttpOutcome) -> String {
        match outcome {
            HttpOutcome::Response(response) => {
                String::from_utf8_lossy(&response.body).to_string()
            }
            _ => String::new(),
        }
    }

    /// 构造实验控制器并执行分发。
    fn run(action: &str, method: &str, query: &str, body: &str) -> (String, HttpOutcome) {
        let controller = Controller::new("api")
            .get("ping", |_ctx| json_result(0, "", None))
            .post("echo", |ctx| {
                let text = arg(ctx, "text").unwrap_or_default();
                json_result(0, "", Some(serde_json::json!({ "text": text })))
            });
        let actions = controller.actions;
        let views = controller.views;
        let mut ctx = request(method, &format!("/api/{action}"), query, body);
        ctx.params = vec![("action".to_string(), action.to_string())];
        let outcome = dispatch(&actions, &views, "api", ctx);
        let text = body_of(outcome.clone());
        (text, outcome)
    }

    #[test]
    fn dispatch_get_and_post_actions() {
        let (text, _) = run("ping", "GET", "", "");
        assert!(text.contains("\"code\":0"), "{text}");

        // JSON body 绑定（大小写不敏感）
        let (text, _) = run("echo", "POST", "", "{\"Text\":\"你好\"}");
        assert!(text.contains("你好"), "{text}");
    }

    #[test]
    fn dispatch_rejects_wrong_method() {
        // echo 只注册了 POST；GET 应 405
        let (_, outcome) = run("echo", "GET", "", "");
        match outcome {
            HttpOutcome::Response(response) => assert_eq!(response.status, 405),
            _ => panic!("应为响应"),
        }
    }

    #[test]
    fn dispatch_unknown_action_404() {
        let (text, _) = run("nope", "GET", "", "");
        assert!(text.contains("404"), "{text}");
    }

    #[test]
    fn arg_binding_priority_and_case() {
        // 查询串优先于 body
        let ctx = request("POST", "/api/echo", "text=fromQuery", "{\"TEXT\":\"fromBody\"}");
        assert_eq!(arg(&ctx, "text").as_deref(), Some("fromQuery"));

        // body 字段（大小写不敏感）
        let ctx = request("POST", "/api/echo", "", "{\"TeXT\":\"fromBody\"}");
        assert_eq!(arg(&ctx, "text").as_deref(), Some("fromBody"));

        // 路由参数优先
        let mut ctx = request("GET", "/api/echo", "text=fromQuery", "");
        ctx.params = vec![("text".to_string(), "fromRoute".to_string())];
        assert_eq!(arg(&ctx, "text").as_deref(), Some("fromRoute"));

        // 整数/布尔
        let ctx = request("POST", "/api/echo", "", "{\"count\":\"42\",\"flag\":true}");
        assert_eq!(arg_i64(&ctx, "count"), Some(42));
        assert_eq!(arg_bool(&ctx, "flag"), Some(true));
    }

    #[test]
    fn json_result_omits_empty_fields() {
        let outcome = json_result(0, "", None);
        let text = body_of(match outcome {
            ActionResult::Response(r) => HttpOutcome::Response(r),
            _ => panic!("应为响应"),
        });
        assert_eq!(text, "{\"code\":0}");

        let outcome = json_result(401, "Unauthorized", None);
        let text = body_of(match outcome {
            ActionResult::Response(r) => HttpOutcome::Response(r),
            _ => panic!("应为响应"),
        });
        assert!(text.contains("\"code\":401") && text.contains("Unauthorized"), "{text}");
    }

    #[cfg(feature = "razor")]
    #[test]
    fn view_renders_with_convention() {
        let dir = std::env::temp_dir().join(format!(
            "dhrust-view-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let view_dir = dir.join("Views").join("Home");
        std::fs::create_dir_all(&view_dir).unwrap();
        std::fs::write(view_dir.join("Index.cshtml"), "Hello @Model.Name").unwrap();

        let renderer = ViewRenderer::with_root(dir.join("Views"));
        let html = renderer
            .render("home", "index", "Index", &serde_json::json!({ "Name": "Rust" }))
            .expect("渲染应成功");
        assert_eq!(html, "Hello Rust");

        // 视图不存在 → 明确报错
        let err = renderer
            .render("home", "index", "Missing", &serde_json::json!({}))
            .unwrap_err();
        assert!(err.contains("视图不存在"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pascal_helper() {
        assert_eq!(pascal("home"), "Home");
        assert_eq!(pascal("Api"), "Api");
        assert_eq!(pascal(""), "");
    }
}
