//! 质量样例打印 + 性能基线。
//!
//! ```bash
//! cargo run -q -p wing-math --example render_samples          # debug 基线
//! cargo run -q -p wing-math --release --example render_samples # release 基线
//! ```
//!
//! 输出分三段：行内样例、显示样例（含渲染出的字符网格）、预算/降级样例。
//! 结尾打印 10~20 条公式各渲染一次的总耗时（reviewer 与本步骤的性能基线都用这个数）。
//!
//! 这个 example 是本 crate 的**人肉审阅入口**：不想跑测试也能一眼看到渲染质量。

use std::time::Instant;

use wing_math::{render_display, render_inline};

/// 步骤任务书点名的显示公式样例。
const DISPLAY_SAMPLES: &[&str] = &[
    r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
    r"\int_0^\infty e^{-x^2} dx = \frac{\sqrt{\pi}}{2}",
    r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}",
    r"\mathcal{L}(\theta) = \frac{1}{N}\sum_{i=1}^{N} \log p(x_i|\theta)",
    r"\begin{align} a &= b + c \\ d &= e \end{align}",
    r"f(x) = \begin{cases} x^2 & x>0 \\ 0 & x\le 0 \end{cases}",
    r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}^{-1}",
    r"\nabla \cdot \vec{E} = \frac{\rho}{\epsilon_0}",
    r"\hat{H}\psi = E\psi",
    r"\bar{x} \pm \sigma, \quad p < 0.05",
    // 额外补充（覆盖上游缺陷的修复路径）
    r"\left\{ \frac{a}{b} \right\}",
    r"\sqrt[3]{x}",
    r"\left\{ \begin{aligned} a &= b \\ c &= d \end{aligned} \right.",
    r"\begin{array}{lcr} a & b & c \\ dd & e & f \end{array}",
    r"\binom{n}{k} = \frac{n!}{k!(n-k)!}",
    r"\lim_{n \to \infty} \frac{1}{n} = 0",
    r"\left( \frac{a}{b} \right)^2",
    r"\dfrac{\partial f}{\partial x} \operatorname{softmax}(z)",
];

/// 行内样例。
const INLINE_SAMPLES: &[&str] = &[
    r"x^2 + y^2 = z^2",
    r"E = mc^2",
    r"\alpha + \beta = \gamma",
    r"\mathbb{R}^n",
    r"p < 0.05",
    r"\left| x \right|",
];

/// 应当降级为源码字面量的样例（`render_display` 返回 `None`）。
const DEGRADED_SAMPLES: &[(&str, &str)] = &[
    (r"\ce{2H2O}", "mhchem 不支持"),
    (r"a \\ b", "**任意深度**的行分隔符都会被上游静默截断"),
    (r"a & b", "**任意深度**的列分隔符都会被上游静默截断"),
    (r"\left( x + y", "定界符不成对"),
    (r"\begin{unknown} x \end{unknown}", "未知环境"),
    (r"\begin{align} a &= b", "环境没有闭合"),
];

/// 行内不可用（多行块塞不进行内）的样例。
const INLINE_DEGRADED_SAMPLES: &[(&str, &str)] = &[
    (r"\frac{a}{b}", "分式是 3 行块"),
    (r"\sum_{i=1}^{n} i", "求和带上下限是多行块"),
    (r"\hat{x}", "accent 是 2 行块"),
    (r"\ce{2H2O}", "未知命令"),
];

/// review r1 修复项一览（供复审直接对照）。
const REVIEW_FIXES: &[(&str, &str)] = &[
    // B1：单元格 / 前后缀渲染失败 → 整条降级（不允许空格占位）
    (
        r"\begin{align} a &= \ce{2H2O} \\ c &= d \end{align}",
        "B1 单元格失败",
    ),
    (
        r"\ce{2H2O} \begin{aligned} a &= b \end{aligned}",
        "B1 前缀失败",
    ),
    // B2/S4：\binom 不再 panic，形状正确
    (r"\binom{n}{k}", "B2/S4 形状"),
    (r"\binom{}{b}", "B2 空参数"),
    // B3：控制字符被归零
    ("x = \\frac{-b \\pm \\sqrt{b^2-4ac}}\n{2a}", "B3 换行折行"),
    ("a\nb", "B3 行内换行"),
    // S1：\hat 的字形不再被当成泄漏
    (r"\hat{ab}", "S1 双字符 hat"),
    (r"\hat{abc}", "S1 三字符 hat"),
    // S3：命名算子与运算符间距
    (r"\log p(x) = \sin x \cdot \det A", "S3 算子间距"),
    // N3：只渲染出空白 → 降级
    (r"\sqrt{}", "N3 空白结果"),
];

