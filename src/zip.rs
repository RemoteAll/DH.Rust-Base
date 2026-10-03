//! 极简 ZIP 打包器（store 存储法、零依赖）：内存构建与流式落盘两种形态。
//!
//! 来源：原 DHDeploy.Agent.Rust `service::zip`（迁移验收后收编为框架通用件，2026-09-29）。
//!
//! - [`ZipWriter`]：内存构建（`finish()` 返回完整字节；小体量打包）
//! - [`ZipFileWriter`]：流式写入文件（大目录/大文件不驻留内存；local header 回填 CRC/size）
//! - [`pack_directory`] / [`pack_single_file`]：内存打包（对齐 C# `ZipFile.CreateFromDirectory` 语义）
//! - [`pack_directory_to_file`] / [`pack_single_file_to_file`]：流式打包到目标文件
//!
//! 限制：store 法不压缩（体积与源相当）；单文件/条目 4GB 上限（未实现 Zip64）；条目数 65535 上限。

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// CRC-32（IEEE 802.3；ZIP 规范必需）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    update_crc32(&mut crc, data);
    !crc
}

/// 增量 CRC-32（流式读取时逐块更新；初值为 `0xFFFF_FFFF`，最终取反由调用方处理）。
pub fn update_crc32(crc: &mut u32, data: &[u8]) {
    let mut c = *crc;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            let mask = (c & 1).wrapping_neg();
            c = (c >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    *crc = c;
}

struct DosTime {
    date: u16,
    time: u16,
}

fn dos_now() -> DosTime {
    let now = chrono::Local::now();
    use chrono::{Datelike, Timelike};
    let year = now.year().clamp(1980, 2107) as u16;
    let date = ((year - 1980) << 9) | ((now.month() as u16) << 5) | now.day() as u16;
    let time = (now.hour() as u16) << 11 | (now.minute() as u16) << 5 | (now.second() as u16 / 2);
    DosTime { date, time }
}

fn push_central_entry(
    central: &mut Vec<u8>,
    name: &[u8],
    crc: u32,
    size: u32,
    local_offset: u32,
    dt: &DosTime,
) {
    central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    central.extend_from_slice(&20u16.to_le_bytes()); // version made by
    central.extend_from_slice(&20u16.to_le_bytes()); // version needed
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes());
    central.extend_from_slice(&dt.time.to_le_bytes());
    central.extend_from_slice(&dt.date.to_le_bytes());
    central.extend_from_slice(&crc.to_le_bytes());
    central.extend_from_slice(&size.to_le_bytes());
    central.extend_from_slice(&size.to_le_bytes());
    central.extend_from_slice(&(name.len() as u16).to_le_bytes());
    central.extend_from_slice(&0u16.to_le_bytes()); // extra
    central.extend_from_slice(&0u16.to_le_bytes()); // comment
    central.extend_from_slice(&0u16.to_le_bytes()); // disk
    central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    central.extend_from_slice(&local_offset.to_le_bytes());
    central.extend_from_slice(name);
}

fn write_eocd(buf: &mut Vec<u8>, count: u16, central_size: u32, central_offset: u32) {
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&central_size.to_le_bytes());
    buf.extend_from_slice(&central_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
}

/// ZIP 构建器（store 法、内存）。
pub struct ZipWriter {
    buf: Vec<u8>,
    central: Vec<u8>,
    count: u16,
    offset: u32,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    /// 创建。
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(64 * 1024),
            central: Vec::new(),
            count: 0,
            offset: 0,
        }
    }

    /// 添加一个文件条目（`name` 使用 `/` 分隔的相对路径）。
    pub fn add_file(&mut self, name: &str, data: &[u8]) {
        let crc = crc32(data);
        let dt = dos_now();
        let name_bytes = name.as_bytes();
        let local_offset = self.offset;

        // 本地文件头
        self.buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // method = store
        self.buf.extend_from_slice(&dt.time.to_le_bytes());
        self.buf.extend_from_slice(&dt.date.to_le_bytes());
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.buf
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf
            .extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.buf.extend_from_slice(name_bytes);
        self.buf.extend_from_slice(data);
        self.offset = self.buf.len() as u32;

        // 中央目录项
        push_central_entry(
            &mut self.central,
            name_bytes,
            crc,
            data.len() as u32,
            local_offset,
            &dt,
        );
        self.count += 1;
    }

    /// 添加目录条目（以 `/` 结尾；部分解压工具依赖显式目录）。
    pub fn add_dir(&mut self, name: &str) {
        let mut n = name.to_string();
        if !n.ends_with('/') {
            n.push('/');
        }
        let name_bytes = n.as_bytes();
        let dt = dos_now();
        let local_offset = self.offset;

        self.buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&20u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&dt.time.to_le_bytes());
        self.buf.extend_from_slice(&dt.date.to_le_bytes());
        self.buf.extend_from_slice(&0u32.to_le_bytes()); // crc=0
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.buf
            .extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(name_bytes);
        self.offset = self.buf.len() as u32;

        push_central_entry(&mut self.central, name_bytes, 0, 0, local_offset, &dt);
        self.count += 1;
    }

    /// 完成：返回完整 ZIP 字节。
    pub fn finish(mut self) -> Vec<u8> {
        let central_offset = self.buf.len() as u32;
        let central_size = self.central.len() as u32;
        self.buf.extend_from_slice(&self.central);
        write_eocd(&mut self.buf, self.count, central_size, central_offset);
        self.buf
    }
}

