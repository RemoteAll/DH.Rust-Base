//! net::api_rpc —— NewLife ApiClient 二进制 RPC（UDP 通道，与 C# StarAgent 本地 RPC 字段级互通）。
//!
//! 协议（客户端与服务端同源实现，杜绝双端分叉）：
//! - DefaultMessage 头：`[Flag(高2位模式|低6位类型)=01][Seq][u16LE 长度（0xFFFF 时跟 u32LE 扩展）][信封]`；
//! - EncoderBase 信封：`[7bit actionLen][action]`（响应+错误模式再跟 `Int32LE 错误码`）`[Int32LE argsLen][argsJson]`；
//! - 响应 JSON 为 C# `ServiceOperationResult { Success, Message, ServiceName }`（NewLife 序列化省略默认值——
//!   `Success` 缺席即 false；失败响应形如 `{"Message":"服务不存在","ServiceName":"x"}`）。
//!
//! - **客户端**：`invoke` / `restart_service` / `stop_service` / `start_service`（同步 UDP；**等待期不重发**，
//!   对齐 C# `ApiClient.Invoke` 语义——服务端动作耗时较长，重复发包会导致动作被重复执行）；
//! - **服务端**：`serve_udp`（阻塞循环，分发闭包）/ `serve_udp_until`（可停止）；
//!   响应封装 `ApiReply` + `build_reply_datagram`；`local_only` 对齐 C# `CheckLocal`（仅本机）。
//!
//! 来源：DHDeploy.Agent.Rust `star_agent.rs` 收编（2026-10-01，字节级测试同步迁移）。

use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// 数据类型：二进制数据包（DefaultMessage.Flag 低 6 位默认值 `DataKinds.Packet=1`）。
pub const KIND_PACKET: u8 = 1;

/// StarAgent 本地 RPC 默认地址（对齐 C# 硬编码值）。
pub const STAR_AGENT_ADDR: &str = "127.0.0.1:5500";

/// 业务结果（调用侧响应 / 服务端应答；C# `ServiceOperationResult`）。
#[derive(Debug, Clone, Default)]
pub struct ApiReply {
    /// 业务是否成功。序列化时 `false` 省略（对齐 NewLife 省略默认值）。
    pub success: bool,
    /// 业务消息。
    pub message: String,
    /// 服务名（服务操作类动作回填）。
    pub service_name: String,
}

// ————— 编码 —————

/// 7bit 变长整数写入（对齐 `SpanWriter.WriteEncodedInt` / `BinaryWriter.Write7BitEncodedInt`）。
pub fn write_7bit_len(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut b = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            b |= 0x80;
        }
        out.push(b);
        if value == 0 {
            break;
        }
    }
}

/// 7bit 变长整数读取。
pub fn read_7bit_len(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut shift = 0u32;
    let mut acc = 0u32;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        acc |= ((b & 0x7F) as u32) << shift;
        if b & 0x80 == 0 {
            return Some(acc);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// 构建 API 信封（对齐 `EncoderBase.Encode(action, null, argsPacket)`；请求与响应同构）。
pub fn build_api_payload(action: &str, args_json: Option<&str>) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + action.len() + args_json.map(|s| s.len()).unwrap_or(0));
    write_7bit_len(&mut out, action.len() as u32);
    out.extend_from_slice(action.as_bytes());
    if let Some(args) = args_json {
        out.extend_from_slice(&(args.len() as u32).to_le_bytes());
        out.extend_from_slice(args.as_bytes());
    }
    out
}

/// 构建 DefaultMessage 请求报文（模式 00=请求；`sequence` 由调用方指定，对齐 StandardCodec 首包 =1）。
pub fn build_request_datagram(sequence: u8, payload: &[u8]) -> Vec<u8> {
    build_datagram(0, sequence, payload)
}

/// 构建业务应答报文（模式 10=响应；回填请求序号与动作名）。
pub fn build_reply_datagram(sequence: u8, action: &str, reply: &ApiReply) -> Vec<u8> {
    let json = reply_json(reply);
    let payload = build_api_payload(action, Some(&json));
    build_datagram(2, sequence, &payload)
}

