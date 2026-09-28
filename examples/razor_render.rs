//! Razor 子集引擎渲染示例（互操作 / 调试辅助）。
//!
//! 与 `tools/csharp/RazorInterop`（C# 侧）成对使用，由 `scripts/razor_interop.ps1` 驱动。
//!
//! 用法：
//!   cargo run --features razor --example razor_render -- <template.cshtml> <data.json> [-o out.html]
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
    if args.len() < 2 {
        eprintln!("用法：razor_render <template.cshtml> <data.json> [-o out.html]");
        return 2;
    }
    let template_path = &args[0];
    let data_path = &args[1];
    let out_path = args
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| args.get(i + 1));

    let template_src = match std::fs::read_to_string(template_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读取模板失败：{e}");
            return 1;
        }
    };
    let data_src = match std::fs::read_to_string(data_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读取数据失败：{e}");
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
    let json: serde_json::Value = match serde_json::from_str(&data_src) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("数据 JSON 无效：{e}");
            return 1;
        }
    };
    let model = dhrust::razor::Value::from(json);
    let html = match template.render(&model) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("渲染失败：{e}");
            return 1;
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
