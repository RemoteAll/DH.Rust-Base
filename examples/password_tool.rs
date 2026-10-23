//! 口令散列互通工具（与 C# `NewLife.Security.SaltPasswordProvider` 对照用）。
//!
//! 由 `scripts/password-interop.ps1` 驱动，与 C# 工具 `tools/csharp/DHRustDemo`
//! 的 `password-*` 命令互为反证：
//! - C# 生成散列 → Rust 验证；
//! - Rust 生成散列 → C# 验证；
//! - 固定盐散列两侧**逐字节相等**。
//!
//! 用法：
//! ```text
//! password_tool hash       <算法> <口令>                # 随机盐
//! password_tool hash-fixed <算法> <口令> <盐>            # 固定盐（确定性）
//! password_tool verify     <口令> <散列>                 # 输出 OK / FAIL
//! password_tool verify-alg <算法> <口令> <散列>
//! ```

use dhrust::sign::password::PasswordProvider;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "hash" | "hash-fixed" => {
            let (Some(alg), Some(pwd)) = (args.get(1), args.get(2)) else {
                eprintln!("用法：password_tool {cmd} <算法> <口令> [盐]");
                std::process::exit(2);
            };
            let prv = match PasswordProvider::with_algorithm(alg) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            let stored = if cmd == "hash-fixed" {
                match args.get(3) {
                    Some(salt) => prv.hash_with_salt(pwd, salt),
                    None => {
                        eprintln!("hash-fixed 需要盐值参数");
                        std::process::exit(2);
                    }
                }
            } else {
                prv.hash(pwd)
            };
            println!("{stored}");
        }
        "verify" | "verify-alg" => {
            let (alg, pwd, hash) = if cmd == "verify" {
                (None, args.get(1), args.get(2))
            } else {
                (args.get(1), args.get(2), args.get(3))
            };
            let (Some(pwd), Some(hash)) = (pwd, hash) else {
                eprintln!("用法：password_tool {cmd} [算法] <口令> <散列>");
                std::process::exit(2);
            };
            let prv = match alg {
                Some(a) => PasswordProvider::with_algorithm(a),
                None => Ok(PasswordProvider::new()),
            };
            let ok = match prv {
                Ok(p) => p.verify(pwd, hash),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            println!("{}", if ok { "OK" } else { "FAIL" });
            if !ok {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("用法：password_tool <hash|hash-fixed|verify|verify-alg> …");
            std::process::exit(2);
        }
    }
}
