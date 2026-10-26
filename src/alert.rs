//! 告警中心：企业微信 / 钉钉双通道，**异步派发 + 冷却去重 + 统计**。
//!
//! 之所以收编到公共库：任何带面板/服务的项目都需要「出事了推群」——通道是同一套
//! （[`crate::wecom`] / [`crate::dingtalk`]），派发纪律也是同一套（异步、冷却、不刷屏），
//! 留在各项目里必然写成 N 份。
//!
//! 用法（典型）：
//! ```no_run
//! use dhrust::alert::{AlertHub, AlertLevel, AlertOptions};
//! use std::time::Duration;
//!
//! let hub = AlertHub::new();
//! let opts = AlertOptions::default()
//!     .with_wecom("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx");
//! // 业务告警：异步、30 分钟冷却
//! hub.notify(&opts, "biz:wx1", AlertLevel::Critical, "取令牌失败", &["连续 3 次".into()], Duration::from_secs(1800));
//! ```
//!
//! 设计要点：
//! - **异步**：[`AlertHub::notify`] 只做「冷却判定 + 起线程」就返回，调用方（请求线程、
//!   启动流程、定时作业）一秒都不被网络阻塞；发送失败只记日志。
//! - **冷却 + 在途去重**：同一 `key` 在冷却窗口内只发一次，且同一 `key` 不会并发发多条
//!   （依赖抖动时不会刷屏）。
//! - **配置由调用方给**：[`AlertOptions`] 是纯值对象，项目里从自己的配置/面板映射过来即可，
//!   改完下一次调用立即生效（无需重启）。
//! - 不引入新依赖：企业微信走 [`crate::wecom`]，钉钉走 [`crate::dingtalk`]。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// 告警级别（影响标题图标、级别文字与日志级别）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertLevel {
    /// 恢复 / 信息
    Info,
    /// 需要注意（降级但仍可用）
    Warn,
    /// 严重（功能受损）
    Critical,
}

impl AlertLevel {
    /// 标题图标。
    pub fn icon(self) -> &'static str {
        match self {
            AlertLevel::Info => "✅",
            AlertLevel::Warn => "⚠️",
            AlertLevel::Critical => "🚨",
        }
    }

    /// 级别文字。
    pub fn label(self) -> &'static str {
        match self {
            AlertLevel::Info => "恢复",
            AlertLevel::Warn => "警告",
            AlertLevel::Critical => "严重",
        }
    }
}

/// 告警通道。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertChannel {
    /// 仅企业微信
    WeCom,
    /// 仅钉钉
    DingTalk,
    /// 两者都发（任一成功即算成功）
    Both,
}

impl AlertChannel {
    /// 从配置文本解析（`wecom` / `dingtalk` / `both`，大小写与空白不敏感；未知按 `wecom`）。
    pub fn parse(text: &str) -> AlertChannel {
        match text.trim().to_ascii_lowercase().as_str() {
            "dingtalk" | "ding" => AlertChannel::DingTalk,
            "both" | "all" => AlertChannel::Both,
            _ => AlertChannel::WeCom,
        }
    }

    /// 配置文本（写配置/面板回显用）。
    pub fn as_str(self) -> &'static str {
        match self {
            AlertChannel::WeCom => "wecom",
            AlertChannel::DingTalk => "dingtalk",
            AlertChannel::Both => "both",
        }
    }

    /// 中文名。
    pub fn label(self) -> &'static str {
        match self {
            AlertChannel::WeCom => "企业微信",
            AlertChannel::DingTalk => "钉钉",
            AlertChannel::Both => "企业微信+钉钉",
        }
    }
}

/// 告警选项（纯值对象；由调用方从自己的配置映射）。
#[derive(Clone, Debug)]
pub struct AlertOptions {
    /// 总开关
    pub enabled: bool,
    /// 通道
    pub channel: AlertChannel,
    /// 企业微信机器人 webhook（含 key）
    pub wecom_webhook: String,
    /// 钉钉群机器人 webhook（含 access_token）
    pub dingtalk_webhook: String,
    /// 钉钉「加签」密钥（留空 = 不加签）
    pub dingtalk_secret: String,
    /// 单次请求超时
    pub timeout: Duration,
    /// 忽略证书校验（内网自签场景）
    pub insecure_tls: bool,
}

