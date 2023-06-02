pub mod io; // 加了pub之后为公共模块可以为外部调用
pub mod logs;

pub fn bar() {
    io::foo();
    logs::baz();
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
