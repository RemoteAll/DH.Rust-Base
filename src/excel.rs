//! Excel（.xlsx）读写：导出数据 / 导入模板 / 读回用户填写的表格。
//!
//! # 为什么在基础库里
//! 「导出一张表给用户填，再读回来」是管理系统/工具的通用能力（HLZX.EIP 的人员、供应商等模块，
//! 后续对接财务/仓管的导出都用同一套）。各项目各写一份的话，列宽/日期/浮点/空行的处理口径
//! 必然漂移——所以按用户级指令**下沉到 dhrust**。
//!
//! # 依赖与平台
//! - 写：[`rust_xlsxwriter`]；读：[`calamine`]。两者都是**纯 Rust**，
//!   可 zig 交叉编 musl（不需要 LibreOffice / 系统库），与生态其它服务一致。
//! - 仅 feature `excel` 引入，不用 Excel 的项目零成本。
//!
//! # 版式（与 PHP 端 `Office::buildExcel` 对齐）
//! ```text
//! 第 1 行   标题（跨列合并、加粗居中）      ← title 为空则省略
//! 第 2 行   表头（加粗 + 浅灰底 + 细边框）  ← 冻结窗格从数据行开始
//! 第 3 行~  数据
//! 末尾     说明行（如「总数：12」，灰色小字）
//! ```
//!
//! # 导入的列匹配
//! 用 [`column_index`] **按表头名称**匹配（去空白、忽略大小写），不硬编码 A~L 位置——
//! 用户调换列顺序、或多加几列都不影响；表头整行为空时才由调用方按位置兜底。
//!
//! # 示例
//! ```
//! use dhrust::excel::{self, Sheet};
//!
//! // 导出
//! let mut sheet = Sheet::new("人员", vec!["人员名称".into(), "人员编号".into()]);
//! sheet.rows.push(vec!["李章炎".into(), "RY0003".into()]);
//! sheet.notes.push("总数：1".into());
//! let bytes = excel::build(&sheet).unwrap();
//!
//! // 读回（用户改过之后再上传）
//! let rows = excel::read(&bytes).unwrap();
//! assert_eq!(rows[1][0], "人员名称");
//! assert_eq!(rows[2][1], "RY0003");
//! let name_col = excel::column_index(&rows[1], &["人员名称", "name"]);
//! assert_eq!(name_col, Some(0));
//! ```

use std::io::Cursor;

use calamine::{open_workbook_auto_from_rs, Data, Reader};
use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, Workbook};

/// 一张待导出的表（标题 + 表头 + 数据 + 末尾说明）。
pub struct Sheet {
    /// 第 1 行的标题（为空则不写标题行）。
    pub title: String,
    /// 第 2 行表头。
    pub head: Vec<String>,
    /// 数据行（每行长度不足时按空串补齐）。
    pub rows: Vec<Vec<String>>,
    /// 末尾说明行（如「总数：12」），为空则不写。
    pub notes: Vec<String>,
}

