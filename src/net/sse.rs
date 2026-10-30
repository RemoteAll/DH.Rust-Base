//! Server-Sent Events（SSE）读写基础件：**按行切分上游分片 + 构造下游帧**。
//!
//! 纯内存、无 IO、零网络依赖（无条件可用）。收编自 Pek.DeepSeekProxy 的协议翻译层
//! （OpenAI Chat / Anthropic Messages / OpenAI Responses 三套流式共用同一份缓冲与帧构造），
//! 供其它项目做 SSE 时直接复用，避免每个项目各写一份「半包处理」。
//!
//! 约定：
//! - **读**：上游分片常把一行拆成多块（TCP 边界与行边界无关），[`SseBuffer`] 按 `\n` 切分并保留残余；
//!   流结束时用 [`SseBuffer::take_rest`] 交出末尾没有换行的一行（不丢最后一条）。
//! - **写**：帧以**空行**结束（`\n\n`）；`data:` 后带一个空格（`data: {...}`，浏览器与官方 SDK 都接受）。
//! - [`DONE`] 是 OpenAI 风格的结束标记；Anthropic 用 `event: message_stop` 收尾，不需要它。
//!
//! ```no_run
//! use dhrust::net::sse::{SseBuffer, event_json_frame, json_of};
//!
//! // 读：两次分片拼出完整行
//! let mut buffer = SseBuffer::default();
//! assert!(buffer.push(b"data: {\"a\"").is_empty());
//! let lines = buffer.push(b":1}\n\n");
//! let value = json_of(&lines[0]).unwrap();
//! assert_eq!(value["a"], 1);
//!
//! // 写：带事件名的帧
//! let frame = event_json_frame("message_start", &serde_json::json!({"type":"message_start"}));
//! assert!(frame.starts_with(b"event: message_start\ndata: "));
//! ```

use serde_json::Value;

/// SSE 响应的 `Content-Type`（不含 charset；需要时自行追加 `; charset=utf-8`）。
pub const CONTENT_TYPE: &str = "text/event-stream";

/// OpenAI 风格的流结束标记（`data: [DONE]`）。
pub const DONE: &str = "[DONE]";

/// SSE 行缓冲：上游分片可能把一行拆成多块，这里按 `\n` 切分并保留残余。
///
/// 只做**字节级**切分（不做 UTF-8 校验、不解析字段），因此多字节字符被拆到两个分片里也不会乱码
/// ——切出来的行是完整行，天然落在字符边界上。
#[derive(Default)]
pub struct SseBuffer {
    rest: Vec<u8>,
}

impl SseBuffer {
    /// 新建空缓冲。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加分片，返回其中**完整的行**（去掉行尾 `\r`/`\n`）。
    ///
    /// # 参数
    /// - `chunk`：本次读到的字节（可以是半行、多行、或空）
    ///
    /// # 返回值
    /// 组成完整的行（按出现顺序）；没有完整行时返回空 `Vec`。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.rest.extend_from_slice(chunk);
        let mut lines = Vec::new();

        while let Some(pos) = self.rest.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.rest.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            lines.push(text.trim_end_matches(['\r', '\n']).to_string());
        }

        lines
    }

    /// 流结束时取出残余（没有换行结尾的最后一行）；空缓冲返回 `None`。
    pub fn take_rest(&mut self) -> Option<String> {
        if self.rest.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&self.rest).to_string();
        self.rest.clear();
        Some(text.trim_end_matches(['\r', '\n']).to_string())
    }

    /// 残余字节数（排查半包卡住时看这个）。
    pub fn pending_len(&self) -> usize {
        self.rest.len()
    }

    /// 是否没有任何残余。
    pub fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    /// 清空残余（重连/换流时用）。
    pub fn clear(&mut self) {
        self.rest.clear();
    }
}

/// 取 `data:` 后的负载（`[DONE]`、空负载、非 `data:` 行都返回 `None`）。
///
/// 零分配：返回的是原行里的切片。
pub fn data_of(line: &str) -> Option<&str> {
    let payload = line.trim().strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == DONE {
        return None;
    }
    Some(payload)
}

/// 取 `data:` 的 JSON（解析失败 / `[DONE]` / 非 `data:` 行 → `None`）。
pub fn json_of(line: &str) -> Option<Value> {
    serde_json::from_str(data_of(line)?).ok()
}

/// 构造 `data: {data}\n\n`（`data` 原样写入，不转义、不加引号）。
pub fn frame(data: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    push_frame(&mut out, data);
    out
}

/// 构造 `event: {event}\ndata: {data}\n\n`。
pub fn event_frame(event: &str, data: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(event.len() + data.len() + 16);
    push_event_frame(&mut out, event, data);
    out
}

/// 构造 `data: {json}\n\n`（JSON 只序列化一次）。
pub fn json_frame(data: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    push_json(&mut out, data);
    out
}

/// 构造 `event: {event}\ndata: {json}\n\n`（JSON 只序列化一次）。
pub fn event_json_frame(event: &str, data: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(event.len() + 64);
    push_event_json(&mut out, event, data);
    out
}

