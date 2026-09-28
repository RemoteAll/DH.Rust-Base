#![cfg(feature = "razor")]
//! F011 用例集（Rust 侧）：tests/razor_cases/<case>/{template.cshtml, data.json, expected.html}
//!
//! 对每个用例：Rust 引擎渲染结果与 expected.html 逐字节比对。
//! C# 端比对由 `scripts/razor_interop.ps1` 完成（T009）。
//!
//! 运行：`cargo test --features razor --test razor_cases -- --nocapture`

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn razor_cases_match_expected_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/razor_cases");
    let mut case_dirs: Vec<PathBuf> = fs::read_dir(&root)
        .expect("用例目录 tests/razor_cases 应存在")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    case_dirs.sort();
    assert!(
        case_dirs.len() >= 5,
        "用例数应 >= 5，实际 {}",
        case_dirs.len()
    );

    let mut failures: Vec<String> = Vec::new();
    let mut passed = 0usize;
    for dir in &case_dirs {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let template_src = fs::read_to_string(dir.join("template.cshtml"))
            .unwrap_or_else(|e| panic!("[{name}] 读取 template.cshtml 失败：{e}"));
        let data_src = fs::read_to_string(dir.join("data.json"))
            .unwrap_or_else(|e| panic!("[{name}] 读取 data.json 失败：{e}"));
        let expected = fs::read_to_string(dir.join("expected.html"))
            .unwrap_or_else(|e| panic!("[{name}] 读取 expected.html 失败：{e}"));

        let json: serde_json::Value = match serde_json::from_str(&data_src) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("[{name}] data.json 无效：{e}"));
                continue;
            }
        };
        let model = dhrust::razor::Value::from(json);

        // 页面模式（F008/F009）：目录含 `_*.cshtml`（布局/Partial）→ 走视图引擎
        let page_mode = fs::read_dir(dir)
            .map(|entries| {
                entries.flatten().any(|e| {
                    let file_name = e.file_name().to_string_lossy().to_string();
                    file_name.starts_with('_') && file_name.ends_with(".cshtml")
                })
            })
            .unwrap_or(false);
        let result = if page_mode {
            let engine = dhrust::razor::view::ViewEngine::new(Box::new(
                dhrust::razor::view::DirViewLoader::new(dir),
            ));
            engine
                .render("template", &model)
                .map_err(|e| format!("{e}"))
        } else {
            match dhrust::razor::Template::parse(&template_src) {
                Ok(t) => t.render(&model).map_err(|e| format!("{e}")),
                Err(e) => Err(format!("{e}")),
            }
        };
        match result {
            Ok(actual) if actual == expected => passed += 1,
            Ok(actual) => failures.push(format!(
                "[{name}] 输出不一致：\n---- 期望 ----\n{expected}\n---- 实际 ----\n{actual}\n----"
            )),
            Err(e) => failures.push(format!("[{name}] 渲染失败：{e}")),
        }
    }

    assert!(
        failures.is_empty(),
        "用例失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("RAZOR CASES PASSED: {passed}/{}", case_dirs.len());
}
