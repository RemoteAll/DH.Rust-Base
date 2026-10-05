//! 监听生命周期管理（专用线程 + 就绪握手 + 停机信号）：**绑定成功后才返回**、
//! `shutdown` 后端口即释放——支撑运行时“热重绑”监听地址（免进程重启）。
//!
//! 本模块把三个消费方各自实现的同款逻辑（HlkProductTool `api.rs` 的 `ServerHandle`、
//! Pek.RPanlServer `server.rs` 的 `spawn_http`、DHDeploy.Agent.Rust `listener.rs`）收敛为一处：
//!
//! - 每个监听器跑在**专用线程**（`current_thread` 运行时）上——调用方无需自备运行时，
//!   在同步上下文（面板动作）与异步上下文（tokio handler）中均可安全使用；
//! - 绑定结果经 `mpsc` 就绪握手确认（成功返回实际 [`SocketAddr`]，失败返回错误且不残留线程）；
//! - 停机：`oneshot` 触发 `serve_with_shutdown` 的 accept 循环退出 → join 线程 → 端口释放；
//!   已接受的连接按 `HttpServerOptions` 的连接模型自然收尾（不影响在途请求）。
//!
//! 典型热重绑流程（回滚策略由调用方决定，如配置回写）：
//!
//! ```ignore
//! let old = slot.take();
//! if let Some(l) = old { l.shutdown(); }               // 旧监听停机、端口释放
//! match Listener::bind_with_retry(addr, "my-http", handler, options, 10, Duration::from_millis(300)) {
//!     Ok(l) => slot.replace(l),                        // 新监听就绪
//!     Err(e) => { /* 回滚：按原地址重新 bind 并恢复配置 */ }
//! }
//! ```

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::net::http::{HttpHandler, HttpServer, HttpServerOptions};

/// 已启动的监听器。
pub struct Listener {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    addr: SocketAddr,
}

impl Listener {
    /// 绑定并启动（仅 HTTP）。
    pub fn bind(
        addr: &str,
        thread_name: &str,
        handler: HttpHandler,
        options: HttpServerOptions,
    ) -> Result<Self, String> {
        Self::bind_inner(addr, thread_name, None, handler, options)
    }

    /// 绑定并启动（HTTPS；`cert_pem`/`key_pem` 为 PEM 字节）。
    #[cfg(feature = "net-tls")]
    pub fn bind_tls(
        addr: &str,
        thread_name: &str,
        cert_pem: &[u8],
        key_pem: &[u8],
        handler: HttpHandler,
        options: HttpServerOptions,
    ) -> Result<Self, String> {
        Self::bind_inner(
            addr,
            thread_name,
            Some((cert_pem.to_vec(), key_pem.to_vec())),
            handler,
            options,
        )
    }

    /// 带重试的绑定（热重绑场景：旧监听刚关闭、端口尚在释放的短暂窗口；
    /// HlkProductTool 实践沉淀——10 次 × 300ms 可覆盖）。
    pub fn bind_with_retry(
        addr: &str,
        thread_name: &str,
        handler: HttpHandler,
        options: HttpServerOptions,
        attempts: usize,
        interval: Duration,
    ) -> Result<Self, String> {
        let mut last = String::new();
        for attempt in 0..attempts.max(1) {
            match Self::bind(addr, thread_name, handler.clone(), options.clone()) {
                Ok(listener) => return Ok(listener),
                Err(e) => {
                    last = e;
                    if attempt + 1 < attempts {
                        std::thread::sleep(interval);
                    }
                }
            }
        }
        Err(last)
    }

