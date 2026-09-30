//! MQTT 客户端（协议 3.1 / 3.1.1 / 5.0）：连接认证 / QoS0·1 发布 / 保活心跳 / 断线自动重连。
//!
//! 面向“设备数据上行”的最小客户端（互通目标：NewLife.MQTT / DH.NMQTT 服务端——
//! 其 V310/V311/V500 三档均已支持；默认 3.1.1）。
//! 特性门控 `feature = "mqtt"`（仅依赖 tokio，不含 TLS——对齐组织“明文局域网”现状）。
//!
//! 行为约定：
//! - [`MqttClient::spawn`] 立即返回句柄，后台任务负责建连、握手、保活与重连；
//! - [`MqttClient::publish`] 在未连接时**快速失败**（Err），已连接时：
//!   QoS0 写出即返回；QoS1 等待服务端 `PUBACK`（超时由调用方限定）——
//!   调用方可据此把发布纳入“失败重试”的上层保障（如发件箱补发）；
//! - 连接状态变化通过 [`MqttClient::status`] / [`MqttClient::status_generation`] 暴露，
//!   供调用方打日志（变化时 generation 自增）。

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{mpsc, oneshot},
};

/// MQTT 协议版本（与服务端 NewLife.MQTT 的三档对齐；默认 3.1.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MqttVersion {
    /// MQTT 3.1（协议名 `MQIsdp`，协议级别 3；客户端标识惯例限 23 字节）
    V310,
    /// MQTT 3.1.1（默认，最广泛兼容）
    #[default]
    V311,
    /// MQTT 5.0（报文含属性字段；本客户端发送空属性）
    V500,
}

impl MqttVersion {
    /// 协议名（3.1 为 `MQIsdp`，其余为 `MQTT`）。
    /// <returns>协议名</returns>
    pub fn protocol_name(self) -> &'static str {
        match self {
            Self::V310 => "MQIsdp",
            _ => "MQTT",
        }
    }

    /// 协议级别数字（3 / 4 / 5）。
    /// <returns>协议级别</returns>
    pub fn level(self) -> u8 {
        match self {
            Self::V310 => 3,
            Self::V311 => 4,
            Self::V500 => 5,
        }
    }

    /// 是否 5.0 及以上（CONNECT/PUBLISH 含属性字段）。
    /// <returns>是否带属性</returns>
    pub fn has_properties(self) -> bool {
        matches!(self, Self::V500)
    }

    /// 解析版本文本（支持 `3.1` / `3.1.1` / `5.0`，宽容常见写法）。
    /// <param name="text">版本文本</param>
    /// <returns>版本；无法识别时为 None</returns>
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "3.1" | "310" | "v310" => Some(Self::V310),
            "3.1.1" | "311" | "v311" | "4" => Some(Self::V311),
            "5" | "5.0" | "500" | "v500" => Some(Self::V500),
            _ => None,
        }
    }

    /// 版本名称（`3.1` / `3.1.1` / `5.0`）。
    /// <returns>名称</returns>
    pub fn name(self) -> &'static str {
        match self {
            Self::V310 => "3.1",
            Self::V311 => "3.1.1",
            Self::V500 => "5.0",
        }
    }
}

/// 连接参数。
#[derive(Debug, Clone)]
pub struct MqttOptions {
    /// 服务端地址（IP 或域名）
    pub host: String,
    /// 服务端端口（MQTT 默认 1883）
    pub port: u16,
    /// 协议版本（默认 3.1.1）
    pub version: MqttVersion,
    /// 客户端标识（空时按进程自动生成）
    pub client_id: String,
    /// 用户名（空串表示不带用户名）
    pub username: String,
    /// 密码（空串表示不带密码）
    pub password: String,
    /// 保活时长（PINGREQ 发送周期；`Duration::ZERO` 表示不启用）
    pub keepalive: Duration,
    /// 断线重连间隔（下限 50ms，避免忙等）
    pub reconnect: Duration,
    /// 建连与握手超时
    pub connect_timeout: Duration,
}

