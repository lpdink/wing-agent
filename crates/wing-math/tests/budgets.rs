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
/// 花括号嵌套上限（`guard::MAX_BRACE_DEPTH`；review r3 把它与解析器递归闸对齐到 47）。
const MAX_BRACE_DEPTH: usize = 47;
/// 解析器递归深度上限（`api::MAX_PARSE_DEPTH`；一层结构 ≈ 2 个深度单位）。
const MAX_PARSE_DEPTH: usize = 96;
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
    // 配平且在上限之内：可渲染
    let ok = format!(
        "{}x{}",
        "{".repeat(MAX_BRACE_DEPTH - 4),
        "}".repeat(MAX_BRACE_DEPTH - 4)
    );
    assert!(
        render_block(&ok).is_some(),
        "depth under the limit must render"
    );

    // 只有左括号：不配平（会丢内容）→ 拒绝
    assert!(render_block(&"{".repeat(MAX_BRACE_DEPTH + 1)).is_none());

    // 配平但超深：撞深度闸（花括号闸与解析器递归闸两道）
    let too_deep = format!(
        "{}x{}",
        "{".repeat(MAX_BRACE_DEPTH + 4),
        "}".repeat(MAX_BRACE_DEPTH + 4)
    );
    assert!(render_block(&too_deep).is_none());

    // 嵌套分式：受解析器递归深度闸约束（标定见 `api.rs::MAX_PARSE_DEPTH`）
    let nested = |levels: usize| {
        let mut src = "x".to_string();
        for _ in 0..levels {
            src = format!(r"\frac{{{}}}{{1}}", src);
        }
        src
    };
    assert!(render_block(&nested(20)).is_some());
    assert!(render_block(&nested(50)).is_none());

    // 花括号闸与解析器闸的边界必须对得上：47 层过、48 层拒（实测深度 96 / 98）
    assert!(render_block(&format!("{}x{}", "{".repeat(47), "}".repeat(47))).is_some());
    assert!(render_block(&format!("{}x{}", "{".repeat(48), "}".repeat(48))).is_none());
    // 只有左括号（不配平）同样是拒绝
    assert!(render_block(&"{".repeat(MAX_PARSE_DEPTH)).is_none());

    // 极限附近不得栈溢出（review r2 的 S1）
    for depth in [8, 32, 63, 64, 200] {
        let src = format!("{}x{}", "{".repeat(depth), "}".repeat(depth));
        let _ = render_block(&src);
    }
}

#[test]
fn brace_free_chains_hit_the_depth_gate_before_layout() {
    // review r2 的 S1：`\left(` / `\frac ` / `\sqrt ` / `\hat ` 这类**不带花括号**的
    // 链式嵌套没有花括号信号，必须由解析器的递归深度闸拦下来，而且要在排版之前。
    let chains: &[(&str, String, String)] = &[
        ("\\left(", "\\left(".repeat(250), String::new()),
        ("\\frac ", "\\frac ".repeat(250), String::new()),
        ("\\sqrt ", "\\sqrt ".repeat(300), "x".to_string()),
        ("\\hat ", "\\hat ".repeat(400), String::new()),
        ("\\mathbb ", "\\mathbb ".repeat(400), String::new()),
        ("\\overline ", "\\overline ".repeat(400), String::new()),
        // 接近 8192 字符上限的最坏形态
        ("\\frac ", "\\frac ".repeat(1364), String::new()),
    ];
    for (label, head, tail) in chains {
        let src = format!("{head}{tail}");
        assert!(src.chars().count() <= MAX_SOURCE_CHARS, "{label}");
        let start = Instant::now();
        assert!(render_block(&src).is_none(), "{label} should be rejected");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "{label}: rejecting took {elapsed:?}（应当远在排版之前）"
        );
    }

    // 预算之内的链照常可用
    assert!(render_block(&("\\sqrt ".repeat(30) + "x")).is_some());
    assert!(render_block(&("\\frac ".repeat(30) + "x y")).is_some());
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
    // review r1 的 S2 / r2 的 S1 / r3 的 S1：debug 构建 + 小线程栈下，深嵌套会把
    // 解析器与排版递归到 stack overflow（不可捕获的 abort）。深度闸必须在**解析过程中**
    // 就置位（不用等排版），让"上限附近 + 远超上限"的输入在小栈里也能跑完。
    //
    // 栈尺寸怎么定的（debug、子进程二分实测最小可用栈，见 `api::MAX_PARSE_DEPTH`）：
    //
    // | 输入 | 最小可用线程栈 |
    // |---|---|
    // | `\frac ` / `\sqrt ` 链 ×1364（最坏闸门路径） | 689 KiB |
    // | `\hat ` / `\mathbb ` 链 ×1364 | 625 KiB |
    // | 嵌套分式 ×40（真的渲染） | 353 KiB |
    //
    // 取 1.5 MiB ≈ 2.2x 余量：既是有效的回归哨兵（栈需求翻倍就会红），又不会因为
    // rustc 版本的帧大小微调就抖动。1 MiB 实测也过（1.49x），512 KiB 不行。
    let handle = std::thread::Builder::new()
        .stack_size(1536 * 1024)
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

            // brace-free 链（r2 的 S1 主角）
            for depth in [32usize, 64, 129, 250, 1000] {
                let _ = render_block(&"\\left(".repeat(depth));
                let _ = render_block(&("\\frac ".repeat(depth) + "x y"));
                let _ = render_block(&("\\sqrt ".repeat(depth) + "x"));
                let _ = render_block(&("\\hat ".repeat(depth) + "x"));
                let _ = render_block(&("\\mathbb ".repeat(depth) + "x"));
                let _ = render_block(&("\\overline ".repeat(depth) + "x"));
                let _ = render_block(&("\\text ".repeat(depth) + "x"));
            }
        })
        .expect("spawn");
    handle.join().expect("small-stack rendering must not abort");
}
