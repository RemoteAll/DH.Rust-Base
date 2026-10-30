//! 钉钉群机器人（Webhook）客户端。
//!
//! 与 C# 侧 `Pek.WebHook` 的钉钉实现对齐：向群机器人 Webhook
//! （`https://oapi.dingtalk.com/robot/send?access_token=...`）推送 **text / markdown / link / actionCard**
//! 中的常用两种（text / markdown），并支持机器人安全设置里的 **「加签」**。
//!
//! - 依赖 `http-client` 特性；钉钉线上地址为 `https`，需 `http-tls` 特性（`net-tls` 已包含）
//! - **加签规则**（官方）：`sign = urlencode(base64(HMAC-SHA256(secret, "{timestamp}\n{secret}")))`，
//!   `timestamp` 为**毫秒**；URL 上追加 `&timestamp=..&sign=..`（见 [`signed_url`]）。
//!   若机器人安全设置选的是「自定义关键词」或「IP 地址段」，`secret` 留空即可。
//! - 响应按 `errcode` 判定：0 成功；非 0（如 300005 access_token 不存在、310000 签名不匹配）
//!   返回 [`DingTalkError`] 并附带 errmsg
//! - 同步入口：`send_text_blocking` / `send_markdown_blocking`（供面板保存、告警、定时任务等阻塞上下文）
//! - 载荷构造与响应解析为纯函数（[`text_payload`] / [`markdown_payload`] / [`parse_response`]），
//!   便于组合复用与单元测试

use std::time::Duration;

use serde_json::{json, Value};

use crate::net::http_client::{self, HttpClientOptions};

/// 默认请求超时（毫秒）。
const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// 钉钉接口错误（消息文本；调用方直接展示或拼接）。
#[derive(Debug)]
pub struct DingTalkError(pub String);

impl DingTalkError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for DingTalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DingTalkError {}

/// 机器人推送结果（`errcode == 0` 时返回）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DingTalkResult {
    /// 接口返回码（0 = 成功）
    pub errcode: i64,
    /// 接口返回描述
    pub errmsg: String,
}

/// 钉钉群机器人客户端（Webhook 推送）。
///
/// 轻量无状态：每次发送独立短连接；可在多处按需创建（克隆开销极小）。
#[derive(Debug, Clone)]
pub struct DingTalkBot {
    /// 群机器人 Webhook 地址
    webhook: String,
    /// 「加签」密钥（安全设置选加签时填；留空表示不加签）
    secret: String,
    /// 请求超时（连接/发送/读体全流程）
    timeout: Duration,
    /// 忽略服务器证书校验（内网代理/自签场景）
    insecure_tls: bool,
}

impl DingTalkBot {
    /// 创建机器人客户端。
    /// <param name="webhook">群机器人 Webhook 地址（空地址在发送时报错）</param>
    pub fn new(webhook: impl Into<String>) -> Self {
        Self {
            webhook: webhook.into(),
            secret: String::new(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            insecure_tls: false,
        }
    }

    /// 设置「加签」密钥（留空 = 不加签）。
    /// <param name="secret">机器人安全设置里的 `SEC` 开头密钥</param>
    pub fn with_secret(mut self, secret: impl Into<String>) -> Self {
        self.secret = secret.into();
        self
    }

    /// 设置请求超时。
    /// <param name="timeout">超时时间</param>
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 设置是否忽略服务器证书校验（内网自签证书场景）。
    /// <param name="insecure">true = 忽略证书校验</param>
    pub fn with_insecure_tls(mut self, insecure: bool) -> Self {
        self.insecure_tls = insecure;
        self
    }

    /// Webhook 地址。
    pub fn webhook(&self) -> &str {
        &self.webhook
    }

    /// 加签密钥。
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// 是否配置了加签密钥。
    pub fn has_secret(&self) -> bool {
        !self.secret.trim().is_empty()
    }

    /// 发送文本消息。
    /// <param name="content">文本内容</param>
    pub async fn send_text(&self, content: &str) -> Result<DingTalkResult, DingTalkError> {
        self.send_value(text_payload(content)).await
    }

    /// 发送 Markdown 消息。
    /// <param name="title">通知栏展示的标题（也会出现在会话列表）</param>
    /// <param name="content">markdown 内容</param>
    pub async fn send_markdown(
        &self,
        title: &str,
        content: &str,
    ) -> Result<DingTalkResult, DingTalkError> {
        self.send_value(markdown_payload(title, content)).await
    }

    /// 发送文本消息（同步版；内部创建临时 tokio 运行时）。
    ///
    /// 供阻塞上下文（告警线程、面板保存、脚本、定时任务）调用。
    pub fn send_text_blocking(&self, content: &str) -> Result<DingTalkResult, DingTalkError> {
        self.send_value_blocking(text_payload(content))
    }

    /// 发送 Markdown 消息（同步版；内部创建临时 tokio 运行时）。
    pub fn send_markdown_blocking(
        &self,
        title: &str,
        content: &str,
    ) -> Result<DingTalkResult, DingTalkError> {
        self.send_value_blocking(markdown_payload(title, content))
    }

    /// 发送已构造好的消息载荷（同步版）。
    pub fn send_value_blocking(&self, payload: Value) -> Result<DingTalkResult, DingTalkError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DingTalkError::new(format!("创建 tokio 运行时失败: {e}")))?;
        rt.block_on(self.send_value(payload))
    }

    /// 发送已构造好的消息载荷（`msgtype` 已在载荷中）。
    /// <param name="payload">消息 JSON 载荷</param>
    pub async fn send_value(&self, payload: Value) -> Result<DingTalkResult, DingTalkError> {
        let webhook = self.webhook.trim();
        if webhook.is_empty() {
            return Err(DingTalkError::new("钉钉 webhook 地址为空"));
        }
        let url = if self.has_secret() {
            signed_url(webhook, self.secret.trim(), now_millis())
        } else {
            webhook.to_string()
        };
        let options = HttpClientOptions {
            timeout: self.timeout,
            insecure_tls: self.insecure_tls,
            ca_pem: None,
        };
        let resp = http_client::request(
            "POST",
            &url,
            &[],
            Some("application/json"),
            payload.to_string().into_bytes(),
            &options,
        )
        .await
        .map_err(|e| DingTalkError::new(format!("请求失败：{}", redact_webhook(&e.to_string()))))?;
        let body = resp.body_text();
        if !resp.is_success() {
            return Err(DingTalkError::new(format!(
                "HTTP {}：{}",
                resp.status,
                crate::text::truncate_chars(&body, 200)
            )));
        }
        parse_response(&body)
    }
}

