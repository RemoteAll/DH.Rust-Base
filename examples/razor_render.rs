//! Razor 子集引擎渲染示例（互操作 / 调试辅助）。
//!
//! 与 `tools/csharp/RazorInterop`（C# 侧）成对使用，由 `scripts/razor_interop.ps1` 驱动。
//!
//! 用法：
//!   cargo run --features razor --example razor_render -- <template.cshtml> <data.json> [-o out.html]
//!   cargo run --features razor --example razor_render -- --root <目录> <视图名> <data.json> [-o out.html]
//!
//! `--root` 为页面模式（F008 布局 / F009 Partial）：以目录为模板根，视图名不含扩展名
//! （如 `template` → `<目录>/template.cshtml`，布局/Partial 按名称从同目录解析）。
//!
//! 行为与 C# 侧保持一致：输出 UTF-8 无 BOM；无 `-o` 时写标准输出。

#[cfg(feature = "razor")]
fn main() {
    std::process::exit(run());
}

#[cfg(not(feature = "razor"))]
fn main() {
    eprintln!("razor_render 需要启用 feature：cargo run --features razor --example razor_render");
    std::process::exit(2);
}

#[cfg(feature = "razor")]
fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--root" {
            match args.get(i + 1) {
                Some(v) => {
                    root = Some(v.clone());
                    i += 2;
                }
                None => {
                    eprintln!("--root 缺少目录参数");
                    return 2;
                }
            }
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    if rest.len() < 2 {
        eprintln!(
            "用法：razor_render [--root <目录>] <template.cshtml|视图名> <data.json> [-o out.html]"
        );
        return 2;
    }
    let template_path = &rest[0];
    let data_path = &rest[1];
    let out_path = rest
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| rest.get(i + 1));

    let data_src = match std::fs::read_to_string(data_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读取数据失败：{e}");
            return 1;
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&data_src) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("数据 JSON 无效：{e}");
            return 1;
        }
    };
    let model = dhrust::razor::Value::from(json);

    let html = if let Some(root_dir) = &root {
        // 页面模式（F008/F009）：布局链 / 分区 / Partial 由视图引擎编排
        let engine = dhrust::razor::view::ViewEngine::new(Box::new(
            dhrust::razor::view::DirViewLoader::new(root_dir),
        ));
        match engine.render(template_path, &model) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("渲染失败：{e}");
                return 1;
            }
        }
    } else {
        let template_src = match std::fs::read_to_string(template_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("读取模板失败：{e}");
                return 1;
            }
        };
        let template = match dhrust::razor::Template::parse(&template_src) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("模板解析失败：{e}");
                return 1;
            }
        };
        match template.render(&model) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("渲染失败：{e}");
                return 1;
            }
        }
    };

    match out_path {
        Some(path) => match std::fs::write(path, html.as_bytes()) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("写输出失败：{e}");
                1
            }
        },
        None => {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            match stdout
                .write_all(html.as_bytes())
                .and_then(|_| stdout.flush())
            {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("写标准输出失败：{e}");
                    1
                }
            }
        }
    }
}
