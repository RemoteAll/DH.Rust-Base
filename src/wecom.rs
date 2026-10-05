//! 企业微信机器人（Webhook）客户端。
//!
//! 对齐 C# `Pek.WebHook` 的 `WeChatWorkRobot`：文本 / Markdown / Markdown V2 三种消息，
//! 推送至群机器人 Webhook 地址（`https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=...`）。
//!
//! - 依赖 `http-client` 特性；企业微信线上地址为 `https`，需 `http-tls` 特性（`net-tls` 已包含）
//! - 注意（官方文档）：`<font color="info|comment|warning">` 字体颜色仅 `markdown` 消息支持，
//!   `markdown_v2` 不支持颜色与 @群成员
//! - 注意（实测）：markdown 类消息在**微信端无法查看**（显示“不支持的内容”），需要全端可见时请用 [`WeComBot::send_text`]
//! - 响应按 `errcode` 判定：0 成功；非 0（如 93000 无效 key）返回 [`WeComError`] 并附带 errmsg
//! - 载荷构造与响应解析为纯函数（[`text_payload`] / [`markdown_payload`] / [`markdown_v2_payload`] /
//!   [`parse_response`]），便于组合复用与单元测试

use std::time::Duration;

use serde_json::{Value, json};

use crate::net::http_client::{self, HttpClientOptions};

/// 默认请求超时（毫秒）。
const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// 企业微信接口错误（消息文本；调用方直接展示或拼接）。
#[derive(Debug)]
pub struct WeComError(pub String);

impl WeComError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for WeComError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for WeComError {}

/// 机器人推送结果（`errcode == 0` 时返回）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeComResult {
    /// 接口返回码（0 = 成功）
    pub errcode: i64,
    /// 接口返回描述
    pub errmsg: String,
}

/// 企业微信机器人客户端（Webhook 推送）。
///
/// 轻量无状态：每次发送独立短连接；可在多处按需创建（克隆开销极小）。
#[derive(Debug, Clone)]
pub struct WeComBot {
    /// 群机器人 Webhook 地址
    webhook: String,
    /// 请求超时（连接/发送/读体全流程）
    timeout: Duration,
    /// 忽略服务器证书校验（内网代理/自签场景）
    insecure_tls: bool,
}

impl WeComBot {
    /// 创建机器人客户端。
    /// <param name="webhook">群机器人 Webhook 地址（空地址在发送时报错）</param>
    pub fn new(webhook: impl Into<String>) -> Self {
        Self {
            webhook: webhook.into(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            insecure_tls: false,
        }
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

    /// 发送文本消息。
    /// <param name="content">文本内容</param>
    /// <param name="mentioned_list">提醒的 userid 列表（`"@all"` 表示提醒所有人；空数组不提醒）</param>
    /// <param name="mentioned_mobile_list">提醒的手机号列表（`"@all"` 同上）</param>
    pub async fn send_text(
        &self,
        content: &str,
        mentioned_list: &[&str],
        mentioned_mobile_list: &[&str],
    ) -> Result<WeComResult, WeComError> {
        self.send_value(text_payload(content, mentioned_list, mentioned_mobile_list))
            .await
    }

    /// 发送 Markdown 消息。
    /// <param name="content">markdown 内容</param>
    pub async fn send_markdown(&self, content: &str) -> Result<WeComResult, WeComError> {
        self.send_value(markdown_payload(content)).await
    }

    /// 发送 Markdown V2 消息（支持表格、列表等更丰富语法）。
    /// <param name="content">markdown_v2 内容（最长 4096 字节，由调用方保证）</param>
    pub async fn send_markdown_v2(&self, content: &str) -> Result<WeComResult, WeComError> {
        self.send_value(markdown_v2_payload(content)).await
    }

    /// 发送 Markdown V2 消息（同步版；内部创建临时 tokio 运行时）。
    ///
    /// 供阻塞上下文低频调用（告警/日报任务线程等）；异步上下文请直接用
    /// [`send_markdown_v2`]。
    pub fn send_markdown_v2_blocking(&self, content: &str) -> Result<WeComResult, WeComError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| WeComError::new(format!("创建 tokio 运行时失败: {e}")))?;
        rt.block_on(self.send_markdown_v2(content))
    }

    /// 发送已构造好的消息载荷（`msgtype` 已在载荷中）。
    /// <param name="payload">消息 JSON 载荷</param>
    async fn send_value(&self, payload: Value) -> Result<WeComResult, WeComError> {
        let webhook = self.webhook.trim();
        if webhook.is_empty() {
            return Err(WeComError::new("企业微信 webhook 地址为空"));
        }
        let options = HttpClientOptions {
            timeout: self.timeout,
            insecure_tls: self.insecure_tls,
            ca_pem: None,
        };
        let resp = http_client::request(
            "POST",
            webhook,
            &[],
            Some("application/json"),
            payload.to_string().into_bytes(),
            &options,
        )
        .await
        .map_err(|e| WeComError::new(format!("请求失败：{e}")))?;
        let body = resp.body_text();
        if !resp.is_success() {
            return Err(WeComError::new(format!("HTTP {}：{body}", resp.status)));
        }
        parse_response(&body)
    }
}