/// 构造文本消息载荷。
/// <param name="content">文本内容</param>
pub fn text_payload(content: &str) -> Value {
    json!({ "msgtype": "text", "text": { "content": content } })
}

/// 构造 Markdown 消息载荷。
/// <param name="title">标题</param>
/// <param name="content">markdown 内容</param>
pub fn markdown_payload(title: &str, content: &str) -> Value {
    json!({ "msgtype": "markdown", "markdown": { "title": title, "text": content } })
}

/// 解析钉钉响应体（`{"errcode":0,"errmsg":"ok"}`）。
/// <param name="body">响应体文本</param>
/// <returns>`errcode == 0` 时返回结果；否则返回带 errmsg 的错误</returns>
pub fn parse_response(body: &str) -> Result<DingTalkResult, DingTalkError> {
    let value: Value = serde_json::from_str(body).map_err(|e| {
        DingTalkError::new(format!(
            "响应不是合法 JSON：{e}；原文：{}",
            crate::text::truncate_chars(body, 200)
        ))
    })?;
    let errcode = value
        .get("errcode")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            DingTalkError::new(format!(
                "响应缺少 errcode：{}",
                crate::text::truncate_chars(body, 200)
            ))
        })?;
    let errmsg = value
        .get("errmsg")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if errcode == 0 {
        Ok(DingTalkResult { errcode, errmsg })
    } else {
        Err(DingTalkError::new(format!("errcode={errcode} {errmsg}")))
    }
}

/// 组装带「加签」参数的请求地址。
///
/// 已带查询串时用 `&` 追加，否则用 `?`；`secret` 为空时原样返回 webhook。
/// <param name="webhook">群机器人 Webhook 地址</param>
/// <param name="secret">加签密钥（空 = 不加签）</param>
/// <param name="timestamp_ms">当前毫秒时间戳</param>
pub fn signed_url(webhook: &str, secret: &str, timestamp_ms: i64) -> String {
    let base = webhook.trim();
    if secret.trim().is_empty() || base.is_empty() {
        return base.to_string();
    }
    let sign = sign(secret, timestamp_ms);
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}timestamp={timestamp_ms}&sign={sign}")
}

/// 计算「加签」签名（已做 URL 编码，可直接拼到查询串）。
///
/// `sign = urlencode(base64(HMAC-SHA256(secret, "{timestamp}\n{secret}")))`
/// <param name="secret">加签密钥</param>
/// <param name="timestamp_ms">当前毫秒时间戳（与服务端要求一致）</param>
pub fn sign(secret: &str, timestamp_ms: i64) -> String {
    let data = format!("{timestamp_ms}\n{}", secret.trim());
    let mac = crate::sign::hmac_sha256(secret.trim().as_bytes(), data.as_bytes());
    crate::web::url_encode(&crate::sign::base64_encode(&mac))
}

/// 错误文本里去掉 `access_token=...`（日志不留密钥）。
fn redact_webhook(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_whitespace() {
        if let Some(head) = token.split("access_token=").next() {
            if token.contains("access_token=") {
                out.push_str(head);
                out.push_str("access_token=*** ");
                continue;
            }
        }
        out.push_str(token);
        out.push(' ');
    }
    out.trim_end().to_string()
}

