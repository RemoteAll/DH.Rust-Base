pub mod config; // 配置项（对应 DH.NCore Setting/Configuration）
pub mod io; // 加了pub之后为公共模块可以为外部调用
pub mod logs;
pub mod random; // 安全随机（OS 熵；令牌/密钥等安全凭证）
pub mod sign;
pub mod threading; // 线程与定时调度（对应 DH.NCore Threading）
pub mod times;
pub mod zip; // 极简 ZIP 打包器（store 法；内存 + 流式落盘两种形态）

#[cfg(feature = "razor")]
pub mod razor; // Razor 子集模板引擎（方案 C）：同一份 .cshtml 双端渲染

#[cfg(feature = "net")]
pub mod net; // 网络内核（hyper + fastwebsockets + 自研语义层；DHDeploy.Agent Rust 迁移）

pub fn bar() {
    io::foo();
    logs::baz();
    times::foo();
}

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
