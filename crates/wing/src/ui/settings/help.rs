//! Keyboard help for the settings list.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;

use super::text::LineBuf;
use super::text::display_width;
use super::text::wrap_rows;

/// 键位列宽（`Ctrl+R` 6 列 + 空白）。
const KEY_COLUMN: usize = 12;

/// 帮助表格的一行。
enum HelpRow {
    /// 分节标题。
    Section(&'static str),
    /// 一个键（或一组键）+ 它的作用。
    Key(&'static str, &'static str),
}

/// 键位表（顺序 = 使用频率）。
const TABLE: &[HelpRow] = &[
    HelpRow::Section("通用"),
    HelpRow::Key("↑ ↓", "移动光标"),
    HelpRow::Key("Esc", "取消操作 / 返回分类 / 关闭"),
    HelpRow::Key("Enter", "进入分类 / 展开 / 编辑 / 新增"),
    HelpRow::Key("Space", "切换 bool / 选中选项"),
    HelpRow::Key("s", "保存两边（Gateway + Interface）"),
    HelpRow::Key("/", "搜索（跨分组；Enter 保留 · Esc 恢复）"),
    HelpRow::Key("p", "问题清单（再按一次返回）"),
    HelpRow::Key("R", "从磁盘读取（丢弃草稿）"),
    HelpRow::Key("Ctrl+R", "立即重启网关（有待重启的变更时）"),
    HelpRow::Key("PgUp PgDn", "翻页；Home End 跳首尾"),
    HelpRow::Key("?", "帮助（再按 ? 或 Esc 关闭）"),
    HelpRow::Key("Ctrl+C", "双击退出 TUI（面板不吞）"),
    HelpRow::Section("设置项"),
    HelpRow::Key("Tab", "切换开关 / 循环枚举"),
    HelpRow::Key("← →", "折叠 / 展开；enum 行循环切值"),
    HelpRow::Key("a", "给最近的列表新增一项"),
    HelpRow::Key("d", "删除列表项 / 清空标量（二次确认）"),
    HelpRow::Key("J K", "列表项下移 / 上移"),
    HelpRow::Key("r", "复位为默认（移除这一项）"),
    HelpRow::Section("编辑器"),
    HelpRow::Key("Enter", "提交（空密文保留原值）"),
    HelpRow::Key("Esc", "取消编辑，缓冲丢弃"),
    HelpRow::Key("← →", "移动光标；Home End 行首行尾"),
    HelpRow::Key("Bksp Del", "删除字符；Ctrl+U 清空缓冲"),
    HelpRow::Section("问题清单 / 选择项"),
    HelpRow::Key("↑ ↓", "选择；Enter / → 跳到该字段 · Space 选中"),
    HelpRow::Key("← Esc", "返回（不选）"),
];

/// Wrap each key description to the available list width.
pub(crate) fn help_lines(
    palette: &ThemePalette,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let mut buf = LineBuf::new(width, height);
    if width == 0 || height == 0 {
        return buf.take();
    }
    let note = Style::default().fg(palette.warning);
    let section = Style::default()
        .fg(palette.dim)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(palette.accent);
    let text = Style::default().fg(palette.text);

    buf.push(Line::from(Span::styled(
        "改动只在按 s 保存后才落盘。".to_string(),
        note,
    )));
    for row in TABLE {
        match row {
            HelpRow::Section(name) => {
                buf.blank();
                buf.text(&format!("─ {name}"), section);
            }
            HelpRow::Key(keys, description) => {
                let key_width = display_width(keys);
                let indent = (KEY_COLUMN + 2).min(width / 2).max(key_width + 2);
                let description_width = width.saturating_sub(indent);
                if description_width < 6 {
                    buf.text(keys, key);
                    for line in wrap_rows(description, width, buf.remaining()) {
                        buf.text(&line, text);
                    }
                } else {
                    for (index, line) in wrap_rows(description, description_width, buf.remaining())
                        .into_iter()
                        .enumerate()
                    {
                        let prefix = if index == 0 { keys } else { "" };
                        buf.push(Line::from(vec![
                            Span::styled(prefix.to_owned(), key),
                            Span::raw(" ".repeat(indent - display_width(prefix))),
                            Span::styled(line, text),
                        ]));
                    }
                }
            }
        }
    }
    buf.take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::settings::test_support as fx;

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_help_lists_every_key_from_the_design_table() {
        let lines = help_lines(&fx::palette(), 72, 200);
        let joined = texts(&lines).join("\n");
        for key in [
            "↑ ↓",
            "← →",
            "Enter",
            "Space",
            "a",
            "d",
            "J K",
            "r",
            "s",
            "/",
            "p",
            "Tab",
            "R",
            "Ctrl+R",
            "PgUp PgDn",
            "Esc",
            "Ctrl+C",
            "Bksp Del",
        ] {
            assert!(joined.contains(key), "缺少键位 {key}：\n{joined}");
        }
        assert!(joined.contains("改动只在按 s 保存后才落盘。"), "{joined}");
        assert!(joined.contains("折叠 / 展开"), "{joined}");
        assert!(joined.contains("循环枚举"), "{joined}");
        assert!(joined.contains("返回分类"), "{joined}");
    }

    #[test]
    fn the_save_note_survives_a_tiny_overlay() {
        let lines = help_lines(&fx::palette(), 40, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("按 s 保存"), "{:?}", lines[0]);
    }

    #[test]
    fn the_help_never_exceeds_its_box() {
        for width in [16usize, 24, 40, 72] {
            for height in [4usize, 12, 40] {
                let lines = help_lines(&fx::palette(), width, height);
                assert!(lines.len() <= height, "h {height}: {}", lines.len());
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
}
