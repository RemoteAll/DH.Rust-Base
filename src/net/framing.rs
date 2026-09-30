//! 字节流分帧器：按终结符把 TCP 字节流切分为一条条数据帧（半包/粘包安全）。
//!
//! 来源：tcp-scanner-server 现场实践沉淀（2026-09-30 收编），与 DH.NCore 的“逐包处理”
//! 语义配套：无编解码器时“收到的每个数据块即一条消息”——整块恰为保活串时上层可直接
//! 计数跳过（[`FrameDecoder::pure_keepalives`]），流内保活串由分帧器整串剥除并逐次上报
//! 事件，二者共同保证“任何时候不粘连”。
//!
//! 能力：
//! - 终结符族：CRLF / LF / CR（三种换行互相兼容）/ ETX / 自定义单字节
//! - 保活串（设备心跳等无终结符的周期串）：仅在帧起始处整串匹配，不误伤帧内容中的同名字样
//! - 超长帧保护：超过 `max_len` 的帧整体丢弃，直到下一个终结符后恢复
//! - 空闲结算：未带终结符的残留内容在连接空闲后整段丢弃，绝不与下一批数据拼接
//! - [`FrameDecoder::apply_options`] 热更新：对已建立连接即时生效（含清理已积压的保活串）
//!
//! 纯标准库实现，无任何特性依赖。

use std::fmt;
use std::str::FromStr;

/// 帧终结符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// 回车换行 `\r\n`（兼容单独的回车或换行）
    CrLf,
    /// 换行 `\n`（兼容回车）
    Lf,
    /// 回车 `\r`（兼容换行）
    Cr,
    /// ETX 结束符 `0x03`，常见于 STX/ETX 包裹协议
    Etx,
    /// 自定义单字节终结符
    Byte(u8),
}

impl Terminator {
    /// 判断某个字节是否为帧分隔符。
    ///
    /// 说明：换行族（CRLF/LF/CR）互相兼容，避免设备换行设置与配置不一致时无法分帧；
    /// 连续出现的 `\r\n` 会产生一个空帧，由解码器统一忽略。
    pub fn is_separator(&self, b: u8) -> bool {
        match self {
            Terminator::CrLf | Terminator::Lf | Terminator::Cr => b == b'\r' || b == b'\n',
            Terminator::Etx => b == 0x03,
            Terminator::Byte(x) => b == *x,
        }
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::CrLf => f.write_str("CRLF"),
            Terminator::Lf => f.write_str("LF"),
            Terminator::Cr => f.write_str("CR"),
            Terminator::Etx => f.write_str("ETX"),
            Terminator::Byte(b) => write!(f, "HEX:{b:02X}"),
        }
    }
}

impl FromStr for Terminator {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v = s.trim();
        match v.to_ascii_uppercase().as_str() {
            "CRLF" | "CR+LF" | "0D0A" | "\\R\\N" => Ok(Terminator::CrLf),
            "LF" | "0A" | "\\N" => Ok(Terminator::Lf),
            "CR" | "0D" | "\\R" => Ok(Terminator::Cr),
            "ETX" | "03" => Ok(Terminator::Etx),
            other => match other.strip_prefix("HEX:") {
                Some(hex) => {
                    let b = u8::from_str_radix(hex.trim(), 16)
                        .map_err(|_| format!("无效的十六进制终结符 HEX:{hex}，应形如 HEX:0D"))?;
                    Ok(Terminator::Byte(b))
                }
                None => Err(format!(
                    "无效的终结符 \"{s}\"，支持 CRLF / LF / CR / ETX / HEX:xx"
                )),
            },
        }
    }
}

