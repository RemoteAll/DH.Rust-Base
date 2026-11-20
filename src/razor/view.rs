//! Razor 子集模板引擎：视图引擎（F008 布局与分区 / F009 Partials / F010 缓存）。
//!
//! 语义与 C# 侧 `PageHost`（互操作工程）一致：
//! - 页面渲染收集 `Layout` 与分区 → 逐级布局包裹（上限 8 层）→ 最终 HTML；
//! - `@RenderBody()` / `@await RenderSectionAsync("X", required)` 仅在布局可用；
//! - `@await Html.PartialAsync("Name", model)` 由本引擎回调渲染（嵌套上限 8 层；
//!   Partial 禁止 Layout 与分区定义）；
//! - 解析缓存按「名称 + 变更戳（mtime 纳秒 + 长度）」失效（F010）；
//! - 注册了原生产物（F014）的视图优先走生成代码，否则走解释器。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use crate::razor::error::RenderError;
use crate::razor::native::NativeTemplate;
use crate::razor::parser::Template;
use crate::razor::rt::{self, PartialHost, RenderState};
use crate::razor::value::Value;
use crate::razor::Options;

/// 布局嵌套上限（与 C# 侧一致）。
pub const MAX_LAYOUT_DEPTH: u32 = 8;

/// Partial 嵌套上限（与 C# 侧一致）。
pub const MAX_PARTIAL_DEPTH: u32 = 8;

/// 模板来源（名称 → 源码 + 变更戳）。
pub struct ViewSource {
    /// 模板源码
    pub source: String,
    /// 变更戳（缓存失效判断；如 mtime 纳秒 + 长度）
    pub stamp: String,
}

/// 模板加载器（宿主提供；名称已由调用方做过安全校验）。
pub trait ViewLoader {
    /// 按名称加载模板源码。
    fn load(&self, name: &str) -> Result<ViewSource, String>;

    /// 仅取变更戳（F010 热路径：缓存命中时零文件读取；默认回退完整加载）。
    fn stamp(&self, name: &str) -> Result<String, String> {
        self.load(name).map(|s| s.stamp)
    }

    /// 指定名称是否可加载（布局/Partial 裸名逐级查找用；默认回退变更戳）。
    fn exists(&self, name: &str) -> bool {
        self.stamp(name).is_ok()
    }
}

/// 目录加载器：`root/<name>.cshtml`；变更戳 = mtime 纳秒 + 长度（F010）。
pub struct DirViewLoader {
    root: PathBuf,
}

impl DirViewLoader {
    /// 以目录为模板根创建加载器。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 名称安全校验（`字母/数字/_/-//`；禁止 `..` 与绝对路径），返回文件路径。
    pub fn resolve(&self, name: &str) -> Result<PathBuf, String> {
        if name.is_empty() || name.contains("..") || name.starts_with('/') || name.starts_with('\\')
        {
            return Err(format!("非法的模板名：{name}"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'))
        {
            return Err(format!("非法的模板名：{name}（仅允许字母/数字/_/-//）"));
        }
        Ok(self.root.join(format!("{name}.cshtml")))
    }
}

impl ViewLoader for DirViewLoader {
    fn load(&self, name: &str) -> Result<ViewSource, String> {
        let path = self.resolve(name)?;
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取模板失败：{}（{e}）", path.display()))?;
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("读取模板信息失败：{}（{e}）", path.display()))?;
        Ok(ViewSource {
            source,
            stamp: Self::stamp_of(&meta),
        })
    }

    fn stamp(&self, name: &str) -> Result<String, String> {
        let path = self.resolve(name)?;
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("读取模板信息失败：{}（{e}）", path.display()))?;
        Ok(Self::stamp_of(&meta))
    }
}

impl DirViewLoader {
    /// 变更戳：mtime 纳秒 + 长度。
    fn stamp_of(meta: &std::fs::Metadata) -> String {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{mtime}-{}", meta.len())
    }
}

/// 内存加载器：内置模板（`名称 → 源码`），对齐 ASP.NET Core 里「引用的库自带视图」。
///
/// 名称与 [`DirViewLoader`] 同格式（不含 `.cshtml`）；变更戳 = 长度 + FNV-1a 指纹，
/// 库升级后内容变化会自动失效缓存。
pub struct MemoryViewLoader {
    entries: Vec<(String, String)>,
}

impl MemoryViewLoader {
    /// 以「名称 → 源码」表创建（名称不含 `.cshtml`）。
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self { entries }
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 是否存在该名称。
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n == name)
    }
}