/// 组装 DefaultMessage 报文（`mode`：0=请求 2=响应 3=响应+错误；缺省长度字段 0xFFFF 时跟 u32 扩展）。
fn build_datagram(mode: u8, sequence: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.push((mode << 6) | KIND_PACKET);
    out.push(sequence);
    if payload.len() >= 0xFFFF {
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// 业务 JSON（按 C# NewLife 序列化省略默认值语义）。
fn reply_json(reply: &ApiReply) -> String {
    let mut s = String::from("{");
    let mut first = true;

    let mut push_kv = |s: &mut String, first: &mut bool, key: &str, value: &str, raw: bool| {
        if !*first {
            s.push(',');
        }
        *first = false;
        s.push('"');
        s.push_str(key);
        s.push_str("\":");
        if raw {
            s.push_str(value);
        } else {
            s.push('"');
            s.push_str(&crate::web::json_escape(value));
            s.push('"');
        }
    };

    if reply.success {
        push_kv(&mut s, &mut first, "Success", "true", true);
    }
    if !reply.message.is_empty() {
        push_kv(&mut s, &mut first, "Message", &reply.message, false);
    }
    if !reply.service_name.is_empty() {
        push_kv(&mut s, &mut first, "ServiceName", &reply.service_name, false);
    }

    s.push('}');
    s
}

// ————— 解码 —————

/// 解析 DefaultMessage 报文头。返回 `(mode, error, seq, payload)`；mode：2=响应 3=响应+错误。
pub fn decode_message(bytes: &[u8]) -> Option<(u8, bool, u8, &[u8])> {
    if bytes.len() < 4 {
        return None;
    }
    let mode = bytes[0] >> 6;
    let error = mode == 3;
    let seq = bytes[1];
    let mut len = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    let mut off = 4usize;
    if len == 0xFFFF {
        if bytes.len() < 8 {
            return None;
        }
        len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        off = 8;
    }
    if bytes.len() < off + len {
        return None;
    }
    Some((mode, error, seq, &bytes[off..off + len]))
}

/// 解析 API 信封（对齐 `EncoderBase.Decode`）。
///
/// 返回 `(action, code, data JSON)`；`is_reply_error` 时 action 后解析 `Int32LE` 错误码。
pub fn decode_api_payload(
    payload: &[u8],
    is_reply_error: bool,
) -> Option<(String, Option<i32>, Option<String>)> {
    let mut pos = 0usize;
    let action_len = read_7bit_len(payload, &mut pos)? as usize;
    if payload.len() < pos + action_len {
        return None;
    }
    let action = String::from_utf8_lossy(&payload[pos..pos + action_len]).to_string();
    pos += action_len;

    let mut code = None;
    if is_reply_error {
        if payload.len() < pos + 4 {
            return None;
        }
        code = Some(i32::from_le_bytes([
            payload[pos],
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
        ]));
        pos += 4;
    }

    let mut data = None;
    if payload.len() >= pos + 4 {
        let len = u32::from_le_bytes([
            payload[pos],
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
        ]) as usize;
        pos += 4;
        if len > 0 && payload.len() >= pos + len {
            data = Some(String::from_utf8_lossy(&payload[pos..pos + len]).to_string());
        }
    }
    Some((action, code, data))
}

/// 解析响应报文为业务结果。
pub fn parse_reply(bytes: &[u8]) -> Option<ApiReply> {
    let (mode, error, _seq, payload) = decode_message(bytes)?;
    if mode != 2 && mode != 3 {
        return None;
    }
    let (action, code, data) = decode_api_payload(payload, error)?;

    // 初始按报文模式（响应=2→true；错误模式=3→false）；存在业务 JSON 时以其中 Success 字段为准
    let mut reply = ApiReply {
        success: !error,
        ..Default::default()
    };
    let mut message = String::new();
    if let Some(json) = data {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
            // 对齐 C# `ServiceOperationResult` 反序列化语义：`Success` 字段缺席即 false。
            // NewLife Json 序列化忽略默认值——C# Agent 失败响应实际形如
            // `{"Message":"服务不存在","ServiceName":"x"}`（无 Success 字段），
            // 若按报文模式默认 true，会把失败误判为成功。
            reply.success = v
                .get("Success")
                .or_else(|| v.get("success"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            message = v
                .get("Message")
                .or_else(|| v.get("message"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            reply.service_name = v
                .get("ServiceName")
                .or_else(|| v.get("serviceName"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
        } else {
            message = json;
        }
    }
    if message.is_empty() {
        if let Some(c) = code {
            message = format!("错误码 {c}");
        } else {
            message = format!("{action} 返回");
        }
    }
    reply.message = message;
    Some(reply)
}

// ————— 调用（客户端） —————

/// 通用调用（同步；调用方需在阻塞上下文执行）。
///
/// - `action`：动作名（`RestartService` / `StopService` / `StartService` / `Info` 等）；
/// - `args_json`：参数 JSON（如 `{"serviceName":"xxx"}`）；
/// - `timeout`：整体等待上限（发送一次后轮询读取，直至收到响应或超时）。
///
/// 对齐 C# `ApiClient.Invoke` 语义：**等待期间不重发**——`RestartService` 等服务端
/// 动作耗时较长（C# 侧统一 60s 超时），重复发包会导致同一动作被服务端重复执行（重复重启）。
pub fn invoke(
    addr: &str,
    action: &str,
    args_json: Option<&str>,
    timeout: Duration,
) -> Result<ApiReply, String> {
    let payload = build_api_payload(action, args_json);
    let datagram = build_request_datagram(1, &payload);

    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("UDP 绑定失败: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(800)))
        .map_err(|e| format!("设置超时失败: {e}"))?;
    sock.connect(addr)
        .map_err(|e| format!("连接 {addr} 失败: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();
    let mut sent = false;
    while Instant::now() < deadline {
        if !sent {
            match sock.send(&datagram) {
                Ok(_) => sent = true,
                Err(e) => {
                    // 发送失败（本地网络层错误）：短暂退避后重试发送
                    last_err = format!("发送失败: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        let mut buf = [0u8; 16384];
        match sock.recv(&mut buf) {
            Ok(n) => {
                if let Some(reply) = parse_reply(&buf[..n]) {
                    return Ok(reply);
                }
                // 非预期报文（其他会话/半包）：忽略并继续等待
                last_err = "收到非预期报文".to_string();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                last_err = "等待响应超时".to_string();
            }
            Err(e) => {
                last_err = format!("接收失败: {e}");
            }
        }
    }
    Err(format!(
        "StarAgent({addr}) 无响应（{last_err}）；Action={action}"
    ))
}

/// 服务操作调用参数（对齐 C# `new { serviceName }`）。
pub fn service_args(service_name: &str) -> String {
    format!(
        "{{\"serviceName\":\"{}\"}}",
        crate::web::json_escape(service_name)
    )
}

/// 重启服务（对齐 C# `LocalStarClient.Invoke("RestartService", new { serviceName })`）。
pub fn restart_service(
    addr: &str,
    service_name: &str,
    timeout: Duration,
) -> Result<ApiReply, String> {
    invoke(addr, "RestartService", Some(&service_args(service_name)), timeout)
}

/// 停止服务（对齐 C# `StopService`；调用方按 C# 语义可忽略失败）。
pub fn stop_service(addr: &str, service_name: &str, timeout: Duration) -> Result<ApiReply, String> {
    invoke(addr, "StopService", Some(&service_args(service_name)), timeout)
}

/// 启动服务（对齐 C# `StartService`；调用方按 C# 语义可忽略失败）。
pub fn start_service(addr: &str, service_name: &str, timeout: Duration) -> Result<ApiReply, String> {
    invoke(addr, "StartService", Some(&service_args(service_name)), timeout)
}

// ————— 服务端 —————

/// UDP RPC 服务端：阻塞循环（适用于专用线程）。
///
/// - `addr`：监听地址（如 `127.0.0.1:5500`）；
/// - `local_only`：仅接受本机来源（对齐 C# `CheckLocal`）；
/// - `handler`：`(action, args_json) -> ApiReply` 业务分发。
pub fn serve_udp<F>(addr: &str, local_only: bool, mut handler: F) -> std::io::Result<()>
where
    F: FnMut(&str, Option<&str>) -> ApiReply,
{
    let sock = UdpSocket::bind(addr)?;
    let mut buf = [0u8; 16384];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, peer)) => {
                let _ = handle_datagram(&sock, &buf[..n], peer, local_only, &mut handler);
            }
            Err(_) => continue,
        }
    }
}

/// UDP RPC 服务端：可停止版本（`shutdown` 置位后返回；轮询间隔 250ms）。
pub fn serve_udp_until<F>(
    addr: &str,
    local_only: bool,
    shutdown: &std::sync::atomic::AtomicBool,
    mut handler: F,
) -> std::io::Result<()>
where
    F: FnMut(&str, Option<&str>) -> ApiReply,
{
    use std::sync::atomic::Ordering;

    let sock = UdpSocket::bind(addr)?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut buf = [0u8; 16384];
    while !shutdown.load(Ordering::Relaxed) {
        match sock.recv_from(&mut buf) {
            Ok((n, peer)) => {
                let _ = handle_datagram(&sock, &buf[..n], peer, local_only, &mut handler);
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// 处理单个请求报文并回发应答。
fn handle_datagram<F>(
    sock: &UdpSocket,
    bytes: &[u8],
    peer: std::net::SocketAddr,
    local_only: bool,
    handler: &mut F,
) -> Option<()>
where
    F: FnMut(&str, Option<&str>) -> ApiReply,
{
    if local_only && !peer.ip().is_loopback() {
        return None;
    }

    let (mode, _error, seq, payload) = decode_message(bytes)?;
    if mode != 0 {
        return None; // 仅处理请求
    }

    let (action, _code, args) = decode_api_payload(payload, false)?;
    let reply = handler(&action, args.as_deref());
    let datagram = build_reply_datagram(seq, &action, &reply);
    let _ = sock.send_to(&datagram, peer);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_7bit_bytes(v: u32) -> Vec<u8> {
        let mut out = Vec::new();
        write_7bit_len(&mut out, v);
        out
    }

    #[test]
    fn seven_bit_len_roundtrip() {
        for v in [0u32, 1, 127, 128, 300, 16383, 16384, 0x0FFF_FFFF] {
            let bytes = write_7bit_bytes(v);
            let mut pos = 0usize;
            assert_eq!(read_7bit_len(&bytes, &mut pos), Some(v), "v={v}");
            assert_eq!(pos, bytes.len());
        }
    }

    #[test]
    fn api_payload_byte_exact() {
        // action="Info"（4B）+ 7bit 长度前缀；无参数
        let payload = build_api_payload("Info", None);
        assert_eq!(payload[0], 4);
        assert_eq!(&payload[1..5], b"Info");
        assert_eq!(payload.len(), 5);

        // 带参数：action + u32LE 长度 + JSON
        let args = r#"{"serviceName":"test"}"#;
        let payload = build_api_payload("RestartService", Some(args));
        assert_eq!(payload[0], 14);
        let len_off = 1 + 14;
        let len = u32::from_le_bytes([
            payload[len_off],
            payload[len_off + 1],
            payload[len_off + 2],
            payload[len_off + 3],
        ]) as usize;
        assert_eq!(len, args.len());
        assert_eq!(&payload[len_off + 4..], args.as_bytes());
    }

    #[test]
    fn request_datagram_header() {
        let payload = build_api_payload("Info", None);
        let datagram = build_request_datagram(1, &payload);
        assert_eq!(datagram[0], KIND_PACKET); // 模式 00 + Packet
        assert_eq!(datagram[1], 1); // seq
        let len = u16::from_le_bytes([datagram[2], datagram[3]]) as usize;
        assert_eq!(len, payload.len());
        assert_eq!(&datagram[4..], &payload[..]);
    }

    #[test]
    fn parse_success_reply() {
        // 模拟 StarAgent 成功响应：mode=2，JSON 含 Success=true
        let json = r#"{"Success":true,"Message":"服务重启成功","ServiceName":"test"}"#;
        let payload = build_api_payload("", Some(json));
        let datagram = build_datagram(2, 1, &payload);
        let reply = parse_reply(&datagram).unwrap();
        assert!(reply.success);
        assert_eq!(reply.message, "服务重启成功");
        assert_eq!(reply.service_name, "test");
    }

    #[test]
    fn parse_error_reply() {
        // mode=3（响应+错误）：action 后跟 Int32LE 错误码
        let json = r#"{"Message":"服务不存在"}"#;
        let action = "RestartService";
        let mut payload = Vec::new();
        write_7bit_len(&mut payload, action.len() as u32);
        payload.extend_from_slice(action.as_bytes());
        payload.extend_from_slice(&(-2i32).to_le_bytes());
        payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
        payload.extend_from_slice(json.as_bytes());

        let datagram = build_datagram(3, 1, &payload);
        let reply = parse_reply(&datagram).unwrap();
        assert!(!reply.success);
        assert_eq!(reply.message, "服务不存在");
    }

    #[test]
    fn parse_omitted_success_means_failure() {
        // C#/NewLife 失败响应无 Success 字段——必须判定为失败（曾误判为成功）
        let json = r#"{"Message":"服务不存在","ServiceName":"test"}"#;
        let payload = build_api_payload("", Some(json));
        let datagram = build_datagram(2, 1, &payload);
        let reply = parse_reply(&datagram).unwrap();
        assert!(!reply.success, "Success 缺席应判定失败");
        assert_eq!(reply.message, "服务不存在");
    }

    #[test]
    fn reply_json_omits_defaults_and_order() {
        let reply = ApiReply {
            success: true,
            message: "服务重启成功".to_string(),
            service_name: "test".to_string(),
        };
        assert_eq!(
            reply_json(&reply),
            r#"{"Success":true,"Message":"服务重启成功","ServiceName":"test"}"#
        );

        // 失败：Success 省略；仅 Message
        let reply = ApiReply {
            success: false,
            message: "服务不存在".to_string(),
            service_name: String::new(),
        };
        assert_eq!(reply_json(&reply), r#"{"Message":"服务不存在"}"#);

        // 消息含引号/反斜杠转义
        let reply = ApiReply {
            success: false,
            message: "a\"b\\c".to_string(),
            service_name: String::new(),
        };
        assert_eq!(reply_json(&reply), r#"{"Message":"a\"b\\c"}"#);
    }

    #[test]
    fn udp_roundtrip_with_server() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let port = {
            // 随机端口：先绑定再释放，降低冲突概率
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap().port()
        };
        let addr = format!("127.0.0.1:{port}");
        let shutdown = Arc::new(AtomicBool::new(false));

        let flag = shutdown.clone();
        let server = std::thread::spawn({
            let addr = addr.clone();
            move || {
                serve_udp_until(
                    &addr,
                    true,
                    &flag,
                    |action, args| match action {
                        "RestartService" => {
                            let name = args
                                .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                                .and_then(|v| v.get("serviceName").and_then(|x| x.as_str()).map(|s| s.to_string()))
                                .unwrap_or_default();
                            if name == "demo" {
                                ApiReply {
                                    success: true,
                                    message: "服务重启成功".to_string(),
                                    service_name: name,
                                }
                            } else {
                                ApiReply {
                                    success: false,
                                    message: "服务不存在".to_string(),
                                    service_name: name,
                                }
                            }
                        }
                        _ => ApiReply {
                            success: false,
                            message: format!("不支持的动作：{action}"),
                            service_name: String::new(),
                        },
                    },
                )
                .unwrap();
            }
        });

        // 给服务端一点启动时间
        std::thread::sleep(Duration::from_millis(150));

        let reply = restart_service(&addr, "demo", Duration::from_secs(3)).unwrap();
        assert!(reply.success);
        assert_eq!(reply.message, "服务重启成功");

        let reply = restart_service(&addr, "missing", Duration::from_secs(3)).unwrap();
        assert!(!reply.success);
        assert_eq!(reply.message, "服务不存在");

        // 请求报文回读：build_reply 的响应能被 decode 还原
        let reply = invoke(&addr, "Unknown", None, Duration::from_secs(3)).unwrap();
        assert!(!reply.success);
        assert!(reply.message.contains("不支持的动作"));

        shutdown.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(400));
        let _ = server.join();
    }
}
