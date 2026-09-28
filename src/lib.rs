pub mod config; // 配置项（对应 DH.NCore Setting/Configuration）
pub mod io; // 加了pub之后为公共模块可以为外部调用
pub mod logs;
pub mod sign;
pub mod threading; // 线程与定时调度（对应 DH.NCore Threading）
pub mod times;

#[cfg(feature = "razor")]
pub mod razor; // Razor 子集模板引擎（方案 C）：同一份 .cshtml 双端渲染

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
