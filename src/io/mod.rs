use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// 读取文本文件全文（UTF-8；自动去除 BOM，对齐 C# `File.ReadAllText` 行为）。
pub fn read_all_text<P: AsRef<Path>>(path: P) -> io::Result<String> {
    let text = fs::read_to_string(path)?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// 写入文本文件全文（UTF-8 不带 BOM，对齐 .NET Core `File.WriteAllText` 行为）。
pub fn write_all_text<P: AsRef<Path>>(path: P, text: &str) -> io::Result<()> {
    fs::write(path, text)
}

/// 原子写文本文件：先写 `.tmp` 再改名替换，避免中途失败留下半截内容。
///
/// 目标被占用等导致改名失败时降级为直接写入（尽力而为），并清理临时文件。
pub fn write_all_text_atomic<P: AsRef<Path>>(path: P, text: &str) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, text)?;

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // 目标被占用时降级为直接写入（尽力而为），并清理临时文件
            let rs = fs::write(path, text);
            let _ = fs::remove_file(&tmp);
            rs
        }
    }
}

/// 简单通配匹配：`*`（任意串）与 `?`（单字符），大小写不敏感。
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn matches(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }

        match p[0] {
            '*' => (0..=t.len()).any(|i| matches(&p[1..], &t[i..])),
            '?' => !t.is_empty() && matches(&p[1..], &t[1..]),
            c => {
                !t.is_empty()
                    && c.eq_ignore_ascii_case(&t[0])
                    && matches(&p[1..], &t[1..])
            }
        }
    }

    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    matches(&p, &t)
}

/// 拆分命令行参数字符串（支持双引号包裹；与 C# ProcessStartInfo 的常见用法对齐）。
///
/// 例：`urls=http://*:8080 "a b" c` → `["urls=http://*:8080", "a b", "c"]`
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for c in text.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            ' ' | '\t' if !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            _ => {
                cur.push(c);
                has_token = true;
            }
        }
    }

    if has_token {
        out.push(cur);
    }

    out
}

