//! 步骤 02（math_glyphs）的字形 / 符号覆盖面测试。
//!
//! 三块内容，逐条钉住（删掉任何一条映射都会让这里的断言变红）：
//!
//! 1. **上标字形表**（`grid/layout.rs::to_superscript_char`）：Unicode 里可得的上标字形
//!    逐个断言（含码点注释），缺失字形仍走"堆叠"回退（行内 → 源码）；
//! 2. **符号表 + 二元算子间距**（`latex/parser.rs`）：`\top` / `\odot` 有映射，
//!    `⊙ ⊕ ⊗ ∘ ∗ ⋆` 两侧各一个空格；`\top` 在上标位置的特例（⊤ → ᵀ）；
//! 3. **三条支持边界**：不是本步骤要修的功能缺口，而是"刻意不修、现状如此"的行为
//!    （多环境公式 / 行内 accent / 行内带下标的命名算子），这里钉住现状，免得将来有人
//!    以为它们是回归。文档登记见 `docs/dev/tui-rendering.md` 第四节「已知边界」。
//!
//! 只用公开 API（`render_inline` / `render_display` / `render_block` / `latex_to_unicode`）。

use wing_math::latex::latex_to_unicode;
use wing_math::{render_block, render_display, render_inline};

/// `render_inline` 必须渲染出**这个**字面量。
fn inline(src: &str, expected: &str) {
    let got = render_inline(src).unwrap_or_else(|| panic!("{src} 应当能行内渲染，实际 None"));
    assert_eq!(got, expected, "inline render mismatch for {src}");
}

// ============================================================
// 1. 上标字形表
// ============================================================

#[test]
fn inline_superscripts_for_acceptance_list() {
    // 步骤任务书的验收清单：ML 高频写法必须落在单行上。
    inline(r"x^d", "xᵈ");
    inline(r"x^k", "xᵏ");
    inline(r"x^p", "xᵖ");
    inline(r"x^w", "xʷ");
    inline(r"W^T", "Wᵀ");
    // 验收清单里的整句形态
    inline(r"x^d x^k x^p x^w W^T", "xᵈ xᵏ xᵖ xʷ Wᵀ");
    inline(r"w^\top x", "wᵀ x");
    inline(r"w^\top", "wᵀ");
    // 花括号形态与不带花括号等价
    inline(r"x^{d}", "xᵈ");
    inline(r"W^{T}", "Wᵀ");
}