/// 构造结束帧 `data: [DONE]\n\n`。
pub fn done_frame() -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    push_done(&mut out);
    out
}

/// 追加 `data: {data}\n\n` 到已有缓冲（流式写时复用同一个 `Vec`，避免每帧一次分配）。
pub fn push_frame(out: &mut Vec<u8>, data: &str) {
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(data.as_bytes());
    out.extend_from_slice(b"\n\n");
}

/// 追加 `event: {event}\ndata: {data}\n\n` 到已有缓冲。
pub fn push_event_frame(out: &mut Vec<u8>, event: &str, data: &str) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(data.as_bytes());
    out.extend_from_slice(b"\n\n");
}

/// 追加 `data: {json}\n\n` 到已有缓冲。
pub fn push_json(out: &mut Vec<u8>, data: &Value) {
    push_frame(out, &data.to_string());
}

/// 追加 `event: {event}\ndata: {json}\n\n` 到已有缓冲。
pub fn push_event_json(out: &mut Vec<u8>, event: &str, data: &Value) {
    push_event_frame(out, event, &data.to_string());
}

/// 追加结束帧 `data: [DONE]\n\n` 到已有缓冲。
pub fn push_done(out: &mut Vec<u8>) {
    push_frame(out, DONE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_partial_lines_and_keeps_rest() {
        let mut buffer = SseBuffer::default();
        assert!(buffer.push(b"data: {\"a\":1}").is_empty());
        assert_eq!(buffer.pending_len(), 13);

        // 两段分片拼出一行、一个空行、再一整行
        let lines = buffer.push(b"\n\ndata: {\"b\":2}\n");
        assert_eq!(
            lines,
            vec![
                "data: {\"a\":1}".to_string(),
                String::new(),
                "data: {\"b\":2}".to_string()
            ]
        );
        assert!(buffer.is_empty());

        // 没有换行结尾的尾巴由 take_rest 交出来（流结束时补处理）
        assert!(buffer.take_rest().is_none());
        let _ = buffer.push(b"data: tail");
        assert_eq!(buffer.take_rest().as_deref(), Some("data: tail"));
    }

    #[test]
    fn handles_crlf_and_multibyte_split() {
        let mut buffer = SseBuffer::default();
        let lines = buffer.push("data: 你好\r\n\r\n".as_bytes());
        assert_eq!(lines, vec!["data: 你好".to_string(), String::new()]);

        // 多字节字符被拆到两个分片：切出来的行仍是完整 UTF-8
        let mut split = SseBuffer::default();
        let text = "data: 中文\n";
        let bytes = text.as_bytes();
        let cut = "data: 中".len();
        assert!(split.push(&bytes[..cut]).is_empty());
        let lines = split.push(&bytes[cut..]);
        assert_eq!(lines, vec!["data: 中文".to_string()]);
    }

    #[test]
    fn data_of_ignores_done_and_non_data_lines() {
        assert_eq!(data_of("data: {\"x\":1}"), Some("{\"x\":1}"));
        assert_eq!(data_of("data:{\"x\":1}"), Some("{\"x\":1}"));
        assert!(data_of("data: [DONE]").is_none());
        assert!(data_of("data: ").is_none());
        assert!(data_of("event: ping").is_none());
        assert!(data_of("").is_none());
    }

    #[test]
    fn json_of_parses_only_valid_payload() {
        assert!(json_of("data: [DONE]").is_none());
        assert!(json_of("event: ping").is_none());
        assert!(json_of("data: not-json").is_none());
        assert_eq!(json_of("data: {\"x\":1}").unwrap()["x"], 1);
    }

    #[test]
    fn frames_use_blank_line_terminator() {
        assert_eq!(frame("{}"), b"data: {}\n\n".to_vec());
        assert_eq!(done_frame(), b"data: [DONE]\n\n".to_vec());
        assert_eq!(
            event_frame("ping", "{}"),
            b"event: ping\ndata: {}\n\n".to_vec()
        );

        let json = serde_json::json!({"type":"message_start"});
        assert_eq!(
            json_frame(&json),
            format!("data: {json}\n\n").into_bytes()
        );
        assert_eq!(
            event_json_frame("message_start", &json),
            format!("event: message_start\ndata: {json}\n\n").into_bytes()
        );
    }

    #[test]
    fn push_variants_append_to_same_buffer() {
        let mut out = Vec::new();
        push_event_json(&mut out, "a", &serde_json::json!({"n":1}));
        push_json(&mut out, &serde_json::json!({"n":2}));
        push_done(&mut out);

        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "event: a\ndata: {\"n\":1}\n\ndata: {\"n\":2}\n\ndata: [DONE]\n\n"
        );
    }

    #[test]
    fn clear_drops_pending_bytes() {
        let mut buffer = SseBuffer::default();
        let _ = buffer.push(b"data: half");
        assert_eq!(buffer.pending_len(), 10);
        buffer.clear();
        assert!(buffer.is_empty());
    }
}
