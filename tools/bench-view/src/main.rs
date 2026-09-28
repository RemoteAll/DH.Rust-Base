//! Razor 子集引擎渲染基准（Rust 侧）。
//!
//! 同模板、同数据、warm 状态（解析/编译一次）下重复渲染，输出吞吐与分位延迟；
//! 与 `tools/csharp/RazorInterop bench`（C# 原生 Razor 编译缓存同口径）对照，
//! 由 `scripts/bench_view.ps1` 驱动。
//!
//! 引擎：
//! - `rust`：解释器（`Template::render`）；
//! - `rust-native`：F014 生成代码（传 dll 路径时启用，`NativeTemplate::render`）。
//!
//! 用法：
//!   bench-view <template.cshtml> <data.json> [iterations] [warmup] [native.dll]

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 页面模式（F008/F009）：bench-view --view <目录> <data.json> [iters] [warmup] [nativeDir]
    if args.first().map(String::as_str) == Some("--view") {
        std::process::exit(bench_view_mode(&args[1..]));
    }
    if args.len() < 2 {
        eprintln!("用法：bench-view <template.cshtml> <data.json> [iterations] [warmup] [native.dll]");
        eprintln!("      bench-view --view <目录> <data.json> [iterations] [warmup] [nativeDir]");
        std::process::exit(2);
    }
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let warmup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1_000);

    let template_src = std::fs::read_to_string(&args[0]).expect("读取模板失败");
    let data_src = std::fs::read_to_string(&args[1]).expect("读取数据失败");
    let json: serde_json::Value = serde_json::from_str(&data_src).expect("数据 JSON 无效");
    let model = dhrust::razor::Value::from(json);

    // 引擎一：解释器（解析一次）
    let template = dhrust::razor::Template::parse(&template_src).expect("模板解析失败");
    bench("rust", iters, warmup, || template.render(&model).expect("渲染失败"));

    // 引擎二：F014 生成代码（可选，传第 5 个参数为 dll 路径）
    if let Some(dll) = args.get(4) {
        let native =
            dhrust::razor::native::NativeTemplate::load(dll).expect("加载原生模板失败（dll 路径/ABI）");
        bench("rust-native", iters, warmup, || {
            native.render(&model).expect("渲染失败")
        });
    }
}

/// 页面模式基准（F008/F009）：视图引擎（可选原生产物），入口视图名约定 `template`。
fn bench_view_mode(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("用法：bench-view --view <目录> <data.json> [iterations] [warmup] [nativeDir]");
        return 2;
    }
    let dir = &args[0];
    let data_path = &args[1];
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let warmup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1_000);
    let native_dir = args.get(4);

    let engine = dhrust::razor::view::ViewEngine::new(Box::new(
        dhrust::razor::view::DirViewLoader::new(dir),
    ));
    if let Some(nd) = native_dir {
        if let Ok(entries) = std::fs::read_dir(nd) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("dll") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                match dhrust::razor::native::NativeTemplate::load(&path) {
                    Ok(native) => engine.register_native(stem, native),
                    Err(e) => {
                        eprintln!("加载原生视图 {stem} 失败：{e}");
                        return 2;
                    }
                }
            }
        }
    }
    let label = if native_dir.is_some() {
        "rust-view-native"
    } else {
        "rust-view"
    };
    let data_src = std::fs::read_to_string(data_path).expect("读取数据失败");
    let json: serde_json::Value = serde_json::from_str(&data_src).expect("数据 JSON 无效");
    let model = dhrust::razor::Value::from(json);
    bench(label, iters, warmup, || {
        engine.render("template", &model).expect("渲染失败")
    });
    0
}

/// 单引擎计时：warmup → 计时循环（逐次采样）→ 吞吐/分位输出。
fn bench(engine: &str, iters: usize, warmup: usize, mut render: impl FnMut() -> String) {
    let mut checksum: u64 = 0;
    for _ in 0..warmup {
        checksum ^= render().len() as u64;
    }

    let mut samples: Vec<u64> = Vec::with_capacity(iters);
    let total = Instant::now();
    for _ in 0..iters {
        let t0 = Instant::now();
        let out = render();
        samples.push(t0.elapsed().as_nanos() as u64);
        checksum = checksum.wrapping_add(out.len() as u64);
    }
    let total = total.elapsed();

    samples.sort_unstable();
    let pct = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize] as f64 / 1000.0;
    let mean = samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1000.0;

    println!("engine={engine}");
    println!("iterations={iters}");
    println!("total_ms={:.1}", total.as_secs_f64() * 1000.0);
    println!("ops_per_sec={:.0}", iters as f64 / total.as_secs_f64());
    println!("mean_us={mean:.2}");
    println!("p50_us={:.2}", pct(0.50));
    println!("p90_us={:.2}", pct(0.90));
    println!("p99_us={:.2}", pct(0.99));
    println!("checksum={checksum}");
}