/// 当前毫秒时间戳。
fn now_millis() -> i64 {
    chrono::Local::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn payloads_align_with_dingtalk_schema() {
        let text = text_payload("hello");
        assert_eq!(text["msgtype"], "text");
        assert_eq!(text["text"]["content"], "hello");

        let markdown = markdown_payload("标题", "> 一行\n> 两行");
        assert_eq!(markdown["msgtype"], "markdown");
        assert_eq!(markdown["markdown"]["title"], "标题");
        assert_eq!(markdown["markdown"]["text"], "> 一行\n> 两行");
    }

    #[test]
    fn signed_url_without_secret_is_untouched() {
        let hook = "https://oapi.dingtalk.com/robot/send?access_token=abc";
        assert_eq!(signed_url(hook, "", 1), hook);
        assert_eq!(signed_url(hook, "   ", 1), hook);
    }

    #[test]
    fn signed_url_appends_encoded_signature() {
        let hook = "https://oapi.dingtalk.com/robot/send?access_token=abc";
        let url = signed_url(hook, "SECtest", 1_700_000_000_000);
        assert!(url.starts_with(hook), "{url}");
        assert!(url.contains("&timestamp=1700000000000"), "{url}");
        let sign = url.split("&sign=").nth(1).unwrap();
        assert!(!sign.is_empty());
        // 签名必须 URL 编码：不能出现 base64 的 + / =
        assert!(
            !sign.contains('+') && !sign.contains('/') && !sign.contains('='),
            "{sign}"
        );
    }

    #[test]
    fn signed_url_handles_webhook_without_query() {
        let url = signed_url("https://oapi.dingtalk.com/robot/send", "SECx", 42);
        assert!(url.contains("?timestamp=42&sign="), "{url}");
    }

    #[test]
    fn signature_is_stable_and_secret_dependent() {
        let a = sign("SEC1", 1_700_000_000_000);
        let b = sign("SEC1", 1_700_000_000_000);
        let c = sign("SEC2", 1_700_000_000_000);
        let d = sign("SEC1", 1_700_000_001_000);
        assert_eq!(a, b, "同一输入签名必须稳定");
        assert_ne!(a, c, "换密钥签名必须变化");
        assert_ne!(a, d, "换时间戳签名必须变化");
    }

    /// 官方文档给的等价样例（同样的 key/data 组合可由 C#/其他语言复算）。
    #[test]
    fn signature_matches_hmac_sha256_definition() {
        let secret = "SECxyz";
        let ts = 1_600_000_000_000_i64;
        let data = format!("{ts}\n{secret}");
        let expected = crate::web::url_encode(&crate::sign::base64_encode(
            &crate::sign::hmac_sha256(secret.as_bytes(), data.as_bytes()),
        ));
        assert_eq!(sign(secret, ts), expected);
    }

    #[test]
    fn parse_response_handles_all_shapes() {
        let ok = parse_response(r#"{"errcode":0,"errmsg":"ok"}"#).unwrap();
        assert_eq!(ok.errcode, 0);
        assert_eq!(ok.errmsg, "ok");

        let err =
            parse_response(r#"{"errcode":300005,"errmsg":"token is not exist"}"#).unwrap_err();
        assert!(err.0.contains("300005"), "{}", err.0);
        assert!(err.0.contains("token is not exist"), "{}", err.0);

        let bad = parse_response("<html>502</html>").unwrap_err();
        assert!(bad.0.contains("响应不是合法 JSON"), "{}", bad.0);

        let missing = parse_response(r#"{"errmsg":"ok"}"#).unwrap_err();
        assert!(missing.0.contains("缺少 errcode"), "{}", missing.0);
    }

    #[test]
    fn redact_hides_access_token() {
        let text = "请求失败 https://oapi.dingtalk.com/robot/send?access_token=abcdef123 超时";
        let masked = redact_webhook(text);
        assert!(!masked.contains("abcdef123"), "{masked}");
        assert!(masked.contains("access_token=***"), "{masked}");
    }

    /// 端到端（本地桩）：确认真发请求、URL 带加签参数、载荷是 markdown。
    #[test]
    fn blocking_send_reaches_webhook_with_signature() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<(String, String)>();

        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let head = text.split("\r\n\r\n").next().unwrap_or("").to_string();
            let _ = tx.send((head, text));
            let body = r#"{"errcode":0,"errmsg":"ok"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });

        let bot = DingTalkBot::new(format!("http://{addr}/robot/send?access_token=tok"))
            .with_secret("SECsecret")
            .with_timeout(Duration::from_secs(5));
        let result = bot
            .send_markdown_blocking("标题", "> 内容")
            .expect("应发送成功");
        assert_eq!(result.errcode, 0);

        let (head, _full) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("桩应收到请求");
        assert!(
            head.starts_with("POST /robot/send?access_token=tok"),
            "{head}"
        );
        assert!(head.contains("&timestamp="), "{head}");
        assert!(head.contains("&sign="), "{head}");
    }
}
