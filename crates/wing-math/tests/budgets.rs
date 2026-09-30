//! 预算闸测试（`design.md → D8`，review r1 的 S2 后闸门前移）。
//!
//! 引擎对畸形/超大输入必须有界：源码长度、花括号嵌套深度、行/列分隔符数量、结果高度、
//! 结果面积，任一超限都返回 `None`（上层降级为字面量），绝不 OOM / 栈溢出。
//!
//! S2 之后有两层防线：
//! 1. **输入侧**（扫描阶段，不解析不排版）：源码长度、花括号深度、`\\` / `&` 数量；
//! 2. **输出侧**（排版之后兜底）：结果高度、面积、空白。
//!
//! 注：**面积闸**（宽 × 高 > 262144）在真实源码长度上限下很难触达，由
//! `guard.rs` 的单元测试 `rejects_empty_and_oversized_blocks` 直接钉住。

use std::time::{Duration, Instant};
use wing_math::{render_block, render_display};

/// 源码长度上限。
const MAX_SOURCE_CHARS: usize = 8192;
/// 花括号嵌套上限（review r1 的 S2：debug + 2 MiB 栈下 ~195 层就会栈溢出，取 64 留余量）。
const MAX_BRACE_DEPTH: usize = 64;
/// 行分隔符数量上限（每个 `\\` 至少一行）。
const MAX_ROW_SEPARATORS: usize = 256;

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

    // 极限附近不得栈溢出（review r1 的 S2：debug + 小栈下 ~195 层会 abort）
    for depth in [MAX_BRACE_DEPTH - 1, MAX_BRACE_DEPTH, MAX_BRACE_DEPTH + 1] {
        let src = format!("{}x{}", "{".repeat(depth), "}".repeat(depth));
        let _ = render_block(&src);
    }

    // 嵌套分式同属"深度"问题
    let nested = |levels: usize| {
        let mut src = "x".to_string();
        for _ in 0..levels {
            src = format!(r"\frac{{{}}}{{1}}", src);
        }
        src
    };
    assert!(render_block(&nested(MAX_BRACE_DEPTH)).is_some());
    assert!(render_block(&nested(MAX_BRACE_DEPTH + 1)).is_none());
}

#[test]
fn result_height_budget() {
    // 300 行数组：行分隔符数量先于排版触发 TooTall
    let tall = format!(
        r"\begin{{array}}{{c}} {} \end{{array}}",
        vec!["1"; 301].join(r" \\ ")
    );
    assert!(render_block(&tall).is_none());

    // 上限之内可以正常渲染
    let ok = format!(
        r"\begin{{array}}{{c}} {} \end{{array}}",
        vec!["1"; 200].join(r" \\ ")
    );
    let block = render_block(&ok).expect("200 rows must render");
    assert_eq!(block.height(), 200);
}

#[test]
fn row_and_column_separator_budgets_reject_before_laying_out() {
    // 1000 行的 align：必须在排版之前就被拒（S2：把预算闸前移）
    let many_rows = format!(
        r"\begin{{align}} {} \end{{align}}",
        vec!["a &= b"; MAX_ROW_SEPARATORS + 100].join(r" \\ ")
    );
    let start = Instant::now();
    assert!(render_block(&many_rows).is_none());
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "rejecting {MAX_ROW_SEPARATORS}+ rows took {elapsed:?} (should be pre-layout)"
    );

    // 1000 列的 array：同上
    let many_cols = format!(
        r"\begin{{array}}{{c}} {} \end{{array}}",
        vec!["1"; 1000].join(" & ")
    );
    let start = Instant::now();
    assert!(render_block(&many_cols).is_none());
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "rejecting 1000 columns took {elapsed:?} (should be pre-layout)"
    );

    // 但正常的单行公式即使 `&` 很多也不会被误伤（`&` 在花括号里不计入）
    let many_text_amp = format!(r"\text{{{}}}", "a & b ".repeat(300));
    assert!(render_block(&many_text_amp).is_some());
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

#[test]
fn long_flat_formulas_are_not_quadratic() {
    // review r1 的 S2：`layout_seq` 原来是 O(n²)（2000 项 release 282 ms）。
    // 这里给一个宽松的上界（真实值远低于它），防止二次方路径回归。
    let src = "x + ".repeat(1000) + "y"; // 2001 项，4005 字符
    let start = Instant::now();
    let block = render_block(&src).unwrap();
    let elapsed = start.elapsed();
    assert!(block.width() > 4000);
    assert!(
        elapsed < Duration::from_millis(300),
        "2000-term formula took {elapsed:?} (quadratic regression?)"
    );

    // 项数翻倍，耗时不应翻两倍以上（二次方会 ~4×）
    let src2 = "x + ".repeat(2000) + "y";
    let start = Instant::now();
    let _ = render_block(&src2);
    let doubled = start.elapsed();
    assert!(
        doubled < elapsed * 4,
        "doubling terms scaled {elapsed:?} -> {doubled:?} (quadratic?)"
    );
}

#[test]
fn max_depth_inputs_survive_a_small_stack() {
    // review r1 的 S2：debug 构建 + 2 MiB 线程栈下，嵌套 `\frac` 到 ~195 层会
    // stack overflow（不可捕获的 abort）。深度闸必须在**解析之前**挡住更深的输入，
    // 让"预算允许的最深输入"在 1 MiB 栈里也能跑完。
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            for depth in [
                MAX_BRACE_DEPTH - 1,
                MAX_BRACE_DEPTH,
                MAX_BRACE_DEPTH + 1,
                MAX_BRACE_DEPTH * 4,
                200,
            ] {
                let braces = format!("{}x{}", "{".repeat(depth), "}".repeat(depth));
                let _ = render_block(&braces);

                let mut frac = "x".to_string();
                for _ in 0..depth {
                    frac = format!(r"\frac{{{}}}{{1}}", frac);
                }
                let _ = render_block(&frac);

                let mut sqrt = "x".to_string();
                for _ in 0..depth {
                    sqrt = format!(r"\sqrt{{{}}}", sqrt);
                }
                let _ = render_block(&sqrt);
            }
        })
        .expect("spawn");
    handle.join().expect("small-stack rendering must not abort");
}
