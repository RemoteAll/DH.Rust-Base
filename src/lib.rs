pub mod audit; // 审计日志脱敏与摘要（敏感字段 *** / 动作名 / 参数摘要；多消费方收编）
pub mod cert; // 证书与域名工具（PEM 拆包 / 最小 X.509 解析 / 通配符匹配；证书下发与部署共用）
pub mod config; // 配置项（对应 DH.NCore Setting/Configuration）
#[cfg(feature = "fileman")]
pub mod fileman; // 文件管理（浏览/上传下载/编辑/压缩解压/搜索；Pek.RAgent 与 Pek.RPanlServer 共用）
pub mod io; // 加了pub之后为公共模块可以为外部调用
pub mod json; // JSON 值读取小工具（第三方接口响应解析；Option 语义）
pub mod logs;
#[cfg(feature = "patch")]
pub mod patch;
#[cfg(feature = "plugin")]
pub mod plugin; // 插件包格式 + 插件源目录签名（Ed25519；Pek.RAgent 与 Pek.RPanlServer 共用）
pub mod random; // 安全随机（OS 熵；令牌/密钥等安全凭证）
pub mod ratelimit; // 固定窗口限流（通用计数引擎；进程内存储可替换 Redis；HTTP 策略见 pek_rwaf::throttle）
#[cfg(feature = "secret")]
pub mod secret; // 可逆加密（AES-256-GCM；敏感值“防直读”存储；配置密码/凭据场景，见模块文档）
#[cfg(feature = "service")]
pub mod service; // 跨平台服务管理（Windows SCM / systemd / procd / SysV / launchd；Pek.RAgent 下沉）
pub mod sign;
pub mod staragent; // 星尘（StarAgent）配置注册：影子模式条目的 upsert/移除（Pek.RPanlServer 与 HlktechIoT MQTT 共用）
#[cfg(feature = "term")]
pub mod term; // 真 PTY 会话引擎（ConPTY/openpty；在线终端等服务端能力共用）
pub mod text; // 通用文本小工具（按字符截断等；多消费方收编）
pub mod threading; // 线程与定时调度（对应 DH.NCore Threading）
pub mod times;
pub mod upgrade; // 自升级管线·应用层（识别/静置/自检/原子替换/交接重启；Pek.RAgent、Pek.RPanlServer、HLZX.EIP 三套实现收编）
pub mod version; // 版本号工具（数字段比较：Agent 自动升级的版本判断）
pub mod web; // Web 辅助（URL 编码 / JSON 转义；对应 DH.NCore NewLife.Web 系列）
pub mod zip; // 极简 ZIP 打包器（store 法；内存 + 流式落盘两种形态） // zstd 字典差分补丁（增量升级：生成/还原；Pek.RAgent 与 DHDeploy.Agent.Rust 共用）

#[cfg(feature = "razor")]
pub mod razor; // Razor 子集模板引擎（方案 C）：同一份 .cshtml 双端渲染

pub mod net; // 网络模块（基础：本机 IP + 字节流分帧；http-client/http-tls/net/stun/net-tls 特性扩展 HTTP/WS/RPC/TLS/STUN）

pub mod sys; // 系统信息采集（磁盘挂载过滤 / 网卡累计流量；跨平台）

#[cfg(feature = "http-client")]
pub mod alert; // 告警中心（企业微信 + 钉钉双通道；异步派发 / 冷却去重 / 统计；多项目收编）

#[cfg(feature = "http-client")]
pub mod dingtalk; // 钉钉群机器人（Webhook 推送：text/markdown；支持「加签」）

#[cfg(feature = "http-client")]
pub mod wecom; // 企业微信机器人（Webhook 推送：文本/Markdown/Markdown V2；对应 C# Pek.WebHook 的 WeChatWorkRobot）