impl Default for AlertOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: AlertChannel::WeCom,
            wecom_webhook: String::new(),
            dingtalk_webhook: String::new(),
            dingtalk_secret: String::new(),
            timeout: Duration::from_secs(8),
            insecure_tls: false,
        }
    }
}

impl AlertOptions {
    /// 设置企业微信 webhook。
    pub fn with_wecom(mut self, webhook: impl Into<String>) -> Self {
        self.wecom_webhook = webhook.into();
        self.enabled = true;
        self
    }

    /// 设置钉钉 webhook（`secret` 留空表示不加签）。
    pub fn with_dingtalk(mut self, webhook: impl Into<String>, secret: impl Into<String>) -> Self {
        self.dingtalk_webhook = webhook.into();
        self.dingtalk_secret = secret.into();
        self.enabled = true;
        self
    }

    /// 企业微信是否可用（通道选中且已配 webhook）。
    pub fn wecom_ready(&self) -> bool {
        matches!(self.channel, AlertChannel::WeCom | AlertChannel::Both)
            && !self.wecom_webhook.trim().is_empty()
    }

    /// 钉钉是否可用（通道选中且已配 webhook）。
    pub fn dingtalk_ready(&self) -> bool {
        matches!(self.channel, AlertChannel::DingTalk | AlertChannel::Both)
            && !self.dingtalk_webhook.trim().is_empty()
    }

    /// 至少一个通道可用（与开关无关）。
    pub fn configured(&self) -> bool {
        self.wecom_ready() || self.dingtalk_ready()
    }

    /// 真的能发（开关 + 至少一个通道可用）。
    pub fn can_send(&self) -> bool {
        self.enabled && self.configured()
    }
}

/// 告警统计（纯内存、零轮询）。
#[derive(Clone, Debug, Default)]
pub struct AlertStats {
    /// 已成功发出的条数
    pub sent: u64,
    /// 发送失败条数
    pub failed: u64,
    /// 因冷却/在途/未配置而跳过的条数
    pub skipped: u64,
    /// 最近一次成功时间（Unix 秒）
    pub last_sent_at: i64,
    /// 最近一次成功摘要
    pub last_message: String,
    /// 最近一次失败原因（**不含 webhook 原文**）
    pub last_error: String,
}

/// 告警中心（可全局共享；`Arc<AlertHub>` 建议进程内只建一个）。
pub struct AlertHub {
    /// 冷却表：key → 上次成功发送时刻
    last: Arc<Mutex<HashMap<String, Instant>>>,
    /// 在途表：同一 key 同时只有一条在发
    in_flight: Arc<Mutex<HashSet<String>>>,
    stats: Arc<RwLock<AlertStats>>,
}

impl Default for AlertHub {
    fn default() -> Self {
        Self {
            last: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            stats: Arc::new(RwLock::new(AlertStats::default())),
        }
    }
}

impl AlertHub {
    /// 新建（返回 `Arc`，便于跨线程共享）。
    pub fn new() -> Arc<AlertHub> {
        Arc::new(AlertHub::default())
    }

