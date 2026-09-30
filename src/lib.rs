pub mod config; // 配置项（对应 DH.NCore Setting/Configuration）
pub mod io; // 加了pub之后为公共模块可以为外部调用
pub mod logs;
pub mod random; // 安全随机（OS 熵；令牌/密钥等安全凭证）
pub mod sign;
pub mod threading; // 线程与定时调度（对应 DH.NCore Threading）
pub mod times;
pub mod web; // Web 辅助（URL 编码 / JSON 转义；对应 DH.NCore NewLife.Web 系列）
pub mod zip; // 极简 ZIP 打包器（store 法；内存 + 流式落盘两种形态）

#[cfg(feature = "razor")]
pub mod razor; // Razor 子集模板引擎（方案 C）：同一份 .cshtml 双端渲染

pub mod net; // 网络模块（基础：本机 IP + 字节流分帧；http-client/http-tls/net/stun/net-tls 特性扩展 HTTP/WS/RPC/TLS/STUN）

#[cfg(feature = "http-client")]
pub mod wecom; // 企业微信机器人（Webhook 推送：文本/Markdown/Markdown V2；对应 C# Pek.WebHook 的 WeChatWorkRobot）
