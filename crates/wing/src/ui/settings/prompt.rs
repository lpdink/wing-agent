//! 模态提示浮层（删除确认 / 清空确认 / 形态选择 / 放弃改动）。
//!
//! 07 的 [`PromptView`] 是纯数据（`kind` / `title` / `lines` / `options` / `cursor`），
//! 这里把它画成一个居中浮层：
//!
//! ```text
//! ┌ 删除 "default"？ ─────────────────┐
//! │ 它的模型会从目录消失。             │
//! │ ❯ 确认删除                        │
//! │   取消                            │
//! └───────────────────────────────────┘
//! ```
//!
//! `options` 为空 = 纯确认（Enter 即确认），此时补一行 dim 的 `Enter 确认 · Esc 取消`，
//! 免得用户面对一个没有提示的空盒子。

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;
use crate::shared::panels::settings::PromptView;

use super::text::LineBuf;
use super::text::display_width;

/// 浮层最小宽度（标题 + 一列选项的可读下限）。
const MIN_WIDTH: u16 = 24;

/// 浮层内容行（宽度已裁到 `width`）。
pub(crate) fn prompt_lines(
    prompt: &PromptView,
    palette: &ThemePalette,
    width: usize,
) -> Vec<Line<'static>> {
    let mut buf = LineBuf::new(width, usize::MAX);
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    let cursor = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);

    for line in &prompt.lines {
        buf.prose(line, text);
    }
    if prompt.options.is_empty() {
        if !prompt.lines.is_empty() {
            buf.blank();
        }
        buf.text("Enter 确认 · Esc 取消", dim);
    } else {
        for (index, option) in prompt.options.iter().enumerate() {
            let is_cursor = index == prompt.cursor;
            buf.push(Line::from(vec![
                if is_cursor {
                    Span::styled("❯ ".to_string(), cursor)
                } else {
                    Span::raw("  ")
                },
                Span::styled(option.clone(), if is_cursor { cursor } else { text }),
            ]));
        }
    }
    buf.take()
}

/// 提示浮层的外框尺寸（宽 / 高）：内容 + 边框与内边距，按可用区域钳制。
pub(crate) fn prompt_box_size(
    prompt: &PromptView,
    palette: &ThemePalette,
    available: ratatui::layout::Rect,
) -> (u16, u16) {
    let content_width = prompt
        .lines
        .iter()
        .map(|line| display_width(line))
        .chain(
            prompt
                .options
                .iter()
                .map(|option| display_width(option) + 2),
        )
        .chain([display_width(&prompt.title) + 2])
        .max()
        .unwrap_or(0);
    let width = (content_width as u16 + 6)
        .max(MIN_WIDTH)
        .min(available.width);
    let lines = prompt_lines(prompt, palette, width.saturating_sub(4) as usize);
    let height = (lines.len() as u16 + 2).max(3).min(available.height);
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::PromptKind;
    use crate::ui::settings::test_support as fx;

    fn view(kind: PromptKind, options: &[&str], cursor: usize) -> PromptView {
        PromptView {
            kind,
            title: "删除 \"default\"？".into(),
            lines: vec!["它的模型会从目录消失。".into()],
            options: options.iter().map(|o| (*o).to_string()).collect(),
            cursor,
        }
    }

    #[test]
    fn options_render_with_the_cursor_on_the_selected_row() {
        let lines = prompt_lines(
            &view(
                PromptKind::Variants,
                &["简单形态（裸字符串）", "完整形态（对象）"],
                1,
            ),
            &fx::palette(),
            40,
        );
        let texts: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(
            texts.iter().any(|l| l.contains("它的模型会从目录消失。")),
            "{texts:?}"
        );
        let selected = texts.iter().find(|l| l.contains("完整形态")).unwrap();
        assert!(selected.starts_with("❯ "), "{selected:?}");
        let plain = texts.iter().find(|l| l.contains("简单形态")).unwrap();
        assert!(plain.starts_with("  "), "{plain:?}");
    }

    #[test]
    fn a_pure_confirmation_gets_the_default_hint_line() {
        let lines = prompt_lines(&view(PromptKind::Confirm, &[], 0), &fx::palette(), 40);
        let texts: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(
            texts.iter().any(|l| l.contains("Enter 确认 · Esc 取消")),
            "{texts:?}"
        );
    }

    #[test]
    fn the_box_stays_inside_the_available_area() {
        let area = ratatui::layout::Rect::new(0, 0, 30, 8);
        let long = PromptView {
            kind: PromptKind::Confirm,
            title: "确认".into(),
            lines: vec!["很长很长的说明文字".repeat(10)],
            options: vec![],
            cursor: 0,
        };
        let (width, height) = prompt_box_size(&long, &fx::palette(), area);
        assert!(
            width <= area.width && height <= area.height,
            "{width}x{height}"
        );
        assert!(width >= MIN_WIDTH.min(area.width));
    }

    #[test]
    fn prompt_content_never_overflows_its_width() {
        let view = view(PromptKind::Confirm, &[], 0);
        for width in [8usize, 16, 40, 80] {
            let lines = prompt_lines(&view, &fx::palette(), width);
            for line in &lines {
                let text = line.to_string();
                assert!(
                    unicode_width::UnicodeWidthStr::width(text.as_str()) <= width,
                    "w {width}: {text:?}"
                );
            }
        }
    }
}