/// 构造文本消息载荷。
/// <param name="content">文本内容</param>
/// <param name="mentioned_list">userid 列表（空则不输出该字段）</param>
/// <param name="mentioned_mobile_list">手机号列表（空则不输出该字段）</param>
pub fn text_payload(
    content: &str,
    mentioned_list: &[&str],
    mentioned_mobile_list: &[&str],
) -> Value {
    let mut text = json!({ "content": content });
    if !mentioned_list.is_empty() {
        text["mentioned_list"] = json!(mentioned_list);
    }
    if !mentioned_mobile_list.is_empty() {
        text["mentioned_mobile_list"] = json!(mentioned_mobile_list);
    }
    json!({ "msgtype": "text", "text": text })
}

/// 构造 Markdown 消息载荷。
/// <param name="content">markdown 内容</param>
pub fn markdown_payload(content: &str) -> Value {
    json!({ "msgtype": "markdown", "markdown": { "content": content } })
}

/// 构造 Markdown V2 消息载荷。
/// <param name="content">markdown_v2 内容</param>
pub fn markdown_v2_payload(content: &str) -> Value {
    json!({ "msgtype": "markdown_v2", "markdown_v2": { "content": content } })
}

/// 解析企业微信响应体（`{"errcode":0,"errmsg":"ok"}`）。
/// <param name="body">响应体文本</param>
/// <returns>`errcode == 0` 时返回结果；否则返回带 errmsg 的错误</returns>
pub fn parse_response(body: &str) -> Result<WeComResult, WeComError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| WeComError::new(format!("响应不是合法 JSON：{e}；原文：{body}")))?;
    let errcode = value
        .get("errcode")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| WeComError::new(format!("响应缺少 errcode：{body}")))?;
    let errmsg = value
        .get("errmsg")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if errcode == 0 {
        Ok(WeComResult { errcode, errmsg })
    } else {
        Err(WeComError::new(format!(
            "接口返回错误（errcode={errcode}）：{errmsg}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn payloads_align_with_wechat_work_schema() {
        let text = text_payload("hello", &["@all"], &[]);
        assert_eq!(text["msgtype"], "text");
        assert_eq!(text["text"]["content"], "hello");
        assert_eq!(text["text"]["mentioned_list"][0], "@all");
        assert!(text["text"].get("mentioned_mobile_list").is_none(), "空列表不输出");

        let markdown = markdown_payload("**加粗**");
        assert_eq!(markdown["msgtype"], "markdown");
        assert_eq!(markdown["markdown"]["content"], "**加粗**");

        let v2 = markdown_v2_payload("# 标题\n> 引用");
        assert_eq!(v2["msgtype"], "markdown_v2");
        assert_eq!(v2["markdown_v2"]["content"], "# 标题\n> 引用");
    }

    #[test]
    fn response_parsing_covers_ok_and_errors() {
        let ok = parse_response(r#"{"errcode":0,"errmsg":"ok"}"#).unwrap();
        assert_eq!(ok.errcode, 0);

        let err = parse_response(r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#).unwrap_err();
        assert!(err.to_string().contains("93000"), "{err}");
        assert!(err.to_string().contains("invalid webhook url"), "{err}");

        let err = parse_response("not-json").unwrap_err();
        assert!(err.to_string().contains("不是合法 JSON"), "{err}");

        let err = parse_response(r#"{"errmsg":"missing"}"#).unwrap_err();
        assert!(err.to_string().contains("缺少 errcode"), "{err}");
    }

    #[tokio::test]
    async fn empty_webhook_is_rejected_locally() {
        let bot = WeComBot::new("  ");
        let err = bot.send_text("hi", &[], &[]).await.unwrap_err();
        assert!(err.to_string().contains("为空"), "{err}");
    }

    /// 启动单连接 HTTP 桩：返回（端口, 收到的请求体）；按给定响应体回 200。
    async fn stub_once(response_body: &'static str) -> (u16, Arc<Mutex<String>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(String::new()));
        let got = received.clone();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            // 读请求头（至空行）
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            let header_end = loop {
                let Ok(n) = sock.read(&mut tmp).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            // 按 Content-Length 读请求体
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let len: usize = headers
                .split("\r\n")
                .find_map(|l| {
                    let mut it = l.splitn(2, ':');
                    let name = it.next()?.trim().to_ascii_lowercase();
                    let value = it.next()?.trim();
                    (name == "content-length").then(|| value.parse::<usize>().ok()).flatten()
                })
                .unwrap_or(0);
            while buf.len() < header_end + len {
                let Ok(n) = sock.read(&mut tmp).await else {
                    break;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let end = (header_end + len).min(buf.len());
            *got.lock().unwrap() = String::from_utf8_lossy(&buf[header_end..end]).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        (port, received)
    }

    #[tokio::test]
    async fn send_markdown_v2_against_local_stub() {
        let (port, received) = stub_once(r#"{"errcode":0,"errmsg":"ok"}"#).await;
        let bot = WeComBot::new(format!("http://127.0.0.1:{port}/send"));
        let result = bot.send_markdown_v2("# 测试标题\n条码 SD26145916").await.unwrap();
        assert_eq!(result.errcode, 0);

        let body = received.lock().unwrap().clone();
        assert!(body.contains("\"msgtype\":\"markdown_v2\""), "{body}");
        assert!(body.contains("# 测试标题"), "{body}");
        assert!(body.contains("SD26145916"), "{body}");
    }

    #[tokio::test]
    async fn business_error_is_surfaced() {
        let (port, _received) = stub_once(r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#).await;
        let bot = WeComBot::new(format!("http://127.0.0.1:{port}/send"));
        let err = bot.send_markdown("hi").await.unwrap_err();
        assert!(err.to_string().contains("93000"), "{err}");
    }
}
