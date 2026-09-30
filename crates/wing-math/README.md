# wing-math

LaTeX 数学子集 → 终端字符网格（character grid）渲染引擎。

这是 wing TUI 公式渲染（`crates/wing/src/render/markdown/`）的底座：**不做任何 I/O**，
只把一段 LaTeX 数学源码变成字符串（行内）或等宽字符网格（显示公式）。
上游引擎是借鉴内联的 `term-maths 1.0.0` + `rust-latex-parser 0.1.0`，
外面套了我们自己的归一化 / 自检 / 多行环境适配层。

## 窄接口

```rust
// 行内：必须落在单行，否则 None（上层降级为源码字面量）
wing_math::render_inline(r"x^2 + y^2")   // Some("x² + y²")
wing_math::render_inline(r"\frac{a}{b}") // None（分式是 3 行块）

// 显示：字符网格（超宽 / 疑似截断 / 超预算 → None）
let m = wing_math::render_display(r"\frac{a}{b}", 40).unwrap();
m.lines();     // [" a", "───", " b"]
m.width();     // 3
m.baseline();  // 1

// 原始网格（要自己拼装时用）
wing_math::render_block(r"\sum_{i=1}^{n} i")
```

`None` 的完整语义（空输入 / 顶层 `&`、`\\` / 环境或定界符不配对 / 未支持命令 /
超宽 / 超预算 / 行内非单行）见 `render_*` 的文档注释与 `src/api.rs`。

## 看一眼渲染效果

```bash
cargo run -q -p wing-math --example render_samples            # 人肉审阅入口
cargo run -q -p wing-math --release --example render_samples  # 含性能基线
```

## 出处与许可

内联源码的出处、版本、sha1、许可与**逐条本地改动**见 [`NOTICE`](NOTICE)；
许可原文随包保留在本目录（`LICENSE-MIT` / `LICENSE-APACHE` /
`LICENSE-MIT-rust-latex-parser`）。本 crate 自身许可是工作区的 Apache-2.0。

## 边界

- **零 I/O、零终端依赖**：宽度策略、居中、截断、主题色、记忆化都在调用方（渲染层）。
- **不静默丢内容**：任何"渲染不了"的单元格 / 前后缀都会让**整条公式**返回 `None`，
  绝不退化成空格或半截网格。
- **任何输入都不 panic、不栈溢出**：预算闸（源码长度 / 嵌套深度 / 行数 / 列数 /
  结果高度与面积）在 `src/guard.rs`。
