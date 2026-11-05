//! Web 面板共享前端资产。
//!
//! 与 [`crate::fileman`]（服务端能力）配对：面板的文件管理交互逻辑写在 JS 里，
//! 两个消费方（Pek.RAgent、Pek.RPanlServer）各自内嵌同一份资源，避免协议实现抄两遍。
//!
//! 用法（消费方的静态资源服务）：
//! ```ignore
//! statics.embed("/assets/fm-transport.js",
//!     dhrust::assets::FM_TRANSPORT_JS.as_bytes(), "application/javascript; charset=utf-8", ...);
//! ```
//! 前端 `<script src="/assets/fm-transport.js"></script>` 后即可用 `FmClient.configure({...})`。

/// 文件管理传输内核：分片断点上传、Range 断点续传下载、传输任务面板、
/// 批量分批提交、剪贴板模型、拖拽文件夹递归遍历。
///
/// 面板只需注入「外壳」：接口路径（`/star/*` 或 `/api/*`）、`api()` 封装、
/// 路径拼接、文案与刷新回调——见源文件头部注释。
pub const FM_TRANSPORT_JS: &str = include_str!("../web/fm-transport.js");

/// 文件管理前端通用样式（`.fm-jobs` 传输任务面板 / `.fm-clip` 剪贴板提示条）。
///
/// 面板既可整段引入，也可按需摘抄；类名与 [`FM_TRANSPORT_JS`] 生成的 DOM 对应。
pub const FM_TRANSPORT_CSS: &str = include_str!("../web/fm-transport.css");

#[cfg(test)]
mod tests {
    #[test]
    fn shared_assets_are_embedded_and_non_empty() {
        let js = super::FM_TRANSPORT_JS;
        assert!(js.len() > 5000, "传输内核 JS 太小，可能 include 路径失效");
        assert!(js.contains("global.FmClient = {"), "必须导出 FmClient");
        // 关键能力缺一个都算回归
        for needle in [
            "uploadChunked",
            "downloadResume",
            "fetchRange",
            "postPathsChunked",
            "walkEntry",
            "clipLoad",
        ] {
            assert!(js.contains(needle), "缺少能力：{needle}");
        }
        let css = super::FM_TRANSPORT_CSS;
        assert!(css.contains(".fm-jobs"), "传输任务面板样式缺失");
        assert!(css.contains(".fm-clip"), "剪贴板提示条样式缺失");
    }
}