    /// 统计快照。
    pub fn stats(&self) -> AlertStats {
        self.stats
            .read()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// 异步发送一条告警。
    ///
    /// 返回 `true` 表示已入队（可能稍后才真正发出）；`false` 表示未配置/冷却中/在途/线程创建失败。
    /// <param name="opts">通道与开关</param>
    /// <param name="key">去重键（同一业务对象/事件用同一个 key，例如 `biz:wx1`、`sys:redis`）</param>
    /// <param name="level">级别</param>
    /// <param name="title">标题</param>
    /// <param name="lines">正文行（自动加 `> ` 前缀）</param>
    /// <param name="cooldown">同一 key 的冷却时长</param>
    pub fn notify(
        &self,
        opts: &AlertOptions,
        key: &str,
        level: AlertLevel,
        title: &str,
        lines: &[String],
        cooldown: Duration,
    ) -> bool {
        if !opts.can_send() {
            return false;
        }
        {
            let mut last = match self.last.lock() {
                Ok(v) => v,
                Err(e) => e.into_inner(),
            };
            if let Some(at) = last.get(key) {
                if at.elapsed() < cooldown {
                    self.bump(|s| s.skipped += 1);
                    return false;
                }
            }
            // 顺便清理早已过期的键（键数量 = 账号数 + 少量系统键，开销可忽略）
            let horizon = cooldown.max(Duration::from_secs(60)) * 2;
            last.retain(|_, at| at.elapsed() < horizon);
        }
        {
            let mut in_flight = match self.in_flight.lock() {
                Ok(v) => v,
                Err(e) => e.into_inner(),
            };
            if !in_flight.insert(key.to_string()) {
                self.bump(|s| s.skipped += 1);
                return false;
            }
        }

        let opts = opts.clone();
        let key = key.to_string();
        let title = title.to_string();
        let text = format_message(level, &title, lines, &[]);
        let stats = self.stats.clone();
        let last = self.last.clone();
        let in_flight = self.in_flight.clone();
        let thread_key = key.clone();

        let spawned = std::thread::Builder::new()
            .name("dhrust-alert".to_string())
            .spawn(move || {
                match send_all(&opts, &title, &text) {
                    Ok(channels) => {
                        {
                            let mut s = lock_rw(&stats);
                            s.sent += 1;
                            s.last_sent_at = crate::times::gettimestamp() as i64;
                            s.last_message = format!("[{key}] {title} → {channels}");
                        }
                        lock_plain(&last).insert(key.clone(), Instant::now());
                        if level == AlertLevel::Info {
                            crate::info!("告警已发送（{key}）：{channels}");
                        } else {
                            crate::warn!("告警已发送（{key}）：{channels}");
                        }
                    }
                    Err(e) => {
                        {
                            let mut s = lock_rw(&stats);
                            s.failed += 1;
                            s.last_error = e.clone();
                        }
                        crate::warn!("告警发送失败（{key}）：{e}");
                    }
                }
                lock_plain(&in_flight).remove(&key);
            });
        if spawned.is_err() {
            lock_plain(&self.in_flight).remove(thread_key.as_str());
            self.bump(|s| s.failed += 1);
            crate::warn!("告警线程创建失败（不影响业务）：{thread_key}");
            return false;
        }
        true
    }

    /// 同步发送一条消息（面板「发送测试」用；**不受开关与冷却限制**，但要求通道已配置）。
    ///
    /// 返回成功的通道名（如 `企业微信、钉钉`）；未配置或全部失败时返回错误文本。
    pub fn send_test(&self, opts: &AlertOptions, title: &str, lines: &[String]) -> Result<String, String> {
        if !opts.configured() {
            return Err(format!("当前通道（{}）未配置 webhook，无法发送", opts.channel.label()));
        }
        let text = format_message(AlertLevel::Info, title, lines, &[]);
        send_all(opts, title, &text)
    }

    fn bump(&self, f: impl FnOnce(&mut AlertStats)) {
        let mut stats = lock_rw(&self.stats);
        f(&mut stats);
    }
}

fn lock_plain<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(v) => v,
        Err(e) => e.into_inner(),
    }
}

fn lock_rw<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match m.write() {
        Ok(v) => v,
        Err(e) => e.into_inner(),
    }
}

/// 组装 markdown 正文（两个通道共用；末尾自动补「时间 / 主机 / PID」）。
/// <param name="level">级别</param>
/// <param name="title">标题</param>
/// <param name="lines">正文行</param>
/// <param name="footer">额外附加行（可空）</param>
pub fn format_message(level: AlertLevel, title: &str, lines: &[String], footer: &[String]) -> String {
    let mut text = format!("{} **{}**（{}）\n", level.icon(), title, level.label());
    for line in lines {
        text.push_str("> ");
        text.push_str(line);
        text.push('\n');
    }
    for line in footer {
        text.push_str("> ");
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(&format!(
        "> 时间：{}\n> 主机：{} · PID {}",
        crate::times::now_local_text(),
        hostname(),
        std::process::id()
    ));
    text
}

/// 主机名（拿不到就用 `-`）。
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "-".to_string())
}

