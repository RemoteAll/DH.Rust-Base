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
    ///
    /// **全部按文本写入**（Excel 里是左对齐的字符串，不能直接求和）。
    /// 需要数值参与公式/汇总时改用 [`Sheet::typed_rows`] / [`Sheet::push_cells`]。
    pub rows: Vec<Vec<String>>,
    /// 带类型的数据行：`Cell::Number` 写成**数值单元格**（Excel 里可直接 `SUM`）。
    ///
    /// **与 [`Sheet::rows`] 互斥**：`typed_rows` 非空时以它为准（`rows` 被忽略），
    /// 避免两套数据在同一个表里交错出现、列位置错乱。
    pub typed_rows: Vec<Vec<Cell>>,
    /// 末尾说明行（如「总数：12」），为空则不写。
    pub notes: Vec<String>,
}

/// 单元格值（导出用）。
///
/// 只有 [`Cell::Number`] 会写成数值单元格；其余一律按文本（保持既有行为）。
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    /// 文本（空串等同空白单元格，不写格式）。
    Text(String),
    /// 数值（金额/数量/百分比用小数即可；整数也走这里）。
    Number(f64),
    /// 空白（占位，保持列位置）。
    Blank,
}

impl Cell {
    /// 文本单元格的便捷构造。
    pub fn text(v: impl Into<String>) -> Self {
        Cell::Text(v.into())
    }

    /// 「能解析成数字就给数值单元格，否则文本」——报表导出的通用口径。
    ///
    /// 接受前导/尾随空白、`1,234.56`（千分位）、`12%`（按 0.12 写入并保留百分号格式不做）。
    /// 空串返回 [`Cell::Blank`]（避免把「—」这类占位写进数值列）。
    pub fn auto(v: impl AsRef<str>) -> Self {
        let raw = v.as_ref();
        let text = raw.trim();
        if text.is_empty() {
            return Cell::Blank;
        }
        let cleaned: String = text.chars().filter(|c| *c != ',' && *c != ' ').collect();
        let (body, percent) = match cleaned.strip_suffix('%') {
            Some(rest) => (rest, true),
            None => (cleaned.as_str(), false),
        };
        match body.parse::<f64>() {
            Ok(n) if n.is_finite() => Cell::Number(if percent { n / 100.0 } else { n }),
            _ => Cell::Text(raw.to_string()),
        }
    }
}

