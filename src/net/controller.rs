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

#[cfg(feature = "razor")]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(feature = "razor")]
use std::rc::Rc;
use std::sync::Arc;

use serde_json::Value as Json;

#[cfg(feature = "razor")]
use crate::razor::value::Value as RazorValue;
#[cfg(feature = "razor")]
use crate::razor::view::{
    DirViewLoader, MemoryViewLoader, OverlayViewLoader, SearchContext, ViewEngine, ViewLoader,
};

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

    /// 设置视图的**区域回退**：控制器前缀为 `{Area}/{Controller}` 时，若在指定区域下
    /// 找不到视图，回退到 `fallback_area` 的同名视图。
    ///
    /// 对齐 ASP.NET Core Areas（`Areas/{Area}/Views`）与 NCube 的「子项目视图重写父项目视图」：
    /// 默认视图只需维护在默认区域（如 `Admin`）下，消费方改区域名（如 `Biz`）不必复制一份。
    pub fn with_fallback_area(mut self, fallback_area: impl Into<String>) -> Self {
        self.views.set_fallback_area(fallback_area);
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
    /// 视图引擎缓存：键 = 视图根路径（可带 `#embedded:N` 后缀，区分是否叠加内置模板）。
    static ENGINES: RefCell<HashMap<String, Rc<ViewEngine>>> = RefCell::new(HashMap::new());
}

/// 构建视图引擎：**项目目录优先，其次内置（库自带）模板**。
///
/// 对齐 ASP.NET Core「项目视图覆盖引用库视图」：相对路径相同的文件以项目为准；
/// 项目缺失时回退内置模板，库升级直接生效。
#[cfg(feature = "razor")]
fn build_engine(
    root: PathBuf,
    embedded: Option<Arc<Vec<(String, String)>>>,
    stamp_ttl_ms: u64,
) -> ViewEngine {
    let options = crate::razor::Options {
        stamp_ttl_ms,
        ..Default::default()
    };
    let dir = DirViewLoader::new(root);
    match embedded {
        None => ViewEngine::with_options(Box::new(dir), options),
        Some(entries) => {
            let memory = MemoryViewLoader::new((*entries).clone());
            ViewEngine::with_options(
                Box::new(OverlayViewLoader::new(
                    dir,
                    Box::new(memory) as Box<dyn ViewLoader>,
                )),
                options,
            )
        }
    }
}

/// 默认解析缓存时长（毫秒）：稳态零 IO，改视图最多延迟 1 秒生效。
pub const DEFAULT_META_TTL_MS: u64 = 1000;

/// 页面级静态资源类型（决定注入位置与标签）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAssetKind {
    /// 样式表：注入到 `</head>` 之前。
    Style,
    /// 脚本：注入到 `</body>` 之前（布局 `Scripts` 区之后）。
    Script,
}

/// 页面级静态资源规则（扩展点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageAssetRule {
    /// 扩展名（不含点，如 `css` / `js`）。
    pub extension: String,
    /// 类型（决定标签与插入位置）。
    pub kind: PageAssetKind,
}

impl PageAssetRule {
    /// 新建规则。
    pub fn new(extension: impl Into<String>, kind: PageAssetKind) -> Self {
        Self {
            extension: extension.into(),
            kind,
        }
    }
}

/// 已解析出的页面资源（注入用，结构小、可缓存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAsset {
    /// 资源类型（决定标签与插入位置）。
    pub kind: PageAssetKind,
    /// 资源 URL（已按前缀拼好）。
    pub url: String,
}

/// 页面级静态资源约定（对齐 JS 生态「同名文件自动加载」）。
///
/// 约定：视图 `{Page}` 渲染时，若 `{root}/{Page}.{ext}` 存在，就注入对应标签：
/// 样式进 `<head>` 末尾、脚本进 `</body>` 之前。项目覆盖库模板时，资源文件与视图
/// 同名同路径，天然跟着覆盖走。
///
/// **性能**：解析结果按页名缓存（默认 30 秒，进程内 `TtlCache`，一次加锁 + 一次查表），
/// 稳态下每页 **0 次文件系统调用**；未配置本项（`None`）时整条逻辑不进入，零开销。
/// 调测期可把 TTL 调到 0（不缓存）以获得「改完文件立刻生效」。
///
/// **资源只在磁盘**：这里只做 `is_file()` 存在判定 + 拼 URL，**不读取、不嵌入、不压缩**
/// 任何 css/js 到二进制；实际内容由宿主静态文件服务（带 ETag/304）直接从磁盘发出。
#[derive(Clone)]
pub struct PageAssets {
    /// 资源磁盘根（通常是 `wwwroot/static/views`）。
    pub root: PathBuf,
    /// 资源 URL 前缀（通常是 `/static/views`）。
    pub url_prefix: String,
    /// 规则（默认 `css` + `js`；可增删改，扩展名与注入位置都由此控制）。
    pub rules: Vec<PageAssetRule>,
    /// 按页名缓存的解析结果（避免每页 2 次 `is_file()`）。
    cache: Arc<crate::cache::TtlCache<String, Vec<ResolvedAsset>>>,
}

impl std::fmt::Debug for PageAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageAssets")
            .field("root", &self.root)
            .field("url_prefix", &self.url_prefix)
            .field("rules", &self.rules)
            .finish_non_exhaustive()
    }
}

impl PartialEq for PageAssets {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
            && self.url_prefix == other.url_prefix
            && self.rules == other.rules
    }
}

impl Eq for PageAssets {}

