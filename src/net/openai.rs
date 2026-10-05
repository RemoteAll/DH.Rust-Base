//! OpenAI 兼容对话客户端（`/chat/completions`）——DeepSeek / OpenAI / Moonshot / 通义兼容模式 /
//! 本地 Ollama 等共用同一入口。
//!
//! 依赖 `http-client` 特性（https 需 `http-tls`，`net-tls` 已包含）。来源：Pek.RAgent
//! 「AI 助手」下沉（2026-10-05）。
//!
//! - [`chat_blocking`]：同步调用（**内部独立线程 + join**——可直接在 tokio 运行时线程
//!   （如 HTTP 服务端处理器）内调用而不触发“运行时内 block_on”panic；这是渲染服务端
//!   调用外部 AI 的推荐入口）；
//! - [`chat`]：异步调用（异步上下文直接使用）；
//! - [`chat_completions_url`] / [`parse_reply`] / [`parse_error_message`]：纯函数（可单测/复用）。
//!
//! 请求体固定 `stream=false`；答复解析兼容 `reasoning_content`（推理模型：仅返回思维链
//! 而无正文时返回明确错误，提示重试）。

use std::time::Duration;

use serde_json::{json, Value as Json};

use super::http_client::{self, HttpClientOptions, HttpResponse};

/// 默认超时（推理模型可能较慢）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// 调用参数。
pub struct OpenAiOptions {
    /// OpenAI 兼容 Base 地址（如 `https://api.deepseek.com/v1`；也可直接给完整
    /// `.../chat/completions` 地址——见 [`chat_completions_url`]）。
    pub base_url: String,
    /// API Key（Bearer 令牌）。
    pub api_key: String,
    /// 模型名（如 `deepseek-chat` / `deepseek-reasoner` / `gpt-4o`）。
    pub model: String,
    /// 采样温度（默认 0.3）。
    pub temperature: f32,
    /// 整体超时（连接/发送/读体；默认 [`DEFAULT_TIMEOUT`]）。
    pub timeout: Duration,
}

impl OpenAiOptions {
    /// 便捷构造（默认温度 0.3、超时 300s）。
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            temperature: 0.3,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// 模型答复。
#[derive(Debug, Clone)]
pub struct ChatReply {
    /// 正文
    pub content: String,
    /// 思维链（`reasoning_content`；无则空串）
    pub reasoning: String,
    /// token 用量（原样透传；可能为 Null）
    pub usage: Json,
    /// 服务端返回的实际模型名
    pub model: String,
}

/// Base 地址 → `chat/completions` 完整地址：已是完整地址则原样（去尾部 `/`）。
pub fn chat_completions_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("chat/completions") {
        b.to_string()
    } else {
        format!("{b}/chat/completions")
    }
}

/// 同步调用（内部独立线程 + join；可在任意上下文安全调用）。
pub fn chat_blocking(opts: &OpenAiOptions, messages: &[Json]) -> Result<ChatReply, String> {
    let url = chat_completions_url(&opts.base_url);
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("AI 接口地址无效（需 http/https）：{url}"));
    }
    let payload = json!({
        "model": opts.model,
        "messages": messages,
        "stream": false,
        "temperature": opts.temperature,
    });
    let body = serde_json::to_vec(&payload).map_err(|e| format!("请求序列化失败：{e}"))?;
    let auth = format!("Bearer {}", opts.api_key.trim());
    let timeout = opts.timeout;

    // 运行时内安全（库内独立线程 + join，见 `blocking_request_offthread`）
    let resp = http_client::blocking_request_offthread(
        "POST",
        &url,
        &[("Authorization", auth.as_str())],
        Some("application/json"),
        body,
        timeout,
    )
    .map_err(|e| format!("调用 AI 接口失败：{}", e.0))?;
    finish(resp)
}