impl Sheet {
    /// 只带标题与表头的空表（下载导入模板时用）。
    pub fn new(title: impl Into<String>, head: Vec<String>) -> Self {
        Self {
            title: title.into(),
            head,
            rows: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// 追加一行（自动转字符串，方便 `iterator.collect` 场景）。
    pub fn push_row<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(cells.into_iter().map(Into::into).collect());
    }
}

/// 生成 xlsx 字节（内存版；要直接落盘用 [`build_to_file`]）。
pub fn build(sheet: &Sheet) -> Result<Vec<u8>, String> {
    let cols = sheet.head.len().max(1);
    // 标题行 / 表头行占用前两行（无标题时表头在第 1 行）
    let has_title = !sheet.title.trim().is_empty();
    let head_row: u32 = if has_title { 1 } else { 0 };

    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();

    let title_fmt = Format::new()
        .set_bold()
        .set_font_size(14)
        .set_align(FormatAlign::Center)
        .set_align(FormatAlign::VerticalCenter);
    let head_fmt = Format::new()
        .set_bold()
        .set_align(FormatAlign::Center)
        .set_background_color(Color::RGB(0xF5F7FA))
        .set_border(FormatBorder::Thin);
    let note_fmt = Format::new().set_font_color(Color::RGB(0x909399));

    if has_title {
        let last_col: u16 = (cols - 1) as u16;
        ws.merge_range(0, 0, 0, last_col, &sheet.title, &title_fmt)
            .map_err(|e| format!("写入标题失败：{e}"))?;
        ws.set_row_height(0, 26)
            .map_err(|e| format!("设置标题行高失败：{e}"))?;
    }
    for (c, text) in sheet.head.iter().enumerate() {
        ws.write_string_with_format(head_row, c as u16, text, &head_fmt)
            .map_err(|e| format!("写入表头失败：{e}"))?;
    }

    let data_start = head_row + 1;
    for (r, row) in sheet.rows.iter().enumerate() {
        for c in 0..cols {
            let text = row.get(c).map(String::as_str).unwrap_or("");
            if text.is_empty() {
                continue;
            }
            ws.write_string_with_format(data_start + r as u32, c as u16, text, &Format::new())
                .map_err(|e| format!("写入数据失败：{e}"))?;
        }
    }
    let mut next_row = data_start + sheet.rows.len() as u32;
    for note in sheet.notes.iter().filter(|n| !n.trim().is_empty()) {
        ws.write_string_with_format(next_row, 0, note, &note_fmt)
            .map_err(|e| format!("写入统计行失败：{e}"))?;
        next_row += 1;
    }

    // 列宽：按「表头 + 数据」的最大显示宽度估算（中文按 2 个字符宽）
    for c in 0..cols {
        let mut width = display_width(sheet.head.get(c).map(String::as_str).unwrap_or(""));
        for row in &sheet.rows {
            let w = display_width(row.get(c).map(String::as_str).unwrap_or(""));
            if w > width {
                width = w;
            }
        }
        let width = width.clamp(8, 60) as f64 + 2.0;
        ws.set_column_width(c as u16, width)
            .map_err(|e| format!("设置列宽失败：{e}"))?;
    }
    ws.set_freeze_panes(data_start, 0)
        .map_err(|e| format!("冻结表头失败：{e}"))?;

    wb.save_to_buffer()
        .map_err(|e| format!("生成 Excel 失败：{e}"))
}

/// 生成 xlsx 并写盘（目录不存在时自动创建）。
pub fn build_to_file(sheet: &Sheet, path: impl AsRef<std::path::Path>) -> Result<(), String> {
    let path = path.as_ref();
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建目录失败（{}）：{e}", dir.display()))?;
        }
    }
    let bytes = build(sheet)?;
    std::fs::write(path, bytes).map_err(|e| format!("写入 Excel（{}）失败：{e}", path.display()))
}

/// 文本显示宽度（CJK 与全角按 2 计）。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| if (c as u32) > 0x2E80 { 2 } else { 1 })
        .sum()
}

/// 读取第一个工作表的全部单元格（`Vec<行>`，单元格一律转字符串）。
///
/// 空的**整行**会被剔除（Excel 常在末尾留下格式化的空行），行内的空单元格保留为 `""`
/// 以维持列位置。仅支持 `.xlsx`（calamine 亦可读 xls/ods，但本项目对外只承诺 xlsx）。
pub fn read(bytes: &[u8]) -> Result<Vec<Vec<String>>, String> {
    if bytes.is_empty() {
        return Err("文件内容为空".to_string());
    }
    let mut wb = open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
        .map_err(|e| format!("打开 Excel 失败（仅支持 .xlsx）：{e}"))?;
    let names = wb.sheet_names().to_vec();
    let Some(first) = names.first().cloned() else {
        return Err("Excel 里没有任何工作表".to_string());
    };
    let range = wb
        .worksheet_range(&first)
        .map_err(|e| format!("读取工作表「{first}」失败：{e}"))?;

    let mut out: Vec<Vec<String>> = Vec::new();
    for row in range.rows() {
        let cells: Vec<String> = row.iter().map(cell_text).collect();
        if cells.iter().all(|c| c.is_empty()) {
            continue;
        }
        out.push(cells);
    }
    Ok(out)
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            // 4001.0 → "4001"（编号/数量在 Excel 里常被存成浮点）
            if (f.trunc() - f).abs() < f64::EPSILON {
                format!("{f:.0}")
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default(),
        Data::DateTimeIso(s) => s.trim().to_string(),
        Data::DurationIso(s) => s.trim().to_string(),
        Data::Error(e) => format!("{e:?}"),
    }
}