impl PageAssets {
    /// 以磁盘根与 URL 前缀创建（默认 `css` → head、`js` → body 末尾，缓存 30 秒）。
    pub fn new(root: impl Into<PathBuf>, url_prefix: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            url_prefix: url_prefix.into(),
            rules: vec![
                PageAssetRule::new("css", PageAssetKind::Style),
                PageAssetRule::new("js", PageAssetKind::Script),
            ],
            cache: Arc::new(crate::cache::TtlCache::new(30, 1024)),
        }
    }

    /// 替换规则表（扩展名 / 注入位置自定义）。
    pub fn with_rules(mut self, rules: Vec<PageAssetRule>) -> Self {
        if !rules.is_empty() {
            self.rules = rules;
        }
        self
    }

    /// 设置解析缓存 TTL（秒；`0` = 不缓存，改文件立即生效，**调测期用**）。
    pub fn with_cache_ttl(mut self, seconds: u64) -> Self {
        self.cache = Arc::new(crate::cache::TtlCache::new(seconds, 1024));
        self
    }

    /// 让某个页面的解析结果立即失效（热更新资源后调用）。
    pub fn invalidate(&self, page: &str) {
        self.cache.remove(&page.to_string());
    }

    /// 清空解析缓存。
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// 某个规则对应的物理文件。
    fn file_for(&self, page: &str, rule: &PageAssetRule) -> PathBuf {
        self.root.join(format!(
            "{}.{}",
            page.replace('/', std::path::MAIN_SEPARATOR_STR),
            rule.extension
        ))
    }

    /// 某个规则对应的 URL。
    fn url_for(&self, page: &str, rule: &PageAssetRule) -> String {
        format!(
            "{}/{}.{}",
            self.url_prefix.trim_end_matches('/'),
            page,
            rule.extension
        )
    }

    /// 解析某页的资源（先查缓存，未命中才做 `is_file()`）。
    pub fn resolve(&self, page: &str) -> Vec<ResolvedAsset> {
        let key = page.to_string();
        if let Some(hit) = self.cache.get(&key) {
            return hit;
        }
        let mut list = Vec::new();
        for rule in &self.rules {
            if self.file_for(page, rule).is_file() {
                list.push(ResolvedAsset {
                    kind: rule.kind,
                    url: self.url_for(page, rule),
                });
            }
        }
        self.cache.insert(key, list.clone());
        list
    }
}

/// 按约定把页面级资源标签注入渲染结果（无匹配文件时原样返回）。
///
/// 顺序：样式按规则顺序拼成一段插到 `</head>` 前；脚本按规则顺序拼一段插到 `</body>` 前。
/// 锚点缺失（独立整页视图）时跳过，不做启发式插入。
pub fn inject_page_assets(html: String, page: &str, assets: &PageAssets) -> String {
    let resolved = assets.resolve(page);
    if resolved.is_empty() {
        return html;
    }
    let mut head = String::new();
    let mut body = String::new();
    for asset in &resolved {
        match asset.kind {
            PageAssetKind::Style => {
                head.push_str("<link rel=\"stylesheet\" href=\"");
                crate::razor::rt::escape_html_into(&asset.url, &mut head);
                head.push_str("\" data-view=\"");
                crate::razor::rt::escape_html_into(page, &mut head);
                head.push_str("\">\n");
            }
            PageAssetKind::Script => {
                body.push_str("<script src=\"");
                crate::razor::rt::escape_html_into(&asset.url, &mut body);
                body.push_str("\" data-view=\"");
                crate::razor::rt::escape_html_into(page, &mut body);
                body.push_str("\"></script>\n");
            }
        }
    }
    let mut out = html;
    if !head.is_empty() {
        if let Some(index) = find_ignore_ascii_case(&out, "</head>") {
            out.insert_str(index, &head);
        }
    }
    if !body.is_empty() {
        if let Some(index) = find_ignore_ascii_case(&out, "</body>") {
            out.insert_str(index, &body);
        }
    }
    out
}

/// 大小写不敏感的子串查找（返回字节位置）。
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let target = needle.as_bytes();
    if target.is_empty() || bytes.len() < target.len() {
        return None;
    }
    bytes
        .windows(target.len())
        .position(|window| window.eq_ignore_ascii_case(target))
}

/// 视图目录约定（C# MVC / Areas）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewConvention {    /// 平铺约定：`Views/{Area}/{Controller}/{Action}.cshtml`。
    #[default]
    Flat,
    /// C# Areas 约定：`Areas/{Area}/Views/{Controller}/{Action}.cshtml`。
    Areas,
}

