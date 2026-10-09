//! 问题清单视图（design §14.1）：同一个组件服务三个场景 —— setup 首屏 / 保存失败 / 随时查看。
//!
//! ```text
//! 待修复
//!
//! ❯ 1  providers 不得为空
//!      路径 providers · 至少声明一个 provider
//!   2  配置里出现了未知键
//!      这个问题无法定位到单个字段，请检查文件：~/.wing/core/config.yaml
//!
//! 改完按 s 保存；保存失败会自动回到这里。
//! ```
//!
//! 条目是**变高**的（1 行标题 + ≤3 行细节），所以窗口数学不用 kernel 的定长 `window_range`：
//! 先把光标条目放进去，再向下填满、最后向上补 —— 光标永远可见（这是翻页唯一的意义）。
//! 单条超过可用高度时按行截断（极矮终端下依然不溢出）。

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;
use crate::shared::panels::settings::Problem;
use crate::shared::panels::settings::SettingsPanel;
use crate::ui::panel::cursor_span;

use super::text::clamp_line;
use super::text::single_row;
use super::text::wrap_rows;

/// 条目细节行的最大行数（超过按 `…` 收尾）。
const MAX_DETAIL_ROWS: usize = 3;

/// 问题清单的行列表（含标题与底部说明行）。
pub(crate) fn problem_lines(
    panel: &SettingsPanel,
    palette: &ThemePalette,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    if height == 0 || width == 0 {
        return Vec::new();
    }
    let dim = Style::default().fg(palette.dim);
    let heading = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // 标题行 + 底部说明行各占一行（矮终端下先让位给内容）。
    lines.push(clamp_line(
        Line::from(Span::styled("待修复".to_string(), heading)),
        width,
    ));

    let problems = panel.problems();
    let footnote = height >= 3;
    let list_height = height
        .saturating_sub(1 + usize::from(footnote))
        .max(usize::from(problems.is_empty()));
    if problems.is_empty() {
        lines.push(clamp_line(
            Line::from(Span::styled("暂无问题。".to_string(), dim)),
            width,
        ));
    } else {
        let ordinal_width = ordinal_width(problems.len());
        let entries: Vec<Vec<Line<'static>>> = problems
            .iter()
            .enumerate()
            .map(|(index, problem)| {
                entry_lines(
                    index,
                    ordinal_width,
                    problem,
                    panel.config_path(),
                    index == panel.problem_cursor(),
                    palette,
                    width,
                )
            })
            .collect();
        lines.extend(visible_entries(
            entries,
            panel.problem_cursor(),
            list_height,
        ));
    }

    if footnote {
        let note = if problems.is_empty() {
            "按 Esc 返回树。".to_string()
        } else if panel.setup_mode() {
            "修完全部问题后网关自动进入正常模式，TUI 会继续启动。".to_string()
        } else {
            "改完按 s 保存；保存失败会自动回到这里。".to_string()
        };
        lines.push(clamp_line(Line::from(Span::styled(note, dim)), width));
    }
    lines.truncate(height);
    lines
}

/// 一条问题：`❯ N  <message>` + 缩进的 `路径 <path> · <hint>`（`path == None` 时是文件级文案）。
fn entry_lines(
    index: usize,
    ordinal_width: usize,
    problem: &Problem,
    config_path: &str,
    cursor: bool,
    palette: &ThemePalette,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let message_style = if cursor {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text)
    };
    let number = format!("{:>ordinal_width$}  ", index + 1);
    let indent = 2 + number.len();
    let message_width = width.saturating_sub(indent);

    let mut lines = vec![clamp_line(
        Line::from(vec![
            cursor_span(cursor, palette),
            Span::styled(number, message_style),
            Span::styled(single_row(&problem.message, message_width), message_style),
        ]),
        width,
    )];

    let detail = match problem.path.as_deref() {
        Some(path) => match problem
            .hint
            .as_deref()
            .filter(|hint| !hint.trim().is_empty())
        {
            Some(hint) => format!("路径 {path} · {hint}"),
            None => format!("路径 {path}"),
        },
        None => format!("这个问题无法定位到单个字段，请检查文件：{config_path}"),
    };
    let detail_width = width.saturating_sub(indent);
    for (row_index, row) in wrap_rows(&detail, detail_width, MAX_DETAIL_ROWS)
        .into_iter()
        .enumerate()
    {
        let pad = if row_index == 0 {
            " ".repeat(indent)
        } else {
            " ".repeat(indent + 2)
        };
        lines.push(clamp_line(
            Line::from(vec![Span::raw(pad), Span::styled(row, dim)]),
            width,
        ));
    }
    lines
}

/// 编号列宽（按总数对齐：`1` 与 `10` 的左边界一致）。
fn ordinal_width(count: usize) -> usize {
    count.to_string().len()
}