impl serde::Serialize for Terminator {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Terminator {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// 分帧参数。
#[derive(Debug, Clone)]
pub struct FrameOptions {
    /// 帧终结符
    pub terminator: Terminator,
    /// 单帧最大长度（字节），超过即整帧丢弃（内部下限 8）
    pub max_len: usize,
    /// 是否剔除帧两端的 ASCII 控制字符（STX/ETX 包裹等场景）
    pub strip_control: bool,
    /// 保活串（如设备心跳 "heartbeat"，无终结符周期发送）：整串剥除；空 = 不启用
    pub keepalive: Vec<u8>,
}

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            terminator: Terminator::CrLf,
            max_len: 2048,
            strip_control: true,
            keepalive: Vec::new(),
        }
    }
}

/// 分帧结果事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEvent {
    /// 一条完整数据帧的原始字节
    Data(Vec<u8>),
    /// 一次保活串（整串剥除，不进入任何帧；供上层计数/打印）
    Keepalive,
    /// 一条超长帧被整体丢弃（附带被丢弃的原始字节数）
    Overflow(usize),
}

/// 空闲结算结果（见 [`FrameDecoder::flush_idle`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleFlush {
    /// 无待处理内容
    None,
    /// 有未带终结符的残留内容，已整段丢弃（附字节数）
    Discarded(usize),
}

/// 字节流分帧器：按终结符切分数据，支持半包/粘包。
pub struct FrameDecoder {
    /// 帧终结符
    terminator: Terminator,
    /// 单帧最大长度，超限即丢弃
    max_len: usize,
    /// 是否剔除帧两端控制字符
    strip_control: bool,
    /// 保活串（字节形式；None = 不启用）
    keepalive: Option<Vec<u8>>,
    /// 保活串的当前匹配进度
    ka_match: usize,
    /// 当前帧缓冲
    buf: Vec<u8>,
    /// 是否处于“丢弃超长帧”状态
    overflowing: bool,
    /// 当前超长帧已累计的字节数
    overflow_len: usize,
}

impl FrameDecoder {
    /// 根据分帧参数创建分帧器。
    pub fn new(options: &FrameOptions) -> Self {
        Self {
            terminator: options.terminator,
            max_len: options.max_len.max(8),
            strip_control: options.strip_control,
            keepalive: (!options.keepalive.is_empty()).then(|| options.keepalive.clone()),
            ka_match: 0,
            buf: Vec::with_capacity(256),
            overflowing: false,
            overflow_len: 0,
        }
    }

    /// 应用新的分帧参数（热加载时对**已建立的连接**即时生效）。
    ///
    /// 保留当前缓冲；保活串发生变化时重置匹配进度，并清理缓冲中已积压的保活串
    /// （例如：连接建立时未启用保活，之后热加载启用——已攒下的保活串一次性清除）。
    pub fn apply_options(&mut self, options: &FrameOptions) {
        self.terminator = options.terminator;
        self.max_len = options.max_len.max(8);
        self.strip_control = options.strip_control;

        let new_keepalive = (!options.keepalive.is_empty()).then(|| options.keepalive.clone());
        if new_keepalive != self.keepalive {
            self.keepalive = new_keepalive;
            self.ka_match = 0;
            self.strip_buffered_keepalives();
        }
    }

    /// 清理帧缓冲中已积压的保活串（连接中途启用/更换保活串时的一次性处理）。
    fn strip_buffered_keepalives(&mut self) {
        let Some(token) = &self.keepalive else {
            return;
        };
        if self.overflowing {
            return;
        }
        while self.buf.starts_with(token) {
            self.buf.drain(..token.len());
        }
    }