/// ZIP 流式写入器（store 法，直接写文件、不驻留内存）。
///
/// 用途：大目录/大文件的下载打包（`DownloadFile`/`PrepareTransfer`）与部署备份，
/// 内存占用与文件总量无关（仅 256KB 读缓冲）。
pub struct ZipFileWriter {
    w: BufWriter<File>,
    central: Vec<u8>,
    count: u16,
    offset: u32,
}

impl ZipFileWriter {
    /// 创建并打开目标文件（已存在时截断）。
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            w: BufWriter::with_capacity(256 * 1024, File::create(path)?),
            central: Vec::new(),
            count: 0,
            offset: 0,
        })
    }

    /// 添加一个文件条目（`name` 使用 `/` 分隔的相对路径）。
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> std::io::Result<()> {
        let mut cursor = std::io::Cursor::new(data);
        self.add_file_reader(name, &mut cursor).map(|_| ())
    }

    /// 流式添加文件内容（读一段写一段，CRC 增量计算）；返回写入字节数。
    pub fn add_file_reader<R: Read>(&mut self, name: &str, r: &mut R) -> std::io::Result<u64> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ZIP 条目名过长",
            ));
        }
        let dt = dos_now();
        let header_offset = self.offset;

        // 本地文件头（crc/size 先占位，数据写完回填）
        self.w.write_all(&0x0403_4b50u32.to_le_bytes())?;
        self.w.write_all(&20u16.to_le_bytes())?;
        self.w.write_all(&0u16.to_le_bytes())?;
        self.w.write_all(&0u16.to_le_bytes())?;
        self.w.write_all(&dt.time.to_le_bytes())?;
        self.w.write_all(&dt.date.to_le_bytes())?;
        self.w.write_all(&0u32.to_le_bytes())?; // crc 占位
        self.w.write_all(&0u32.to_le_bytes())?; // 压缩大小占位
        self.w.write_all(&0u32.to_le_bytes())?; // 原始大小占位
        self.w.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
        self.w.write_all(&0u16.to_le_bytes())?;
        self.w.write_all(name_bytes)?;

        // 流式写数据 + CRC
        let mut crc = 0xFFFF_FFFFu32;
        let mut size: u64 = 0;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            update_crc32(&mut crc, &buf[..n]);
            self.w.write_all(&buf[..n])?;
            size += n as u64;
        }
        if size > u32::MAX as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "单个文件超过 4GB，暂不支持 Zip64",
            ));
        }
        let crc = !crc;

        // 回填 crc/size（local header 偏移 14 处），再回到文件尾
        let end_pos = self.w.stream_position()?;
        self.w.seek(SeekFrom::Start(header_offset as u64 + 14))?;
        self.w.write_all(&crc.to_le_bytes())?;
        self.w.write_all(&(size as u32).to_le_bytes())?;
        self.w.write_all(&(size as u32).to_le_bytes())?;
        self.w.seek(SeekFrom::Start(end_pos))?;
        self.offset = end_pos as u32;

        push_central_entry(
            &mut self.central,
            name_bytes,
            crc,
            size as u32,
            header_offset,
            &dt,
        );
        self.count += 1;
        Ok(size)
    }

    /// 添加目录条目（以 `/` 结尾）。
    pub fn add_dir(&mut self, name: &str) -> std::io::Result<()> {
        let mut n = name.to_string();
        if !n.ends_with('/') {
            n.push('/');
        }
        let name_bytes = n.as_bytes();
        let dt = dos_now();
        let header_offset = self.offset;

        self.w.write_all(&0x0403_4b50u32.to_le_bytes())?;
        self.w.write_all(&20u16.to_le_bytes())?;
        self.w.write_all(&0u16.to_le_bytes())?;
        self.w.write_all(&0u16.to_le_bytes())?;
        self.w.write_all(&dt.time.to_le_bytes())?;
        self.w.write_all(&dt.date.to_le_bytes())?;
        self.w.write_all(&0u32.to_le_bytes())?;
        self.w.write_all(&0u32.to_le_bytes())?;
        self.w.write_all(&0u32.to_le_bytes())?;
        self.w.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
        self.w.write_all(&0u16.to_le_bytes())?;
        self.w.write_all(name_bytes)?;
        let pos = self.w.stream_position()?;
        self.offset = pos as u32;

        push_central_entry(&mut self.central, name_bytes, 0, 0, header_offset, &dt);
        self.count += 1;
        Ok(())
    }

    /// 完成：写中央目录与 EOCD 并落盘。
    pub fn finish(mut self) -> std::io::Result<()> {
        let central_offset = self.offset;
        let central_size = self.central.len() as u32;
        self.w.write_all(&self.central)?;
        let mut eocd = Vec::with_capacity(22);
        write_eocd(&mut eocd, self.count, central_size, central_offset);
        self.w.write_all(&eocd)?;
        self.w.flush()?;
        Ok(())
    }
}

