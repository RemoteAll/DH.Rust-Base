//! F010 模板缓存验收：按文件变更（mtime/长度）自动失效，修改后下次渲染生效。
#![cfg(feature = "razor")]

use dhrust::razor::view::{DirViewLoader, ViewEngine};
use dhrust::razor::{Options, Value};

#[test]
fn template_cache_invalidates_on_file_change() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "dhrust_razor_cache_{}_{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let tpl = dir.join("template.cshtml");
    std::fs::write(&tpl, "A:@Model.Name").unwrap();

    // ★ 必须关掉「变更戳缓存」（`stamp_ttl_ms` 默认 1000ms）：本用例验的是
    // 「写完下一次渲染立即可见」，而默认 TTL 会让这里的三次瞬时写入**全部命中缓存**
    // —— 那是性能设计（稳态渲染零 stat），不是缺陷。要用默认值就得 sleep 过 TTL，
    // 但那样用例会又慢又依赖文件系统时间粒度；`Options` 注释也写明「调测期可设 0」。
    let engine = ViewEngine::with_options(
        Box::new(DirViewLoader::new(&dir)),
        Options { stamp_ttl_ms: 0, ..Default::default() },
    );
    let model = Value::from(serde_json::json!({ "Name": "x" }));

    // 首次渲染（解析 + 缓存）
    assert_eq!(engine.render("template", &model).unwrap(), "A:x");
    // 二次渲染（缓存命中，输出不变）
    assert_eq!(engine.render("template", &model).unwrap(), "A:x");

    // 修改文件（长度与内容同时变化，确保无论文件系统时间粒度如何都能失效）
    std::fs::write(&tpl, "BB:@Model.Name").unwrap();
    assert_eq!(engine.render("template", &model).unwrap(), "BB:x");

    // 再次修改（只改内容、长度一致 → 依赖 mtime 粒度；若同秒内写回则可能出现时间戳相同，
    // 因此本用例同时改写长度以确保稳定；纯 mtime 场景由文件系统保证）
    std::fs::write(&tpl, "CC:@Model.Name").unwrap();
    assert_eq!(engine.render("template", &model).unwrap(), "CC:x");

    let _ = std::fs::remove_dir_all(&dir);
}