    /// 送入一段原始字节，把切分出的完整帧追加到 `out`。
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<FrameEvent>) {
        for &b in data {
            // 保活串：仅在当前帧尚未开始时按整串剥除（保活串无终结符，不应进入帧缓冲）
            if self.try_consume_keepalive(b, out) {
                continue;
            }

            if self.terminator.is_separator(b) {
                self.finish_frame(out);
                continue;
            }
            if self.overflowing {
                self.overflow_len += 1;
                continue;
            }
            self.buf.push(b);
            if self.buf.len() > self.max_len {
                // 超长帧：丢弃并进入溢出状态，直到下一个终结符
                self.overflowing = true;
                self.overflow_len = self.buf.len();
                self.buf.clear();
            }
        }
    }

    /// 尝试按保活串消费一个字节（仅在帧起始处匹配）。
    ///
    /// - 命中整串：上报 [`FrameEvent::Keepalive`] 并返回 `true`
    /// - 前缀部分匹配：字节被吞、返回 `true`（等待后续字节）
    /// - 前缀失配：把已吞掉的字节补回帧缓冲并返回 `false`（当前字节交由常规处理）
    fn try_consume_keepalive(&mut self, b: u8, out: &mut Vec<FrameEvent>) -> bool {
        if !self.buf.is_empty() || self.overflowing {
            return false;
        }
        let Some(token) = &self.keepalive else {
            return false;
        };
        if b == token[self.ka_match] {
            self.ka_match += 1;
            if self.ka_match == token.len() {
                // 完整保活串：丢弃，不进入任何帧；上报事件供计数与打印
                self.ka_match = 0;
                out.push(FrameEvent::Keepalive);
            }
            return true;
        }
        if self.ka_match > 0 {
            // 部分匹配失败：把已吞掉的字节补回帧数据，当前字节继续按常规处理
            self.buf.extend_from_slice(&token[..self.ka_match]);
            self.ka_match = 0;
        }
        false
    }

    /// 判断一段数据是否恰好由 K 个完整保活串组成（K ≥ 1），不是则返回 None。
    ///
    /// 用于“逐包处理”（对齐 DH.NCore：无编解码器时每个收到的数据块即一条消息）：
    /// 整块恰为保活串时，上层可直接计数打印，不进入分帧器。
    pub fn pure_keepalives(&self, data: &[u8]) -> Option<usize> {
        let token = self.keepalive.as_deref()?;
        if data.is_empty() || data.len() < token.len() || !data.len().is_multiple_of(token.len()) {
            return None;
        }

        let mut count = 0;
        let mut rest = data;
        while !rest.is_empty() {
            if !rest.starts_with(token) {
                return None;
            }
            rest = &rest[token.len()..];
            count += 1;
        }
        Some(count)
    }

    /// 查看当前缓冲的前若干字节（用于残留告警展示）。
    pub fn peek_pending(&self, max: usize) -> Vec<u8> {
        self.buf.iter().copied().take(max).collect()
    }

    /// 空闲结算：连接上的未带终结符内容在空闲后立即处理并清空，绝不会留到下一批数据。
    ///
    /// 由会话层在空闲超时（如 `idle_flush_ms`）时调用：
    /// - 溢出状态：终结符始终未到，直接结束丢弃
    /// - 空缓冲：无事发生
    /// - 其它残留：整段丢弃（内容对不上任何完整帧；宁可明确丢弃告警，也不与后续数据拼接）
    pub fn flush_idle(&mut self) -> IdleFlush {
        if self.overflowing {
            let len = self.overflow_len;
            self.overflowing = false;
            self.overflow_len = 0;
            self.buf.clear();
            self.ka_match = 0;
            return IdleFlush::Discarded(len);
        }

        if self.buf.is_empty() {
            self.ka_match = 0;
            return IdleFlush::None;
        }

        let len = self.buf.len();
        self.buf.clear();
        self.ka_match = 0;
        IdleFlush::Discarded(len)
    }

    /// 结束当前帧：
    /// - 溢出状态 → 上报丢弃事件
    /// - 空帧 → 忽略（连续终结符、CRLF 中的双分隔符都会产生空帧）
    /// - 正常帧 → 按需剔除控制字符后上报
    fn finish_frame(&mut self, out: &mut Vec<FrameEvent>) {
        if self.overflowing {
            out.push(FrameEvent::Overflow(self.overflow_len));
            self.overflowing = false;
            self.overflow_len = 0;
            self.buf.clear();
            return;
        }

        if self.buf.is_empty() {
            return;
        }

        let raw = std::mem::take(&mut self.buf);
        if self.strip_control {
            let trimmed = trim_control(&raw);
            if !trimmed.is_empty() {
                out.push(FrameEvent::Data(trimmed.to_vec()));
            }
        } else {
            out.push(FrameEvent::Data(raw));
        }
    }

    /// 当前缓冲中未完结的字节数（用于空闲结算与诊断）。
    pub fn pending_len(&self) -> usize {
        self.buf.len()
    }

    /// 是否正处于“丢弃超长帧”状态（用于空闲结算判定）。
    pub fn overflowing(&self) -> bool {
        self.overflowing
    }
}