/// 上标字形表逐条（小写 25 个 + 大写 18 个），码点写在第三个字段里。
#[test]
fn inline_superscripts_cover_every_available_glyph() {
    // 小写 a–z：Unicode 里有上标字形的 25 个（唯一缺 `q`，见下一个用例）。
    let lowercase = [
        ("a", "ᵃ", "U+1D43 MODIFIER LETTER SMALL A"),
        ("b", "ᵇ", "U+1D47 MODIFIER LETTER SMALL B"),
        ("c", "ᶜ", "U+1D9C MODIFIER LETTER SMALL C"),
        ("d", "ᵈ", "U+1D48 MODIFIER LETTER SMALL D"),
        ("e", "ᵉ", "U+1D49 MODIFIER LETTER SMALL E"),
        ("f", "ᶠ", "U+1DA0 MODIFIER LETTER SMALL F"),
        ("g", "ᵍ", "U+1D4D MODIFIER LETTER SMALL G"),
        ("h", "ʰ", "U+02B0 MODIFIER LETTER SMALL H"),
        ("i", "ⁱ", "U+2071 SUPERSCRIPT LATIN SMALL LETTER I"),
        ("j", "ʲ", "U+02B2 MODIFIER LETTER SMALL J"),
        ("k", "ᵏ", "U+1D4F MODIFIER LETTER SMALL K"),
        ("l", "ˡ", "U+02E1 MODIFIER LETTER SMALL L"),
        ("m", "ᵐ", "U+1D50 MODIFIER LETTER SMALL M"),
        ("n", "ⁿ", "U+207F SUPERSCRIPT LATIN SMALL LETTER N"),
        ("o", "ᵒ", "U+1D52 MODIFIER LETTER SMALL O"),
        ("p", "ᵖ", "U+1D56 MODIFIER LETTER SMALL P"),
        ("r", "ʳ", "U+02B3 MODIFIER LETTER SMALL R"),
        ("s", "ˢ", "U+02E2 MODIFIER LETTER SMALL S"),
        ("t", "ᵗ", "U+1D57 MODIFIER LETTER SMALL T"),
        ("u", "ᵘ", "U+1D58 MODIFIER LETTER SMALL U"),
        ("v", "ᵛ", "U+1D5B MODIFIER LETTER SMALL V"),
        ("w", "ʷ", "U+02B7 MODIFIER LETTER SMALL W"),
        ("x", "ˣ", "U+02E3 MODIFIER LETTER SMALL X"),
        ("y", "ʸ", "U+02B8 MODIFIER LETTER SMALL Y"),
        ("z", "ᶻ", "U+1DBB MODIFIER LETTER SMALL Z"),
    ];
    // 大写：Unicode 里有上标字形的 18 个（缺 C F Q S X Y Z —— 见下一个用例）。
    let uppercase = [
        ("A", "ᴬ", "U+1D2C MODIFIER LETTER CAPITAL A"),
        ("B", "ᴮ", "U+1D2E MODIFIER LETTER CAPITAL B"),
        ("D", "ᴰ", "U+1D30 MODIFIER LETTER CAPITAL D"),
        ("E", "ᴱ", "U+1D31 MODIFIER LETTER CAPITAL E"),
        ("G", "ᴳ", "U+1D33 MODIFIER LETTER CAPITAL G"),
        ("H", "ᴴ", "U+1D34 MODIFIER LETTER CAPITAL H"),
        ("I", "ᴵ", "U+1D35 MODIFIER LETTER CAPITAL I"),
        ("J", "ᴶ", "U+1D36 MODIFIER LETTER CAPITAL J"),
        ("K", "ᴷ", "U+1D37 MODIFIER LETTER CAPITAL K"),
        ("L", "ᴸ", "U+1D38 MODIFIER LETTER CAPITAL L"),
        ("M", "ᴹ", "U+1D39 MODIFIER LETTER CAPITAL M"),
        ("N", "ᴺ", "U+1D3A MODIFIER LETTER CAPITAL N"),
        ("O", "ᴼ", "U+1D3C MODIFIER LETTER CAPITAL O"),
        ("P", "ᴾ", "U+1D3E MODIFIER LETTER CAPITAL P"),
        ("R", "ᴿ", "U+1D3F MODIFIER LETTER CAPITAL R"),
        ("T", "ᵀ", "U+1D40 MODIFIER LETTER CAPITAL T"),
        ("U", "ᵁ", "U+1D41 MODIFIER LETTER CAPITAL U"),
        ("V", "ⱽ", "U+2C7D MODIFIER LETTER CAPITAL V"),
        ("W", "ᵂ", "U+1D42 MODIFIER LETTER CAPITAL W"),
    ];

    for (base, sup, codepoint) in lowercase.iter().chain(uppercase.iter()) {
        let got = render_inline(&format!("X^{base}"))
            .unwrap_or_else(|| panic!("X^{base} 应当行内渲染（{codepoint}）"));
        assert_eq!(got, format!("X{sup}"), "字形不符：{codepoint}");
        // 字形必须是**单个字符**且占**一列**（宽度记账进网格，多列会撑坏排版）
        assert_eq!(sup.chars().count(), 1, "不是一个字符：{codepoint}");
        assert_eq!(
            unicode_width::UnicodeWidthChar::width(sup.chars().next().unwrap()),
            Some(1),
            "字形宽度不是 1 列：{codepoint}"
        );
    }
}

