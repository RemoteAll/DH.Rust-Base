pub mod config; // 配置项（对应 DH.NCore Setting/Configuration）
pub mod io; // 加了pub之后为公共模块可以为外部调用
pub mod logs;
pub mod random; // 安全随机（OS 熵；令牌/密钥等安全凭证）
pub mod sign;
#[cfg(feature = "secret")]
pub mod secret; // 可逆加密（AES-256-GCM；敏感值“防直读”存储；配置密码/凭据场景，见模块文档）
#[cfg(feature = "service")]
pub mod service; // 跨平台服务管理（Windows SCM / systemd / procd / SysV / launchd；Pek.RAgent 下沉）
#[cfg(feature = "term")]
pub mod term; // 真 PTY 会话引擎（ConPTY/openpty；在线终端等服务端能力共用）
#[cfg(feature = "plugin")]
pub mod plugin; // 插件包格式 + 插件源目录签名（Ed25519；Pek.RAgent 与 Pek.RPanlServer 共用）
pub mod staragent; // 星尘（StarAgent）配置注册：影子模式条目的 upsert/移除（Pek.RPanlServer 与 HlktechIoT MQTT 共用）
pub mod threading; // 线程与定时调度（对应 DH.NCore Threading）
pub mod times;
pub mod web; // Web 辅助（URL 编码 / JSON 转义；对应 DH.NCore NewLife.Web 系列）
pub mod zip; // 极简 ZIP 打包器（store 法；内存 + 流式落盘两种形态）
pub mod version; // 版本号工具（数字段比较：Agent 自动升级的版本判断）
#[cfg(feature = "patch")]
pub mod patch; // zstd 字典差分补丁（增量升级：生成/还原；Pek.RAgent 与 DHDeploy.Agent.Rust 共用）

#[cfg(feature = "razor")]
pub mod razor; // Razor 子集模板引擎（方案 C）：同一份 .cshtml 双端渲染

pub mod net; // 网络模块（基础：本机 IP + 字节流分帧；http-client/http-tls/net/stun/net-tls 特性扩展 HTTP/WS/RPC/TLS/STUN）

pub mod sys; // 系统信息采集（磁盘挂载过滤 / 网卡累计流量；跨平台）

#[cfg(feature = "http-client")]
pub mod wecom; // 企业微信机器人（Webhook 推送：文本/Markdown/Markdown V2；对应 C# Pek.WebHook 的 WeChatWorkRobot）