impl ViewConvention {
    /// 默认根目录名（`Views` / `Areas`）。
    pub fn root_name(&self) -> &'static str {
        match self {
            ViewConvention::Flat => "Views",
            ViewConvention::Areas => "Areas",
        }
    }

    /// 视图查找位置模板，**顺序即优先级**。
    ///
    /// 语义对齐 C# `RazorViewEngineOptions.ViewLocationFormats` /
    /// `AreaViewLocationFormats`：依次尝试「区域 + 控制器 → 区域 Shared →
    /// 根控制器 → 根 Shared」，`{area}`/`{controller}`/`{view}` 为占位符。
    /// 自定义规则用 [`ViewRenderer::with_location_formats`] 覆盖，不要在调用点写死路径。
    pub fn location_formats(&self) -> &'static [&'static str] {
        match self {
            ViewConvention::Flat => &[
                "{area}/{controller}/{view}",
                "{area}/{controller}/Shared/{view}",
                "{controller}/{view}",
                "Shared/{view}",
            ],
            ViewConvention::Areas => &[
                "{area}/Views/{controller}/{view}",
                "{area}/Views/Shared/{view}",
                "Views/{controller}/{view}",
                "Views/Shared/{view}",
            ],
        }
    }

    /// 控制器路径（含 Area 前缀）。
    pub fn controller(&self, area: &str, controller: &str) -> String {
        let controller = controller.trim_matches('/');
        match self {
            ViewConvention::Flat => format!("{}/{}", area.trim_matches('/'), controller),
            ViewConvention::Areas => {
                if controller.is_empty() {
                    format!("{}/Views", area.trim_matches('/'))
                } else {
                    format!("{}/Views/{}", area.trim_matches('/'), controller)
                }
            }
        }
    }

    /// 共享视图控制器（`Shared` / `{Area}/Views/Shared`）。
    pub fn shared_controller(&self, area: &str) -> String {
        match self {
            ViewConvention::Flat => "Shared".to_string(),
            ViewConvention::Areas => format!("{}/Views/Shared", area.trim_matches('/')),
        }
    }

    /// 从控制器路径反解 `(区域, 控制器)`。
    ///
    /// - `Areas`：`Admin/Views/Config` → `(Admin, Config)`
    /// - `Flat`：`Admin/Config` → `(Admin, Config)`
    /// - 无区域前缀：`api` → `(None, api)`
    pub fn split_controller<'a>(&self, controller: &'a str) -> (Option<&'a str>, &'a str) {
        let controller = controller.trim_matches('/');
        match self {
            ViewConvention::Areas => match controller.split_once('/') {
                Some((area, rest)) => match rest.strip_prefix("Views/") {
                    Some(tail) if !tail.is_empty() => (Some(area), tail),
                    _ => (None, controller),
                },
                None => (None, controller),
            },
            ViewConvention::Flat => match controller.split_once('/') {
                Some((area, rest)) if !rest.is_empty() => (Some(area), rest),
                _ => (None, controller),
            },
        }
    }

    /// 视图文件相对路径（含 `.cshtml`）。
    pub fn view_path(&self, area: &str, controller: &str, view: &str) -> String {
        let controller = controller.trim_matches('/');
        match self {
            ViewConvention::Flat => {
                format!("{}/{}/{}.cshtml", area.trim_matches('/'), controller, view)
            }
            ViewConvention::Areas => format!(
                "{}/Views/{}/{}.cshtml",
                area.trim_matches('/'),
                controller,
                view
            ),
        }
    }

    /// 共享视图文件相对路径（含 `.cshtml`）。
    pub fn shared_path(&self, area: &str, name: &str) -> String {
        match self {
            ViewConvention::Flat => format!("Shared/{name}.cshtml"),
            ViewConvention::Areas => {
                format!("{}/Views/Shared/{name}.cshtml", area.trim_matches('/'))
            }
        }
    }

    /// 区域根目录下的视图文件相对路径（如 `_ViewStart.cshtml`）。
    ///
    /// - `Areas`：`{Area}/Views/{name}.cshtml`
    /// - `Flat`：`{Area}/{name}.cshtml`
    pub fn area_root_path(&self, area: &str, name: &str) -> String {
        match self {
            ViewConvention::Flat => format!("{}/{name}.cshtml", area.trim_matches('/')),
            ViewConvention::Areas => {
                format!("{}/Views/{name}.cshtml", area.trim_matches('/'))
            }
        }
    }
}

/// 视图渲染器：MVC 约定 `Views/{Controller}/{Action}.cshtml`；控制器前缀可含 `/`（Area）。
///
/// 视图查找大小写宽容：先**逐段 Pascal 化**（C# 约定目录，`admin/config` →
/// `Views/Admin/Config/Index.cshtml`），再按原样（如 `Views/admin/config/index.cshtml`）。
///
/// **区域回退**（[`ViewRenderer::with_fallback_area`]）：`{Area}/{Controller}` 在指定区域
/// 找不到时，回退到默认区域的同名视图——对齐 ASP.NET Core Areas 与视图重写。
///
/// **目录约定**（[`ViewConvention`]）：默认平铺 `Views/...`；`Areas` 约定下控制器路径为
/// `{Area}/Views/{Controller}`，视图根为 `Areas`。
pub struct ViewRenderer {
    root: PathBuf,
    /// 回退区域（如 `Admin`）；`None` = 不回退
    fallback_area: Option<String>,
    convention: ViewConvention,
    /// 自定义查找格式；`None` = 用 [`ViewConvention::location_formats`] 的默认规则。
    formats: Option<Vec<String>>,
    /// 页面级静态资源约定（`None` = 不注入）。
    page_assets: Option<PageAssets>,
    /// 解析结果 / 变更戳缓存有效期（毫秒；`0` = 不缓存，改文件立即生效）。
    meta_ttl_ms: u64,
    /// 视图查找结果缓存（键 = 控制器 + 视图；值 = 解析出的相对名）。
    resolve_cache: Arc<crate::cache::TtlCache<(String, String), Option<String>>>,
    /// 内置（引用库自带）视图：`名称（不含 .cshtml）→ 源码`。
    ///
    /// 项目目录里**存在同名文件时以项目文件为准**（对齐 ASP.NET Core「项目视图重写库视图」）。
    embedded: Option<Arc<Vec<(String, String)>>>,
}

impl ViewRenderer {
    /// 默认约定：视图根 `Views`。
    pub fn new() -> ViewRenderer {
        ViewRenderer {
            root: PathBuf::from("Views"),
            fallback_area: None,
            convention: ViewConvention::Flat,
            formats: None,
            page_assets: None,
            meta_ttl_ms: DEFAULT_META_TTL_MS,
            resolve_cache: Arc::new(crate::cache::TtlCache::new(1, 2048)),
            embedded: None,
        }
    }

    /// 指定视图根目录（平铺约定 `Views/...`）。
    pub fn with_root(root: impl Into<PathBuf>) -> ViewRenderer {
        ViewRenderer {
            root: root.into(),
            fallback_area: None,
            convention: ViewConvention::Flat,
            formats: None,
            page_assets: None,
            meta_ttl_ms: DEFAULT_META_TTL_MS,
            resolve_cache: Arc::new(crate::cache::TtlCache::new(1, 2048)),
            embedded: None,
        }
    }

