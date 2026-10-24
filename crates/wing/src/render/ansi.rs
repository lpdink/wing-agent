//! 文本样式 → ANSI SGR：库代码里的**唯一**实现。
//!
//! 终端文本的出口（CLI 纯文本表格、usage 热力图、主题画廊、整屏帧、渲染探针）都从
//! 这里走 —— 第二份「转义数学」会漂：颜色表、属性顺序、复位策略各写一遍，就各错一遍。
//! `welcome_preview` / `thinking_preview` 还各自留着一份只喂 `Line` 的窄编码器（它们
//! 只画牌子 / 思考行，形状与这里不同，留给各自那一波收编）。
//!
//! **自闭合契约**：带样式的段以 `{SGR}{text}{RESET}` 交出，不带样式的段就是裸文本。
//! 调用方因此可以在默认样式的段上直接拼字符串 —— 上一段已经复位，空白格不会继承
//! 前一格的颜色（这正是「预览画出来的和真机不一样」最容易发生的地方）。
//!
//! **`Reset` 层不下发**：`0` 是「复位所有属性」，混进一条组合序列里会把刚设好的
//! 前景一起抹掉。默认色就是不写（与 `ui::html::css` 同一条口径）—— 两个后端因此
//! 描述同一帧。`examples/` 里的调试工具（如 `render_probe`）通过 [`paint`] 消费它。

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

/// ANSI 复位。
const RESET: &str = "\u{1b}[0m";

/// 带样式的 `text`；样式为空或文本为空时原样返回（见模块文档的自闭合契约）。
pub fn paint(style: Style, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let prefix = sgr(style);
    if prefix.is_empty() {
        text.to_string()
    } else {
        format!("{prefix}{text}{RESET}")
    }
}

/// [`Style`] → SGR 序列（空串 = 样式为空）。前景在前、背景次之、修饰位在后。
fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if let Some(fg) = style.fg.filter(|color| *color != Color::Reset) {
        codes.push(color_code(fg, false));
    }
    if let Some(bg) = style.bg.filter(|color| *color != Color::Reset) {
        codes.push(color_code(bg, true));
    }
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if style.add_modifier.contains(flag) {
            codes.push(code.to_string());
        }
    }
    if codes.is_empty() {
        return String::new();
    }
    format!("\u{1b}[{}m", codes.join(";"))
}

/// 一个颜色在给定层（前景 / 背景）上的 SGR 参数。命名色用 30/40 一族的短码，
/// 索引色与真彩用 `38;5;n` / `38;2;r;g;b`。
///
/// `Reset` 到不了这里（[`sgr`] 先滤掉了）；真到了也只给"默认前景 / 默认背景"，
/// 绝不给 `0`。
fn color_code(color: Color, background: bool) -> String {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    match color {
        Color::Reset => (base + 9).to_string(),
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => bright.to_string(),
        Color::LightRed => (bright + 1).to_string(),
        Color::LightGreen => (bright + 2).to_string(),
        Color::LightYellow => (bright + 3).to_string(),
        Color::LightBlue => (bright + 4).to_string(),
        Color::LightMagenta => (bright + 5).to_string(),
        Color::LightCyan => (bright + 6).to_string(),
        Color::White => (bright + 7).to_string(),
        Color::Indexed(i) => format!("{};5;{i}", if background { 48 } else { 38 }),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if background { 48 } else { 38 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_style_or_text_is_left_alone() {
        assert_eq!(paint(Style::default(), "plain"), "plain");
        assert_eq!(paint(Style::default().fg(Color::Red), ""), "");
    }

    #[test]
    fn named_colours_use_the_short_codes() {
        assert_eq!(
            paint(Style::default().fg(Color::White).bold(), "x"),
            "\u{1b}[97;1mx\u{1b}[0m"
        );
    }

    #[test]
    fn every_attribute_and_layer_is_encoded() {
        let style = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6))
            .add_modifier(
                Modifier::BOLD
                    | Modifier::DIM
                    | Modifier::ITALIC
                    | Modifier::UNDERLINED
                    | Modifier::REVERSED
                    | Modifier::CROSSED_OUT,
            );
        assert_eq!(
            paint(style, "x"),
            "\u{1b}[38;2;1;2;3;48;2;4;5;6;1;2;3;4;7;9mx\u{1b}[0m"
        );
        assert_eq!(
            paint(Style::default().bg(Color::Indexed(9)), "x"),
            "\u{1b}[48;5;9mx\u{1b}[0m"
        );
    }

    #[test]
    fn a_reset_layer_is_not_emitted() {
        // `0` 会复位所有属性：混在颜色码后面会把刚设好的前景一起抹掉。
        // 从 `Buffer` 反拼回来的样式常常带 `Reset` 的 bg / fg（`theme_preview` 的
        // 状态栏就是），所以这一条是「两个后端同一帧」的前提。
        let style = Style::default().fg(Color::Rgb(1, 2, 3)).bg(Color::Reset);
        assert_eq!(paint(style, "x"), "\u{1b}[38;2;1;2;3mx\u{1b}[0m");
        assert_eq!(paint(Style::default().fg(Color::Reset), "x"), "x");
        assert_eq!(
            paint(Style::default().fg(Color::Red).bg(Color::Reset), "x"),
            "\u{1b}[31mx\u{1b}[0m"
        );
    }

    #[test]
    fn a_styled_run_closes_itself_so_a_plain_run_cannot_inherit_it() {
        // 回归点：默认样式的段若不复位，空白格会被涂成上一段的底色。
        let mut out = String::new();
        out.push_str(&paint(Style::default().fg(Color::Rgb(1, 2, 3)), "▀"));
        out.push_str(&paint(Style::default(), "  "));
        assert_eq!(out, "\u{1b}[38;2;1;2;3m▀\u{1b}[0m  ");
    }
}
