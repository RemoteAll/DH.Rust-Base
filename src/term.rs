//! 真 PTY 会话引擎（Windows ConPTY / Unix openpty）——在线终端等服务端能力共用。
//!
//! 特性 `term`（可选依赖 portable-pty：Windows 0.8 / Unix 0.9——0.9 的 nix 0.28
//! 支持 loongarch64 交叉编译，0.9 在 Windows 的 PSUEDOCONSOLE_INHERIT_CURSOR 标志下
//! ConPTY 无输出，故双版本拆分，见 Cargo.toml）。
//!
//! 职责：常驻 shell 会话的创建/复用/归属校验、输出回放环、订阅通道（给 WebSocket 等
//! 传输层转发）、输入写入、尺寸调整、空闲回收与整体关闭。**不包含** HTTP/WS/鉴权/审计——
//! 由消费方（如 Pek.RAgent 面板）自行组合（典型用法见 Pek.RAgent `src/terminal.rs`）。
//!
//! ```no_run
//! # fn demo() -> Result<(), String> {
//! use dhrust::term::{TermOptions, Terminal};
//! let term = Terminal::new(TermOptions { cwd: None, ..Default::default() });
//! term.ensure("session01", "admin", 120, 30)?;          // 取或建会话
//! let replay = term.replay("session01");                 // 重连回放
//! let rx = term.subscribe("session01")?;                 // 订阅输出（替换旧订阅者）
//! term.write("session01", b"ls\r\n");                    // 键盘输入
//! term.resize("session01", 140, 40);                     // 终端尺寸
//! // while let Ok(data) = rx.recv() { /* 转发到 WS/SSE... */ }
//! term.reset("session01");                               // 杀 shell（下次 ensure 重建）
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// 终端尺寸限制（ensure/resize 入参钳制）。
pub const MIN_COLS: u16 = 20;
/// 终端尺寸限制。
pub const MAX_COLS: u16 = 500;
/// 终端尺寸限制。
pub const MIN_ROWS: u16 = 5;
/// 终端尺寸限制。
pub const MAX_ROWS: u16 = 200;

/// 会话 id 规则：8~64 位字母/数字/`-`/`_`（防文件名/日志注入；消费方应在入口校验）。
pub fn valid_sid(sid: &str) -> bool {
    (8..=64).contains(&sid.len())
        && sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 引擎配置。
#[derive(Clone)]
pub struct TermOptions {
    /// shell 程序（None = 平台默认：Windows `%COMSPEC%`/cmd.exe；Unix `/bin/bash -l`，缺省 `/bin/sh`）
    pub shell: Option<String>,
    /// shell 附加参数（仅 `shell` 为 Some 时生效）
    pub shell_args: Vec<String>,
    /// 起始工作目录（None = 进程当前目录）
    pub cwd: Option<PathBuf>,
    /// 附加环境变量（固定追加 `TERM=xterm-256color`）
    pub env: Vec<(String, String)>,
    /// 会话上限（满员拒绝新建）
    pub max_sessions: usize,
    /// 回放缓冲上限（重连回放最近输出）
    pub ring_max: usize,
    /// 空闲回收时长（无订阅且空闲超时；`purge_idle` 时执行）
    pub idle_timeout: Duration,
}

impl Default for TermOptions {
    fn default() -> Self {
        Self {
            shell: None,
            shell_args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            max_sessions: 8,
            ring_max: 128 * 1024,
            idle_timeout: Duration::from_secs(1800),
        }
    }
}

/// 订阅汇（当前订阅者；重新 subscribe 时替换，旧转发方的接收端随之断开）。
struct WsSink {
    tx: Mutex<Option<Sender<Vec<u8>>>>,
}

/// 会话（一个常驻 shell）。
struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    ring: Arc<Mutex<Vec<u8>>>,
    sink: Arc<WsSink>,
    alive: Arc<AtomicBool>,
    owner: String,
    last_active: Instant,
    cols: u16,
    rows: u16,
}