impl ViewLoader for MemoryViewLoader {
    fn load(&self, name: &str) -> Result<ViewSource, String> {
        let Some((_, source)) = self.entries.iter().find(|(n, _)| n == name) else {
            return Err(format!("内置模板不存在：{name}"));
        };
        Ok(ViewSource {
            source: source.clone(),
            stamp: format!("mem-{}-{:016x}", source.len(), fnv1a(source.as_bytes())),
        })
    }

    fn stamp(&self, name: &str) -> Result<String, String> {
        self.load(name).map(|s| s.stamp)
    }
}

/// 叠加加载器：**项目目录优先，其次内置模板**（对齐 ASP.NET Core 视图重写规则）。
///
/// 同名相对路径存在项目文件时用项目文件（可任意定制）；不存在时回退库内置模板，
/// 因此库升级能直接生效，不必让项目复制/同步模板。
pub struct OverlayViewLoader {
    primary: DirViewLoader,
    fallback: Box<dyn ViewLoader>,
}

impl OverlayViewLoader {
    /// 以「项目目录 + 内置模板」组合创建。
    pub fn new(primary: DirViewLoader, fallback: Box<dyn ViewLoader>) -> Self {
        Self { primary, fallback }
    }

    /// 项目里是否存在该视图文件。
    pub fn primary_has(&self, name: &str) -> bool {
        self.primary
            .resolve(name)
            .map(|p| p.is_file())
            .unwrap_or(false)
    }
}

impl ViewLoader for OverlayViewLoader {
    fn load(&self, name: &str) -> Result<ViewSource, String> {
        if self.primary_has(name) {
            return self.primary.load(name);
        }
        self.fallback.load(name)
    }

    fn stamp(&self, name: &str) -> Result<String, String> {
        if self.primary_has(name) {
            return self.primary.stamp(name);
        }
        self.fallback.stamp(name)
    }
}

/// FNV-1a 64 位指纹（仅用于变更戳，不作安全用途）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 视图查找上下文：布局 / Partial 用**裸名**时按同一套位置规则定位。
///
/// 对齐 C#：`Layout = "_Layout"`、`Html.PartialAsync("Prompt")` 里的名字不含路径，
/// 由引擎按 `ViewLocationFormats`（区域 + 控制器上下文）展开成真实相对路径。
#[derive(Debug, Clone, Default)]
pub struct SearchContext {
    /// 当前区域（`Areas` 约定下取自页面路径首段）。
    pub area: Option<String>,
    /// 当前控制器（如 `Config`）。
    pub controller: String,
    /// 查找格式（`{area}` / `{controller}` / `{view}` 占位符），顺序即优先级。
    pub formats: Vec<String>,
}

impl SearchContext {
    /// 是否为「什么都不查」的空上下文。
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }

    /// 展开裸名：返回按优先级排列的相对路径（不含 `.cshtml`）。
    pub fn expand(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        for format in &self.formats {
            if format.contains("{area}") && self.area.is_none() {
                continue;
            }
            let candidate = format
                .replace("{area}", self.area.as_deref().unwrap_or(""))
                .replace("{controller}", &self.controller)
                .replace("{view}", name)
                .replace("//", "/");
            let candidate = candidate.trim_matches('/').to_string();
            if !candidate.is_empty() && !out.contains(&candidate) {
                out.push(candidate);
            }
        }
        out
    }
}

/// 缓存条目（F010）。
struct CachedView {
    stamp: String,
    template: Rc<Template>,
}