/// 递归打包目录到 ZIP（内存版；对齐 C# `ZipFile.CreateFromDirectory(..., includeBaseDirectory)`）。
///
/// - `include_base`: true 时归档内包含基础目录名（对齐 C# 目录打包语义）。
pub fn pack_directory(dir: &Path, include_base: bool) -> std::io::Result<Vec<u8>> {
    let base_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut zip = ZipWriter::new();
    let root_prefix = if include_base && !base_name.is_empty() {
        format!("{base_name}/")
    } else {
        String::new()
    };
    if !root_prefix.is_empty() {
        zip.add_dir(base_name.as_str());
    }
    add_dir_recursive(&mut zip, dir, &root_prefix)?;
    Ok(zip.finish())
}

/// 打包单个文件为 ZIP（内存版；归档内仅含该文件，对齐 C# 文件打包语义）。
pub fn pack_single_file(file: &Path) -> std::io::Result<Vec<u8>> {
    let data = std::fs::read(file)?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let mut zip = ZipWriter::new();
    zip.add_file(&name, &data);
    Ok(zip.finish())
}

/// 递归打包目录到 ZIP 文件（流式版；`include_base` 语义同 [`pack_directory`]）。
pub fn pack_directory_to_file(dir: &Path, include_base: bool, dest: &Path) -> std::io::Result<()> {
    let base_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut zip = ZipFileWriter::create(dest)?;
    let root_prefix = if include_base && !base_name.is_empty() {
        zip.add_dir(base_name.as_str())?;
        format!("{base_name}/")
    } else {
        String::new()
    };
    add_dir_recursive_to_file(&mut zip, dir, &root_prefix)?;
    zip.finish()
}

/// 打包单个文件为 ZIP 文件（流式版；归档内仅含该文件）。
pub fn pack_single_file_to_file(file: &Path, dest: &Path) -> std::io::Result<()> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let mut zip = ZipFileWriter::create(dest)?;
    let mut f = std::fs::File::open(file)?;
    zip.add_file_reader(&name, &mut f)?;
    zip.finish()
}

/// 统计 ZIP 中的文件条目数（不含目录项；无法解析/超过防御上限时返回 0，不回退报错）。
pub fn count_file_entries(zip_path: &Path) -> std::io::Result<usize> {
    let mut f = std::fs::File::open(zip_path)?;
    let len = f.metadata()?.len();

    // 尾部 64KB+22 内定位 EOCD（注释上限 64KB）
    let tail_len = len.min(65_558) as usize;
    if tail_len < 22 {
        return Ok(0);
    }
    f.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = vec![0u8; tail_len];
    f.read_exact(&mut tail)?;
    let eocd = match tail
        .windows(4)
        .rposition(|w| w == 0x0605_4b50u32.to_le_bytes())
    {
        Some(p) => p,
        None => return Ok(0),
    };
    let count = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]) as usize;
    let cd_offset = u32::from_le_bytes([
        tail[eocd + 16],
        tail[eocd + 17],
        tail[eocd + 18],
        tail[eocd + 19],
    ]) as u64;
    let cd_size = u32::from_le_bytes([
        tail[eocd + 12],
        tail[eocd + 13],
        tail[eocd + 14],
        tail[eocd + 15],
    ]) as usize;
    if cd_size == 0 || cd_size > 64 * 1024 * 1024 || cd_offset + cd_size as u64 > len {
        return Ok(0);
    }

    f.seek(SeekFrom::Start(cd_offset))?;
    let mut cd = vec![0u8; cd_size];
    f.read_exact(&mut cd)?;

    let mut pos = 0usize;
    let mut files = 0usize;
    for _ in 0..count {
        if pos + 46 > cd.len() || cd[pos..pos + 4] != 0x0201_4b50u32.to_le_bytes() {
            return Ok(0);
        }
        let name_len = u16::from_le_bytes([cd[pos + 28], cd[pos + 29]]) as usize;
        let extra_len = u16::from_le_bytes([cd[pos + 30], cd[pos + 31]]) as usize;
        let comment_len = u16::from_le_bytes([cd[pos + 32], cd[pos + 33]]) as usize;
        let name_start = pos + 46;
        if name_start + name_len > cd.len() {
            return Ok(0);
        }
        if !cd[name_start..name_start + name_len].ends_with(b"/") {
            files += 1;
        }
        pos = name_start + name_len + extra_len + comment_len;
    }
    Ok(files)
}

