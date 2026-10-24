//! 文本样式 → CSS / HTML 转义：HTML 导出的**唯一**实现。
//!
//! 与 [`crate::render::ansi`] 是同一件事的两个出口：一处样式，两种记号。导出的两个
//! 消费者（`theme_preview` 的主题画廊、`splash_preview` 的整屏帧）共用这份定义 ——
//! 各写一份的话，颜色表、转义顺序、`REVERSED`（前景/背景互换）迟早各错一遍。
//!
//! 页面底色（[`page_colors`]）与文档外壳（[`document`]）也在这里：App 只画格子、不画
//! 终端背景，导出 HTML 时必须由页面补上 —— 深色 palette 落在浏览器默认白底上，截图就
//! 成了「全白」。
//!
//! 住在 `ui` 而不是 `render`：颜色 → `#rrggbb` 要走命名色的近似表（`ui::shimmer::to_rgb`），
//! 而 `render` 在分层里位于 `ui` 之下（markdown 渲染被 ui 消费），不能反向依赖它。

use std::fmt::Write as _;

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

/// 一份 HTML 文档的外壳：文档头 + 页面底色 + `<pre>` 里已经编码好的正文。
///
/// 两个导出工具的差别只在排版参数（字号 / 行高 / 边距 / 说明行），壳子是同一份 ——
/// 各写一遍的话 `<!doctype>`、字符集、`white-space:pre` 迟早各错一处。
pub struct Page<'a> {
    /// `<title>`。
    pub title: &'a str,
    /// `<pre>` 上方的说明行（`None` = 不画）。
    pub caption: Option<&'a str>,
    /// 页面底色 / 前景色。
    pub background: &'a str,
    pub foreground: &'a str,
    /// `body` 的内边距。
    pub padding: &'a str,
    /// `<pre>` 的字体声明。行高要显式写：半格像素靠上下两半拼实心，行高一多就有横缝。
    pub font: &'a str,
    /// `<pre>` 里已经编码好的正文（换行自己带）。
    pub body: &'a str,
}

/// 把 [`Page`] 拼成一份可以双击打开的 HTML。
pub fn document(page: &Page<'_>) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n");
    let _ = writeln!(out, "<title>{}</title>", escape(page.title));
    out.push_str("<style>\nhtml,body{margin:0;padding:0}\n");
    let _ = writeln!(
        out,
        "body{{background:{};color:{};padding:{}}}",
        page.background, page.foreground, page.padding
    );
    if page.caption.is_some() {
        out.push_str(".cap{font:12px/1.4 Menlo,\"SF Mono\",\"DejaVu Sans Mono\",monospace;opacity:.55;margin:0 0 10px}\n");
    }
    let _ = writeln!(out, "pre{{margin:0;{};white-space:pre}}", page.font);
    out.push_str("</style></head><body>\n");
    if let Some(caption) = page.caption {
        let _ = writeln!(out, "<div class=\"cap\">{}</div>", escape(caption));
    }
    out.push_str("<pre>\n");
    out.push_str(page.body);
    out.push_str("</pre>\n</body></html>\n");
    out
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
    fn dim_and_strikethrough_reach_the_page() {
        assert_eq!(
            css(Style::default().add_modifier(Modifier::DIM)),
            "opacity:.62"
        );
        assert_eq!(
            css(Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .add_modifier(Modifier::CROSSED_OUT)),
            "color:#010203;text-decoration:line-through"
        );
    }

    #[test]
    fn named_colours_become_hex() {
        assert_eq!(hex(Color::Red), "#cd3131");
        assert_eq!(hex(Color::Rgb(0x0b, 0x0f, 0x16)), "#0b0f16");
    }

    #[test]
    fn escaping_covers_the_three_markup_characters() {
        assert_eq!(escape("<a & b>"), "&lt;a &amp; b&gt;");
    }

    #[test]
    fn a_page_shell_carries_the_caption_only_when_asked() {
        let page = |caption| Page {
            title: "t <1>",
            caption,
            background: "#000000",
            foreground: "#ffffff",
            padding: "1px 2px",
            font: "font:14px/1 Menlo,monospace",
            body: "x\n",
        };
        let with = document(&page(Some("cap & <x>")));
        assert!(with.starts_with("<!doctype html>"));
        assert!(with.contains("<title>t &lt;1&gt;</title>"));
        assert!(with.contains("background:#000000;color:#ffffff;padding:1px 2px"));
        assert!(with.contains(".cap{"));
        assert!(with.contains("<div class=\"cap\">cap &amp; &lt;x&gt;</div>"));
        assert!(with.contains("<pre>\nx\n</pre>\n</body></html>"));
        let without = document(&page(None));
        assert!(!without.contains(".cap{"), "没有说明行就不该有它的样式");
        assert!(!without.contains("<div"), "{without:.200}");
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