impl Session {
    /// 结束进程并断开订阅（幂等）。
    fn cleanup(&mut self) {
        if let Ok(mut g) = self.sink.tx.lock() {
            g.take();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// PTY 会话管理器（可跨线程共享；建议以 `Arc<Terminal>` 持有）。
pub struct Terminal {
    opts: TermOptions,
    map: Arc<Mutex<HashMap<String, Session>>>,
}

impl Terminal {
    /// 创建管理器。
    pub fn new(opts: TermOptions) -> Arc<Terminal> {
        Arc::new(Terminal {
            opts,
            map: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 取或建会话：不存在则新建；存在则校验归属、应用尺寸并刷新活跃时间。
    pub fn ensure(&self, sid: &str, owner: &str, cols: u16, rows: u16) -> Result<(), String> {
        let (cols, rows) = clamp_size(cols, rows);
        let mut map = self.map.lock().unwrap();
        self.purge_locked(&mut map);
        if !map.contains_key(sid) {
            if map.len() >= self.opts.max_sessions.max(1) {
                return Err("终端会话数已达上限（请先重置不用的会话）".to_string());
            }
            let s = self.spawn(sid, owner, cols, rows)?;
            map.insert(sid.to_string(), s);
        }
        let s = map.get_mut(sid).expect("session exists");
        if s.owner != owner {
            return Err("会话归属校验失败（请重置会话）".to_string());
        }
        s.cols = cols;
        s.rows = rows;
        let _ = s.master.resize(pty_size(cols, rows));
        s.last_active = Instant::now();
        Ok(())
    }

    /// 回放缓冲快照（重连时先发再订阅，避免丢新输出）。
    pub fn replay(&self, sid: &str) -> Vec<u8> {
        let map = self.map.lock().unwrap();
        map.get(sid)
            .and_then(|s| s.ring.lock().ok().map(|r| r.clone()))
            .unwrap_or_default()
    }

    /// 订阅输出（替换旧订阅者：旧接收端会随发送端替换而断开）；返回接收端。
    pub fn subscribe(&self, sid: &str) -> Result<std::sync::mpsc::Receiver<Vec<u8>>, String> {
        let map = self.map.lock().unwrap();
        let s = map.get(sid).ok_or_else(|| "会话不存在".to_string())?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Ok(mut g) = s.sink.tx.lock() {
            g.replace(tx);
        }
        Ok(rx)
    }

    /// 取消订阅（断开当前转发方，会话保留）。
    pub fn unsubscribe(&self, sid: &str) {
        let map = self.map.lock().unwrap();
        if let Some(s) = map.get(sid) {
            if let Ok(mut g) = s.sink.tx.lock() {
                g.take();
            }
        }
    }

    /// 写入键盘输入。
    pub fn write(&self, sid: &str, data: &[u8]) {
        let mut map = self.map.lock().unwrap();
        if let Some(s) = map.get_mut(sid) {
            let _ = s.writer.write_all(data);
            let _ = s.writer.flush();
            s.last_active = Instant::now();
        }
    }

    /// 调整终端尺寸（越界自动钳制）。
    pub fn resize(&self, sid: &str, cols: u16, rows: u16) {
        let (cols, rows) = clamp_size(cols, rows);
        let mut map = self.map.lock().unwrap();
        if let Some(s) = map.get_mut(sid) {
            s.cols = cols;
            s.rows = rows;
            let _ = s.master.resize(pty_size(cols, rows));
        }
    }

    /// 重置会话（杀 shell + 断订阅；下次 ensure 重建）。
    pub fn reset(&self, sid: &str) {
        let removed = self.map.lock().unwrap().remove(sid);
        if let Some(mut s) = removed {
            s.cleanup();
        }
    }

    /// 会话是否存活（进程未退出）。
    pub fn is_alive(&self, sid: &str) -> bool {
        let map = self.map.lock().unwrap();
        map.get(sid)
            .map(|s| s.alive.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// 清理空闲（无订阅且超时）或已死亡的会话。
    pub fn purge_idle(&self) {
        let mut map = self.map.lock().unwrap();
        self.purge_locked(&mut map);
    }

    /// 关闭全部会话（进程退出清理用）。
    pub fn close_all(&self) {
        let mut map = self.map.lock().unwrap();
        for (_, mut s) in map.drain() {
            s.cleanup();
        }
    }

    /// 间隔/上限判定（锁内版本）。
    fn purge_locked(&self, map: &mut HashMap<String, Session>) {
        let now = Instant::now();
        let idle_timeout = self.opts.idle_timeout;
        let mut dead: Vec<String> = Vec::new();
        map.retain(|k, s| {
            let subscribed = s.sink.tx.lock().map(|g| g.is_some()).unwrap_or(false);
            let expired = !subscribed && now.duration_since(s.last_active) > idle_timeout;
            if !s.alive.load(Ordering::SeqCst) || expired {
                dead.push(k.clone());
                false
            } else {
                true
            }
        });
        for key in dead {
            if let Some(mut s) = map.remove(&key) {
                s.cleanup();
            }
        }
    }

    /// 启动常驻 shell（PTY）。
    fn spawn(&self, sid: &str, owner: &str, cols: u16, rows: u16) -> Result<Session, String> {
        let pair = native_pty_system()
            .openpty(pty_size(cols, rows))
            .map_err(|e| format!("创建 PTY 失败：{e}"))?;

        let mut cmd = match &self.opts.shell {
            Some(shell) => {
                let mut c = CommandBuilder::new(shell);
                for a in &self.opts.shell_args {
                    c.arg(a);
                }
                c
            }
            None if cfg!(windows) => CommandBuilder::new(
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            ),
            None if std::path::Path::new("/bin/bash").exists() => {
                let mut c = CommandBuilder::new("/bin/bash");
                c.arg("-l"); // 登录 shell（PATH/别名等与 SSH 一致）
                c
            }
            None => CommandBuilder::new("/bin/sh"),
        };
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &self.opts.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &self.opts.cwd {
            if cwd.is_dir() {
                cmd.cwd(cwd);
            }
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动 shell 失败：{e}"))?;
        // 释放从端句柄（否则 master 读取端拿不到 EOF）
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("获取终端读取端失败：{e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("获取终端写入端失败：{e}"))?;

        let ring = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(WsSink {
            tx: Mutex::new(None),
        });
        let alive = Arc::new(AtomicBool::new(true));

        // 读线程：PTY 输出 → 回放环 + 当前订阅通道；EOF（shell 退出）时自清理注册表
        {
            let ring2 = ring.clone();
            let sink2 = sink.clone();
            let alive2 = alive.clone();
            let map2 = self.map.clone();
            let ring_max = self.opts.ring_max;
            let sid2 = sid.to_string();
            std::thread::Builder::new()
                .name(format!("term-{sid}"))
                .spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let data = &buf[..n];
                                if let Ok(mut r) = ring2.lock() {
                                    r.extend_from_slice(data);
                                    let len = r.len();
                                    if len > ring_max {
                                        let cut = len - ring_max;
                                        r.drain(..cut);
                                    }
                                }
                                if let Ok(g) = sink2.tx.lock() {
                                    if let Some(tx) = g.as_ref() {
                                        let _ = tx.send(data.to_vec());
                                    }
                                }
                            }
                        }
                    }
                    alive2.store(false, Ordering::SeqCst);
                    if let Ok(mut g) = sink2.tx.lock() {
                        g.take();
                    }
                    let removed = map2.lock().unwrap().remove(&sid2);
                    if let Some(mut s) = removed {
                        s.cleanup();
                    }
                })
                .map_err(|e| format!("启动终端读取线程失败：{e}"))?;
        }

        Ok(Session {
            master: pair.master,
            writer,
            child,
            ring,
            sink,
            alive,
            owner: owner.to_string(),
            last_active: Instant::now(),
            cols,
            rows,
        })
    }
}

/// 尺寸钳制（公开给消费方在入口复用）。
pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (
        cols.clamp(MIN_COLS, MAX_COLS),
        rows.clamp(MIN_ROWS, MAX_ROWS),
    )
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_sid_rules() {
        assert!(valid_sid("abcd1234"));
        assert!(valid_sid("a-b_c-12345678"));
        assert!(!valid_sid("short"));
        assert!(!valid_sid("has space 12345"));
        assert!(!valid_sid("宝塔12345678"));
        assert!(!valid_sid(&"x".repeat(65)));
    }

    /// 等待回放缓冲出现目标文本。
    fn wait_replay(term: &Terminal, sid: &str, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let r = term.replay(sid);
            if String::from_utf8_lossy(&r).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn pty_roundtrip_echo_cwd_and_chinese() {
        let term = Terminal::new(TermOptions {
            cwd: Some(std::env::temp_dir()),
            ..Default::default()
        });
        term.ensure("test0001", "tester", 100, 30).expect("ensure");
        // 基本回显（真终端：命令会回显 + 输出）
        term.write("test0001", b"echo pty-hello-1\r\n");
        assert!(
            wait_replay(&term, "test0001", "pty-hello-1", Duration::from_secs(20)),
            "echo 回显"
        );
        // 交互式持久：切换目录再查询（PTY 内 cd 持久）
        let cd_cmd = if cfg!(windows) {
            "cd /d %TEMP%"
        } else {
            "cd /tmp"
        };
        term.write("test0001", format!("{cd_cmd}\r\n").as_bytes());
        std::thread::sleep(Duration::from_millis(500));
        term.write("test0001", b"cd\r\n");
        std::thread::sleep(Duration::from_millis(500));
        let out = String::from_utf8_lossy(&term.replay("test0001")).to_string();
        if cfg!(windows) {
            assert!(out.to_ascii_lowercase().contains("temp"), "{out}");
        } else {
            assert!(out.contains("/tmp"), "{out}");
        }
        // 中文（ConPTY/PTY 原生 UTF-8，无需代码页处理）
        term.write("test0001", "echo 中文PTY测试ABC\r\n".as_bytes());
        assert!(
            wait_replay(&term, "test0001", "中文PTY测试ABC", Duration::from_secs(20)),
            "中文输出"
        );
        // 调整尺寸（不应报错）与订阅替换
        term.resize("test0001", 120, 40);
        let rx1 = term.subscribe("test0001").expect("subscribe");
        let _rx2 = term.subscribe("test0001").expect("resubscribe");
        // 旧订阅端应随替换断开（发送端被替换）
        term.write("test0001", b"echo after-resub\r\n");
        let mut got = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx1.try_recv() {
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    got = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        assert!(got, "旧订阅端应断开");
        term.close_all();
    }

    #[test]
    fn ownership_and_max_sessions() {
        let term = Terminal::new(TermOptions {
            cwd: Some(std::env::temp_dir()),
            max_sessions: 2,
            ..Default::default()
        });
        term.ensure("own00001", "userA", 80, 24).expect("first");
        // 归属校验：不同用户使用同一 sid 被拒
        assert!(term.ensure("own00001", "userB", 80, 24).is_err());
        term.ensure("own00002", "userA", 80, 24).expect("second");
        // 上限：第 3 个被拒
        assert!(term.ensure("own00003", "userA", 80, 24).is_err());
        term.reset("own00001");
        term.ensure("own00003", "userA", 80, 24).expect("after reset");
        term.close_all();
    }

    #[test]
    fn cleanup_kills_shell() {
        let term = Terminal::new(TermOptions {
            cwd: Some(std::env::temp_dir()),
            ..Default::default()
        });
        term.ensure("test0002", "tester", 80, 24).expect("ensure");
        assert!(term.is_alive("test0002"));
        term.write("test0002", b"echo ready\r\n");
        assert!(wait_replay(&term, "test0002", "ready", Duration::from_secs(20)));
        term.reset("test0002");
        assert!(!term.is_alive("test0002"));
    }
}
