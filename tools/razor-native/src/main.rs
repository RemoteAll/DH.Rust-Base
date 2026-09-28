//! Razor 原生编译工具（F014）：模板 → Rust 源码 → cdylib 编译 / 渲染验证。
//!
//! - `compile`：解析 `.cshtml`，生成 Rust crate（cdylib），调用 `cargo build --release`，
//!   产物（`razor_tpl_<id>.dll|.so`）拷到目标路径；
//! - `render`：加载产物渲染并输出（供双端一致性校验脚本使用）。
//!
//! 编译缓存：同一 `--workdir`（默认仓库 `target/native/<id>`）复用 `CARGO_TARGET_DIR`
//! （默认 `target/native/target`），dhrust 只编译一次，后续模板增量构建在秒级。

use std::process::Command;

/// 工具入口。
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("compile") => cmd_compile(&args[1..]),
        Some("render") => cmd_render(&args[1..]),
        _ => {
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "用法：\n\
         \x20 razor-native compile <template.cshtml> [-o out.dll] [--name id] [--workdir dir] [--dep spec]\n\
         \x20 razor-native render  <template.dll> <data.json> [-o out.html]"
    );
}

// ————— compile —————

fn cmd_compile(args: &[String]) -> i32 {
    let Some(template_path) = args.first() else {
        usage();
        return 2;
    };
    let template_path = std::path::PathBuf::from(template_path);
    let mut out_path: Option<std::path::PathBuf> = None;
    let mut name: Option<String> = None;
    let mut workdir: Option<std::path::PathBuf> = None;
    let mut dep: Option<String> = None;

    // 解析可选参数
    let mut i = 1;
    while i < args.len() {
        let key = args[i].as_str();
        let val = args.get(i + 1).cloned();
        match (key, val) {
            ("-o", Some(v)) => {
                out_path = Some(v.into());
                i += 2;
            }
            ("--name", Some(v)) => {
                name = Some(v);
                i += 2;
            }
            ("--workdir", Some(v)) => {
                workdir = Some(v.into());
                i += 2;
            }
            ("--dep", Some(v)) => {
                dep = Some(v);
                i += 2;
            }
            _ => {
                eprintln!("未知参数：{key}");
                usage();
                return 2;
            }
        }
    }

    // 1) 解析模板
    let template_text = match std::fs::read_to_string(&template_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取模板失败：{}（{}）", template_path.display(), e);
            return 2;
        }
    };
    let template = match dhrust::razor::Template::parse(&template_text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("模板解析失败：{e}");
            return 2;
        }
    };

    // 2) 生成 crate
    let repo_root = repo_root();
    let id = sanitize_id(name.as_deref().unwrap_or_else(|| {
        template_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("tpl")
    }));
    let crate_name = format!("razor_tpl_{id}");
    let workdir = workdir.unwrap_or_else(|| repo_root.join("target").join("native").join(&id));
    let src_dir = workdir.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!("创建目录失败：{}（{}）", src_dir.display(), e);
        return 2;
    }
    let dep_spec = dep.unwrap_or_else(|| {
        format!(
            "{{ path = \"{}\", features = [\"razor\"] }}",
            repo_root.to_string_lossy().replace('\\', "/")
        )
    });
    let cargo_toml = format!(
        "# 本文件由 razor-native 自动生成，请勿手改。\n\
         [package]\n\
         name = \"{crate_name}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\n\
         [lib]\n\
         crate-type = [\"cdylib\"]\n\n\
         [dependencies]\n\
         dhrust = {dep_spec}\n\n\
         [profile.release]\n\
         lto = true\n\
         codegen-units = 1\n"
    );
    if let Err(e) = std::fs::write(workdir.join("Cargo.toml"), cargo_toml) {
        eprintln!("写入 Cargo.toml 失败：{e}");
        return 2;
    }
    let lib_src = template.to_rust_lib_source();
    if let Err(e) = std::fs::write(src_dir.join("lib.rs"), lib_src) {
        eprintln!("写入 lib.rs 失败：{e}");
        return 2;
    }

    // 3) cargo build --release（共享目标目录，dhrust 只编译一次）
    let shared_target = std::env::var_os("RAZOR_NATIVE_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target").join("native").join("target"));
    let output = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(workdir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &shared_target)
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("无法启动 cargo：{e}（请确认开发环境已安装 Rust 工具链）");
            return 2;
        }
    };
    if !output.status.success() {
        eprintln!("编译失败（cargo build --release）：");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        return 1;
    }

    // 4) 拷贝产物
    let artifact = shared_target.join("release").join(lib_file_name(&crate_name));
    let out_path = out_path.unwrap_or_else(|| workdir.join(lib_file_name(&crate_name)));
    if let Some(parent) = out_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("创建输出目录失败：{}（{}）", parent.display(), e);
            return 2;
        }
    }
    if let Err(e) = std::fs::copy(&artifact, &out_path) {
        eprintln!(
            "拷贝产物失败：{} -> {}（{}）",
            artifact.display(),
            out_path.display(),
            e
        );
        return 2;
    }
    println!("COMPILED {}", out_path.display());
    0
}

fn lib_file_name(crate_name: &str) -> String {
    if cfg!(windows) {
        format!("{crate_name}.dll")
    } else {
        format!("lib{crate_name}.so")
    }
}

fn repo_root() -> std::path::PathBuf {
    // tools/razor-native -> 仓库根
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| manifest.to_path_buf())
}

fn sanitize_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

// ————— render —————

fn cmd_render(args: &[String]) -> i32 {
    let (Some(dll), Some(data_path)) = (args.first(), args.get(1)) else {
        usage();
        return 2;
    };
    let mut out_path: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match (args[i].as_str(), args.get(i + 1)) {
            ("-o", Some(v)) => {
                out_path = Some(v.clone());
                i += 2;
            }
            _ => {
                eprintln!("未知参数：{}", args[i]);
                usage();
                return 2;
            }
        }
    }

    let template = match dhrust::razor::native::NativeTemplate::load(dll) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("加载产物失败：{e}");
            return 2;
        }
    };
    let data_text = match std::fs::read_to_string(data_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取数据失败：{data_path}（{e}）");
            return 2;
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&data_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("数据 JSON 无效：{e}");
            return 2;
        }
    };
    let model = dhrust::razor::Value::from(json);
    let html = match template.render(&model) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("渲染失败：路径={} 消息={}", e.path, e.message);
            return 1;
        }
    };
    match out_path {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, html.as_bytes()) {
                eprintln!("写出失败：{p}（{e}）");
                return 2;
            }
            println!("RENDERED {p}");
        }
        None => {
            use std::io::Write as _;
            let _ = std::io::stdout().write_all(html.as_bytes());
        }
    }
    0
}