/// 视图引擎：页面/布局/Partial 编排 + 解析缓存（F010）+ 可选原生产物（F014）。
pub struct ViewEngine {
    loader: Box<dyn ViewLoader>,
    views: RefCell<HashMap<String, CachedView>>,
    natives: RefCell<HashMap<String, Rc<NativeTemplate>>>,
    options: Options,
    /// 当前渲染的查找上下文（布局/Partial 裸名解析用）。
    search: RefCell<SearchContext>,
    /// 变更戳缓存（`名称 → (取到时刻, 戳)`；稳态渲染零文件调用）。
    stamp_cache: RefCell<HashMap<String, (std::time::Instant, String)>>,
    /// `_ViewStart` 解析缓存（`页面名 → (取到时刻, 布局名)`）。
    view_start_cache: RefCell<HashMap<String, (std::time::Instant, Option<String>)>>,
}

impl ViewEngine {
    /// 以加载器创建引擎（默认渲染选项）。
    pub fn new(loader: Box<dyn ViewLoader>) -> Self {
        Self {
            loader,
            views: RefCell::new(HashMap::new()),
            natives: RefCell::new(HashMap::new()),
            options: Options::default(),
            search: RefCell::new(SearchContext::default()),
            stamp_cache: RefCell::new(HashMap::new()),
            view_start_cache: RefCell::new(HashMap::new()),
        }
    }

    /// 指定渲染选项。
    pub fn with_options(loader: Box<dyn ViewLoader>, options: Options) -> Self {
        Self {
            options,
            ..Self::new(loader)
        }
    }

    /// 设置本次渲染的查找上下文（区域 + 控制器 + 位置格式）。
    pub fn set_search_context(&self, context: SearchContext) {
        *self.search.borrow_mut() = context;
    }

    /// 解析裸名：含 `/` 视为显式相对路径；否则按查找规则定位（找不到时原样返回，便于报错）。
    fn resolve_name(&self, name: &str) -> String {
        if name.is_empty() || name.contains('/') {
            return name.to_string();
        }
        let context = self.search.borrow();
        if context.is_empty() {
            return name.to_string();
        }
        for candidate in context.expand(name) {
            if self.loader.exists(&candidate) {
                return candidate;
            }
        }
        // 根级 `Shared/{name}` 兼容（无区域上下文的旧视图）
        let shared = format!("Shared/{name}");
        if self.loader.exists(&shared) {
            return shared;
        }
        name.to_string()
    }

    /// 注册原生产物（F014）：该视图优先走生成代码渲染。
    pub fn register_native(&self, name: &str, native: NativeTemplate) {
        self.natives
            .borrow_mut()
            .insert(name.to_string(), Rc::new(native));
    }

    /// 渲染页面：页面 → 布局链（逐级包裹）→ 最终 HTML（F008）。
    ///
    /// 先套用 `_ViewStart.cshtml` 的约定布局（由近到远查找，最近的一个生效），
    /// 页面自身再设置 `Layout` 时以页面为准；`Layout = ""` 表示不套布局。
    pub fn render(&self, page: &str, model: &Value) -> Result<String, RenderError> {
        self.render_page(page, model, true).map_err(|e| *e)
    }

    /// 渲染页面（可关闭 `_ViewStart` 约定；Partial / 片段视图用）。
    pub fn render_page(
        &self,
        page: &str,
        model: &Value,
        use_view_start: bool,
    ) -> Result<String, Box<RenderError>> {
        let host = self.host();
        let state = RenderState::for_page(host);
        if use_view_start {
            if let Some(layout) = self.view_start_layout(page, model)? {
                state.set_layout(&layout);
            }
        }
        // 典型页面输出较小；预分配避免多轮扩容（布局链复用同一缓冲）。
        let mut body = String::with_capacity(512);
        self.render_view_into(page, model, &state, &mut body)?;
        let mut layout_name = state.take_layout();
        let mut sections = state.take_sections();
        let mut depth = 0u32;
        while let Some(name) = layout_name {
            if name.is_empty() {
                break;
            }
            depth += 1;
            if depth > MAX_LAYOUT_DEPTH {
                return Err(rt::fail(
                    "Layout",
                    format!("布局嵌套过深（上限 {MAX_LAYOUT_DEPTH}）"),
                ));
            }
            let layout_body = std::mem::take(&mut body);
            let layout_state =
                RenderState::for_layout(host, layout_body, std::mem::take(&mut sections));
            self.render_view_into(&name, model, &layout_state, &mut body)?;
            layout_name = layout_state.take_layout();
            sections = layout_state.take_sections();
        }
        Ok(body)
    }