/// 解析 `k=v;k2=v2` 形式的环境变量串（C# `ServiceInfo.Environments` 语义）。
pub fn parse_environments(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in text.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(p) = item.find('=') {
            let key = item[..p].trim();
            let value = item[p + 1..].trim();
            if !key.is_empty() {
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}

/// 词法归一化路径（不访问文件系统，可处理尚不存在的路径）：消除 `.` 与 `..`。
///
/// 前导 `..` 会保留（如 `../apps/x`）；空路径返回 `.`。
/// 来源：Pek.RAgent 服务注册路径归属校验/影子目录计算（2026-10-03 下沉）。
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }

    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// 读取文件尾部若干行（整文件读入；日志文件规模下可接受）。
pub fn read_tail<P: AsRef<Path>>(path: P, count: usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// 目录下指定扩展名（不区分大小写）中文件名最大的文件（日志/快照等“找最新”场景）。
pub fn latest_file_by_ext<P: AsRef<Path>>(dir: P, extension: &str) -> Option<std::path::PathBuf> {
    let ext = extension.to_ascii_lowercase();
    let mut best: Option<(String, std::path::PathBuf)> = None;
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(&ext) {
            continue;
        }
        if best.as_ref().map(|(b, _)| name > *b).unwrap_or(true) {
            best = Some((name, path));
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip_and_bom_strip() {
        let dir = std::env::temp_dir().join(format!("dhrust-io-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sample.txt");

        // 普通读写
        write_all_text(&file, "你好 hello").unwrap();
        assert_eq!(read_all_text(&file).unwrap(), "你好 hello");

        // 带 BOM 的文件（C# XmlWriter 等常见）读出时自动去掉 BOM
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("<?xml version=\"1.0\"?>".as_bytes());
        fs::write(&file, bytes).unwrap();
        assert_eq!(read_all_text(&file).unwrap(), "<?xml version=\"1.0\"?>");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wildcard_and_split_args() {
        assert!(wildcard_match("*.zip", "a.zip"));
        assert!(wildcard_match("*.ZIP", "a.zip")); // 大小写不敏感
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "abbc"));
        assert!(wildcard_match("*", ""));

        let args = split_args(r#"urls=http://*:8080 "a b" c"#);
        assert_eq!(args, vec!["urls=http://*:8080", "a b", "c"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn environments_parse() {
        let envs = parse_environments("A=1; B=2 ;C=x=y;");
        assert_eq!(
            envs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
                ("C".to_string(), "x=y".to_string()),
            ]
        );
        assert!(parse_environments("").is_empty());
    }

    #[test]
    fn lexical_normalize_handles_parent() {
        assert_eq!(
            lexical_normalize(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("../apps/x")),
            PathBuf::from("../apps/x")
        );
        assert_eq!(lexical_normalize(Path::new("")), PathBuf::from("."));
    }

    #[test]
    fn atomic_write_tail_and_latest() {
        let dir = std::env::temp_dir().join(format!("dhrust-ioutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let file = dir.join("a.log");
        write_all_text_atomic(&file, "l1\nl2\n").unwrap();
        write_all_text_atomic(&file, "l1\nl2\nl3\nl4\n").unwrap(); // 覆盖写
        assert_eq!(fs::read_to_string(&file).unwrap(), "l1\nl2\nl3\nl4\n");
        assert!(!dir.join("a.log.tmp").exists(), "临时文件应被清理");

        let tail = read_tail(&file, 2);
        assert_eq!(tail, vec!["l3".to_string(), "l4".to_string()]);
        assert!(read_tail(dir.join("missing.log"), 2).is_empty());

        // latest 场景：移除 a.log（'a' 的字母序大于日期文件名，避免干扰比较）
        fs::remove_file(&file).unwrap();
        write_all_text_atomic(&dir.join("2026_10_01.log"), "x").unwrap();
        write_all_text_atomic(&dir.join("2026_10_02.log"), "y").unwrap();
        write_all_text_atomic(&dir.join("readme.txt"), "z").unwrap();
        let latest = latest_file_by_ext(&dir, ".log").unwrap();
        assert!(latest.ends_with("2026_10_02.log"), "{latest:?}");

        let _ = fs::remove_dir_all(&dir);
    }
}

/// 程序基础目录：环境变量（按序探测；非空优先，词法归一）→ 可执行文件目录 → 当前目录。
///
/// 收编自 Pek.RAgent（`PEK_RAGENT_BASE`）与 HlkProductTool（`HLK_TOOL_BASE`）的同款实现（2026-10-03）。
pub fn base_dir(env_names: &[&str]) -> PathBuf {
    for name in env_names {
        if let Ok(dir) = std::env::var(name) {
            let dir = dir.trim();
            if !dir.is_empty() {
                return lexical_normalize(Path::new(dir));
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ————— 安全替换（2026-10-03 下沉：Pek.RAgent deploy 的“运行中文件替换”语义；2026-10-09 收编三处消费方统一）—————

/// 替换文件（可处理“目标被占用（运行中）”场景）。
///
/// 1. 目标不存在：直接改名就位（跨设备时退化为 copy + 删除源）；
/// 2. 先原地改名（Unix 覆盖运行中文件合法；Windows 目标未占用时亦可）；
/// 3. 失败（目标被占用）时把目标让位改名为 `backup` 后再就位；失败自动回滚。
///
/// 成功后 `backup` 为让位下来的旧文件（被占用时删除会失败，由调用方决定清理或留待
/// 下次替换/启动时处理）；固定 `backup` 名（如 `{exe}.old`）会在让位前先清掉上一次
/// 遗留的同名备份。
pub fn replace_file(src: &Path, dst: &Path, backup: &Path) -> io::Result<()> {
    if !dst.exists() {
        return fs::rename(src, dst).or_else(|_| {
            fs::copy(src, dst)?;
            let _ = fs::remove_file(src);
            Ok(())
        });
    }

    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }

    // 目标被占用：让位（先清掉可能遗留的同名备份）后再就位；失败回滚
    let _ = fs::remove_file(backup);
    fs::rename(dst, backup)?;
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::rename(backup, dst);
            Err(e)
        }
    }
}

/// 安全替换文件（[`replace_file`] 的便捷封装：让位备份用唯一 `*.del` 名，成功后尽力清理）。
///
/// 1. 原子改名（Unix 可直接覆盖；Windows 目标未占用时亦可）；
/// 2. 目标被占用（运行中）时，把目标改名为 `*.del` 再写入新文件（Windows 允许重命名运行中的文件）；
///    `*.del` 删除失败不报错，待应用停止后由调用方的清理逻辑处理。
pub fn safe_replace_file(src: &Path, dst: &Path) -> io::Result<()> {
    let bak = del_path(dst);
    replace_file(src, dst, &bak)?;
    // 运行中的文件删除会失败，留给后续清理
    let _ = fs::remove_file(&bak);
    Ok(())
}

/// 生成唯一的 `*.del` 路径。
fn del_path(dst: &Path) -> PathBuf {
    let base = PathBuf::from(format!("{}.del", dst.display()));
    if !base.exists() {
        return base;
    }

    for i in 1..10_000 {
        let candidate = PathBuf::from(format!("{}.{}.del", dst.display(), i));
        if !candidate.exists() {
            return candidate;
        }
    }

    base
}

#[cfg(test)]
mod base_dir_tests {
    use super::*;

    #[test]
    fn base_dir_falls_back_to_exe_parent() {
        // 环境变量未设置（用不可能存在的名字）→ 回退到可执行文件目录
        let dir = base_dir(&["DHRUST_TEST_BASE_DIR_NOT_SET_XYZ"]);
        let exe = std::env::current_exe().unwrap();
        assert_eq!(dir, exe.parent().unwrap().to_path_buf());
        assert!(dir.is_absolute());
    }
}

#[cfg(test)]
mod safe_replace_tests {
    use super::*;

    #[test]
    fn safe_replace_overwrites_and_creates() {
        let dir = std::env::temp_dir().join(format!("dhrust-saferp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 目标存在：覆盖
        let src = dir.join("new.txt");
        let dst = dir.join("old.txt");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();
        safe_replace_file(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"new");
        assert!(!src.exists());

        // 目标不存在：创建
        let src2 = dir.join("fresh.txt");
        let dst2 = dir.join("missing.txt");
        fs::write(&src2, b"fresh").unwrap();
        safe_replace_file(&src2, &dst2).unwrap();
        assert_eq!(fs::read(&dst2).unwrap(), b"fresh");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `replace_file`：目标不存在（直接就位）、正常替换（不产生备份）、就位失败（让位后回滚）。
    #[test]
    fn replace_file_variants_and_rollback() {
        let dir = std::env::temp_dir().join(format!("dhrust-replacefile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let dst = dir.join("app.bin");
        let bak = dir.join("app.bin.old");

        // 目标不存在：直接就位，不产生备份
        let src = dir.join("v1.bin");
        fs::write(&src, b"v1").unwrap();
        replace_file(&src, &dst, &bak).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"v1");
        assert!(!bak.exists());

        // 正常替换：原地改名，不产生备份
        let src2 = dir.join("v2.bin");
        fs::write(&src2, b"v2").unwrap();
        replace_file(&src2, &dst, &bak).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"v2");
        assert!(!bak.exists());

        // 就位失败（源缺失）：目标先让位、失败后必须回滚恢复，备份不残留
        let missing = dir.join("not-exist.bin");
        let err = replace_file(&missing, &dst, &bak).unwrap_err();
        let _ = err;
        assert!(dst.exists(), "失败后目标必须回滚恢复");
        assert_eq!(fs::read(&dst).unwrap(), b"v2");
        assert!(!bak.exists(), "回滚后备份不应残留");

        let _ = fs::remove_dir_all(&dir);
    }
}