#[test]
fn inline_superscripts_for_digits_and_signs_are_unchanged() {
    // 上游原有的数字 / 符号映射必须一条不少。
    for (base, sup) in [
        ("0", "⁰"),
        ("1", "¹"),
        ("2", "²"),
        ("3", "³"),
        ("4", "⁴"),
        ("5", "⁵"),
        ("6", "⁶"),
        ("7", "⁷"),
        ("8", "⁸"),
        ("9", "⁹"),
    ] {
        inline(&format!("x^{base}"), &format!("x{sup}"));
    }
    inline(r"x^{+}", "x⁺");
    inline(r"x^{-}", "x⁻");
    inline(r"x^{=}", "x⁼");
    inline(r"x^{()}", "x⁽⁾");
    inline(r"x^{-1}", "x⁻¹");
    inline(r"x^{(a)}", "x⁽ᵃ⁾");
    // 多字符上标逐字映射
    inline(r"x^{dk}", "xᵈᵏ");
}

#[test]
fn missing_superscript_glyphs_still_fall_back_to_stacking() {
    // 没有上标字形的字符：上标仍走"两行堆叠" → 行内不成立（`render_inline` → None，
    // 上层显示源码）。这是**刻意保留**的语义（任务书：「缺失字形维持既有堆叠回退语义」）。
    for src in [
        r"x^q",        // 小写 q：只有 Latin Extended-F 的 U+107A5，字体基本没有
        r"x^C",        // 大写 C：Unicode 无上标字形
        r"x^F",        // 大写 F
        r"x^Q",        // 大写 Q
        r"x^S",        // 大写 S
        r"x^X",        // 大写 X
        r"x^Y",        // 大写 Y
        r"x^Z",        // 大写 Z
        r"e^{\pi}",    // 希腊字母 π：无上标字形
        r"e^{i\pi}",   // 混合：只要有一个字符没有字形就整体回退
        r"x^{\alpha}", // 拉丁命令产出的希腊字母同理
        // 既有限制（与字形表无关）：上标里的 `+` 被上游包成 `Seq[Space, Text, Space]`，
        // 而 `extract_flat_text` 不做嵌套 Seq 的扁平化（只有 `layout_seq` 会）→ 不扁平
        // → 回退。`x^{dk}` 这类直接由 Text 组成的上标不受影响。
        r"x^{i+}",
    ] {
        assert_eq!(render_inline(src), None, "{src} 应当回退（不是行内）");
        // 回退路径本身没坏：显示模式下仍然画出来（堆叠成 2 行）。
        let m = render_display(src, 40).unwrap_or_else(|| panic!("{src} 显示模式应当能渲染"));
        assert!(m.height() >= 2, "{src} 应当堆叠成多行：{:?}", m.lines());
    }
}

// ============================================================
// 2. 符号表 + 二元算子间距
// ============================================================

#[test]
fn symbol_table_additions() {
    // 本轮补进 `latex_to_unicode` 的两条。
    assert_eq!(latex_to_unicode("top").as_deref(), Some("⊤")); // U+22A4 DOWN TACK
    assert_eq!(latex_to_unicode("odot").as_deref(), Some("⊙")); // U+2299 CIRCLED DOT OPERATOR
    // 任务书点名的其余四条上游已有，作为对照一起钉住。
    assert_eq!(latex_to_unicode("oplus").as_deref(), Some("⊕")); // U+2295 CIRCLED PLUS
    assert_eq!(latex_to_unicode("ast").as_deref(), Some("∗")); // U+2217 ASTERISK OPERATOR
    assert_eq!(latex_to_unicode("star").as_deref(), Some("⋆")); // U+22C6 STAR OPERATOR
    assert_eq!(latex_to_unicode("circ").as_deref(), Some("∘")); // U+2218 RING OPERATOR
    // 未知命令仍是 None（AST 泄漏自检依赖这条）
    assert_eq!(latex_to_unicode("topx"), None);
}

#[test]
fn transpose_has_an_inline_form_via_top() {
    // `\top` 在**正文**位置是 ⊤，在**上标**位置是 ᵀ（`to_superscript_char` 的特例）。
    inline(r"\top", "⊤");
    inline(r"A^\top", "Aᵀ");
    inline(r"A^{\top}", "Aᵀ");
    // 用户直接写 Unicode ⊤ 也走同一条路
    inline(r"A^⊤", "Aᵀ");
    // 对照：`\perp`（⊥）同为 ⊤/⊥ 族、同样不是二元算子，正文位置不带间距
    inline(r"\perp", "⊥");
}