/// 异步调用（异步上下文直接使用）。
pub async fn chat(opts: &OpenAiOptions, messages: &[Json]) -> Result<ChatReply, String> {
    let url = chat_completions_url(&opts.base_url);
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("AI 接口地址无效（需 http/https）：{url}"));
    }
    let payload = json!({
        "model": opts.model,
        "messages": messages,
        "stream": false,
        "temperature": opts.temperature,
    });
    let body = serde_json::to_vec(&payload).map_err(|e| format!("请求序列化失败：{e}"))?;
    let auth = format!("Bearer {}", opts.api_key.trim());
    let options = HttpClientOptions {
        timeout: opts.timeout,
        ..Default::default()
    };
    let resp = http_client::request(
        "POST",
        &url,
        &[("Authorization", auth.as_str())],
        Some("application/json"),
        body,
        &options,
    )
    .await
    .map_err(|e| format!("调用 AI 接口失败：{}", e.0))?;
    finish(resp)
}

/// 统一收尾：非 2xx → 错误消息（优先 `{"error":{"message":…}}`）；2xx → 解析答复。
fn finish(resp: HttpResponse) -> Result<ChatReply, String> {
    let text = resp.body_text();
    if !resp.is_success() {
        let msg = parse_error_message(&text).unwrap_or_else(|| truncate(text.clone(), 300));
        return Err(format!("AI 接口返回 HTTP {}：{msg}", resp.status));
    }
    parse_reply(&text)
}

/// 解析模型响应（纯函数）。
pub fn parse_reply(body: &str) -> Result<ChatReply, String> {
    let v: Json = serde_json::from_str(body).map_err(|e| format!("AI 响应不是有效 JSON：{e}"))?;
    let msg = match v.pointer("/choices/0/message") {
        Some(m) => m,
        None => {
            let detail =
                parse_error_message(body).unwrap_or_else(|| truncate(body.to_string(), 300));
            return Err(format!("AI 响应缺少 choices：{detail}"));
        }
    };
    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let reasoning = msg
        .get("reasoning_content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() {
        if !reasoning.is_empty() {
            return Err("模型仅返回了推理过程、未给出最终答复，请重试".to_string());
        }
        return Err("AI 返回了空答复".to_string());
    }
    Ok(ChatReply {
        content,
        reasoning,
        usage: v.get("usage").cloned().unwrap_or(Json::Null),
        model: v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 从错误响应体提取消息（`{"error":{"message":…}}` 或 `{"message":…}`）。
pub fn parse_error_message(body: &str) -> Option<String> {
    let v: Json = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")
        .and_then(|m| m.as_str())
        .or_else(|| v.get("message").and_then(|m| m.as_str()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 按字符截断（不破坏 UTF-8 边界）。
fn truncate(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_url_join() {
        assert_eq!(
            chat_completions_url("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:11434/v1/chat/completions"),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn parse_reply_normal_and_reasoning() {
        let body = r#"{"model":"deepseek-chat","choices":[{"message":{"role":"assistant","content":"磁盘 C 盘剩余不足 10%。"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let reply = parse_reply(body).unwrap();
        assert!(reply.content.contains("磁盘"));
        assert_eq!(reply.model, "deepseek-chat");
        assert_eq!(reply.usage["completion_tokens"], 5);

        // 推理模型：content 为空但有 reasoning_content → 明确报错
        let body = r#"{"choices":[{"message":{"content":"","reasoning_content":"让我想想"}}]}"#;
        let err = parse_reply(body).unwrap_err();
        assert!(err.contains("推理过程"), "{err}");

        // 错误结构体
        let err = parse_reply(r#"{"error":{"message":"Insufficient Balance"}}"#).unwrap_err();
        assert!(err.contains("Insufficient Balance"), "{err}");

        // 非 JSON
        assert!(parse_reply("<html>oops</html>").is_err());
    }

    #[test]
    fn parse_error_message_extracts() {
        assert_eq!(
            parse_error_message(r#"{"error":{"message":" Invalid API key "}}"#).unwrap(),
            "Invalid API key"
        );
        assert_eq!(
            parse_error_message(r#"{"message":"missing"}"#).unwrap(),
            "missing"
        );
        assert!(parse_error_message("plain").is_none());
    }
}