    /// 指定 **C# Areas 根目录**（通常为 `Areas`），使用 `Areas/{Area}/Views/...` 约定。
    pub fn with_areas_root(root: impl Into<PathBuf>) -> ViewRenderer {
        ViewRenderer {
            root: root.into(),
            fallback_area: None,
            convention: ViewConvention::Areas,
            formats: None,
            page_assets: None,
            meta_ttl_ms: DEFAULT_META_TTL_MS,
            resolve_cache: Arc::new(crate::cache::TtlCache::new(1, 2048)),
            embedded: None,
        }
    }

    /// 设置解析缓存有效期（毫秒；默认 [`DEFAULT_META_TTL_MS`]，`0` = 每次都探盘）。
    ///
    /// 稳态（缓存期内）每页 **零文件系统调用**；调测期建议设 `0` 获得「保存即生效」。
    pub fn with_meta_ttl_ms(mut self, ms: u64) -> Self {
        self.meta_ttl_ms = ms;
        let secs = ms.div_ceil(1000);
        self.resolve_cache = Arc::new(crate::cache::TtlCache::new(secs, 2048));
        self
    }

    /// 清除视图查找缓存（部署新视图后调用）。
    pub fn clear_resolve_cache(&self) {
        self.resolve_cache.clear();
    }

    /// 启用页面级静态资源约定（同名 `.css` / `.js` 自动注入）；传 `None` 关闭。
    pub fn with_page_assets(mut self, assets: Option<PageAssets>) -> Self {
        self.page_assets = assets;
        self
    }

    /// 当前页面资源约定（未启用时为 `None`）。
    pub fn page_assets(&self) -> Option<&PageAssets> {
        self.page_assets.as_ref()
    }