/// 剔除字节序列两端的 ASCII 控制字符（含 STX/ETX/DEL 等）。
///
/// 说明：GBK 与 UTF-8 的多字节字符都不会使用 `< 0x20` 或 `0x7F` 作为首尾字节，
/// 因此该裁剪对两种编码都是安全的。
pub fn trim_control(data: &[u8]) -> &[u8] {
    let is_ctrl = |b: u8| b < 0x20 || b == 0x7F;
    let start = data.iter().position(|&b| !is_ctrl(b)).unwrap_or(data.len());
    let end = data.iter().rposition(|&b| !is_ctrl(b)).map_or(start, |i| i + 1);
    &data[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 启用保活串（heartbeat）的默认参数。
    fn keepalive_options() -> FrameOptions {
        FrameOptions {
            keepalive: b"heartbeat".to_vec(),
            ..Default::default()
        }
    }

    fn data_of(events: &[FrameEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                FrameEvent::Data(d) => Some(String::from_utf8_lossy(d).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn split_crlf_basic() {
        let mut d = FrameDecoder::new(&FrameOptions::default());
        let mut out = vec![];
        d.feed(b"ABC123\r\nDEF456\r\n", &mut out);
        assert_eq!(data_of(&out), vec!["ABC123", "DEF456"]);
    }

    #[test]
    fn split_across_partial_feeds() {
        let mut d = FrameDecoder::new(&FrameOptions::default());
        let mut out = vec![];
        // 半包：CR 与 LF 分别到达
        d.feed(b"HLT2026", &mut out);
        assert!(out.is_empty(), "未遇到终结符不应出帧");
        d.feed(b"0926", &mut out);
        d.feed(b"\r", &mut out);
        d.feed(b"\n", &mut out);
        assert_eq!(data_of(&out), vec!["HLT20260926"]);
    }

    #[test]
    fn newline_family_is_tolerant() {
        // 配置 CRLF，但设备发来的是单独 LF 或 CR，也应能分帧
        for term in ["\n", "\r"] {
            let mut d = FrameDecoder::new(&FrameOptions::default());
            let mut out = vec![];
            d.feed(format!("A{term}B{term}").as_bytes(), &mut out);
            assert_eq!(data_of(&out), vec!["A", "B"], "终结符 {term:?} 应兼容");
        }
    }

    #[test]
    fn split_etx_with_stx_wrapper() {
        let options = FrameOptions {
            terminator: Terminator::Etx,
            ..Default::default()
        };
        let mut d = FrameDecoder::new(&options);
        let mut out = vec![];
        // 常见的 STX...ETX 包裹协议
        d.feed(b"\x02SN123456\x03\x02SN654321\x03", &mut out);
        assert_eq!(data_of(&out), vec!["SN123456", "SN654321"]);
    }

    #[test]
    fn split_custom_byte() {
        let options = FrameOptions {
            terminator: Terminator::Byte(0x7C), // '|'
            ..Default::default()
        };
        let mut d = FrameDecoder::new(&options);
        let mut out = vec![];
        d.feed(b"A|B|C|", &mut out);
        assert_eq!(data_of(&out), vec!["A", "B", "C"]);
    }

    #[test]
    fn empty_frames_are_ignored() {
        let mut d = FrameDecoder::new(&FrameOptions::default());
        let mut out = vec![];
        d.feed(b"\r\n\r\nX\r\n\r\n", &mut out);
        assert_eq!(data_of(&out), vec!["X"]);
    }

    #[test]
    fn oversize_frame_is_dropped_and_recovers() {
        let options = FrameOptions {
            max_len: 16,
            ..Default::default()
        };
        let mut d = FrameDecoder::new(&options);
        let mut out = vec![];

        let big = vec![b'9'; 100];
        d.feed(&big, &mut out);
        d.feed(b"\r\nOK\r\n", &mut out);

        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], FrameEvent::Overflow(n) if n >= 100));
        assert_eq!(data_of(&out[1..]), vec!["OK"]);
    }

    #[test]
    fn keep_control_when_disabled() {
        let options = FrameOptions {
            strip_control: false,
            ..Default::default()
        };
        let mut d = FrameDecoder::new(&options);
        let mut out = vec![];
        d.feed(b"\x02SN1\x03\r\n", &mut out);
        assert_eq!(data_of(&out), vec!["\u{2}SN1\u{3}"]);
    }

    #[test]
    fn trim_control_edges() {
        assert_eq!(trim_control(b"\x02ABC\x03"), b"ABC");
        assert_eq!(trim_control(b"ABC"), b"ABC");
        assert_eq!(trim_control(b"\x00\x01"), b"");
        assert_eq!(trim_control(b" A "), b" A ");
    }

    #[test]
    fn terminator_parse_and_display() {
        assert_eq!("CRLF".parse::<Terminator>().unwrap(), Terminator::CrLf);
        assert_eq!("lf".parse::<Terminator>().unwrap(), Terminator::Lf);
        assert_eq!("ETX".parse::<Terminator>().unwrap(), Terminator::Etx);
        assert_eq!("HEX:0d".parse::<Terminator>().unwrap(), Terminator::Byte(0x0D));
        assert!("XYZ".parse::<Terminator>().is_err());
        assert_eq!(Terminator::Byte(0x7C).to_string(), "HEX:7C");
    }

    #[test]
    fn keepalive_prefix_is_stripped_from_frame() {
        // 复刻现场：6 个裸心跳（无换行） + 业务 JSON + CRLF
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];
        let json = r#"{"SN":"DB0496509", "MSG":"http://weixin.qq.com/q/m3VN5IPlXVwkTrMk0VkA"}"#;
        let stream = format!("{}{json}\r\n", "heartbeat".repeat(6));
        d.feed(stream.as_bytes(), &mut out);

        assert_eq!(data_of(&out), vec![json]);
        assert_eq!(d.pending_len(), 0, "保活串不应进入帧缓冲");
    }

    #[test]
    fn keepalive_split_across_feeds() {
        // 逐字节喂入：保活串与数据都可能跨包到达
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];
        for b in b"heartbeatheartbeat{\"SN\":1}\r\n" {
            d.feed(&[*b], &mut out);
        }
        assert_eq!(data_of(&out), vec![r#"{"SN":1}"#]);
    }

    #[test]
    fn lone_keepalives_never_accumulate() {
        // 长时间只有保活串（无终结符）：不应产生帧、不应堆积（否则会触发超长帧保护）
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];
        for _ in 0..200 {
            d.feed(b"heartbeat", &mut out);
        }
        assert_eq!(out.len(), 200, "每个保活串应上报一次事件");
        assert!(data_of(&out).is_empty(), "保活串不得进入任何帧");
        assert_eq!(d.pending_len(), 0, "保活串不应在缓冲区累积");
    }

    #[test]
    fn keepalive_inside_frame_is_kept() {
        // 剥离仅在帧开始时生效：条码内容里出现同名字样不受影响
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];
        d.feed(b"ABheartbeatCD\r\n", &mut out);
        assert_eq!(data_of(&out), vec!["ABheartbeatCD"]);
    }

    #[test]
    fn keepalive_disabled_keeps_current_behavior() {
        // 未配置保活串时保持原行为（整体作为一帧）
        let mut d = FrameDecoder::new(&FrameOptions::default());
        let mut out = vec![];
        d.feed(b"heartbeat{\"SN\":1}\r\n", &mut out);
        assert_eq!(data_of(&out), vec![r#"heartbeat{"SN":1}"#]);
    }

    #[test]
    fn partial_keepalive_is_flushed_as_data() {
        // 与保活串同前缀但不是保活串的数据：按原样保留，且不污染后续解析
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];
        d.feed(b"hea\r\n", &mut out);
        assert_eq!(data_of(&out), vec!["hea"]);

        out.clear();
        d.feed(b"heartbeatXYZ\r\n", &mut out);
        assert_eq!(data_of(&out), vec!["XYZ"]);
    }

    #[test]
    fn apply_options_strips_backlogged_keepalives() {
        // 连接建立时未启用保活；保活串积压后再启用 → 积压应被一次性清理，后续不再进入缓冲
        let mut d = FrameDecoder::new(&FrameOptions::default());
        let mut out = vec![];
        d.feed(b"heartbeatheartbeatheartbeat", &mut out);
        assert_eq!(d.pending_len(), 27, "未启用时应按原逻辑积压在缓冲");

        d.apply_options(&keepalive_options());
        assert_eq!(d.pending_len(), 0, "已积压的裸保活串应在热更新时被清理");

        d.feed(b"heartbeat{\"SN\":1}\r\n", &mut out);
        assert_eq!(data_of(&out), vec![r#"{"SN":1}"#]);
    }

    #[test]
    fn keepalive_emits_event_per_token() {
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];
        d.feed(b"heartbeatheartbeat", &mut out);
        assert_eq!(
            out.iter().filter(|e| matches!(e, FrameEvent::Keepalive)).count(),
            2,
            "每个完整保活串应上报一次事件：{out:?}"
        );
        assert!(data_of(&out).is_empty());
    }

    #[test]
    fn pure_keepalives_detects_whole_tokens_only() {
        let d = FrameDecoder::new(&keepalive_options());
        assert_eq!(d.pure_keepalives(b"heartbeat"), Some(1));
        assert_eq!(d.pure_keepalives(b"heartbeatheartbeat"), Some(2));
        assert_eq!(d.pure_keepalives(b"heartbeatX"), None);
        assert_eq!(d.pure_keepalives(b"hea"), None);
        assert_eq!(d.pure_keepalives(b"yheartbeat"), None);

        let plain = FrameDecoder::new(&FrameOptions::default());
        assert_eq!(plain.pure_keepalives(b"heartbeat"), None, "未配置保活串时不应识别");
    }

    #[test]
    fn idle_flush_discards_residue_and_ends_overflow() {
        let mut d = FrameDecoder::new(&keepalive_options());
        let mut out = vec![];

        assert_eq!(d.flush_idle(), IdleFlush::None, "空缓冲无结算");

        // 未带终结符的残留内容：空闲结算整段丢弃，绝不留到下一批数据
        d.feed(b"PINGPONG", &mut out);
        assert_eq!(d.peek_pending(8), b"PINGPONG".to_vec());
        assert_eq!(d.flush_idle(), IdleFlush::Discarded(8));
        assert_eq!(d.pending_len(), 0);

        // 超长帧未等到终结符也要能结束，避免吞掉下一条扫码
        let mut d2 = FrameDecoder::new(&FrameOptions {
            max_len: 8,
            ..Default::default()
        });
        d2.feed(&[b'x'; 32], &mut out);
        assert!(matches!(d2.flush_idle(), IdleFlush::Discarded(32)));
        assert_eq!(d2.pending_len(), 0);
    }
}