/// 在表头里找列：返回第一个命中 `names` 中任一名称的下标。
///
/// 匹配前统一去空白（含全角空格），并忽略大小写（用户可能写「人员名称」也可能写 `Name`）。
pub fn column_index(head: &[String], names: &[&str]) -> Option<usize> {
    for (i, cell) in head.iter().enumerate() {
        let cell = normalize(cell);
        if names.iter().any(|n| normalize(n) == cell) {
            return Some(i);
        }
    }
    None
}

fn normalize(text: &str) -> String {
    text.replace(['\u{3000}', ' '], "").to_lowercase()
}

/// 取行内第 `idx` 列（`None` 或越界给空串）。
pub fn cell(row: &[String], idx: Option<usize>) -> String {
    idx.and_then(|i| row.get(i).cloned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_then_read_roundtrip() {
        let mut sheet = Sheet::new("人员", vec!["人员名称".into(), "人员编号".into()]);
        sheet.rows.push(vec!["李章炎".into(), "RY0003".into()]);
        sheet.rows.push(vec!["张小娟".into(), "RY0001".into()]);
        sheet.notes.push("总数：2".into());

        let bytes = build(&sheet).expect("build");
        let rows = read(&bytes).expect("read");
        // 第 1 行标题、第 2 行表头、其后数据、最后统计
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], "人员");
        assert_eq!(rows[1][0], "人员名称");
        assert_eq!(rows[1][1], "人员编号");
        assert_eq!(rows[2], vec!["李章炎".to_string(), "RY0003".to_string()]);
        assert_eq!(rows[4][0], "总数：2");
    }

    #[test]
    fn sheet_without_title_starts_with_head() {
        let mut sheet = Sheet::new("", vec!["A".into(), "B".into()]);
        sheet.push_row(["1", "2"]);
        let rows = read(&build(&sheet).expect("build")).expect("read");
        assert_eq!(rows[0], vec!["A".to_string(), "B".to_string()]);
        assert_eq!(rows[1], vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn column_match_ignores_space_and_case() {
        let head = vec![
            " 人员名称 ".to_string(),
            "人员编号".to_string(),
            "NAME".to_string(),
        ];
        assert_eq!(column_index(&head, &["人员名称", "name"]), Some(0));
        assert_eq!(column_index(&head, &["人员编号"]), Some(1));
        assert_eq!(column_index(&head, &["name"]), Some(2));
        assert_eq!(column_index(&head, &["不存在"]), None);
    }

    #[test]
    fn cell_reads_by_index_safely() {
        let row = vec!["a".to_string()];
        assert_eq!(cell(&row, Some(0)), "a");
        assert_eq!(cell(&row, Some(3)), "");
        assert_eq!(cell(&row, None), "");
    }

    #[test]
    fn empty_bytes_rejected() {
        assert!(read(&[]).is_err());
    }

    #[test]
    fn float_integers_render_without_decimal() {
        assert_eq!(cell_text(&Data::Float(4001.0)), "4001");
        assert_eq!(cell_text(&Data::Float(1.5)), "1.5");
        assert_eq!(cell_text(&Data::Int(7)), "7");
        assert_eq!(cell_text(&Data::Bool(true)), "TRUE");
    }
}
