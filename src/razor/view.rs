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
}

impl ViewEngine {
    /// 以加载器创建引擎（默认渲染选项）。
    pub fn new(loader: Box<dyn ViewLoader>) -> Self {
        Self {
            loader,
            views: RefCell::new(HashMap::new()),
            natives: RefCell::new(HashMap::new()),
            options: Options::default(),
        }
    }

    /// 指定渲染选项。
    pub fn with_options(loader: Box<dyn ViewLoader>, options: Options) -> Self {
        Self {
            options,
            ..Self::new(loader)
        }
    }

    /// 注册原生产物（F014）：该视图优先走生成代码渲染。
    pub fn register_native(&self, name: &str, native: NativeTemplate) {
        self.natives
            .borrow_mut()
            .insert(name.to_string(), Rc::new(native));
    }

    /// 渲染页面：页面 → 布局链（逐级包裹）→ 最终 HTML（F008）。
    pub fn render(&self, page: &str, model: &Value) -> Result<String, RenderError> {
        self.render_boxed(page, model).map_err(|e| *e)
    }

    fn render_boxed(&self, page: &str, model: &Value) -> Result<String, Box<RenderError>> {
        let host = self.host();
        let state = RenderState::for_page(host);
        // 典型页面输出较小；预分配避免多轮扩容（布局链复用同一缓冲）。
        let mut body = String::with_capacity(512);
        self.render_view_into(page, model, &state, &mut body)?;
        let mut layout_name = state.take_layout();
        let mut sections = state.take_sections();
        let mut depth = 0u32;
        while let Some(name) = layout_name {
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
        let native = self.natives.borrow().get(name).map(Rc::clone);
        if let Some(native) = native {
            return native
                .render_with_state(model, &self.options, state, out)
                .map_err(Box::new);
        }
        let template = self.get_template(name)?;
        crate::razor::render::render_into(&template, model, &self.options, state, out)
    }

    /// 取解析模板（F010：先比对轻量变更戳，命中时零文件读取）。
    fn get_template(&self, name: &str) -> Result<Rc<Template>, Box<RenderError>> {
        let stamp = self
            .loader
            .stamp(name)
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