fn add_dir_recursive(zip: &mut ZipWriter, dir: &Path, prefix: &str) -> std::io::Result<()> {
    for (rel, path, is_dir) in walk_dir(dir, prefix)? {
        if is_dir {
            zip.add_dir(&rel);
        } else if let Ok(data) = std::fs::read(&path) {
            zip.add_file(&rel, &data);
        }
    }
    Ok(())
}

fn add_dir_recursive_to_file(
    zip: &mut ZipFileWriter,
    dir: &Path,
    prefix: &str,
) -> std::io::Result<()> {
    for (rel, path, is_dir) in walk_dir(dir, prefix)? {
        if is_dir {
            zip.add_dir(&rel)?;
        } else if let Ok(mut f) = std::fs::File::open(&path) {
            zip.add_file_reader(&rel, &mut f)?;
        }
    }
    Ok(())
}

/// 深度优先遍历目录（排序稳定：目录优先于同层文件，整体按名称排序）。
///
/// 返回 `(归档相对路径, 磁盘路径, 是否目录)` 序列；目录条目带 `/` 后缀。
fn walk_dir(root: &Path, prefix: &str) -> std::io::Result<Vec<(String, PathBuf, bool)>> {
    let mut out = Vec::new();
    walk_dir_into(root, prefix, &mut out)?;
    Ok(out)
}