impl Default for MqttOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1883,
            version: MqttVersion::default(),
            client_id: String::new(),
            username: String::new(),
            password: String::new(),
            keepalive: Duration::from_secs(60),
            reconnect: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// 共享状态（连接标志 / 状态文本 / 变化代数）。
struct Shared {
    connected: AtomicBool,
    generation: AtomicU64,
    status: Mutex<String>,
}

impl Shared {
    /// 更新状态并推进代数。
    fn set(&self, connected: bool, text: String) {
        self.connected.store(connected, Ordering::Release);
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = text;
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// 后台命令。
enum Cmd {
    /// 发布消息（`ack` 回传最终结果）
    Publish {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        ack: oneshot::Sender<Result<(), String>>,
    },
}

/// MQTT 客户端句柄。
///
/// 句柄可自由克隆/跨任务共享（内部仅通道发送端与共享状态）；全部句柄释放后后台任务退出。
#[derive(Clone)]
pub struct MqttClient {
    tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl MqttClient {
    /// 创建客户端并启动后台连接任务（立即返回，连接结果见 [`Self::is_connected`]）。
    pub fn spawn(opts: MqttOptions) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            connected: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new("尚未连接".to_string()),
        });
        tokio::spawn(supervisor(opts, rx, shared.clone()));
        Self { tx, shared }
    }

    /// 当前是否已连接（已完成 MQTT 握手）。
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// 状态文本（如 `192.168.2.9:1883 已连接` / `... 连接失败（...）`）。
    pub fn status(&self) -> String {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 状态变化代数（连接成功/断开/重连均自增；调用方对比前后值即可只在变化时打日志）。
    pub fn status_generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// 发布一条消息。
    ///
    /// - 未连接：立即返回 `Err`（调用方可稍后重试/由上层补发）
    /// - QoS0：写出成功即 `Ok`
    /// - QoS1：等待服务端 `PUBACK`（受 `timeout` 限定；超时返回 `Err`，消息可能已送出，
    ///   上层重试需容忍重复——MQTT QoS1 本身即“至少一次”语义）
    pub async fn publish(
        &self,
        topic: &str,
        payload: &[u8],
        qos: u8,
        timeout: Duration,
    ) -> Result<(), String> {
        if qos > 1 {
            return Err(format!("仅支持 QoS 0/1（收到 {qos}）"));
        }
        if topic.is_empty() {
            return Err("主题不能为空".to_string());
        }
        if !self.is_connected() {
            return Err(format!("MQTT 未连接（{}）", self.status()));
        }

        let (ack_tx, ack_rx) = oneshot::channel();
        let cmd = Cmd::Publish {
            topic: topic.to_string(),
            payload: payload.to_vec(),
            qos,
            ack: ack_tx,
        };
        if self.tx.send(cmd).is_err() {
            return Err("MQTT 后台任务已退出".to_string());
        }

        match tokio::time::timeout(timeout, ack_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("MQTT 发布确认通道已关闭".to_string()),
            Err(_) => Err(format!("MQTT 发布超时（{} 秒）", timeout.as_secs())),
        }
    }
}

/// 连接管理器：循环“建连 → 会话 → 断线等待重连”。
async fn supervisor(opts: MqttOptions, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Shared>) {
    loop {
        match connect_and_handshake(&opts).await {
            Ok(stream) => {
                shared.set(true, format!("{}:{} 已连接", opts.host, opts.port));
                session(stream, &mut rx, &opts).await;
                shared.set(false, format!("{}:{} 连接已断开", opts.host, opts.port));
            }
            Err(e) => {
                shared.set(false, format!("{}:{} 连接失败（{e}）", opts.host, opts.port));
            }
        }

        // 重连等待：期间到达的发布直接回错误（未连接语义），通道关闭则退出
        let wait = opts.reconnect.max(Duration::from_millis(50));
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                cmd = rx.recv() => match cmd {
                    None => return,
                    Some(Cmd::Publish { ack, .. }) => {
                        let _ = ack.send(Err("MQTT 未连接（等待重连）".to_string()));
                    }
                },
            }
        }
    }
}