/// 按选项把消息发到所有可用通道；返回成功通道列表。
fn send_all(opts: &AlertOptions, title: &str, text: &str) -> Result<String, String> {
    let mut ok: Vec<&str> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    if opts.wecom_ready() {
        let bot = crate::wecom::WeComBot::new(opts.wecom_webhook.trim())
            .with_timeout(opts.timeout)
            .with_insecure_tls(opts.insecure_tls);
        match bot.send_markdown_blocking(text) {
            Ok(_) => ok.push("企业微信"),
            Err(e) => errors.push(format!("企业微信：{}", e.0)),
        }
    }
    if opts.dingtalk_ready() {
        let bot = crate::dingtalk::DingTalkBot::new(opts.dingtalk_webhook.trim())
            .with_secret(opts.dingtalk_secret.trim())
            .with_timeout(opts.timeout)
            .with_insecure_tls(opts.insecure_tls);
        match bot.send_markdown_blocking(title, text) {
            Ok(_) => ok.push("钉钉"),
            Err(e) => errors.push(format!("钉钉：{}", e.0)),
        }
    }

    if errors.is_empty() {
        Ok(ok.join("、"))
    } else if ok.is_empty() {
        Err(errors.join("；"))
    } else {
        // 部分成功：按成功处理，失败原因记日志
        for e in &errors {
            crate::warn!("告警部分通道失败：{e}");
        }
        Ok(ok.join("、"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_parse_and_roundtrip() {
        assert_eq!(AlertChannel::parse("DingTalk"), AlertChannel::DingTalk);
        assert_eq!(AlertChannel::parse(" both "), AlertChannel::Both);
        assert_eq!(AlertChannel::parse("wecom"), AlertChannel::WeCom);
        assert_eq!(AlertChannel::parse("bogus"), AlertChannel::WeCom, "未知按企业微信");
        for ch in [AlertChannel::WeCom, AlertChannel::DingTalk, AlertChannel::Both] {
            assert_eq!(AlertChannel::parse(ch.as_str()), ch);
        }
    }

    #[test]
    fn options_readiness_follows_channel() {
        let mut opts = AlertOptions {
            enabled: true,
            ..Default::default()
        };
        assert!(!opts.can_send(), "没配 webhook 不能发");

        opts.wecom_webhook = "https://qyapi.weixin.qq.com/...".into();
        assert!(opts.can_send(), "wecom 通道 + webhook 应可发");

        opts.channel = AlertChannel::DingTalk;
        assert!(!opts.can_send(), "切到钉钉后企业微信 webhook 不算数");

        opts.dingtalk_webhook = "https://oapi.dingtalk.com/...".into();
        assert!(opts.can_send());

        opts.channel = AlertChannel::Both;
        opts.wecom_webhook.clear();
        assert!(opts.can_send(), "both 通道下钉钉可用即可发");

        opts.enabled = false;
        assert!(!opts.can_send(), "总开关关闭时一律不发");
    }

    #[test]
    fn notify_is_rejected_when_not_configured() {
        let hub = AlertHub::new();
        let opts = AlertOptions {
            enabled: true,
            ..Default::default()
        };
        assert!(!hub.notify(&opts, "k", AlertLevel::Warn, "t", &[], Duration::from_secs(60)));
        assert_eq!(hub.stats().sent, 0);
    }

    #[test]
    fn cooldown_blocks_duplicate_keys() {
        let hub = AlertHub::new();
        let opts = AlertOptions::default().with_wecom("http://127.0.0.1:1/never");
        // 第一次入队（地址不可达，发送会失败，但冷却只记成功 → 这里只验证「第一次不被跳过」）
        assert!(hub.notify(&opts, "dup", AlertLevel::Warn, "t", &[], Duration::from_secs(600)));
        // 在途表尚未释放（发送很慢/失败），第二次必须是跳过
        let second = hub.notify(&opts, "dup", AlertLevel::Warn, "t", &[], Duration::from_secs(600));
        assert!(!second, "同一 key 并发只允许一条");
        assert!(hub.stats().skipped >= 1);
    }

    #[test]
    fn format_message_contains_level_title_and_meta() {
        let text = format_message(
            AlertLevel::Critical,
            "Redis 不可用",
            &["已降级为进程内锁".to_string()],
            &["版本 v1.2.3".to_string()],
        );
        assert!(text.contains("Redis 不可用"), "{text}");
        assert!(text.contains("已降级为进程内锁"), "{text}");
        assert!(text.contains("严重"), "{text}");
        assert!(text.contains("版本 v1.2.3"), "{text}");
        assert!(text.contains("时间："), "{text}");
        assert!(text.contains("PID "), "{text}");
    }

    #[test]
    fn send_test_requires_configured_channel() {
        let hub = AlertHub::new();
        let err = hub.send_test(&AlertOptions::default(), "标题", &[]).unwrap_err();
        assert!(err.contains("未配置"), "{err}");
    }
}