    /// 自定义视图查找格式（覆盖约定默认，语义同 C# `ViewLocationFormats`）。
    ///
    /// 占位符：`{area}` / `{controller}` / `{view}`；顺序即优先级。
    /// 传空表 = 恢复约定默认。
    pub fn with_location_formats<I, S>(mut self, formats: I) -> ViewRenderer
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let list: Vec<String> = formats.into_iter().map(Into::into).collect();
        self.formats = if list.is_empty() { None } else { Some(list) };
        // 规则变了，旧的解析结果必须作废
        self.resolve_cache.clear();
        self
    }

    /// 当前生效的查找格式（自定义优先，否则取约定默认）。
    pub fn formats(&self) -> Vec<String> {
        match &self.formats {
            Some(list) => list.clone(),
            None => self
                .convention
                .location_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    /// 设置内置（引用库自带）视图：`名称（不含 .cshtml）→ 源码`。
    ///
    /// 解析顺序 **项目目录 → 内置模板**：项目里存在同相对路径的文件时用项目文件
    /// （可按项目定制），否则回退库内置模板（库升级直接生效，不必同步副本）。
    pub fn with_embedded_views(mut self, entries: Vec<(String, String)>) -> ViewRenderer {
        self.embedded = if entries.is_empty() {
            None
        } else {
            Some(Arc::new(entries))
        };
        self
    }

    /// 当前内置视图表（未设置时为 `None`）。
    pub fn embedded_views(&self) -> Option<&[(String, String)]> {
        self.embedded.as_deref().map(|v| v.as_slice())
    }

    /// 内置模板名是否可解析出。
    fn embedded_has(&self, name: &str) -> bool {
        self.embedded
            .as_ref()
            .map(|list| list.iter().any(|(n, _)| n == name))
            .unwrap_or(false)
    }

    /// 视图根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 当前目录约定。
    pub fn convention(&self) -> ViewConvention {
        self.convention
    }

    /// 设置目录约定（链式）。
    pub fn with_convention(mut self, convention: ViewConvention) -> ViewRenderer {
        self.convention = convention;
        self.resolve_cache.clear();
        self
    }

    /// 当前回退区域（未设置时为 `None`）。
    pub fn fallback_area(&self) -> Option<&str> {
        self.fallback_area.as_deref()
    }

    /// 设置回退区域（链式；空串/空白 = 取消回退）。
    pub fn with_fallback_area(mut self, area: impl Into<String>) -> ViewRenderer {
        self.set_fallback_area(area);
        self
    }

    /// 设置回退区域（就地修改；空串/空白 = 取消回退）。
    pub fn set_fallback_area(&mut self, area: impl Into<String>) {
        let area = area.into();
        let trimmed = area.trim();
        self.fallback_area = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        // 回退区域变了，旧的解析结果必须作废
        self.resolve_cache.clear();
    }

    /// 按约定渲染视图并返回 HTML（套用 `_ViewStart` 约定布局）。
    ///
    /// 需要启用 `razor` feature（视图引擎）；未启用时返回明确错误。
    pub fn render(
        &self,
        controller: &str,
        action: &str,
        view: &str,
        model: &Json,
    ) -> Result<String, String> {
        self.render_with(controller, action, view, model, true)
    }

    /// 渲染片段视图（**不套用** `_ViewStart` 约定布局，对齐 C# Partial 行为）。
    pub fn render_isolated(
        &self,
        controller: &str,
        action: &str,
        view: &str,
        model: &Json,
    ) -> Result<String, String> {
        self.render_with(controller, action, view, model, false)
    }

    /// 渲染实现（`use_view_start` 控制是否套用 `_ViewStart` 约定布局）。
    fn render_with(
        &self,
        controller: &str,
        _action: &str,
        view: &str,
        model: &Json,
        use_view_start: bool,
    ) -> Result<String, String> {
        let name = self.resolve(controller, view).ok_or_else(|| {
            let tried = candidates_for(
                &self.formats(),
                self.convention.split_controller(controller).0,
                self.convention.split_controller(controller).1,
                view,
            )
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{}/{}.cshtml", pascal_path(controller), pascal_path(view)));
            match &self.fallback_area {
                Some(area) if controller.contains('/') => format!(
                    "视图不存在：{tried}.cshtml（视图根 {}；已回退区域 {area}）",
                    self.root.display()
                ),
                _ => format!(
                    "视图不存在：{tried}.cshtml（视图根 {}）",
                    self.root.display()
                ),
            }
        })?;

        #[cfg(feature = "razor")]
        {
            // 缓存键必须区分「有无内置模板 / TTL」，否则不同渲染器会互相串味。
            let key = match &self.embedded {
                None => format!("{}#ttl{}", self.root.display(), self.meta_ttl_ms),
                Some(list) => format!(
                    "{}#ttl{}#embedded:{}",
                    self.root.display(),
                    self.meta_ttl_ms,
                    list.len()
                ),
            };
            let root = self.root.clone();
            let embedded = self.embedded.clone();
            let ttl = self.meta_ttl_ms;
            // 布局 / Partial 用**裸名**（`Layout = \"_AdminLayout\"`）时，按同一套查找规则定位，
            // 与 C# 一致：页面上下文（区域 + 控制器）决定了它去哪个 `Shared` 目录找。
            let (area, ctrl) = self.convention.split_controller(name.as_str());
            let search = SearchContext {
                area: area.map(str::to_string),
                controller: ctrl.to_string(),
                formats: self.formats(),
            };
            return ENGINES.with(|slot| {
                let mut engines = slot.borrow_mut();
                let engine = engines
                    .entry(key)
                    .or_insert_with(|| Rc::new(build_engine(root, embedded, ttl)));
                engine.set_search_context(search);
                let value = RazorValue::from(model.clone());
                let html = engine
                    .render_page(&name, &value, use_view_start)
                    .map_err(|e| format!("{e}"))?;
                // 页面级资源约定：同名 `.css` / `.js` 存在才注入（无文件零开销）。
                return Ok(match (&self.page_assets, use_view_start) {
                    (Some(assets), true) => inject_page_assets(html, &name, assets),
                    _ => html,
                });
            });
        }

        #[cfg(not(feature = "razor"))]
        {
            let _ = (name, model, use_view_start);
            Err("视图渲染需要启用 dhrust 的 razor feature".to_string())
        }
    }

    /// 按 Area 约定渲染（自动拼控制器路径）。
    ///
    /// - `Flat`：`Views/{Area}/{Controller}/{View}.cshtml`
    /// - `Areas`：`Areas/{Area}/Views/{Controller}/{View}.cshtml`
    pub fn render_area(
        &self,
        area: &str,
        controller: &str,
        action: &str,
        view: &str,
        model: &Json,
    ) -> Result<String, String> {
        let controller = self.convention.controller(area, controller);
        self.render(&controller, action, view, model)
    }

    /// 按 Area 约定渲染共享视图。
    ///
    /// - `Flat`：`Views/Shared/{Name}.cshtml`
    /// - `Areas`：`Areas/{Area}/Views/Shared/{Name}.cshtml`
    pub fn render_shared_area(
        &self,
        area: &str,
        name: &str,
        model: &Json,
    ) -> Result<String, String> {
        let controller = self.convention.shared_controller(area);
        self.render(&controller, name, name, model)
    }

    /// 约定解析：返回相对视图名（`{Controller}/{View}`，不含扩展名）。
    ///
    /// 按 [`ViewConvention::location_formats`]（或自定义格式）逐条展开候选，
    /// 每条格式内再按「逐段 Pascal」与「原样」两种写法尝试；
    /// 配了回退区域时，最后再用回退区域重试一遍（`Biz/View/Config` → `Admin/...`）。
    ///
    /// 命中判定：**项目目录优先，其次内置（引用库自带）模板**。
    fn resolve(&self, controller: &str, view: &str) -> Option<String> {
        let key = (controller.to_string(), view.to_string());
        if self.meta_ttl_ms > 0 {
            if let Some(hit) = self.resolve_cache.get(&key) {
                return hit;
            }
        }
        let resolved = self.resolve_uncached(controller, view);
        if self.meta_ttl_ms > 0 {
            self.resolve_cache.insert(key, resolved.clone());
        }
        resolved
    }

    /// 实际查找（未命中缓存时）。
    fn resolve_uncached(&self, controller: &str, view: &str) -> Option<String> {
        let (area, controller) = self.convention.split_controller(controller.trim_matches('/'));
        let mut areas: Vec<Option<String>> = vec![area.map(str::to_string)];
        if let Some(fallback) = &self.fallback_area {
            let same = area
                .map(|a| a.eq_ignore_ascii_case(fallback))
                .unwrap_or(false);
            if !same {
                areas.push(Some(fallback.clone()));
            }
        }
        let formats = self.formats();
        for area in areas {
            for name in candidates_for(&formats, area.as_deref(), controller, view) {
                if self.root.join(format!("{name}.cshtml")).is_file() || self.embedded_has(&name) {
                    return Some(name);
                }
            }
        }
        None
    }
}

/// 展开视图查找格式：`{area}` / `{controller}` / `{view}` 占位符。
///
/// 区域缺失时跳过含 `{area}` 的格式（对齐 C#：非区域请求只查根级位置）。
pub(crate) fn candidates_for(
    formats: &[String],
    area: Option<&str>,
    controller: &str,
    view: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for format in formats {
        if format.contains("{area}") && area.is_none() {
            continue;
        }
        let areas = match area {
            Some(a) => vec![pascal(a), a.to_string()],
            None => vec![String::new()],
        };
        let controllers = [pascal_path(controller), controller.to_string()];
        let views = [pascal_path(view), view.to_string()];
        for a in &areas {
            for c in &controllers {
                for v in &views {
                    let name = format
                        .replace("{area}", a)
                        .replace("{controller}", c)
                        .replace("{view}", v)
                        .replace("//", "/");
                    let name = name.trim_matches('/').to_string();
                    if !name.is_empty() && !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
    }
    out
}

/// 逐段 Pascal 化（`admin/config` → `Admin/Config`；单段与 [`pascal`] 等价）。
fn pascal_path(text: &str) -> String {
    text.split('/').map(pascal).collect::<Vec<_>>().join("/")
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
            HttpOutcome::Response(response) => String::from_utf8_lossy(&response.body).to_string(),
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
        let ctx = request(
            "POST",
            "/api/echo",
            "text=fromQuery",
            "{\"TEXT\":\"fromBody\"}",
        );
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
        assert!(
            text.contains("\"code\":401") && text.contains("Unauthorized"),
            "{text}"
        );
    }

    #[cfg(feature = "razor")]
    #[test]
    fn area_views_follow_csharp_convention() {
        let dir = std::env::temp_dir().join(format!(
            "dhrust-area-view-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let views = dir.join("Areas").join("Admin").join("Views");
        std::fs::create_dir_all(views.join("Config")).unwrap();
        std::fs::create_dir_all(views.join("Shared")).unwrap();
        std::fs::write(
            views.join("Shared").join("_Layout.cshtml"),
            "<html>@RenderBody()</html>",
        )
        .unwrap();
        std::fs::write(
            views.join("Config").join("Index.cshtml"),
            "@{ Layout = \"_Layout\"; }<p>@Model.Title</p>",
        )
        .unwrap();

        let renderer = ViewRenderer::with_areas_root(dir.join("Areas"));
        let html = renderer
            .render_area(
                "Admin",
                "Config",
                "Index",
                "Index",
                &serde_json::json!({ "Title": "Admin" }),
            )
            .expect("Areas 视图应渲染成功");
        assert!(html.contains("<p>Admin</p>"), "{html}");
        assert!(html.contains("<html>"), "应套用 Areas 布局：{html}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_convention_paths_match_csharp_layout() {
        assert_eq!(
            ViewConvention::Areas.view_path("Admin", "Config", "Index"),
            "Admin/Views/Config/Index.cshtml"
        );
        assert_eq!(
            ViewConvention::Areas.shared_path("Admin", "_Layout"),
            "Admin/Views/Shared/_Layout.cshtml"
        );
        assert_eq!(
            ViewConvention::Areas
                .location_formats()
                .first()
                .copied(),
            Some("{area}/Views/{controller}/{view}")
        );
        assert_eq!(
            ViewConvention::Areas.split_controller("Admin/Views/Config"),
            (Some("Admin"), "Config")
        );
        assert_eq!(
            ViewConvention::Flat.view_path("Admin", "Config", "Index"),
            "Admin/Config/Index.cshtml"
        );
        assert_eq!(
            ViewConvention::Flat.shared_path("Admin", "Prompt"),
            "Shared/Prompt.cshtml"
        );
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
            .render(
                "home",
                "index",
                "Index",
                &serde_json::json!({ "Name": "Rust" }),
            )
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

    #[test]
    fn pascal_path_cases_each_segment() {
        assert_eq!(pascal_path("admin/config"), "Admin/Config");
        assert_eq!(pascal_path("biz/home"), "Biz/Home");
        assert_eq!(pascal_path("home"), "Home");
    }

    /// 造一个临时视图根：`Views/{area}/{controller}/{view}.cshtml`。
    fn temp_views_root(tag: &str, area: &str, controller: &str, view: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dhrust-controller-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join(area).join(controller);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{view}.cshtml")), "x").unwrap();
        root
    }

    #[test]
    fn resolve_cases_area_segments() {
        let root = temp_views_root("area-seg", "Admin", "Config", "Index");
        let renderer = ViewRenderer::with_root(&root);
        assert_eq!(
            renderer.resolve("admin/config", "index").as_deref(),
            Some("Admin/Config/Index")
        );

        // 不含回退时，其它区域找不到
        assert!(renderer.resolve("biz/config", "index").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_falls_back_to_default_area() {
        let root = temp_views_root("area-fallback", "Admin", "Config", "Index");
        let renderer = ViewRenderer::with_root(&root).with_fallback_area("Admin");
        assert_eq!(renderer.fallback_area(), Some("Admin"));
        assert_eq!(
            renderer.resolve("biz/config", "index").as_deref(),
            Some("Admin/Config/Index"),
            "非默认区域应回退到默认区域的同名视图"
        );

        // 回退区域自身不应无限递归/改写
        assert_eq!(
            renderer.resolve("admin/config", "index").as_deref(),
            Some("Admin/Config/Index")
        );

        // 空串 = 取消回退
        let mut renderer = renderer;
        renderer.set_fallback_area("  ");
        assert!(renderer.fallback_area().is_none());
        assert!(renderer.resolve("biz/config", "index").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn controller_exposes_fallback_area() {
        let controller = Controller::new("biz/config").with_fallback_area("Admin");
        assert_eq!(controller.views.fallback_area(), Some("Admin"));

        // 未设置时保持默认（无回退）
        let controller = Controller::new("api");
        assert!(controller.views.fallback_area().is_none());
    }

    /// 造一个 C# Areas 视图树：`Areas/Admin/Views/{controller}/{view}.cshtml`。
    fn temp_area_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dhrust-areas-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Admin/Views/Config")).unwrap();
        std::fs::create_dir_all(root.join("Admin/Views/Shared")).unwrap();
        root
    }

    #[cfg(feature = "razor")]
    #[test]
    fn view_start_provides_conventional_layout() {
        let root = temp_area_root("viewstart");
        std::fs::write(
            root.join("Admin/Views/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_Layout.cshtml"),
            "<html><head></head><body>@RenderBody()</body></html>",
        )
        .unwrap();
        // 视图自己不写 Layout，靠 `_ViewStart` 的约定值
        std::fs::write(
            root.join("Admin/Views/Config/Index.cshtml"),
            "<p>@Model.Title</p>",
        )
        .unwrap();

        let renderer = ViewRenderer::with_areas_root(&root);
        let html = renderer
            .render_area(
                "Admin",
                "Config",
                "Index",
                "Index",
                &serde_json::json!({ "Title": "Page" }),
            )
            .unwrap();
        assert!(html.contains("<body><p>Page</p></body>"), "{html}");

        // 关闭约定（片段视图）时不套布局
        let raw = renderer
            .render_isolated(
                "Admin/Views/Config",
                "Index",
                "Index",
                &serde_json::json!({ "Title": "Page" }),
            )
            .unwrap();
        assert_eq!(raw, "<p>Page</p>");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "razor")]
    #[test]
    fn nested_view_start_overrides_area_default() {
        let root = temp_area_root("viewstart-nested");
        std::fs::write(
            root.join("Admin/Views/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_Layout.cshtml"),
            "<html>@RenderBody()</html>",
        )
        .unwrap();
        // 就近的 `_ViewStart` 用空布局 = 不套布局（独立整页）
        std::fs::create_dir_all(root.join("Admin/Views/Config")).unwrap();
        std::fs::write(
            root.join("Admin/Views/Config/_ViewStart.cshtml"),
            "@{ Layout = \"\"; }",
        )
        .unwrap();
        std::fs::write(root.join("Admin/Views/Config/Index.cshtml"), "<p>独立</p>").unwrap();

        let html = ViewRenderer::with_areas_root(&root)
            .render_area(
                "Admin",
                "Config",
                "Index",
                "Index",
                &serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(html, "<p>独立</p>");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "razor")]
    #[test]
    fn embedded_views_fill_missing_files_and_project_wins() {
        let root = temp_area_root("embedded");
        std::fs::write(
            root.join("Admin/Views/Config/Index.cshtml"),
            "<p>项目自有</p>",
        )
        .unwrap();
        let embedded = vec![
            ("Admin/Views/Config/Index".to_string(), "<p>库内置</p>".to_string()),
            (
                "Admin/Views/Shared/_Layout".to_string(),
                "<html>@RenderBody()</html>".to_string(),
            ),
        ];
        std::fs::write(
            root.join("Admin/Views/Config/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();

        let html = ViewRenderer::with_areas_root(&root)
            .with_embedded_views(embedded)
            .render_area(
                "Admin",
                "Config",
                "Index",
                "Index",
                &serde_json::json!({}),
            )
            .unwrap();
        // 项目有同名视图 → 项目优先；布局只存在于库内置 → 库兜底
        assert!(html.contains("项目自有"), "{html}");
        assert!(!html.contains("库内置"), "{html}");
        assert!(html.starts_with("<html>"), "布局应来自内置模板：{html}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "razor")]
    #[test]
    fn page_assets_are_injected_by_convention() {
        let root = temp_area_root("assets");
        std::fs::write(
            root.join("Admin/Views/Config/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_Layout.cshtml"),
            "<html><head></head><body>@RenderBody()</body></html>",
        )
        .unwrap();
        std::fs::write(root.join("Admin/Views/Config/Index.cshtml"), "<p>x</p>").unwrap();

        let assets_root = root.join("static-views");
        std::fs::create_dir_all(assets_root.join("Admin/Views/Config")).unwrap();
        std::fs::write(assets_root.join("Admin/Views/Config/Index.css"), "p{}").unwrap();
        std::fs::write(assets_root.join("Admin/Views/Config/Index.js"), "void 0").unwrap();

        // 调测口径：TTL = 0（不缓存），改文件立刻生效
        let assets = PageAssets::new(&assets_root, "/static/views").with_cache_ttl(0);
        let renderer =
            ViewRenderer::with_areas_root(&root).with_page_assets(Some(assets.clone()));
        let html = renderer
            .render_area(
                "Admin",
                "Config",
                "Index",
                "Index",
                &serde_json::json!({}),
            )
            .unwrap();
        assert!(
            html.contains("href=\"/static/views/Admin/Views/Config/Index.css\""),
            "{html}"
        );
        assert!(
            html.contains("src=\"/static/views/Admin/Views/Config/Index.js\""),
            "{html}"
        );
        // 顺序：样式在 body 之前，脚本在 </body> 之前
        let css_at = html.find("Index.css").unwrap();
        let body_at = html.find("<body>").unwrap();
        let js_at = html.find("Index.js").unwrap();
        assert!(css_at < body_at && body_at < js_at, "{html}");

        // 无同名文件 → 不注入
        std::fs::remove_file(assets_root.join("Admin/Views/Config/Index.css")).unwrap();
        std::fs::remove_file(assets_root.join("Admin/Views/Config/Index.js")).unwrap();
        let html = renderer
            .render_area(
                "Admin",
                "Config",
                "Index",
                "Index",
                &serde_json::json!({}),
            )
            .unwrap();
        assert!(
            !html.contains("Index.css") && !html.contains("Index.js"),
            "{html}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "razor")]
    #[test]
    fn page_assets_cache_hit_and_invalidate() {
        let root = temp_area_root("assets-cache");
        std::fs::write(
            root.join("Admin/Views/Config/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_Layout.cshtml"),
            "<html><head></head><body>@RenderBody()</body></html>",
        )
        .unwrap();
        std::fs::write(root.join("Admin/Views/Config/Index.cshtml"), "<p>x</p>").unwrap();

        let assets_root = root.join("static-views");
        std::fs::create_dir_all(assets_root.join("Admin/Views/Config")).unwrap();
        let assets = PageAssets::new(&assets_root, "/static/views");
        // 首次解析：无文件 → 空列表（结果被缓存）
        assert!(assets.resolve("Admin/Views/Config/Index").is_empty());
        std::fs::write(assets_root.join("Admin/Views/Config/Index.css"), "p{}").unwrap();
        assert!(
            assets.resolve("Admin/Views/Config/Index").is_empty(),
            "缓存期内不重新探测（稳态 0 次文件系统调用）"
        );
        // 主动失效后命中新文件
        assets.invalidate("Admin/Views/Config/Index");
        assert_eq!(assets.resolve("Admin/Views/Config/Index").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 多份布局时的抉择顺序（对齐 C#）：
    /// 1) `_ViewStart.cshtml` 给默认（就近目录生效）；
    /// 2) 页面里 `Layout = "..."` 覆盖默认；
    /// 3) 页面里 `Layout = ""` 明确「不套布局」。
    #[cfg(feature = "razor")]
    #[test]
    fn layout_decision_order_default_override_and_opt_out() {
        let root = temp_area_root("layout-order");
        std::fs::create_dir_all(root.join("Admin/Views/Shared")).unwrap();
        std::fs::create_dir_all(root.join("Admin/Views/Plain")).unwrap();
        std::fs::write(
            root.join("Admin/Views/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_Layout.cshtml"),
            "<main data-layout=\"default\">@RenderBody()</main>",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_LayoutAlt.cshtml"),
            "<main data-layout=\"alt\">@RenderBody()</main>",
        )
        .unwrap();
        // ① 不声明 Layout → 用 _ViewStart 的默认
        std::fs::write(root.join("Admin/Views/Config/Index.cshtml"), "<p>A</p>").unwrap();
        // ② 页面显式指定 → 覆盖默认
        std::fs::write(
            root.join("Admin/Views/Plain/Alt.cshtml"),
            "@{ Layout = \"_LayoutAlt\"; }<p>B</p>",
        )
        .unwrap();
        // ③ 页面显式空布局 → 不套任何布局（独立整页）
        std::fs::write(
            root.join("Admin/Views/Plain/Bare.cshtml"),
            "@{ Layout = \"\"; }<p>C</p>",
        )
        .unwrap();

        let renderer = ViewRenderer::with_areas_root(&root);
        let model = serde_json::json!({});

        let a = renderer
            .render_area("Admin", "Config", "Index", "Index", &model)
            .unwrap();
        assert!(a.contains("data-layout=\"default\""), "① 默认布局：{a}");

        let b = renderer
            .render_area("Admin", "Plain", "Alt", "Alt", &model)
            .unwrap();
        assert!(b.contains("data-layout=\"alt\""), "② 页面覆盖：{b}");
        assert!(!b.contains("data-layout=\"default\""), "② 不应回落到默认：{b}");

        let c = renderer
            .render_area("Admin", "Plain", "Bare", "Bare", &model)
            .unwrap();
        assert_eq!(c, "<p>C</p>", "③ 空布局应原样输出：{c}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 性能基准（不参与常规回归）：
    /// `cargo test --release --features net,razor --lib bench_page_render -- --ignored --nocapture`
    #[cfg(feature = "razor")]    #[test]
    #[ignore = "性能基准：手动运行，输出 ns/次"]
    fn bench_page_render() {
        let root = temp_area_root("bench");
        std::fs::write(
            root.join("Admin/Views/_ViewStart.cshtml"),
            "@{ Layout = \"_Layout\"; }",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Shared/_Layout.cshtml"),
            "<html><head><title>@Model.title</title></head><body>@RenderBody()</body></html>",
        )
        .unwrap();
        std::fs::write(
            root.join("Admin/Views/Config/Index.cshtml"),
            "<div class=\"page\"><p>@Model.siteName</p></div>",
        )
        .unwrap();
        let assets_root = root.join("static-views");
        std::fs::create_dir_all(assets_root.join("Admin/Views/Config")).unwrap();
        std::fs::write(assets_root.join("Admin/Views/Config/Index.css"), "p{}").unwrap();
        std::fs::write(assets_root.join("Admin/Views/Config/Index.js"), "void 0").unwrap();

        let model = serde_json::json!({ "title": "t", "siteName": "s" });
        let base = ViewRenderer::with_areas_root(&root);
        let with_cache = ViewRenderer::with_areas_root(&root)
            .with_page_assets(Some(PageAssets::new(&assets_root, "/static/views")));
        let no_cache = ViewRenderer::with_areas_root(&root).with_page_assets(Some(
            PageAssets::new(&assets_root, "/static/views").with_cache_ttl(0),
        ));

        let bench = |label: &str, renderer: &ViewRenderer| {
            for _ in 0..200 {
                renderer
                    .render_area("Admin", "Config", "Index", "Index", &model)
                    .unwrap();
            }
            let rounds = 2000;
            let start = std::time::Instant::now();
            for _ in 0..rounds {
                renderer
                    .render_area("Admin", "Config", "Index", "Index", &model)
                    .unwrap();
            }
            let per = start.elapsed().as_nanos() as f64 / rounds as f64;
            println!("{label}: {per:.1} ns/页");
        };
        bench("无页面资源约定", &base);
        bench("有约定·缓存命中", &with_cache);
        bench("有约定·不缓存(调测档)", &no_cache);
        let _ = std::fs::remove_dir_all(&root);
    }
}