/// 建连 + MQTT 握手（CONNECT → CONNACK）。
async fn connect_and_handshake(opts: &MqttOptions) -> Result<TcpStream, String> {
    let addr = format!("{}:{}", opts.host, opts.port);
    let mut stream = tokio::time::timeout(opts.connect_timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| format!("连接 {addr} 超时"))?
        .map_err(|e| format!("连接 {addr} 失败：{e}"))?;
    let _ = stream.set_nodelay(true);

    stream
        .write_all(&codec::encode_connect(opts))
        .await
        .map_err(|e| format!("发送 CONNECT 失败：{e}"))?;

    let (first, data) = tokio::time::timeout(opts.connect_timeout, codec::read_packet(&mut stream))
        .await
        .map_err(|_| "等待 CONNACK 超时".to_string())?
        .map_err(|e| format!("读取 CONNACK 失败：{e}"))?;

    if codec::packet_type(first) != codec::CONNACK
    {
        return Err(format!("握手响应异常（报文类型 {}）", codec::packet_type(first)));
    }
    let code = data.get(1).copied().unwrap_or(0xFF);
    if code != 0 {
        return Err(format!("服务端拒绝连接（返回码 {code}）"));
    }
    Ok(stream)
}

/// 单次连接上的会话循环：处理发布命令、服务端报文与保活心跳；返回即断开。
///
/// 读写半部分离：写半部专职发送（发布/PINGREQ），读半部专职接收
/// （PUBACK 按 packet id 唤醒对应发布，PINGRESP/服务端踢出等在此识别）。
async fn session(stream: TcpStream, rx: &mut mpsc::UnboundedReceiver<Cmd>, opts: &MqttOptions) {
    let (mut rd, mut wr) = tokio::io::split(stream);

    // 保活：每 keepalive 周期发送一次 PINGREQ（首 tick 跳过）
    let mut ping = if opts.keepalive.is_zero() {
        None
    } else {
        let mut t = tokio::time::interval(opts.keepalive);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t.tick().await;
        Some(t)
    };

    // 等待 PUBACK 的发布（packet id → 结果回调）
    let mut pending: HashMap<u16, oneshot::Sender<Result<(), String>>> = HashMap::new();

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                // 句柄全部释放：正常退出
                None => break,
                Some(Cmd::Publish { topic, payload, qos, ack }) => {
                    let id = next_packet_id();
                    let packet = codec::encode_publish(&topic, &payload, qos, id, opts.version);
                    if let Err(e) = wr.write_all(&packet).await {
                        let _ = ack.send(Err(format!("发送失败：{e}")));
                        break;
                    }
                    if qos == 0 {
                        let _ = ack.send(Ok(()));
                    } else {
                        // 等待读半部收到 PUBACK；若发布方超时放弃，条目由后续 PUBACK 或断线清理
                        pending.insert(id, ack);
                    }
                }
            },
            packet = codec::read_packet(&mut rd) => match packet {
                Ok((first, data)) => match codec::packet_type(first) {
                    codec::PUBACK => {
                        if let Some(id) = codec::parse_puback(&data) {
                            if let Some(ack) = pending.remove(&id) {
                                let _ = ack.send(Ok(()));
                            }
                        }
                    }
                    codec::PINGRESP => {}
                    // 未订阅主题：忽略服务端下发的发布消息（预留订阅能力）
                    codec::PUBLISH => {}
                    // 服务端主动断开（如会话被顶替）
                    codec::DISCONNECT => break,
                    _ => {}
                },
                Err(_) => break,
            },
            _ = next_ping(&mut ping) => {
                if wr.write_all(&codec::PINGREQ).await.is_err() {
                    break;
                }
            }
        }
    }

    // 连接结束：未确认的发布统一失败（上层按失败重试处理）
    for (_, ack) in pending.drain() {
        let _ = ack.send(Err("连接已断开".to_string()));
    }
}

