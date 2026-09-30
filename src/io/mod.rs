use std::fs;
use std::io;
use std::path::Path;

/// 读取文本文件全文（UTF-8；自动去除 BOM，对齐 C# `File.ReadAllText` 行为）。
pub fn read_all_text<P: AsRef<Path>>(path: P) -> io::Result<String> {
    let text = fs::read_to_string(path)?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// 写入文本文件全文（UTF-8 不带 BOM，对齐 .NET Core `File.WriteAllText` 行为）。
pub fn write_all_text<P: AsRef<Path>>(path: P, text: &str) -> io::Result<()> {
    fs::write(path, text)
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
}