    /// 渲染单个视图（原生优先；解释器回退）。
    fn render_view_into(
        &self,
        name: &str,
        model: &Value,
        state: &RenderState,
        out: &mut String,
    ) -> Result<(), Box<RenderError>> {
        // 布局 / Partial 传入裸名时先按查找规则定位（C# 同口径）。
        let resolved = self.resolve_name(name);
        let name = resolved.as_str();
        let native = self.natives.borrow().get(name).map(Rc::clone);
        if let Some(native) = native {
            return native
                .render_with_state(model, &self.options, state, out)
                .map_err(Box::new);
        }
        let template = self.get_template(name)?;
        crate::razor::render::render_into(&template, model, &self.options, state, out)
    }

    /// 按目录层级查找 `_ViewStart.cshtml` 并取其中的约定布局。
    ///
    /// 对齐 ASP.NET Core：从页面所在目录**由近到远**向上找，最近的一个生效
    /// （`Areas/Admin/Views/Config/_ViewStart.cshtml` → `Areas/Admin/Views/_ViewStart.cshtml`
    /// → `Views/_ViewStart.cshtml`）；文件里 `@{ Layout = "_Layout"; }` 即默认布局。
    ///
    /// **多份布局的取舍**（优先级从高到低）：
    /// 1. 页面自身 `Layout = "..."`（渲染时覆盖这里的预设值）；
    /// 2. 页面自身 `Layout = ""`（明确不套布局，独立整页）；
    /// 3. 本函数返回的约定值（就近目录优先）。
    fn view_start_layout(
        &self,
        page: &str,
        model: &Value,
    ) -> Result<Option<String>, Box<RenderError>> {
        // 缓存命中即返回（稳态下 `_ViewStart` 不再被读取/执行）
        let ttl = self.options.stamp_ttl_ms;
        if ttl > 0 {
            let ttl = std::time::Duration::from_millis(ttl);
            if let Some((at, cached)) = self.view_start_cache.borrow().get(page) {
                if at.elapsed() < ttl {
                    return Ok(cached.clone());
                }
            }
        }
        let resolved = self.view_start_layout_uncached(page, model)?;
        if ttl > 0 {
            self.view_start_cache
                .borrow_mut()
                .insert(page.to_string(), (std::time::Instant::now(), resolved.clone()));
        }
        Ok(resolved)
    }

    /// 未命中缓存时的实际查找与执行。
    fn view_start_layout_uncached(
        &self,
        page: &str,
        model: &Value,
    ) -> Result<Option<String>, Box<RenderError>> {
        let mut dir = page.rsplit_once('/').map(|(dir, _)| dir.to_string());
        loop {
            let candidate = match &dir {
                Some(d) if !d.is_empty() => format!("{d}/_ViewStart"),
                _ => "_ViewStart".to_string(),
            };
            if self.loader.exists(&candidate) {
                let state = RenderState::for_page(self.host());
                let mut scratch = String::new();
                self.render_view_into(&candidate, model, &state, &mut scratch)?;
                if let Some(layout) = state.take_layout() {
                    let layout = layout.to_string();
                    if layout.is_empty() {
                        return Ok(None);
                    }
                    // `_ViewStart` 里通常写裸名（`_Layout`），按同一套规则定位。
                    return Ok(Some(self.resolve_name(&layout)));
                }
                return Ok(None);
            }
            dir = match &dir {
                Some(d) => d.rsplit_once('/').map(|(parent, _)| parent.to_string()),
                None => break,
            };
            if dir.as_deref() == Some("") {
                dir = None;
            }
        }
        Ok(None)
    }

