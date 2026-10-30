//! `ConnStats`：跨平台连接计数（独立于 `HttpServerOptions`，不破坏任何现有消费方）。
//!
//! 覆盖：完整请求后 `current` 归零、`total` 累加；两条并发连接时 `current`/`peak` 正确；
//! 未传统计器的 `bind` 行为不变。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use dhrust::net::http::{ConnStats, HttpHandler, HttpOutcome, HttpResponse, HttpServerOptions};
use dhrust::net::listener::Listener;
use dhrust::net::router::{route, Ctx, Router};

fn ping_handler() -> HttpHandler {
    let mut router = Router::new();
    router.map_get(
        "/ping",
        route(|_ctx: Ctx| async move {
            HttpOutcome::Response(HttpResponse::text(200, "pong"))
        }),
    );
    router.into_handler()
}

fn http_get(addr: SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).expect("连接监听");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

#[test]
fn conn_stats_tracks_current_total_and_peak() {
    let stats = ConnStats::new();
    let listener = Listener::bind_with_stats(
        "127.0.0.1:0",
        "test-conn-stats",
        ping_handler(),
        HttpServerOptions::default(),
        stats.clone(),
    )
    .expect("绑定应成功");
    let addr = listener.addr();

    // 1) 一次完整请求（Connection: close）→ 累计 +1，结束后当前回到 0
    let resp = http_get(addr);
    assert!(resp.contains("200"), "响应异常：{resp}");
    assert!(resp.contains("pong"), "响应异常：{resp}");
    assert_eq!(stats.total(), 1, "应累计 1 条连接");
    assert!(
        wait_until(|| stats.current() == 0, Duration::from_secs(5)),
        "请求结束后当前连接应归零，实际 {}",
        stats.current()
    );

    // 2) 两条并发连接（不发请求、保持打开）→ 当前 = 2、峰值 ≥ 2、累计 ≥ 3
    let s1 = TcpStream::connect(addr).expect("连接 1");
    let s2 = TcpStream::connect(addr).expect("连接 2");
    assert!(
        wait_until(|| stats.current() == 2, Duration::from_secs(5)),
        "应有 2 条活动连接，实际 {}",
        stats.current()
    );
    assert!(stats.peak() >= 2, "峰值应 ≥ 2，实际 {}", stats.peak());
    assert!(
        stats.total() >= 3,
        "累计应包含两条新连接，实际 {}",
        stats.total()
    );

    drop(s1);
    drop(s2);
    assert!(
        wait_until(|| stats.current() == 0, Duration::from_secs(5)),
        "连接关闭后应归零，实际 {}",
        stats.current()
    );

    // 3) 峰值可重置为当前值（分窗口观测）
    stats.reset_peak();
    assert_eq!(stats.peak(), stats.current());

    listener.shutdown();
}

#[test]
fn bind_without_stats_keeps_working() {
    let listener = Listener::bind(
        "127.0.0.1:0",
        "test-no-stats",
        ping_handler(),
        HttpServerOptions::default(),
    )
    .expect("绑定应成功");
    let resp = http_get(listener.addr());
    assert!(resp.contains("pong"), "响应异常：{resp}");
    listener.shutdown();
}