fn main() {
    println!("=== 行内公式（render_inline）===");
    for src in INLINE_SAMPLES {
        match render_inline(src) {
            Some(s) => println!("  {src}\n      -> {s}"),
            None => println!("  {src}\n      -> <None（降级为字面量）>"),
        }
    }

    println!();
    println!("=== 显示公式（render_display，max_width = 100）===");
    for src in DISPLAY_SAMPLES {
        println!("  ── {src}");
        match render_display(src, 100) {
            Some(m) => {
                for line in m.lines() {
                    println!("  |{line}");
                }
                println!(
                    "  (width={} height={} baseline={})",
                    m.width(),
                    m.height(),
                    m.baseline()
                );
            }
            None => println!("  <None（降级为字面量）>"),
        }
    }

    println!();
    println!("=== 预期降级（render_display 应返回 None）===");
    for (src, why) in DEGRADED_SAMPLES {
        let got = render_display(src, 100);
        let mark = if got.is_none() { "ok  " } else { "FAIL" };
        println!("  {mark} {src}  — {why}");
    }

    println!();
    println!("=== 行内预期降级（render_inline 应返回 None）===");
    for (src, why) in INLINE_DEGRADED_SAMPLES {
        let got = render_inline(src);
        let mark = if got.is_none() { "ok  " } else { "FAIL" };
        println!("  {mark} {src}  — {why}");
    }

    println!();
    println!("=== review r1 修复项（B1/B2/B3/S1/S3/S4 + N3）===");
    for (src, why) in REVIEW_FIXES {
        println!("  ── {why}: {src}");
        match render_display(src, 100) {
            Some(m) => {
                for line in m.lines() {
                    println!("  |{line}");
                }
            }
            None => println!("  <None（降级为源码字面量）>"),
        }
    }

    println!();
    println!("=== 性能基线 ===");
    let total = DISPLAY_SAMPLES.len() + INLINE_SAMPLES.len();

    let start = Instant::now();
    for _ in 0..10 {
        for src in DISPLAY_SAMPLES {
            let _ = render_display(src, 100);
        }
        for src in INLINE_SAMPLES {
            let _ = render_inline(src);
        }
    }
    let elapsed = start.elapsed();
    println!(
        "  {total} 条公式 × 10 轮：{:?}（单轮均值 {:?}）",
        elapsed,
        elapsed / 10
    );

    // 单条最贵公式的单独计时（嵌套环境 + 定界符拉伸）
    let heaviest = r"\left\{ \begin{aligned} a &= b \\ c &= d \end{aligned} \right.";
    let start = Instant::now();
    for _ in 0..100 {
        let _ = render_display(heaviest, 100);
    }
    println!("  最重样例 × 100：{:?}", start.elapsed());

    println!();
    println!("=== 引擎拒绝判据（都会返 None 的输入类别）===");
    println!(
        "  空输入 / 任意深度的游离 `&` / `\\` / `cases` 行超 2 列 / 环境或定界符不配对 / 未知命令 / 超宽 / 预算超限"
    );
    check_contracts();

    fn check_contracts() {
        let cases: &[(&str, bool)] = &[
            ("", false),
            (r"x^2", true),
            (r"\ce{2H2O}", false),
            (r"a \\ b", false),
            (r"\begin{align} a &= b \\ c &= d \end{align}", true),
            (r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", true),
        ];
        let mut bad = 0;
        for (src, expect_some) in cases {
            let got = render_display(src, 100).is_some();
            if got != *expect_some {
                bad += 1;
                println!("  FAIL {src:?}: expected some={expect_some}, got {got}");
            }
        }
        if bad == 0 {
            println!("  契约自检：全部通过（{} 例）", cases.len());
        }
    }
}