fn walk_dir_into(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf, bool)>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = format!("{prefix}{name}");
        let meta = entry.metadata()?;
        if meta.is_dir() {
            out.push((format!("{rel}/"), path.clone(), true));
            walk_dir_into(&path, &format!("{rel}/"), out)?;
        } else if meta.is_file() {
            out.push((rel, path, false));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn zip_structure_valid() {
        let mut z = ZipWriter::new();
        z.add_dir("root");
        z.add_file("root/a.txt", b"hello");
        z.add_file("b.bin", &[0u8, 1, 2, 3]);
        let bytes = z.finish();

        // EOCD 定位
        let eocd = bytes
            .windows(4)
            .rposition(|w| w == 0x0605_4b50u32.to_le_bytes())
            .expect("EOCD");
        assert_eq!(&bytes[eocd..eocd + 4], &0x0605_4b50u32.to_le_bytes());
        let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]);
        assert_eq!(count, 3);
        let size = u32::from_le_bytes([
            bytes[eocd + 12],
            bytes[eocd + 13],
            bytes[eocd + 14],
            bytes[eocd + 15],
        ]);
        let offset = u32::from_le_bytes([
            bytes[eocd + 16],
            bytes[eocd + 17],
            bytes[eocd + 18],
            bytes[eocd + 19],
        ]);
        assert_eq!((offset + size) as usize, eocd);
    }

    #[test]
    fn pack_directory_roundtrip_names() {
        let dir = std::env::temp_dir().join(format!("zip-test-{}", rand_suffix()));
        let sub = dir.join("pkg/child");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.txt"), "x").unwrap();
        std::fs::write(dir.join("pkg/y.txt"), "y").unwrap();

        let bytes = pack_directory(&dir.join("pkg"), true).unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("pkg/y.txt"));
        assert!(text.contains("pkg/child/x.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_writer_stream_roundtrip() {
        let dir = std::env::temp_dir().join(format!("zip-fw-test-{}", rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "stream-content").unwrap();

        let dest = dir.join("out.zip");
        pack_single_file_to_file(&dir.join("f.txt"), &dest).unwrap();

        // store 法：名字与内容明文可见；EOCD 可定位
        let bytes = std::fs::read(&dest).unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("f.txt"));
        assert!(text.contains("stream-content"));
        assert!(bytes.windows(4).any(|w| w == 0x0605_4b50u32.to_le_bytes()));

        // 回填校验：local header 的 CRC 与独立计算一致
        let idx = bytes
            .windows(4)
            .position(|w| w == 0x0403_4b50u32.to_le_bytes())
            .unwrap();
        let crc = u32::from_le_bytes([
            bytes[idx + 14],
            bytes[idx + 15],
            bytes[idx + 16],
            bytes[idx + 17],
        ]);
        assert_eq!(crc, crc32(b"stream-content"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_writer_directory_roundtrip() {
        let dir = std::env::temp_dir().join(format!("zip-fw-dir-{}", rand_suffix()));
        let sub = dir.join("pkg/child");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.txt"), "x-stream").unwrap();
        std::fs::write(dir.join("pkg/y.txt"), "y-stream").unwrap();

        let dest = dir.join("pkg.zip");
        pack_directory_to_file(&dir.join("pkg"), true, &dest).unwrap();

        let bytes = std::fs::read(&dest).unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(text.contains("pkg/y.txt"));
        assert!(text.contains("pkg/child/x.txt"));
        assert!(text.contains("x-stream"));
        assert!(text.contains("y-stream"));

        // 文件条目计数：2 个文件 + 2 个目录（pkg/ 与 pkg/child/）
        assert_eq!(count_file_entries(&dest).unwrap(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn count_file_entries_single() {
        let dir = std::env::temp_dir().join(format!("zip-cnt-{}", rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let dest = dir.join("a.zip");
        pack_single_file_to_file(&dir.join("a.txt"), &dest).unwrap();
        assert_eq!(count_file_entries(&dest).unwrap(), 1);

        // 非 ZIP 文件：回退 0
        std::fs::write(dir.join("bad.zip"), "not a zip").unwrap();
        assert_eq!(count_file_entries(&dir.join("bad.zip")).unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rand_suffix() -> String {
        use rand::Rng;
        let n: u32 = rand::thread_rng().gen();
        format!("{n:08x}")
    }
}

// ————— ZIP 解压（feature `zip-extract`；2026-10-03 下沉：Pek.RAgent deploy / DHDeploy.Agent 共用）—————

/// 解压 zip 到目标目录（防目录穿越；占用文件安全替换；保留可执行位；返回写入文件数）。
///
/// - 目录穿越条目（`enclosed_name()` 为 None）直接跳过；
/// - 每个文件先写 `{path}.{pid}.{i}.tmp` 再经 [`crate::io::safe_replace_file`] 替换，
///   运行中的目标文件在 Windows 上也会被改名让位（详见该函数文档）；
/// - Unix 下保留可执行位（`mode & 0o111` 时 `| 0o755`）。
#[cfg(feature = "zip-extract")]
pub fn extract_zip(zip_path: &std::path::Path, target: &std::path::Path) -> Result<usize, String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("打开压缩包失败 {}：{}", zip_path.display(), e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("读取压缩包失败：{}", e))?;

    std::fs::create_dir_all(target).map_err(|e| format!("创建目录失败：{}", e))?;

    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取压缩包条目失败：{}", e))?;

        let Some(rel) = entry.enclosed_name() else {
            continue; // 目录穿越条目，跳过
        };
        let out_path = target.join(rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| format!("创建目录失败：{}", e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{}", e))?;
        }

        let mut data = Vec::with_capacity(entry.size() as usize);
        std::io::copy(&mut entry, &mut data).map_err(|e| format!("解压数据失败：{}", e))?;

        let tmp = std::path::PathBuf::from(format!(
            "{}.{}.{}.tmp",
            out_path.display(),
            std::process::id(),
            i
        ));
        std::fs::write(&tmp, &data).map_err(|e| format!("写入临时文件失败：{}", e))?;
        crate::io::safe_replace_file(&tmp, &out_path)
            .map_err(|e| format!("替换文件失败 {}：{}", out_path.display(), e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                if mode & 0o111 != 0 {
                    let _ = std::fs::set_permissions(
                        &out_path,
                        std::fs::Permissions::from_mode(mode | 0o755),
                    );
                }
            }
        }

        count += 1;
    }

    Ok(count)
}

#[cfg(all(test, feature = "zip-extract"))]
mod extract_tests {
    use super::*;

    #[test]
    fn extract_zip_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dhrust-zipx-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join("a.zip");

        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file("sub/hello.txt", options).unwrap();
            use std::io::Write;
            writer.write_all(b"hello").unwrap();
            writer.finish().unwrap();
        }

        let out = dir.join("out");
        let n = extract_zip(&zip_path, &out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(out.join("sub/hello.txt")).unwrap(), b"hello");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
