//! Razor 子集引擎渲染基准（Rust 侧）。
//!
//! 同模板、同数据、warm 状态（解析一次）下重复渲染，输出吞吐与分位延迟；
//! 与 `tools/csharp/RazorInterop bench`（C# 原生 Razor 编译缓存同口径）对照，
//! 由 `scripts/bench_view.ps1` 驱动。
//!
//! 用法：
//!   bench-view <template.cshtml> <data.json> [iterations] [warmup]

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法：bench-view <template.cshtml> <data.json> [iterations] [warmup]");
        std::process::exit(2);
    }
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let warmup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1_000);

    let template_src = std::fs::read_to_string(&args[0]).expect("读取模板失败");
    let data_src = std::fs::read_to_string(&args[1]).expect("读取数据失败");
    let template = dhrust::razor::Template::parse(&template_src).expect("模板解析失败");
    let json: serde_json::Value = serde_json::from_str(&data_src).expect("数据 JSON 无效");
    let model = dhrust::razor::Value::from(json);

    // warm
    let mut checksum: u64 = 0;
    for _ in 0..warmup {
        checksum ^= template.render(&model).expect("渲染失败").len() as u64;
    }

    let mut samples: Vec<u64> = Vec::with_capacity(iters);
    let total = Instant::now();
    for _ in 0..iters {
        let t0 = Instant::now();
        let out = template.render(&model).expect("渲染失败");
        samples.push(t0.elapsed().as_nanos() as u64);
        checksum = checksum.wrapping_add(out.len() as u64);
    }
    let total = total.elapsed();

    samples.sort_unstable();
    let pct = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize] as f64 / 1000.0;
    let mean = samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1000.0;

    println!("engine=rust");
    println!("iterations={iters}");
    println!("total_ms={:.1}", total.as_secs_f64() * 1000.0);
    println!("ops_per_sec={:.0}", iters as f64 / total.as_secs_f64());
    println!("mean_us={mean:.2}");
    println!("p50_us={:.2}", pct(0.50));
    println!("p90_us={:.2}", pct(0.90));
    println!("p99_us={:.2}", pct(0.99));
    println!("checksum={checksum}");
}
