//! 网格拼接辅助（本项目自研，非上游代码）。
//!
//! 上游 `RenderedBlock::beside` 是二元操作，`layout_seq` 对 n 个子节点反复调用它时，
//! 第 k 步都要复制"前 k 个子块拼出来的整个网格"，总代价 O(n²·h)。实测
//! 一条 2000 项的单行公式在 release 下要 282 ms。
//!
//! 这里给出语义**完全等价**的一次性实现：先算总宽 / 总高 / 基线，再逐块 blit，
//! 代价 O(总面积)。`beside_all` 的正确性由 `tests/compose.rs` 与随机块的
//! 左折叠对拍保证。

use crate::grid::rendered_block::RenderedBlock;

/// 把若干块按**基线对齐**横向拼成一个块。
///
/// 与 `blocks.iter().fold(RenderedBlock::empty(), |acc, b| acc.beside(b))` 结果逐格相同：
/// 空块被忽略（`beside` 对空块是恒等）；合并后的基线取各块基线的最大值；每块的行放在
/// `final_baseline - block.baseline()` 处；不足的行补 `block.width()` 个空格。
pub(crate) fn beside_all(blocks: &[RenderedBlock]) -> RenderedBlock {
    let live: Vec<&RenderedBlock> = blocks.iter().filter(|b| !b.is_empty()).collect();
    match live.len() {
        0 => return RenderedBlock::empty(),
        1 => return live[0].clone(),
        _ => {}
    }

    let baseline = live.iter().map(|b| b.baseline()).max().unwrap_or(0);
    let below = live
        .iter()
        .map(|b| b.height().saturating_sub(b.baseline() + 1))
        .max()
        .unwrap_or(0);
    let height = baseline + 1 + below;

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(height);
    for r in 0..height {
        let mut row: Vec<String> = Vec::new();
        for block in &live {
            let top_pad = baseline - block.baseline();
            match r.checked_sub(top_pad) {
                Some(idx) if idx < block.height() => row.extend(block.cells()[idx].iter().cloned()),
                _ => row.extend(std::iter::repeat_n(" ".to_string(), block.width())),
            }
        }
        rows.push(row);
    }
    RenderedBlock::new(rows, baseline)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 左折叠参考实现（上游 `layout_seq` 原来的写法）。
    fn fold_beside(blocks: &[RenderedBlock]) -> RenderedBlock {
        blocks
            .iter()
            .fold(RenderedBlock::empty(), |acc, b| acc.beside(b))
    }

    fn block(rows: &[&str], baseline: usize) -> RenderedBlock {
        let cells = rows
            .iter()
            .map(|r| r.chars().map(|c| c.to_string()).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        RenderedBlock::new(cells, baseline)
    }

    fn assert_same(blocks: &[RenderedBlock]) {
        let a = beside_all(blocks);
        let b = fold_beside(blocks);
        assert_eq!(a.width(), b.width(), "width mismatch");
        assert_eq!(a.height(), b.height(), "height mismatch");
        assert_eq!(a.baseline(), b.baseline(), "baseline mismatch");
        assert_eq!(a.cells(), b.cells(), "cells mismatch");
    }

    #[test]
    fn matches_fold_for_mixed_baselines() {
        assert_same(&[
            block(&["ab", "cd"], 0),
            block(&["x"], 0),
            block(&[" 1 ", "───", " 2 "], 1),
            block(&["q", "r", "s", "t"], 2),
        ]);
    }

    #[test]
    fn matches_fold_with_empty_blocks() {
        assert_same(&[RenderedBlock::empty(), block(&["x"], 0)]);
        assert_same(&[block(&["x"], 0), RenderedBlock::empty()]);
        assert_same(&[RenderedBlock::empty(), RenderedBlock::empty()]);
    }

    #[test]
    fn matches_fold_on_single_block() {
        assert_same(&[block(&["ab", "cd"], 1)]);
    }

    #[test]
    fn matches_fold_on_empty_slice() {
        assert_same(&[]);
    }

    #[test]
    fn matches_fold_on_pseudo_random_blocks() {
        // 确定性伪随机：不同高度 / 基线 / 内容混排
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let mut next = |m: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % m
        };
        for _ in 0..200 {
            let count = 1 + next(6) as usize;
            let mut blocks = Vec::new();
            for _ in 0..count {
                let h = 1 + next(4) as usize;
                let w = 1 + next(4) as usize;
                let rows: Vec<Vec<String>> = (0..h)
                    .map(|r| {
                        (0..w)
                            .map(|c| {
                                if (r + c) % 3 == 0 {
                                    " ".to_string()
                                } else {
                                    ((b'a' + next(26) as u8) as char).to_string()
                                }
                            })
                            .collect()
                    })
                    .collect();
                let baseline = next(h as u64) as usize;
                blocks.push(RenderedBlock::new(rows, baseline));
            }
            assert_same(&blocks);
        }
    }
}
