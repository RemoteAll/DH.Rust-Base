//! zstd 字典差分补丁（生成 / 还原）——以旧文件为字典压缩新文件的最小组件。
//!
//! 用途：Agent 增量升级（Pek.RAgent 与 DHDeploy.Agent.Rust 共用）。
//! 约定：`diff_zstd` 用旧版本字节作“原文”字典压缩新版本字节；`apply_zstd` 用同一
//! 旧版本字节作字典还原出**完整**新版本。新旧版本高度相似时补丁约为完整包的 3%~10%。
//!
//! 与 C# 侧（DHDeploy.Server 的 ZstdSharp）产出的补丁互不承诺互通——同语言栈内配对使用。

/// 常用输出上限：64MB（Agent 二进制远小于此值，防异常数据撑爆内存）。
pub const MAX_OUTPUT_64MB: usize = 64 * 1024 * 1024;

/// 以 `old` 为字典压缩 `new`（生成补丁）。`level` 建议 19（离线生成、体积优先）。
/// <summary>生成 zstd 字典差分补丁</summary>
pub fn diff_zstd(old: &[u8], new: &[u8], level: i32) -> Result<Vec<u8>, String> {
    let mut compressor = zstd::bulk::Compressor::with_dictionary(level, old)
        .map_err(|e| format!("初始化压缩器失败：{e}"))?;
    compressor
        .compress(new)
        .map_err(|e| format!("补丁生成失败：{e}"))
}

/// 以 `old` 为字典还原补丁 → 完整新数据（`max_output` 为输出上限，防异常数据）。
/// <summary>应用 zstd 字典差分补丁</summary>
pub fn apply_zstd(old: &[u8], patch: &[u8], max_output: usize) -> Result<Vec<u8>, String> {
    let mut decoder = zstd::bulk::Decompressor::with_dictionary(old)
        .map_err(|e| format!("初始化解压器失败：{e}"))?;
    decoder
        .decompress(patch, max_output)
        .map_err(|e| format!("补丁解压失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造“同源小改”数据：新数据大量引用旧数据（补丁应显著小于完整数据）。
    fn sample_pair() -> (Vec<u8>, Vec<u8>) {
        let mut old = Vec::with_capacity(1024 * 1024);
        for i in 0..(1024 * 1024 / 16) {
            old.extend_from_slice(format!("{:015}\n", i).as_bytes());
        }
        let mut new = old.clone();
        // 修改中段一小块 + 尾部追加
        let mid = new.len() / 2;
        for (j, b) in new.iter_mut().enumerate().skip(mid).take(4096) {
            *b = b.wrapping_add((j as u8 & 0x0F) + 1);
        }
        new.extend_from_slice(b"tail-added-block");
        (old, new)
    }

    #[test]
    fn roundtrip_restores_target() {
        let (old, new) = sample_pair();
        let patch = diff_zstd(&old, &new, 19).unwrap();
        assert!(
            patch.len() < new.len() / 4,
            "补丁 {} 字节，目标 {} 字节（字典应显著缩小体积）",
            patch.len(),
            new.len()
        );
        let restored = apply_zstd(&old, &patch, MAX_OUTPUT_64MB).unwrap();
        assert_eq!(restored, new);
    }

    #[test]
    fn wrong_dictionary_never_yields_target() {
        let (old, new) = sample_pair();
        let patch = diff_zstd(&old, &new, 19).unwrap();
        // 用“相近但错误”的字典：不得还原出正确目标
        let mut wrong = old.clone();
        for b in wrong.iter_mut() {
            *b = b.wrapping_add(7);
        }
        match apply_zstd(&wrong, &patch, MAX_OUTPUT_64MB) {
            Ok(v) => assert_ne!(v, new, "错误字典不得还原出目标"),
            Err(_) => {} // 报错同样视为通过
        }
    }

    #[test]
    fn empty_dictionary_roundtrip() {
        // 空字典场景等价普通压缩，仍须可用
        let new = b"hello patch world".to_vec();
        let patch = diff_zstd(&[], &new, 19).unwrap();
        let restored = apply_zstd(&[], &patch, MAX_OUTPUT_64MB).unwrap();
        assert_eq!(restored, new);
    }

    #[test]
    fn output_limit_is_enforced() {
        let (old, new) = sample_pair();
        let patch = diff_zstd(&old, &new, 19).unwrap();
        let out = apply_zstd(&old, &patch, 1024);
        assert!(out.is_err(), "超出输出上限应报错");
    }
}