/// 保活等待（未启用时永不就绪）。
async fn next_ping(ping: &mut Option<tokio::time::Interval>) {
    match ping {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// 报文 id（1..=65535 循环；QoS1 发布使用）。
fn next_packet_id() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    if id == 0 { 1 } else { id }
}

/// MQTT 3.1.1 报文编解码（纯函数，便于单测）。
pub(crate) mod codec {
    use tokio::io::{AsyncRead, AsyncReadExt};

    use super::{MqttOptions, MqttVersion};

    /// 报文类型（CONNACK）
    pub const CONNACK: u8 = 2;
    /// 报文类型（PUBLISH）
    pub const PUBLISH: u8 = 3;
    /// 报文类型（PUBACK）
    pub const PUBACK: u8 = 4;
    /// 报文类型（PINGRESP）
    pub const PINGRESP: u8 = 13;
    /// 报文类型（DISCONNECT）
    pub const DISCONNECT: u8 = 14;

    /// PINGREQ 报文（固定两字节）
    pub const PINGREQ: [u8; 2] = [0xC0, 0x00];

    /// 取报文类型（固定头高 4 位）。
    pub fn packet_type(first: u8) -> u8 {
        first >> 4
    }

    /// 编码“剩余长度”（变长，1~4 字节）。
    pub fn encode_remaining_length(out: &mut Vec<u8>, mut n: usize) {
        loop {
            let mut byte = (n % 128) as u8;
            n /= 128;
            if n > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if n == 0 {
                break;
            }
        }
    }

    /// 编码 UTF-8 字符串（2 字节大端长度 + 内容）。
    fn encode_string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// 编码 CONNECT（协议名/级别按 [`MqttVersion`]；3.1.1 与 5.0 可选用户名密码；keepalive 秒）。
    pub fn encode_connect(opts: &MqttOptions) -> Vec<u8> {
        let version = opts.version;
        let client_id = if opts.client_id.is_empty() {
            format!("dhrust-{}", std::process::id())
        } else {
            opts.client_id.clone()
        };

        let mut flags = 0x02u8; // Clean Session / Clean Start
        if !opts.username.is_empty() {
            flags |= 0x80;
        }
        if !opts.password.is_empty() {
            flags |= 0x40;
        }
        let keepalive = opts.keepalive.as_secs().min(u16::MAX as u64) as u16;

        let mut body = Vec::with_capacity(64);
        encode_string(&mut body, version.protocol_name());
        body.push(version.level());
        body.push(flags);
        body.extend_from_slice(&keepalive.to_be_bytes());
        if version.has_properties() {
            // 5.0：连接属性长度 = 0（无属性）
            body.push(0x00);
        }
        encode_string(&mut body, &client_id);
        if !opts.username.is_empty() {
            encode_string(&mut body, &opts.username);
        }
        if !opts.password.is_empty() {
            encode_string(&mut body, &opts.password);
        }

        let mut out = Vec::with_capacity(body.len() + 6);
        out.push(0x10); // CONNECT
        encode_remaining_length(&mut out, body.len());
        out.extend_from_slice(&body);
        out
    }

    /// 编码 PUBLISH（QoS0 不含报文 id；QoS1 含 2 字节 id；5.0 含空属性长度）。
    pub fn encode_publish(
        topic: &str,
        payload: &[u8],
        qos: u8,
        packet_id: u16,
        version: MqttVersion,
    ) -> Vec<u8> {
        let mut body = Vec::with_capacity(topic.len() + payload.len() + 8);
        encode_string(&mut body, topic);
        if qos > 0 {
            body.extend_from_slice(&packet_id.to_be_bytes());
        }
        if version.has_properties() {
            // 5.0：发布属性长度 = 0（无属性）
            body.push(0x00);
        }
        body.extend_from_slice(payload);

        let mut out = Vec::with_capacity(body.len() + 6);
        out.push(0x30 | ((qos & 0x03) << 1)); // PUBLISH（DUP=0、RETAIN=0）
        encode_remaining_length(&mut out, body.len());
        out.extend_from_slice(&body);
        out
    }

    /// 解析 PUBACK 的报文 id。
    pub fn parse_puback(data: &[u8]) -> Option<u16> {
        (data.len() >= 2).then(|| u16::from_be_bytes([data[0], data[1]]))
    }

    /// 读取一个完整报文：返回 `(固定头首字节, 正文)`。
    pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(u8, Vec<u8>), String> {
        let mut first = [0u8; 1];
        reader
            .read_exact(&mut first)
            .await
            .map_err(|e| e.to_string())?;

        // 剩余长度：最多 4 字节变长
        let mut len = 0usize;
        let mut multiplier = 1usize;
        loop {
            let mut byte = [0u8; 1];
            reader
                .read_exact(&mut byte)
                .await
                .map_err(|e| e.to_string())?;
            len += (byte[0] & 0x7F) as usize * multiplier;
            if byte[0] & 0x80 == 0 {
                break;
            }
            multiplier *= 128;
            if multiplier > 128 * 128 * 128 {
                return Err("MQTT 报文长度非法（超过 4 字节变长）".to_string());
            }
        }

        let mut data = vec![0u8; len];
        if len > 0 {
            reader
                .read_exact(&mut data)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok((first[0], data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn test_opts(port: u16) -> MqttOptions {
        MqttOptions {
            host: "127.0.0.1".into(),
            port,
            version: MqttVersion::default(),
            client_id: "test-client".into(),
            username: "u".into(),
            password: "p".into(),
            keepalive: Duration::ZERO,
            reconnect: Duration::from_millis(100),
            connect_timeout: Duration::from_secs(2),
        }
    }

    /// 服务端桩：接受连接、校验 CONNECT、回 CONNACK，返回套接字。
    async fn broker_accept(listener: &TcpListener) -> TcpStream {
        let (mut sock, _) = listener.accept().await.unwrap();
        let (first, _data) = codec::read_packet(&mut sock).await.unwrap();
        assert_eq!(codec::packet_type(first), 1, "首包应为 CONNECT");
        sock.write_all(&[0x20, 0x02, 0x00, 0x00]).await.unwrap();
        sock
    }

    async fn wait_connected(client: &MqttClient, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if client.is_connected() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[test]
    fn encode_remaining_length_boundaries() {
        let mut out = Vec::new();
        codec::encode_remaining_length(&mut out, 0);
        assert_eq!(out, vec![0x00]);

        out.clear();
        codec::encode_remaining_length(&mut out, 127);
        assert_eq!(out, vec![0x7F]);

        out.clear();
        codec::encode_remaining_length(&mut out, 128);
        assert_eq!(out, vec![0x80, 0x01]);

        out.clear();
        codec::encode_remaining_length(&mut out, 16383);
        assert_eq!(out, vec![0xFF, 0x7F]);

        out.clear();
        codec::encode_remaining_length(&mut out, 16384);
        assert_eq!(out, vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn encode_connect_matches_spec() {
        let opts = MqttOptions {
            host: "x".into(),
            port: 1,
            client_id: "abc".into(),
            username: "u".into(),
            password: "p".into(),
            keepalive: Duration::from_secs(60),
            ..Default::default()
        };
        let packet = codec::encode_connect(&opts);
        let expected: Vec<u8> = [
            // 固定头：CONNECT + 剩余长度 21
            0x10, 0x15,
            // 协议名 "MQTT"
            0x00, 0x04, b'M', b'Q', b'T', b'T',
            // 版本 4 + flags（clean=1、user=1、pass=1）
            0x04, 0xC2,
            // keepalive 60
            0x00, 0x3C,
            // client id "abc"
            0x00, 0x03, b'a', b'b', b'c',
            // username "u"
            0x00, 0x01, b'u',
            // password "p"
            0x00, 0x01, b'p',
        ]
        .to_vec();
        assert_eq!(packet, expected);
    }

    #[test]
    fn encode_publish_qos_variants() {
        // QoS1：含 2 字节报文 id
        let packet = codec::encode_publish("t", b"hi", 1, 7, MqttVersion::V311);
        assert_eq!(
            packet,
            [0x32, 0x07, 0x00, 0x01, b't', 0x00, 0x07, b'h', b'i'].to_vec()
        );

        // QoS0：无报文 id
        let packet = codec::encode_publish("t", b"hi", 0, 0, MqttVersion::V311);
        assert_eq!(packet, [0x30, 0x05, 0x00, 0x01, b't', b'h', b'i'].to_vec());

        // 5.0：报文 id（或主题）之后含空属性长度
        let packet = codec::encode_publish("t", b"hi", 1, 7, MqttVersion::V500);
        assert_eq!(
            packet,
            [0x32, 0x08, 0x00, 0x01, b't', 0x00, 0x07, 0x00, b'h', b'i'].to_vec()
        );
        let packet = codec::encode_publish("t", b"hi", 0, 0, MqttVersion::V500);
        assert_eq!(
            packet,
            [0x30, 0x06, 0x00, 0x01, b't', 0x00, b'h', b'i'].to_vec()
        );

        assert_eq!(codec::parse_puback(&[0x00, 0x07]), Some(7));
        assert_eq!(codec::parse_puback(&[0x00]), None);
    }

    #[test]
    fn version_parse_and_connect_variants() {
        // 版本解析与属性
        assert_eq!(MqttVersion::parse("3.1"), Some(MqttVersion::V310));
        assert_eq!(MqttVersion::parse("3.1.1"), Some(MqttVersion::V311));
        assert_eq!(MqttVersion::parse("5.0"), Some(MqttVersion::V500));
        assert_eq!(MqttVersion::parse("v500"), Some(MqttVersion::V500));
        assert_eq!(MqttVersion::parse("4"), Some(MqttVersion::V311));
        assert_eq!(MqttVersion::parse("bogus"), None);
        assert_eq!(MqttVersion::default(), MqttVersion::V311);
        assert_eq!(MqttVersion::V500.name(), "5.0");
        assert!(MqttVersion::V500.has_properties());
        assert!(!MqttVersion::V311.has_properties());
        assert_eq!(MqttVersion::V310.protocol_name(), "MQIsdp");
        assert_eq!(MqttVersion::V311.protocol_name(), "MQTT");

        let options = |version: MqttVersion| MqttOptions {
            client_id: "abc".into(),
            username: "u".into(),
            password: "p".into(),
            keepalive: Duration::from_secs(60),
            version,
            ..Default::default()
        };

        // 3.1：协议名 MQIsdp（0x00 0x06）+ 级别 3
        let packet = codec::encode_connect(&options(MqttVersion::V310));
        assert_eq!(packet[0], 0x10);
        assert_eq!(packet[1], 0x17, "剩余长度 23（协议名多 2 字节）");
        assert_eq!(&packet[2..10], b"\x00\x06MQIsdp");
        assert_eq!(packet[10], 3, "协议级别 3");
        assert_eq!(packet[11], 0xC2, "connect flags");
        assert_eq!(&packet[12..14], &[0x00, 0x3C], "keepalive 60");
        assert_eq!(&packet[14..19], b"\x00\x03abc");

        // 5.0：级别 5 + 空属性长度（keepalive 后）
        let packet = codec::encode_connect(&options(MqttVersion::V500));
        assert_eq!(packet[1], 0x16, "剩余长度 22");
        assert_eq!(&packet[2..8], b"\x00\x04MQTT");
        assert_eq!(packet[8], 5, "协议级别 5");
        assert_eq!(packet[9], 0xC2);
        assert_eq!(&packet[10..12], &[0x00, 0x3C]);
        assert_eq!(packet[12], 0x00, "5.0 连接属性长度应为 0");
        assert_eq!(&packet[13..18], b"\x00\x03abc");
    }

    #[tokio::test]
    async fn connect_publish_qos1_and_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let broker = tokio::spawn(async move {
            // 第一段会话：收一条 QoS1 发布 → 回 PUBACK → 断开
            let mut sock = broker_accept(&listener).await;
            let (first, data) = codec::read_packet(&mut sock).await.unwrap();
            assert_eq!(codec::packet_type(first), codec::PUBLISH);
            let tlen = u16::from_be_bytes([data[0], data[1]]) as usize;
            let topic = std::str::from_utf8(&data[2..2 + tlen]).unwrap().to_string();
            let pid = u16::from_be_bytes([data[2 + tlen], data[3 + tlen]]);
            let payload = data[4 + tlen..].to_vec();
            sock.write_all(&[0x40, 0x02, (pid >> 8) as u8, pid as u8])
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(sock); // 断开 → 触发客户端重连

            // 第二段：验证自动重连（10 秒内未重连则测试失败）
            let _sock2 = broker_accept(&listener).await;
            (topic, payload)
        });

        let client = MqttClient::spawn(test_opts(port));
        assert!(wait_connected(&client, Duration::from_secs(5)).await, "首连失败：{}", client.status());

        client
            .publish("WFSCAN", b"{\"sn\":\"x\"}", 1, Duration::from_secs(3))
            .await
            .unwrap();

        let (topic, payload) = tokio::time::timeout(Duration::from_secs(10), broker)
            .await
            .expect("重连未发生")
            .unwrap();
        assert_eq!(topic, "WFSCAN");
        assert_eq!(payload, b"{\"sn\":\"x\"}");
    }

    #[tokio::test]
    async fn publish_when_disconnected_fails_fast() {
        // 预留端口后立即释放：连接必然被拒
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let client = MqttClient::spawn(MqttOptions {
            reconnect: Duration::from_millis(50),
            connect_timeout: Duration::from_millis(300),
            ..test_opts(port)
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!client.is_connected());
        assert!(client.status().contains("连接失败"), "状态：{}", client.status());

        let err = client
            .publish("t", b"x", 1, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.contains("未连接"), "{err}");
    }

    #[tokio::test]
    async fn keepalive_ping_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let broker = tokio::spawn(async move {
            let mut sock = broker_accept(&listener).await;
            // 3 秒内应至少收到一次 PINGREQ（keepalive = 1s）
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let mut pings = 0usize;
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(
                    Duration::from_millis(500),
                    codec::read_packet(&mut sock),
                )
                .await
                {
                    Ok(Ok((first, _))) => {
                        if codec::packet_type(first) == 12 {
                            pings += 1;
                            sock.write_all(&[0xD0, 0x00]).await.unwrap(); // PINGRESP
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
                if pings >= 2 {
                    break;
                }
            }
            pings
        });

        let mut opts = test_opts(port);
        opts.keepalive = Duration::from_secs(1);
        let client = MqttClient::spawn(opts);
        assert!(wait_connected(&client, Duration::from_secs(5)).await);

        let pings = tokio::time::timeout(Duration::from_secs(8), broker)
            .await
            .expect("等 PINGREQ 超时")
            .unwrap();
        assert!(pings >= 1, "至少应收到一次 PINGREQ，实际 {pings}");
    }
}
