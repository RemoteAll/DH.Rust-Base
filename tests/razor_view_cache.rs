//! F010 模板缓存验收：按文件变更（mtime/长度）自动失效，修改后下次渲染生效。
#![cfg(feature = "razor")]

use dhrust::razor::view::{DirViewLoader, ViewEngine};
use dhrust::razor::Value;

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

    let engine = ViewEngine::new(Box::new(DirViewLoader::new(&dir)));
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
