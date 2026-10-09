//! 帮助浮层（`?`）的内容：design §12.3 的四列全表转成终端排版。
//!
//! 四列（树 / 编辑器 / 问题清单 / 选择项）在 76 列的浮层里放不下，因此按**用途分组**
//! 重排 —— 键位一个不少，只是同一行的归属从「列」变成「节」。
//!
//! 第一行是 `改动只在按 s 保存后才落盘。`：浮层在极矮终端会从底部截断，
//! 这句话是唯一一句「不知道会误解整个面板」的话，所以它必须活下来。

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;

use super::text::LineBuf;
use super::text::display_width;

/// 浮层宽度上限（再宽也读不动；更窄的终端按可用宽度缩）。
pub(crate) const HELP_WIDTH: u16 = 76;

/// 键位列宽（`Ctrl+R` 6 列 + 空白）。
const KEY_COLUMN: usize = 12;

/// 帮助表格的一行。
enum HelpRow {
    /// 分节标题。
    Section(&'static str),
    /// 一个键（或一组键）+ 它的作用。
    Key(&'static str, &'static str),
}

/// 键位表（design §12.3 的全表；顺序 = 使用频率）。
const TABLE: &[HelpRow] = &[
    HelpRow::Section("通用"),
    HelpRow::Key("↑ ↓", "移动光标 / 选择条目"),
    HelpRow::Key("Enter", "展开 / 切换 / 选择 / 编辑 / 新增"),
    HelpRow::Key("Space", "切换 bool / 选中选项"),
    HelpRow::Key("s", "保存两边（Gateway + Interface）"),
    HelpRow::Key("/", "搜索（Enter 保留 · Esc 恢复）"),
    HelpRow::Key("p", "问题清单（再按一次返回树）"),
    HelpRow::Key("Tab", "切根 Gateway ↔ Interface"),
    HelpRow::Key("R", "重新载入（丢弃本地改动）"),
    // AD1：Ctrl+R 只在有待重启的变更时（`restart_required` 非空）生效并出现在键位栏。
    HelpRow::Key("Ctrl+R", "立即重启网关（有待重启的变更时）"),
    HelpRow::Key("PgUp PgDn", "翻页；Home End 跳首尾"),
    HelpRow::Key("?", "帮助（再按 ? 或 Esc 关闭）"),
    HelpRow::Key("Ctrl+C", "双击退出 TUI（面板不吞）"),
    HelpRow::Section("树视图"),
    HelpRow::Key("← →", "折叠 / 展开；enum 行循环切值"),
    HelpRow::Key("a", "给最近的列表新增一项"),
    HelpRow::Key("d", "删除列表项 / 清空标量（二次确认）"),
    HelpRow::Key("J K", "列表项下移 / 上移"),
    HelpRow::Key("r", "复位为默认（移除这一项）"),
    HelpRow::Key("Esc", "退出搜索 → 关闭面板（脏则二次确认）"),
    HelpRow::Section("编辑器"),
    HelpRow::Key("Enter", "提交（空缓冲 = 取消）"),
    HelpRow::Key("Esc", "取消编辑，缓冲丢弃"),
    HelpRow::Key("← →", "移动光标；Home End 行首行尾"),
    HelpRow::Key("Bksp Del", "删除字符；Ctrl+U 清空缓冲"),
    HelpRow::Section("问题清单 / 选择项"),
    HelpRow::Key("↑ ↓", "选择；Enter / → 跳到该字段 · Space 选中"),
    HelpRow::Key("← Esc", "返回（不选）"),
];

/// 帮助浮层的内容行：`改动只在按 s 保存后才落盘。` + 全表。
///
/// 宽度与高度都由调用方给定（浮层盒子的 inner），超出按行截断。
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
                let pad = KEY_COLUMN.saturating_sub(display_width(keys));
                buf.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(keys.to_string(), key),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(description.to_string(), text),
                ]));
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
        assert!(joined.contains("切根 Gateway ↔ Interface"), "{joined}");
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