/// 「光标条目优先」的变高窗口：先放光标，再向下填，最后向上补。
fn visible_entries(
    entries: Vec<Vec<Line<'static>>>,
    cursor: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    if entries.is_empty() || rows == 0 {
        return Vec::new();
    }
    let cursor = cursor.min(entries.len() - 1);
    let heights: Vec<usize> = entries.iter().map(Vec::len).collect();
    let mut start = cursor;
    let mut used = heights[cursor].min(rows);
    while start > 0 && used + heights[start - 1] <= rows {
        start -= 1;
        used += heights[start];
    }
    let mut end = cursor + 1;
    while end < entries.len() && used + heights[end] <= rows {
        used += heights[end];
        end += 1;
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in &entries[start..end] {
        out.extend(entry.iter().cloned());
    }
    out.truncate(rows);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::settings::test_support as fx;
    use wing_api_client::models::SettingProblem;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn problem(
        path: Option<&str>,
        kind: &str,
        message: &str,
        hint: Option<&str>,
    ) -> SettingProblem {
        SettingProblem {
            path: path.map(str::to_string),
            kind: kind.into(),
            message: message.into(),
            hint: hint.map(str::to_string),
        }
    }

    #[test]
    fn the_view_renders_the_wireframe() {
        let (catalog, mut panel) = fx::panel_from_problems(vec![
            problem(
                Some("providers"),
                "empty_list",
                "providers 不得为空",
                Some("至少声明一个 provider"),
            ),
            problem(None, "invalid_value", "配置里出现了未知键", None),
        ]);
        let lines = problem_lines(&panel, &palette(), 60, 12);
        let joined = texts(&lines).join("\n");
        assert!(joined.contains("待修复"), "{joined}");
        assert!(joined.contains("❯ 1  providers 不得为空"), "{joined}");
        assert!(
            joined.contains("路径 providers · 至少声明一个 provider"),
            "{joined}"
        );
        assert!(
            joined.contains("改完按 s 保存；保存失败会自动回到这里。"),
            "底部说明行：{joined}"
        );
        assert!(
            !joined.contains("修完全部问题"),
            "非 setup 模式不出现 setup 说明：{joined}"
        );

        // 光标下移一条 → 标记跟着走。
        fx::press(&mut panel, crossterm::event::KeyCode::Down);
        let out = texts(&problem_lines(&panel, &palette(), 60, 12));
        let first = out
            .iter()
            .find(|line| line.contains("providers 不得为空"))
            .unwrap();
        let second = out.iter().find(|line| line.contains("未知键")).unwrap();
        assert!(!first.contains('❯'), "{first:?}");
        assert!(second.contains('❯'), "{second:?}");
        let _ = catalog;
    }

    #[test]
    fn document_level_problems_point_at_the_file() {
        let (_, mut panel) = fx::panel_from_problems(vec![
            problem(Some("providers"), "empty_list", "providers 不得为空", None),
            problem(None, "invalid_value", "配置里出现了未知键", None),
        ]);
        fx::press(&mut panel, crossterm::event::KeyCode::Down);
        let joined = texts(&problem_lines(&panel, &palette(), 80, 12)).join("\n");
        assert!(
            joined
                .contains("这个问题无法定位到单个字段，请检查文件：/home/u/.wing/core/config.yaml"),
            "{joined}"
        );
    }

    #[test]
    fn an_empty_problem_list_says_so() {
        let panel = fx::panel();
        assert!(panel.problems().is_empty());
        let joined = texts(&problem_lines(&panel, &palette(), 60, 8)).join("\n");
        assert!(joined.contains("暂无问题。"), "{joined}");
        assert!(joined.contains("按 Esc 返回树"), "{joined}");
    }

    #[test]
    fn long_problem_lists_keep_the_cursor_visible() {
        let mut panel = fx::panel_with_many_problems(12);
        fx::press(&mut panel, crossterm::event::KeyCode::End);
        for height in [3usize, 5, 20] {
            let lines = problem_lines(&panel, &palette(), 60, height);
            assert!(lines.len() <= height, "height {height}: {}", lines.len());
            let joined = texts(&lines).join("\n");
            assert!(
                joined.contains('❯'),
                "光标条目必须可见（height {height}）：{joined}"
            );
        }
    }

    #[test]
    fn entries_never_exceed_the_width_even_with_long_messages() {
        let panel = fx::panel_with_many_problems(3);
        for width in [16usize, 24, 40, 100] {
            for line in &problem_lines(&panel, &palette(), width, 10) {
                let text = line.to_string();
                assert!(
                    unicode_width::UnicodeWidthStr::width(text.as_str()) <= width,
                    "width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn a_single_tall_entry_is_truncated_to_the_window() {
        let panel = fx::panel_with_many_problems(1);
        for height in [2usize, 4] {
            let lines = problem_lines(&panel, &palette(), 40, height);
            assert!(lines.len() <= height, "height {height}: {}", lines.len());
        }
    }
}