    /// 实际监听地址（`0.0.0.0:0` 绑定后为真实端口）。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 停机并等待端口释放（发送停机信号 → join 监听线程）。
    pub fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn bind_inner(
        addr: &str,
        thread_name: &str,
        tls: Option<(Vec<u8>, Vec<u8>)>,
        handler: HttpHandler,
        options: HttpServerOptions,
    ) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<SocketAddr, String>>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let addr_text = addr.to_string();
        let thread = std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("创建监听运行时时失败：{e}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let bound = match tls {
                        #[cfg(feature = "net-tls")]
                        Some((cert, key)) => {
                            HttpServer::bind_tls(&addr_text, &cert, &key).await
                        }
                        #[cfg(not(feature = "net-tls"))]
                        Some(_) => {
                            let _ = ready_tx
                                .send(Err("未启用 net-tls 特性，无法进行 TLS 监听".to_string()));
                            return;
                        }
                        None => HttpServer::bind(&addr_text).await,
                    };
                    let server = match bound {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("监听绑定失败：{e}")));
                            return;
                        }
                    };
                    let local = match server.local_addr() {
                        Ok(a) => a,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("读取监听地址失败：{e}")));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(local));
                    let shutdown = async move {
                        let _ = stop_rx.await;
                    };
                    if let Err(e) = server.serve_with_shutdown(handler, options, shutdown).await {
                        crate::logs::write_line(&format!("监听服务循环退出（{addr_text}）：{e}"));
                    }
                });
            })
            .map_err(|e| format!("启动监听线程失败：{e}"))?;
        let local = match ready_rx.recv() {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err("监听线程提前退出".to_string());
            }
        };
        Ok(Self {
            stop: Some(stop_tx),
            thread: Some(thread),
            addr: local,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// 最小业务处理器（`/ping` → 200 pong）。
    fn ping_handler() -> HttpHandler {
        let mut router = crate::net::router::Router::new();
        router.map_get(
            "/ping",
            crate::net::router::route(|_ctx: crate::net::router::Ctx| async move {
                crate::net::http::HttpOutcome::Response(crate::net::http::HttpResponse::text(
                    200, "pong",
                ))
            }),
        );
        router.into_handler()
    }

    fn http_get(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("连接监听");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        buf
    }

    #[test]
    fn bind_serves_and_shutdown_releases_port() {
        let listener = Listener::bind(
            "127.0.0.1:0",
            "test-listen",
            ping_handler(),
            HttpServerOptions::default(),
        )
        .expect("绑定应成功");
        let addr = listener.addr();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        let resp = http_get(addr, "/ping");
        assert!(resp.contains("200"), "响应异常：{resp}");
        assert!(resp.contains("pong"), "响应异常：{resp}");
        listener.shutdown();

        // 端口已释放：同地址可再次绑定（热重绑场景的核心保证）
        let again = Listener::bind(
            &addr.to_string(),
            "test-listen",
            ping_handler(),
            HttpServerOptions::default(),
        )
        .expect("停机后同地址应可重绑");
        again.shutdown();
    }

    #[test]
    fn bind_fails_on_occupied_port_without_leaking() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("占位监听");
        let occupied = holder.local_addr().unwrap().to_string();
        let err = Listener::bind(
            &occupied,
            "test-listen",
            ping_handler(),
            HttpServerOptions::default(),
        )
        .err()
        .expect("被占用端口应绑定失败");
        assert!(err.contains("监听绑定失败"), "错误信息异常：{err}");
        drop(holder);
        // 占位释放后可绑定（证明失败路径未残留占用）
        let ok = Listener::bind(
            &occupied,
            "test-listen",
            ping_handler(),
            HttpServerOptions::default(),
        )
        .expect("占位释放后应可绑定");
        ok.shutdown();
    }

    #[test]
    fn bind_with_retry_recovers_after_port_released() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("占位监听");
        let addr = holder.local_addr().unwrap().to_string();
        // 100ms 后释放占位；重试窗口 8 × 200ms
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        let listener = Listener::bind_with_retry(
            &addr,
            "test-listen",
            ping_handler(),
            HttpServerOptions::default(),
            8,
            Duration::from_millis(200),
        )
        .expect("重试后应绑定成功");
        let _ = h.join();
        assert_eq!(http_get(listener.addr(), "/ping").contains("200"), true);
        listener.shutdown();
    }
}