impl Sheet {
    /// 只带标题与表头的空表（下载导入模板时用）。
    pub fn new(title: impl Into<String>, head: Vec<String>) -> Self {
        Self {
            title: title.into(),
            head,
            rows: Vec::new(),
            typed_rows: Vec::new(),
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

    /// 追加一行**带类型**的单元格（数值列用 `Cell::Number` / `Cell::auto`）。
    ///
    /// 一旦用过本方法，[`Sheet::build`] 会以 `typed_rows` 为准、忽略 `rows`。
    pub fn push_cells<I>(&mut self, cells: I)
    where
        I: IntoIterator<Item = Cell>,
    {
        self.typed_rows.push(cells.into_iter().collect());
    }

    /// 是否走「带类型」通道。
    pub fn uses_typed(&self) -> bool {
        !self.typed_rows.is_empty()
    }

    /// 某行某列的显示文本（列宽估算用）。
    fn cell_display(&self, row: usize, col: usize) -> String {
        if self.uses_typed() {
            match self.typed_rows.get(row).and_then(|r| r.get(col)) {
                Some(Cell::Text(t)) => t.clone(),
                Some(Cell::Number(n)) => format!("{n}"),
                _ => String::new(),
            }
        } else {
            self.rows
                .get(row)
                .and_then(|r| r.get(col))
                .cloned()
                .unwrap_or_default()
        }
    }

    /// 数据行数（两种通道取其一）。
    fn data_len(&self) -> usize {
        if self.uses_typed() {
            self.typed_rows.len()
        } else {
            self.rows.len()
        }
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
    if sheet.uses_typed() {
        // 带类型通道：Number 写成数值单元格（Excel 里可直接 SUM），Text 仍按文本
        for (r, row) in sheet.typed_rows.iter().enumerate() {
            for c in 0..cols {
                let row_no = data_start + r as u32;
                match row.get(c) {
                    Some(Cell::Number(n)) => {
                        ws.write_number_with_format(row_no, c as u16, *n, &Format::new())
                            .map_err(|e| format!("写入数值单元格失败：{e}"))?;
                    }
                    Some(Cell::Text(t)) if !t.is_empty() => {
                        ws.write_string_with_format(row_no, c as u16, t, &Format::new())
                            .map_err(|e| format!("写入数据失败：{e}"))?;
                    }
                    _ => {}
                }
            }
        }
    } else {
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
    }
    let mut next_row = data_start + sheet.data_len() as u32;
    for note in sheet.notes.iter().filter(|n| !n.trim().is_empty()) {
        ws.write_string_with_format(next_row, 0, note, &note_fmt)
            .map_err(|e| format!("写入统计行失败：{e}"))?;
        next_row += 1;
    }

    // 列宽：按「表头 + 数据」的最大显示宽度估算（中文按 2 个字符宽）
    for c in 0..cols {
        let mut width = display_width(sheet.head.get(c).map(String::as_str).unwrap_or(""));
        for r in 0..sheet.data_len() {
            let w = display_width(&sheet.cell_display(r, c));
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

    #[test]
    fn typed_rows_write_numbers_as_numbers() {
        let mut sheet = Sheet::new("金额", vec!["名称".into(), "金额".into()]);
        // 文本列 + 数值列：数值必须是**数值单元格**（Excel 里能直接 SUM）
        sheet.push_cells([Cell::text("甲"), Cell::Number(123.45)]);
        sheet.push_cells([Cell::text("乙"), Cell::Number(66.55)]);
        sheet.notes.push("合计：190".into());
        assert!(sheet.uses_typed());

        let bytes = build(&sheet).expect("build");
        // 文本读回：小数位不丢
        let rows = read(&bytes).expect("read");
        assert_eq!(rows[1], vec!["名称".to_string(), "金额".to_string()]);
        assert_eq!(rows[2], vec!["甲".to_string(), "123.45".to_string()]);
        assert_eq!(rows[3], vec!["乙".to_string(), "66.55".to_string()]);

        // 关键断言：读回来的**原始类型**是数值（不是字符串）——Excel 里才能直接 SUM
        let raw = raw_types(&bytes);
        assert!(
            matches!(raw[2][1], Data::Float(_)),
            "金额列应为数值单元格：{:?}",
            raw[2]
        );
        assert!(
            matches!(raw[2][0], Data::String(_)),
            "名称列应为文本单元格：{:?}",
            raw[2]
        );
    }

    #[test]
    fn typed_rows_override_plain_rows() {
        let mut sheet = Sheet::new("", vec!["A".into()]);
        sheet.push_row(["忽略我"]);
        sheet.push_cells([Cell::Number(7.0)]);
        let rows = read(&build(&sheet).expect("build")).expect("read");
        assert_eq!(rows.len(), 2, "只应写出 typed_rows 一行数据：{rows:?}");
        assert_eq!(rows[1][0], "7");
    }

    #[test]
    fn cell_auto_parses_numbers_and_keeps_text() {
        assert_eq!(Cell::auto("1234.5"), Cell::Number(1234.5));
        assert_eq!(Cell::auto(" 1,234.50 "), Cell::Number(1234.5));
        assert_eq!(Cell::auto("12%"), Cell::Number(0.12));
        assert_eq!(Cell::auto(""), Cell::Blank);
        assert_eq!(Cell::auto("—"), Cell::Text("—".to_string()));
        assert_eq!(Cell::auto("KH0001"), Cell::Text("KH0001".to_string()));
    }

    /// 第一个工作表的原始 `Data` 网格（判断单元格是文本还是数值；`read` 会统一转字符串）。
    fn raw_types(bytes: &[u8]) -> Vec<Vec<Data>> {
        let mut wb = open_workbook_auto_from_rs(Cursor::new(bytes.to_vec())).expect("open");
        let names = wb.sheet_names().to_vec();
        let range = wb.worksheet_range(&names[0]).expect("range");
        range.rows().map(|r| r.to_vec()).collect()
    }
}
