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

/// 上标字形表逐条（小写 25 个 + 大写 19 个）：期望字形由表里的**码点**导出并断言（N3）。
#[test]
fn inline_superscripts_cover_every_available_glyph() {
    // 小写 a–z：Unicode 里有上标字形的 25 个（唯一缺 `q`，见下一个用例）。
    // (小写字母, 上标字形码点, 字符名)。期望字形由**码点**导出（`char::from_u32`），
    // 所以下面注释里的码点就是断言的一部分 —— 写错码点必红（r1 的 N3）。
    let lowercase = [
        ('a', 0x1D43, "MODIFIER LETTER SMALL A"),
        ('b', 0x1D47, "MODIFIER LETTER SMALL B"),
        ('c', 0x1D9C, "MODIFIER LETTER SMALL C"),
        ('d', 0x1D48, "MODIFIER LETTER SMALL D"),
        ('e', 0x1D49, "MODIFIER LETTER SMALL E"),
        ('f', 0x1DA0, "MODIFIER LETTER SMALL F"),
        ('g', 0x1D4D, "MODIFIER LETTER SMALL G"),
        ('h', 0x02B0, "MODIFIER LETTER SMALL H"),
        ('i', 0x2071, "SUPERSCRIPT LATIN SMALL LETTER I"),
        ('j', 0x02B2, "MODIFIER LETTER SMALL J"),
        ('k', 0x1D4F, "MODIFIER LETTER SMALL K"),
        ('l', 0x02E1, "MODIFIER LETTER SMALL L"),
        ('m', 0x1D50, "MODIFIER LETTER SMALL M"),
        ('n', 0x207F, "SUPERSCRIPT LATIN SMALL LETTER N"),
        ('o', 0x1D52, "MODIFIER LETTER SMALL O"),
        ('p', 0x1D56, "MODIFIER LETTER SMALL P"),
        ('r', 0x02B3, "MODIFIER LETTER SMALL R"),
        ('s', 0x02E2, "MODIFIER LETTER SMALL S"),
        ('t', 0x1D57, "MODIFIER LETTER SMALL T"),
        ('u', 0x1D58, "MODIFIER LETTER SMALL U"),
        ('v', 0x1D5B, "MODIFIER LETTER SMALL V"),
        ('w', 0x02B7, "MODIFIER LETTER SMALL W"),
        ('x', 0x02E3, "MODIFIER LETTER SMALL X"),
        ('y', 0x02B8, "MODIFIER LETTER SMALL Y"),
        ('z', 0x1DBB, "MODIFIER LETTER SMALL Z"),
    ];
    // 大写：Unicode 里有上标字形的 19 个。不收的是 `S X Z`（UCD 里没有常规大写修饰
    // 字形）与 `q` / `Y` / `C` / `F` / `Q`（有码位：U+107A5、U+107B2 MODIFIER LETTER
    // SMALL CAPITAL Y、U+A7F2 / U+A7F3 / U+A7F4，都在 Latin Extended-F / -D 的补遗里，
    // Unicode 14 起 —— 字体覆盖差，故不收）。理由详见 `grid/layout.rs` 的注释。
    let uppercase = [
        ('A', 0x1D2C, "MODIFIER LETTER CAPITAL A"),
        ('B', 0x1D2E, "MODIFIER LETTER CAPITAL B"),
        ('D', 0x1D30, "MODIFIER LETTER CAPITAL D"),
        ('E', 0x1D31, "MODIFIER LETTER CAPITAL E"),
        ('G', 0x1D33, "MODIFIER LETTER CAPITAL G"),
        ('H', 0x1D34, "MODIFIER LETTER CAPITAL H"),
        ('I', 0x1D35, "MODIFIER LETTER CAPITAL I"),
        ('J', 0x1D36, "MODIFIER LETTER CAPITAL J"),
        ('K', 0x1D37, "MODIFIER LETTER CAPITAL K"),
        ('L', 0x1D38, "MODIFIER LETTER CAPITAL L"),
        ('M', 0x1D39, "MODIFIER LETTER CAPITAL M"),
        ('N', 0x1D3A, "MODIFIER LETTER CAPITAL N"),
        ('O', 0x1D3C, "MODIFIER LETTER CAPITAL O"),
        ('P', 0x1D3E, "MODIFIER LETTER CAPITAL P"),
        ('R', 0x1D3F, "MODIFIER LETTER CAPITAL R"),
        ('T', 0x1D40, "MODIFIER LETTER CAPITAL T"),
        ('U', 0x1D41, "MODIFIER LETTER CAPITAL U"),
        ('V', 0x2C7D, "MODIFIER LETTER CAPITAL V"),
        ('W', 0x1D42, "MODIFIER LETTER CAPITAL W"),
    ];

    for (base, codepoint, name) in lowercase.iter().chain(uppercase.iter()) {
        let sup = char::from_u32(*codepoint)
            .unwrap_or_else(|| panic!("U+{codepoint:04X} 不是合法码点（{name}）"));
        let got = render_inline(&format!("X^{base}"))
            .unwrap_or_else(|| panic!("X^{base} 应当行内渲染（U+{codepoint:04X} {name}）"));
        assert_eq!(got, format!("X{sup}"), "字形不符：U+{codepoint:04X} {name}");
        // 字形必须占**一列**（宽度记账进网格，多列会撑坏排版）
        assert_eq!(
            unicode_width::UnicodeWidthChar::width(sup),
            Some(1),
            "字形宽度不是 1 列：U+{codepoint:04X} {name}"
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
        r"x^C",        // 大写 C：有码位 U+A7F2（Unicode 14 起）但字体覆盖差，不收（同 q）
        r"x^F",        // 大写 F：U+A7F3，同上
        r"x^Q",        // 大写 Q：U+A7F4，同上
        r"x^S",        // 大写 S：UCD 里没有常规大写修饰字形
        r"x^X",        // 大写 X：同上
        r"x^Y",        // 大写 Y：只有 U+107B2（SMALL CAPITAL Y，新版码位/覆盖差），同 q
        r"x^Z",        // 大写 Z：同 S / X
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
    // 期望字形由码点导出（N3）：ᵀ = U+1D40 MODIFIER LETTER CAPITAL T，⊤ = U+22A4 DOWN TACK。
    let sup_t = char::from_u32(0x1D40).unwrap(); // MODIFIER LETTER CAPITAL T（上标形态）
    let top = char::from_u32(0x22A4).unwrap(); // DOWN TACK（正文形态）
    inline(r"\top", &top.to_string());
    inline(r"A^\top", &format!("A{sup_t}"));
    inline(r"A^{\top}", &format!("A{sup_t}"));
    // 用户直接写 Unicode ⊤ 也走同一条路
    inline(r"A^⊤", &format!("A{sup_t}"));
    // 大括号内的尾随空格不再把上标挤出字形路径（r1 的 S1 收窄）
    inline(r"x^{\top }", &format!("x{sup_t}"));
    // 对照：`\perp`（⊥）同为 ⊤/⊥ 族、同样不是二元算子，正文位置不带间距
    inline(r"\perp", "⊥");
}

/// 现状登记（N2）：`⊤` **不是**二元算子 —— `is_spaced_operator` 里没有它，所以 `\top`
/// 自己不贡献任何间距（`a\top b` 只有源里 / 退回输入流的空格，没有两侧间距）。
///
/// 这条断言同时钉住 design D4 的决策：把 `U+22A4` 加进间距集会让下面第一行变成
/// `a ⊤ b`（变异实验可判）。
#[test]
fn transpose_symbol_is_not_a_binary_operator() {
    inline(r"a\top b", "a⊤ b"); // 无间距：只有命令后那个语义空格
    inline(r"a \top b", "a ⊤ b"); // 源里两侧都写了空格
    inline(r"a\perp b", "a⊥ b"); // ⊥ 同理（对照组）
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
    // 命令后面是另一个**命令**（同样是一个对象）时也保留
    inline(r"\pi \alpha", "π α");
    // 上标位置不受影响：空格落在上标之后，不会污染上标参数
    inline(r"w^\top x", "wᵀ x");
    // 相邻空白仍然折叠成一个
    inline(r"\pi  x", "π x");
}

/// 回归（r1 的 S1）：退回的空格**只在下一位是另一个对象时才保留**。下一位是上下标、
/// 右定界符、环境分隔符或标点时不能退 —— 否则本来正确的结果会坏掉：
/// 上标挂到空格上、大括号内尾随空格把整条公式踢回源码、`cases` 单元格多占一列。
#[test]
fn symbol_command_space_does_not_detach_scripts_or_punctuation() {
    // 上下标必须仍然挂在符号上（改前实测：`π ²` / `π ₂` / `ε ₀`）
    inline(r"\pi ^2", "π²");
    inline(r"\pi ^{2}", "π²");
    inline(r"\pi _2", "π₂");
    inline(r"\epsilon _0", "ε₀");
    // 撇号同样贴前一个对象
    inline(r"\pi ' x", "π' x");
    // 大括号内的尾随空格不再让上标掉回源码（`x^{\top }` == `x^{\top}`）
    assert_eq!(
        render_inline(r"x^{\top }"),
        render_inline(r"x^{\top}"),
        "大括号里的尾随空格不该改变结果"
    );
    // 标点紧贴前一个对象排版（不保留空格）
    inline(r"\pi , x", "π, x");
    inline(r"\pi ; x", "π; x");
    inline(r"\pi . x", "π. x");
    inline(r"\pi : x", "π: x");
    inline(r"\pi ! x", "π! x");
    inline(r"\pi ? x", "π? x");
    // 多打一个空格的形态（`\pi  ^2`）：判据要越过连续空格，否则退回的空格自己成了
    // `^` 的宿主（r2 的 N6）
    inline(r"\pi  ^2", "π ²");
    inline(r"\pi  _2", "π ₂");
    inline(r"\pi  x", "π x");
    // 输入结束时的尾随空格没有对象可分隔（渲染结果不受影响）
    inline(r"x + \pi ", "x + π");
    // 显示形态：上下标、分式线宽、环境列宽都回到"空格被吃掉"的尺寸
    let sum = render_display(r"\sum_{i=1}^{n} \pi _i", 40).unwrap();
    assert_eq!(sum.lines(), ["  n", "  ∑   πᵢ", "i = 1"]);
    let sup_sub = render_display(r"\pi _\theta ^2", 40).unwrap();
    assert_eq!(sup_sub.lines(), [" 2", "π", " θ"]);
    let frac = render_display(r"\frac{\pi }{2}", 40).unwrap();
    assert_eq!(frac.lines(), [" π", "───", " 2"]);
    // `&` 之后的分隔符形态：`cases` 单元格不 trim（见下一个用例），列宽必须是 10
    let cases = render_display(r"\begin{cases} \pi & a \\ b & c \end{cases}", 40).unwrap();
    assert_eq!(cases.width(), 10, "{:?}", cases.lines());
}

/// 回归（r2 的 S1）：`\right` / `\end` 与 `}` / `)` / `]` 同类 —— 它们之前也不能退回空格。
/// 形态选择：`\left( … \right)` 单独渲染就能判别（行尾多余空白会直接进宽度）；
/// `cases` 的两条必须用**后面还跟内容**的组合形态 —— 单独的 `\begin{cases} … \end{cases}`
/// 渲染时行尾空白被 trim，看不出差异（断言会被掩蔽）。
#[test]
fn symbol_command_space_is_not_kept_before_closing_right_or_end() {
    let delimited = render_display(r"\left( \alpha \right)", 60).unwrap();
    assert_eq!(delimited.lines(), ["( α)"]);
    assert_eq!(delimited.width(), 4);
    let delimited_then_content = render_display(r"\left( \alpha \right) x", 60).unwrap();
    assert_eq!(delimited_then_content.lines(), ["( α) x"]);
    assert_eq!(delimited_then_content.width(), 6);
    let env_then_content = render_display(r"\begin{cases} \pi \end{cases} y", 60).unwrap();
    assert_eq!(env_then_content.lines(), ["{ π y"]);
    assert_eq!(env_then_content.width(), 5);
    let env_cond_then_content =
        render_display(r"\begin{cases} a & \beta \end{cases} y", 60).unwrap();
    assert_eq!(env_cond_then_content.lines(), ["{ a   if  β y"]);
    assert_eq!(env_cond_then_content.width(), 13);
    // 同前缀的命令不受影响：`\rightarrow` 不是 `\right`（`is_command_at` 与上游
    // `lookahead_command` 同判据 —— 命令名后不接字母）。这两行钉的是**渲染结果**
    // （它的间距来自算子表，所以不构成前缀判据的判别实验：前缀写法也测不出差异）。
    inline(r"\pi \rightarrow x", "π → x");
    let arrow_in_delimiters =
        render_display(r"\left( \alpha \rightarrow \beta \right)", 60).unwrap();
    assert_eq!(arrow_in_delimiters.lines(), ["( α → β)"]);
    assert_eq!(arrow_in_delimiters.width(), 8);
}

/// 回归（r2 的 N1）：行分隔符 `\\` 的分支也要有断言 —— 走的是**组合形态**
/// （`cases` 里 `\pi \\ b`，后面还跟 `+ x`），单独渲染时行尾空白会被 trim 掉、测不出差异。
#[test]
fn symbol_command_space_is_not_kept_before_a_row_separator() {
    let block = render_display(r"\begin{cases} \pi \\ b \end{cases} + x", 60).unwrap();
    assert_eq!(block.lines(), ["⎧ π", "⎩b  + x"]);
    assert_eq!(block.width(), 7);
}

/// 现状登记（N4）：`cases` 的值单元格**不 trim** 首尾空白 —— `matrix` / `array` /
/// `aligned` 都走 `trim_node`，`layout_cases` 不走，所以源里 `a` 后面的空格会留下来
/// （`{ a   if  b` 里 `if` 前是 3 个空格），矩阵的同形输入已被 trim。
/// 见 `docs/dev/tui-rendering.md` 第四节「已知边界」。
#[test]
fn cases_cells_keep_trailing_blank_that_matrix_trims() {
    let cases = render_display(r"\begin{cases} a & b \end{cases}", 60).unwrap();
    assert_eq!(cases.lines(), ["{ a   if  b"]);
    let matrix = render_display(r"\begin{pmatrix} a & b \end{pmatrix}", 60).unwrap();
    assert_eq!(matrix.lines(), ["(a  b)"]);
    assert_eq!(matrix.width(), 6);
}

/// 现状登记（N5）：大括号内写尾随空格会让上标整条回退（`x^{d }` → 源码），因为
/// `extract_flat_text` 不 trim，`to_superscript_char(' ')` 又是 `None`。三个版本一致、
/// 非本轮引入 —— 本改动让 `x^{d}` 能渲染后容易误以为"多打一个空格也行"。
/// 见 `docs/dev/tui-rendering.md` 第四节「已知边界」。
#[test]
fn trailing_space_inside_superscript_braces_still_falls_back() {
    assert_eq!(render_inline(r"x^{d }"), None);
    assert_eq!(render_inline(r"W^{T }"), None);
    // 对照：不带空格的同一写法照常行内渲染
    inline(r"x^{d}", "xᵈ");
    inline(r"W^{T}", "Wᵀ");
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
