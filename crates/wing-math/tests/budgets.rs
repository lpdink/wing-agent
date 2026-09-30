//! 预算闸测试（`design.md → D8`）。
//!
//! 引擎对畸形/超大输入必须有界：源码长度、花括号嵌套深度、结果高度、结果面积，
//! 任一超限都返回 `None`（上层降级为字面量），绝不 OOM / 栈溢出。
//!
//! 注：**面积闸**（宽 × 高 > 262144）在真实源码长度上限下很难触达，由
//! `guard.rs` 的单元测试 `rejects_empty_and_oversized_blocks` 直接钉住。

use std::time::{Duration, Instant};
use wing_math::{render_block, render_display};

/// 源码长度上限。
const MAX_SOURCE_CHARS: usize = 8192;
/// 花括号嵌套上限。
const MAX_BRACE_DEPTH: usize = 256;

#[test]
fn source_length_budget() {
    let ok = "x".repeat(MAX_SOURCE_CHARS);
    assert!(
        render_block(&ok).is_some(),
        "source at the limit must render"
    );

    let too_long = "x".repeat(MAX_SOURCE_CHARS + 1);
    assert!(render_block(&too_long).is_none());

    // 巨大输入必须快速拒绝（不是先排版再判断）
    let huge = format!(r"\frac{{{}}}{{{}}}", "a".repeat(50_000), "b".repeat(50_000));
    assert!(render_block(&huge).is_none());
}

#[test]
fn brace_depth_budget() {
    let deep = format!(
        "{}x{}",
        "{".repeat(MAX_BRACE_DEPTH),
        "}".repeat(MAX_BRACE_DEPTH)
    );
    assert!(
        render_block(&deep).is_some(),
        "depth at the limit must render"
    );

    let too_deep = format!(
        "{}x{}",
        "{".repeat(MAX_BRACE_DEPTH + 1),
        "}".repeat(MAX_BRACE_DEPTH + 1)
    );
    assert!(render_block(&too_deep).is_none());

    // 只有左括号也必须被挡（同样是递归深度）
    assert!(render_block(&"{".repeat(MAX_BRACE_DEPTH + 1)).is_none());
}

#[test]
fn result_height_budget() {
    let nested = |levels: usize| {
        let mut src = "x".to_string();
        for _ in 0..levels {
            src = format!(r"\frac{{{}}}{{1}}", src);
        }
        src
    };

    // 100 层嵌套分式：高度 201 ≤ 256 → 可渲染
    let ok = nested(100);
    let block = render_block(&ok).expect("100 levels must render");
    assert_eq!(block.height(), 201);

    // 130 层：高度 261 > 256 → 拒绝
    assert!(render_block(&nested(130)).is_none());
}

#[test]
fn display_width_budget_is_the_callers() {
    let src = "x + ".repeat(40) + "y";
    let m = render_display(&src, 1000).unwrap();
    assert!(m.width() > 100);
    assert!(render_display(&src, m.width()).is_some());
    assert!(render_display(&src, m.width() - 1).is_none());
}

#[test]
fn budget_boundaries_are_fast() {
    // 越界输入应当在毫秒级被拒绝（先查预算、不做无谓排版）
    let huge = "x".repeat(MAX_SOURCE_CHARS + 1000);
    let start = Instant::now();
    for _ in 0..50 {
        assert!(render_block(&huge).is_none());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "rejecting oversized input took {elapsed:?} for 50 iterations"
    );
}
