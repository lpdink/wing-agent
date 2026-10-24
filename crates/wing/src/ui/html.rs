//! 文本样式 → CSS / HTML 转义：HTML 导出的**唯一**实现。
//!
//! 与 [`crate::render::ansi`] 是同一件事的两个出口：一处样式，两种记号。导出的两个
//! 消费者（`theme_preview` 的主题画廊、`splash_preview` 的整屏帧）共用这份定义 ——
//! 各写一份的话，颜色表、转义顺序、`REVERSED`（前景/背景互换）迟早各错一遍。
//!
//! 页面底色（[`page_colors`]）也在这里：App 只画格子、不画终端背景，导出 HTML 时必须
//! 由页面补上 —— 深色 palette 落在浏览器默认白底上，截图就成了「全白」。

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

use crate::config::Appearance;
use crate::config::ThemePalette;
use crate::ui::shimmer::to_rgb;

/// 一段样式的 CSS 声明（空串 = 无样式）。
///
/// `REVERSED` 按终端语义处理：前景与背景互换（浏览器里没有「反显」）。
pub fn css(style: Style) -> String {
    let mut decls: Vec<String> = Vec::new();
    let reversed = style.add_modifier.contains(Modifier::REVERSED);
    let fg = style.fg.filter(|color| *color != Color::Reset);
    let bg = style.bg.filter(|color| *color != Color::Reset);
    let (fg, bg) = if reversed { (bg, fg) } else { (fg, bg) };
    if let Some(color) = fg {
        decls.push(format!("color:{}", hex(color)));
    }
    if let Some(color) = bg {
        decls.push(format!("background-color:{}", hex(color)));
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        decls.push("font-weight:700".into());
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        decls.push("font-style:italic".into());
    }
    let mut decoration: Vec<&str> = Vec::new();
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        decoration.push("underline");
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        decoration.push("line-through");
    }
    if !decoration.is_empty() {
        decls.push(format!("text-decoration:{}", decoration.join(" ")));
    }
    if style.add_modifier.contains(Modifier::DIM) {
        // 终端用「更暗」表达次级；浏览器里没有 dim，压透明度最接近。
        decls.push("opacity:.62".into());
    }
    decls.join(";")
}

/// 颜色 → `#rrggbb`（命名色按 [`to_rgb`] 的近似表）。
pub fn hex(color: Color) -> String {
    let (r, g, b) = to_rgb(color);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// HTML 文本转义。
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// HTML 页面的默认底色 / 前景 —— 与这套 palette 配对的墙面颜色。
pub fn page_colors(palette: &ThemePalette) -> (&'static str, &'static str) {
    match palette.appearance {
        Appearance::Dark => ("#0b0f16", "#c9ced9"),
        Appearance::Light => ("#ffffff", "#1f2328"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_carries_colour_weight_and_decorations() {
        let style = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED);
        assert_eq!(
            css(style),
            "color:#010203;background-color:#040506;font-weight:700;font-style:italic;\
             text-decoration:underline"
        );
        assert_eq!(css(Style::default()), "");
    }

    #[test]
    fn reversed_swaps_the_two_layers() {
        let style = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6))
            .add_modifier(Modifier::REVERSED);
        assert_eq!(css(style), "color:#040506;background-color:#010203");
    }

    #[test]
    fn reset_colours_are_not_painted() {
        assert_eq!(css(Style::default().fg(Color::Reset)), "");
    }

    #[test]
    fn escaping_covers_the_three_markup_characters() {
        assert_eq!(escape("<a & b>"), "&lt;a &amp; b&gt;");
    }

    #[test]
    fn page_colors_follow_the_appearance() {
        let dark = ThemePalette::default();
        assert_eq!(page_colors(&dark).0, "#0b0f16");
        let light = ThemePalette {
            appearance: Appearance::Light,
            ..ThemePalette::default()
        };
        assert_eq!(page_colors(&light), ("#ffffff", "#1f2328"));
    }
}
