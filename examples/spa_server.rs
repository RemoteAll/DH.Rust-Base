//! SPA 与 MVC 一体化示例（对标 ASP.NET Core：`UseStaticFiles` + `MapControllers` + `MapFallbackToFile`）。
//!
//! 运行（默认 `127.0.0.1:8080`，可通过首个参数改端口）：
//!
//! ```text
//! cargo run --features net --example spa_server -- 8080
//! cargo run --features net,razor --example spa_server -- 8080   # 追加真实 Razor 视图渲染
//! ```
//!
//! 一个端口上同时存在四条请求路径（"后端 + SPA 同进程"）：
//! - `/api/{action}`  → JSON 控制器（`Controller` + `json_result` 信封）；
//! - `/home/index`    → MVC 页面（启用 `razor` 特性时走真实视图 `Views/Home/Index.cshtml`）；
//! - `/assets/*.js|css` → 嵌入的 SPA 构建产物（`embed_many`，单文件部署）；
//! - 其余路径         → SPA 深链接回退 `index.html`（`/dashboard`、浏览器硬刷新），
//!   而 `/api` 前缀被 `spa_excludes` 排除，未知接口保持 JSON 404。
//!
//! 提示：真实项目的资源表由 `build.rs` 扫描前端 `dist` 生成后 `include!` 进来，
//! 无需手写；完整生成器模板与工作流见 `Doc/SPA与MVC一体化.md`。

#[cfg(feature = "net")]
#[tokio::main]
async fn main() {
    use dhrust::net::controller::{json_result, Controller};
    use dhrust::net::http::{json_escape, HttpOutcome, HttpResponse, HttpServer};
    use dhrust::net::router::{route, Router};
    use dhrust::net::static_files::StaticFiles;
    use serde_json::json;

    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let mut router = Router::new();

    // ① API 控制器：/api/ping、/api/time（JSON 信封，对齐 C# 控制器惯例）
    Controller::new("api")
        .get("ping", |_ctx| json_result(0, "", None))
        .get("time", |_ctx| {
            json_result(
                0,
                "",
                Some(json!({
                    "time": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
                })),
            )
        })
        .mount(&mut router);

    // ② MVC 页面（服务端渲染）：启用 razor 特性时为真实视图渲染
    #[cfg(feature = "razor")]
    {
        use dhrust::net::controller::view;
        Controller::new("home")
            .with_views_root(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/spa_content/Views"
            ))
            .get("index", |_ctx| {
                view(json!({ "Title": "dhrust · SPA 与 MVC 并存（Razor 服务端渲染）" }))
            })
            .mount(&mut router);
    }
    #[cfg(not(feature = "razor"))]
    {
        // 未启用 razor 特性时的等价页面（启用后自动换为真实视图渲染）
        router.map_get(
            "/home/index",
            route(|_ctx| async move {
                HttpOutcome::Response(HttpResponse::bytes(
                    200,
                    "text/html; charset=utf-8",
                    "<h1>dhrust · SPA 与 MVC 并存</h1>\
                     <p>（启用 razor 特性后可看到真实视图渲染：\
                     cargo run --features net,razor --example spa_server）</p>"
                        .to_string(),
                ))
            }),
        );
    }

    // ③ SPA：编译期嵌入的构建产物（embed_many）+ 深链接回退
    //    真实项目中由 build.rs 生成 SPA_FILES 资源表（见 Doc/SPA与MVC一体化.md）
    let statics = StaticFiles::new("wwwroot") // 开发期可放磁盘目录（嵌入资源优先命中）
        .embed_many(&[
            ("index.html", include_bytes!("spa_content/dist/index.html")),
            (
                "assets/app-4f2a1c.js",
                include_bytes!("spa_content/dist/assets/app-4f2a1c.js"),
            ),
            (
                "assets/app-4f2a1c.css",
                include_bytes!("spa_content/dist/assets/app-4f2a1c.css"),
            ),
        ])
        .spa_fallback(true)
        .spa_excludes(&["/api"]); // 后端命名空间不参与 SPA 回退

    router.fallback(route(move |ctx| {
        let statics = statics.clone();
        async move {
            // GET/HEAD：真实文件 → SPA 回退（带 ETag/条件请求）；其他方法只允许命中真实文件
            let method_ok = ctx.req.method.eq_ignore_ascii_case("GET")
                || ctx.req.method.eq_ignore_ascii_case("HEAD");
            let served = if method_ok {
                statics.try_serve_request(&ctx.req)
            } else {
                statics.try_serve_file(&ctx.req.path)
            };
            if let Some(response) = served {
                return HttpOutcome::Response(response);
            }
            // 其余未命中：JSON 404（与控制器信封风格一致）
            HttpOutcome::Response(HttpResponse::json(
                404,
                format!(
                    "{{\"code\":404,\"message\":\"未找到: {}\"}}",
                    json_escape(&ctx.req.path)
                ),
            ))
        }
    }));

    let addr = format!("127.0.0.1:{port}");
    let server = match HttpServer::bind(addr.as_str()).await {
        Ok(server) => server,
        Err(e) => {
            eprintln!("绑定 {addr} 失败：{e}");
            std::process::exit(1);
        }
    };

    println!("spa_server 已启动（Ctrl+C 退出）：");
    println!("  SPA 首页     http://{addr}/");
    println!("  SPA 深链接   http://{addr}/dashboard        # 回退 index.html");
    println!("  SPA 资源     http://{addr}/assets/app-4f2a1c.js");
    println!("  MVC 页面     http://{addr}/home/index");
    println!("  API          http://{addr}/api/ping");
    println!("  API 404      http://{addr}/api/unknown      # JSON 404（排除前缀）");

    if let Err(e) = server.serve(router.into_handler()).await {
        eprintln!("服务退出：{e}");
    }
}

#[cfg(not(feature = "net"))]
fn main() {
    eprintln!("spa_server 需要启用 feature：cargo run --features net --example spa_server");
    std::process::exit(2);
}
