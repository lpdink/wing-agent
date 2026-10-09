//! 内联编辑器渲染（design §13.1–§13.2）。
//!
//! 编辑器是**行内**的：`label  [ buffer▏ ]`。缓冲文本的唯一来源是
//! [`EditState::visible_buffer`]（07 的密文形态返回 `•` × 长度）——渲染层没有任何第二条
//! 路径能拿到明文，密文红线因此是**类型系统**的，不是纪律的（design §13.2）。
//!
//! 光标是一个**反显空格**（`Modifier::REVERSED`），不是 `▏` 之类的字形：反显块覆盖
//! 一个格子（CJK / 密文点这类双宽字符也一样），到缓冲末尾时光标仍然可见。

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;
use crate::shared::panels::settings::EditState;

use super::text::clamp_line;
use super::text::single_row;

/// 值列的编辑器片段：`[<buffer><cursor>]`（密文空缓冲时是占位提示）。
pub(crate) fn value_spans(state: &EditState, palette: &ThemePalette) -> Vec<Span<'static>> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    let mut spans = vec![Span::styled("[".to_string(), dim)];

    if state.is_secret() && state.buffer_len() == 0 {
        // 占位提示（design §13.2）：Enter 保留原值不变。
        spans.push(cursor_block(palette));
        spans.push(Span::styled(
            "输入新密钥（Enter 保留原值不变）".to_string(),
            dim,
        ));
    } else {
        // 光标是**字符索引**（07 的约定），密文缓冲与可见缓冲的字符数一致。
        let visible = state.visible_buffer();
        let chars: Vec<char> = visible.chars().collect();
        let cursor = state.cursor();
        for (index, ch) in chars.iter().enumerate() {
            let mut span = Span::styled(ch.to_string(), text);
            if index == cursor {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
            spans.push(span);
        }
        if cursor >= chars.len() {
            spans.push(cursor_block(palette));
        }
    }
    spans.push(Span::styled("]".to_string(), dim));
    spans
}

/// 反显空格 —— 光标块。
fn cursor_block(palette: &ThemePalette) -> Span<'static> {
    Span::styled(
        " ".to_string(),
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::REVERSED),
    )
}

/// 校验失败的提示行（`error` 非空时挂在编辑器行的**下一行**）：`↳ <error>`（danger）。
///
/// `column` 是值列（错误挂在方括号底下），但至多退到区域中点 —— 否则窄屏上错误文案会被
/// 截成空串，而「让用户看见错误」正是这一行的全部意义。
pub(crate) fn error_line(
    error: &str,
    column: usize,
    width: usize,
    palette: &ThemePalette,
) -> Line<'static> {
    let danger = Style::default().fg(palette.danger);
    let head = "↳ ";
    let pad = column.min(width / 2).saturating_sub(2);
    let text = single_row(
        error,
        width.saturating_sub(pad + crate::ui::settings::text::display_width(head)),
    );
    clamp_line(
        Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled(head.to_string(), danger),
            Span::styled(text, danger),
        ]),
        width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::settings::test_support as fx;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    fn text_of(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn reversed_count(spans: &[Span<'static>]) -> usize {
        spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .count()
    }

    #[test]
    fn the_buffer_is_bracketed_with_the_cursor_reversed() {
        let mut panel = fx::panel();
        fx::open_editor(&mut panel, "base_url");
        let state = panel.edit_state().expect("编辑器开着");
        assert_eq!(state.path(), "providers[0].base_url");
        let spans = value_spans(state, &fx::palette());
        let out = text_of(&spans);
        assert!(out.starts_with("[https://api.example.com"), "{out:?}");
        assert!(out.ends_with(']'), "{out:?}");
        assert_eq!(reversed_count(&spans), 1, "恰好一个光标块：{out:?}");
    }

    #[test]
    fn a_cursor_at_the_end_still_renders_a_block() {
        let mut panel = fx::panel();
        fx::open_editor(&mut panel, "base_url");
        let state = panel.edit_state().expect("编辑器开着");
        let spans = value_spans(state, &fx::palette());
        let block = spans
            .iter()
            .find(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .expect("光标块");
        assert_eq!(block.content.as_ref(), " ", "末尾光标是一个反显空格");
    }

    #[test]
    fn an_empty_secret_buffer_shows_the_placeholder() {
        let mut panel = fx::panel();
        fx::open_editor(&mut panel, "api_key");
        let state = panel.edit_state().expect("编辑器开着");
        assert!(state.is_secret());
        assert_eq!(state.buffer_len(), 0, "密文编辑器恒从空开始");
        let out = text_of(&value_spans(state, &fx::palette()));
        assert!(out.contains("输入新密钥（Enter 保留原值不变）"), "{out:?}");
    }

    /// 07 的 `visible_buffer()` 是密文的唯一出口 —— 渲染层只看得到 `•`。
    #[test]
    fn a_secret_buffer_renders_as_bullets_only() {
        let mut panel = fx::panel();
        fx::open_editor(&mut panel, "api_key");
        fx::type_text(&mut panel, "sk-super-secret-value-1234");
        let state = panel.edit_state().expect("编辑器开着");
        assert_eq!(state.buffer_len(), 26);
        let out = text_of(&value_spans(state, &fx::palette()));
        assert!(!out.contains("sk-"), "{out:?}");
        assert!(!out.contains("secret"), "{out:?}");
        assert_eq!(
            out,
            format!("[{} ]", "•".repeat(26)),
            "26 个掩码字符 + 末尾光标块：{out:?}"
        );
    }

    #[test]
    fn the_error_line_hangs_under_the_value_column_within_the_width() {
        for width in [20usize, 30, 60, 120] {
            let line = error_line("需要一个整数", 40, width, &fx::palette());
            let out = line.to_string();
            assert!(
                unicode_width::UnicodeWidthStr::width(out.as_str()) <= width,
                "width {width}: {out:?}"
            );
            assert!(out.contains('↳'), "{out:?}");
        }
        assert!(
            error_line("需要一个整数", 40, 6, &fx::palette())
                .to_string()
                .contains('↳')
        );
    }

    #[test]
    fn the_error_line_uses_the_danger_register() {
        let line = error_line("需要一个整数", 10, 60, &fx::palette());
        assert!(
            line.spans
                .iter()
                .filter(|span| !span.content.trim().is_empty())
                .all(|span| span.style.fg == Some(fx::palette().danger)),
            "{line:?}"
        );
    }

    #[test]
    fn the_editor_appears_on_its_own_row_after_a_commit_error() {
        // 真实路径：提交非法值 → 编辑器留在原地并带上 error（07 的语义）。
        let mut panel = fx::panel();
        fx::goto(&mut panel, "gateway.port");
        fx::press(&mut panel, KeyCode::Enter);
        fx::press_with(&mut panel, KeyCode::Char('u'), KeyModifiers::CONTROL);
        fx::type_text(&mut panel, "abc");
        fx::press(&mut panel, KeyCode::Enter);
        let state = panel.edit_state().expect("校验失败时编辑器不关闭");
        assert_eq!(state.error(), Some("需要一个整数"));
        let out = text_of(&value_spans(state, &fx::palette()));
        assert!(out.contains("abc"), "缓冲还在：{out:?}");
    }
}