    /// 变更戳（按 `stamp_ttl_ms` 缓存；缓存期内零文件系统调用）。
    fn stamp_cached(&self, name: &str) -> Result<String, String> {
        let ttl = self.options.stamp_ttl_ms;
        if ttl == 0 {
            return self.loader.stamp(name);
        }
        let ttl = std::time::Duration::from_millis(ttl);
        if let Some((at, stamp)) = self.stamp_cache.borrow().get(name) {
            if at.elapsed() < ttl {
                return Ok(stamp.clone());
            }
        }
        let stamp = self.loader.stamp(name)?;
        self.stamp_cache
            .borrow_mut()
            .insert(name.to_string(), (std::time::Instant::now(), stamp.clone()));
        Ok(stamp)
    }

    /// 清空解析缓存（`stamp` / `_ViewStart`），便于热更新后立即生效。
    pub fn clear_meta_cache(&self) {
        self.stamp_cache.borrow_mut().clear();
        self.view_start_cache.borrow_mut().clear();
    }

    /// 取解析模板（F010：先比对轻量变更戳，命中时零文件读取）。
    fn get_template(&self, name: &str) -> Result<Rc<Template>, Box<RenderError>> {
        let stamp = self
            .stamp_cached(name)
            .map_err(|m| rt::fail(name.to_string(), m))?;
        {
            let views = self.views.borrow();
            if let Some(cached) = views.get(name) {
                if cached.stamp == stamp {
                    return Ok(Rc::clone(&cached.template));
                }
            }
        }
        let vs = self
            .loader
            .load(name)
            .map_err(|m| rt::fail(name.to_string(), m))?;
        let parsed = Template::parse(&vs.source).map_err(|e| {
            rt::fail(
                name.to_string(),
                format!("模板解析失败（{}:{}）：{}", e.line, e.col, e.message),
            )
        })?;
        let rc = Rc::new(parsed);
        self.views.borrow_mut().insert(
            name.to_string(),
            CachedView {
                stamp: vs.stamp,
                template: Rc::clone(&rc),
            },
        );
        Ok(rc)
    }

    /// Partial 宿主回调（C 风格两段指针；引擎在渲染调用期间保证自身地址有效）。
    fn host(&self) -> PartialHost {
        PartialHost {
            render: partial_trampoline,
            data: self as *const ViewEngine as *const (),
        }
    }

    /// 渲染 Partial（F009；深度守卫；禁止 Layout）。
    fn render_partial(
        &self,
        name: &str,
        model: &Value,
        out: &mut String,
        depth: u32,
    ) -> Result<(), Box<RenderError>> {
        if depth > MAX_PARTIAL_DEPTH {
            return Err(rt::fail(
                format!("PartialAsync(\"{name}\")"),
                format!("Partial 嵌套过深（上限 {MAX_PARTIAL_DEPTH}）"),
            ));
        }
        let state = RenderState::for_partial(self.host(), depth);
        self.render_view_into(name, model, &state, out)?;
        if state.layout.borrow().is_some() {
            return Err(rt::fail(
                format!("PartialAsync(\"{name}\")"),
                "Partial 不支持 Layout",
            ));
        }
        Ok(())
    }
}

/// Partial 宿主 trampoline：从薄指针恢复引擎引用并渲染。
///
/// 安全性：`data` 由 [`ViewEngine::host`] 在渲染调用期间生成，指向存活的引擎；
/// 引擎内部全部可变状态经 `RefCell` 访问，调用期间不存在别名可变借用。
fn partial_trampoline(
    data: *const (),
    name: &str,
    model: &Value,
    out: &mut String,
    depth: u32,
) -> Result<(), Box<RenderError>> {
    let engine = unsafe { &*(data as *const ViewEngine) };
    engine.render_partial(name, model, out, depth)
}