#[test]
fn binary_circled_and_star_operators_get_spacing() {
    // 二元算子两侧各一个空格（`\cdot` 先例）；改前 `a \oplus b` 渲染成 `a ⊕b`。
    //
    // 用**紧凑写法**（源里不写空格）做判据：这样断言的是"间距由符号表决定"，而不是
    // "源里的空格恰好被保留"（后者由 `symbol_command_keeps_its_trailing_space` 覆盖）。
    inline(r"a\odot b", "a ⊙ b");
    inline(r"a\oplus b", "a ⊕ b");
    inline(r"a\otimes b", "a ⊗ b");
    inline(r"a\ast b", "a ∗ b");
    inline(r"a\star b", "a ⋆ b");
    inline(r"a\circ b", "a ∘ b");
    inline(r"a\cdot b", "a · b"); // 既有先例，回归保护
    // 源里已经写了空格时，结果一致（相邻空白折叠成一个）
    inline(r"a \odot b", "a ⊙ b");
    inline(r"a \oplus b", "a ⊕ b");
    inline(r"a \ast b", "a ∗ b");
    inline(r"a \star b", "a ⋆ b");
    inline(r"a \circ b", "a ∘ b");
    // 参数位置也一致（间距判据只看符号本身）
    inline(r"a\odot(b+c)", "a ⊙ (b + c)");
    // 三项连排：每个算子两侧都有空格
    inline(r"a\odot b\oplus c", "a ⊙ b ⊕ c");
    // 被包进分式分子/分母时也不丢间距
    let m = render_display(r"\frac{a\odot b}{2}", 40).unwrap();
    assert!(
        m.to_plain_text().contains("a ⊙ b"),
        "分式里丢了间距：{:?}",
        m.lines()
    );
}

#[test]
fn symbol_command_keeps_its_trailing_space() {
    // 符号命令后的语义空格不再被吃掉：`\pi x` 与裸词 `pi x` 结果一致。
    inline(r"\pi x", "π x");
    inline(r"pi x", "π x");
    inline(r"\alpha x", "α x");
    // 上标位置不受影响：空格落在上标之后，不会污染上标参数
    inline(r"w^\top x", "wᵀ x");
    // 相邻空白仍然折叠成一个
    inline(r"\pi  x", "π x");
}

// ============================================================
// 3. 支持边界（刻意不修，钉住现状）
// ============================================================

/// 边界一：一条公式含 **≥ 2 个顶层环境** → 引擎整个拒绝（`render_block` → `None`），
/// 上层显示源码字面量。判据是 `guard::check_source` 的 `MultipleEnvironments`。
#[test]
fn boundary_multiple_environments_decline_the_whole_formula() {
    let two_envs =
        r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}^{-1} \begin{pmatrix} x \\ y \end{pmatrix}";
    assert!(render_block(two_envs).is_none(), "≥2 个环境应当整条降级");
    assert_eq!(render_inline(two_envs), None);
    assert!(render_display(two_envs, 80).is_none());
    // 边界是"≥2 个"，不是"有环境"：单个环境照旧
    let one = r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}^{-1}";
    assert!(render_block(one).is_some());
    assert!(
        render_display(one, 80)
            .unwrap()
            .to_plain_text()
            .contains("⁻¹")
    );
}

/// 边界二 / 三：行内 accent 与行内带下标的命名算子 —— 引擎能渲染（显示模式是多行网格），
/// 但布局是 2 行，行内要求单行 → `render_inline` → `None` → 上层显示源码。
#[test]
fn boundary_inline_accent_and_subscripted_operator_stay_multi_row() {
    for src in [
        r"\vec{v}",
        r"\hat{y}",
        r"\hat{H}\psi",
        r"\max_x f(x)",
        r"\min_y g(y)",
    ] {
        let block = render_block(src).unwrap_or_else(|| panic!("{src} 显示模式应当能渲染"));
        assert_eq!(block.height(), 2, "{src} 应当是 2 行布局：{block}");
        assert_eq!(render_inline(src), None, "{src} 行内应当保持源码");
    }
}
