//! 网卡累计流量采集（跨平台）。
//!
//! - Linux：`/proc/net/dev`（单文件汇总，容器/OpenVZ 内同样可用）；
//! - Windows：`GetIfTable2`（跳过软件回环 `IfType=24`）；
//! - 其它平台：返回 `None`。

/// 解析 `/proc/net/dev` 文本为逐接口 `(名称, 接收字节, 发送字节)`；跳过表头与回环 `lo`。
pub fn parse_net_dev_entries(text: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name == "lo" {
            continue;
        }
        let cols: Vec<&str> = rest.split_whitespace().collect();
        if cols.len() < 9 {
            continue;
        }
        out.push((
            name.to_string(),
            cols[0].parse::<u64>().unwrap_or(0),
            cols[8].parse::<u64>().unwrap_or(0),
        ));
    }
    out
}

/// 汇总 `/proc/net/dev` 非回环接口的 `(接收字节, 发送字节)`。
pub fn parse_net_dev(text: &str) -> (u64, u64) {
    parse_net_dev_entries(text)
        .into_iter()
        .fold((0, 0), |(rx, tx), (_, r, t)| (rx + r, tx + t))
}

/// 本机网络累计总流量 `(接收字节, 发送字节, 接口数)`；无可用接口或采集失败时返回 `None`。
pub fn net_totals() -> Option<(u64, u64, usize)> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/net/dev").ok()?;
        let entries = parse_net_dev_entries(&text);
        if entries.is_empty() {
            return None;
        }
        let (rx, tx) = entries
            .iter()
            .fold((0u64, 0u64), |(rx, tx), (_, r, t)| (rx + r, tx + t));
        Some((rx, tx, entries.len()))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
        };

        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let ret = unsafe { GetIfTable2(&mut table) };
        if ret != 0 || table.is_null() {
            return None;
        }
        let mut rx_total = 0u64;
        let mut tx_total = 0u64;
        let mut count = 0usize;
        unsafe {
            let t = &*table;
            for i in 0..t.NumEntries as usize {
                let row = &*t.Table.as_ptr().add(i);
                // 过滤软件回环（IfType=24）
                if row.Type == 24 {
                    continue;
                }
                rx_total += row.InOctets;
                tx_total += row.OutOctets;
                count += 1;
            }
            FreeMibTable(table as *const _);
        }
        if count == 0 {
            None
        } else {
            Some((rx_total, tx_total, count))
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:   1000       1    0    0    0     0          0         0     2000       1    0    0    0     0       0          0\n  eth0:    500       5    0    0    0     0          0         0      900       5    0    0    0     0       0          0\n  eth1:    530       5    0    0    0     0          0         0     3140       5    0    0    0     0       0          0\n";

    #[test]
    fn parse_net_dev_skips_loopback_and_headers() {
        assert_eq!(parse_net_dev(SAMPLE), (1030, 4040));
    }

    #[test]
    fn parse_net_dev_entries_per_interface() {
        let entries = parse_net_dev_entries(SAMPLE);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "eth0");
        assert_eq!(entries[0].1, 500);
        assert_eq!(entries[1].2, 3140);
    }
}
