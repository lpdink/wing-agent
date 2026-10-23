//! 日历热力图：7 行（周一..周日）× N 列（ISO 周）的格子网格。
//!
//! 纯渲染：输入一段闭区间与「每天的一个数」，输出可直接打印的等宽文本行（可测、无 IO）。
//! 两个产品口径钉在这里：
//!
//! - **档位是相对量**（非零日的四分位，GitHub 同款思想）：一天跑了 200 次调用不会把其余
//!   全部压成最浅一档。绝对值永远可从 `wing usage --by day` / `--json` 读到；
//! - **每一行的格子槽位严格对齐**：所有行共用同一个 gutter（6 列）、同一套 2 列一格的槽位、
//!   同一个总宽度。因此窗口外的日子也画成 0 档 `·`——把它画成空格会让「窗口从周几开始」变成
//!   行首缩进，整张图看起来是错位的。范围边界读标题里的 `since → until`。
//!
//! 有颜色时格子仍是 `█`（填满格子），档位由**前景色明度**表达（`surface → accent` 线性
//! 混色）；无颜色（pipe / `NO_COLOR` / `TERM=dumb`）时退化成 `·░▒▓█` 字形。

use std::collections::HashMap;

use chrono::Datelike;
use chrono::Duration;
use chrono::NaiveDate;
use ratatui::style::Color;
use ratatui::style::Style;

use crate::config::ThemePalette;
use crate::render::table::plain::paint;

/// 左侧标尺宽度：`  Mon `（2 空格 + 3 字母 + 1 空格）。
const GUTTER: usize = 6;
/// 无颜色时的五档字形：0 档 + 4 个非零档。
const TIER_GLYPHS: [char; 5] = ['·', '░', '▒', '▓', '█'];
/// 有颜色时的格子字形（`█` 自己填满格子，明度由前景色表达）。
const TIER_BLOCK: char = '█';
/// 有颜色时 4 个非零档的混色系数（`surface` → `accent`）。
const TIER_MIX: [f64; 4] = [0.25, 0.5, 0.75, 1.0];
/// 相邻月份标签的起点至少隔 2 个格子（= 4 个显示列）：标签宽 3 列，于是两标签之间
/// 永远至少空 1 列，不会挤成一坨。
const MONTH_SLOT_GAP: usize = 2;
/// 裁掉左侧格子时占位的那一列。
const ELISION: char = '…';
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// 一次热力图的输入。
pub struct Heatmap<'a> {
    /// 范围起点（含）。
    pub since: NaiveDate,
    /// 范围终点（含）。
    pub until: NaiveDate,
    /// 每天的度量值（缺席 = 0；范围外的项被忽略）。
    pub values: &'a [(NaiveDate, i64)],
    /// 可用宽度（显示列）。
    pub width: usize,
    /// `Some` = 用这对墨色混出 4 档明度，`None` = 字形档位。
    pub ink: Option<&'a ThemePalette>,
}

/// 渲染结果：行 + 画出的列数 + 窗口横跨的列数（后者用来判断有没有裁剪）。
pub struct HeatmapLines {
    pub lines: Vec<String>,
    /// 实际画出的列数。
    pub columns: usize,
    /// 窗口横跨的列数（`columns < total` 即被裁剪）。
    pub total: usize,
}

impl HeatmapLines {
    pub fn truncated(&self) -> bool {
        self.columns < self.total
    }
}

impl Heatmap<'_> {
    pub fn render(&self) -> HeatmapLines {
        let first_monday = week_start(self.since);
        let total = ((week_start(self.until) - first_monday).num_days() / 7 + 1).max(1) as usize;
        let cap = max_columns(self.width).max(1);
        let truncated = total > cap;
        // 裁剪时最左留一个槽位画 `…`（宽度只够一列时不画：宁可少一格也不越预算）。
        let marker = truncated && cap >= 2;
        let shown = if marker { cap - 1 } else { total.min(cap) };
        let offset = total - shown;

        let by_day: HashMap<NaiveDate, i64> = self.values.iter().copied().collect();
        let thresholds = quartile_thresholds(&self.window_values());
        let cell_offset = usize::from(marker) * 2;

        let mut lines = Vec::with_capacity(10);
        lines.push(self.month_line(first_monday, offset, shown, cell_offset, marker));
        for row in 0..7 {
            let mut cells: Vec<(char, Option<Color>)> = Vec::with_capacity(shown + 1);
            if marker {
                cells.push((ELISION, None));
            }
            for slot in 0..shown {
                let monday = first_monday + Duration::days(((offset + slot) * 7) as i64);
                let date = monday + Duration::days(row as i64);
                // 窗口外的日子走 0 档：格子槽位一律画满，行宽才与其它行相同。
                let value = if date >= self.since && date <= self.until {
                    by_day.get(&date).copied().unwrap_or(0)
                } else {
                    0
                };
                cells.push(self.cell(value, &thresholds));
            }
            lines.push(format!(
                "{}{}",
                row_label(row),
                join_cells(&cells, self.ink.is_some())
            ));
        }
        let legend = self.legend_line(shown, marker);
        if !legend.is_empty() {
            lines.push(legend);
        }
        HeatmapLines {
            lines,
            columns: shown,
            total,
        }
    }

    /// 窗口内仍为正数的那些值（分档只看它们：0 值另有 0 档）。
    fn window_values(&self) -> Vec<i64> {
        let mut values: Vec<i64> = self
            .values
            .iter()
            .filter(|(day, value)| *value > 0 && *day >= self.since && *day <= self.until)
            .map(|(_, value)| *value)
            .collect();
        values.sort_unstable();
        values
    }

    /// 一个格子的字形与其墨色。
    fn cell(&self, value: i64, thresholds: &[i64; 3]) -> (char, Option<Color>) {
        let tier = tier_of(value, thresholds);
        if tier == 0 {
            return (TIER_GLYPHS[0], self.ink.map(|palette| palette.dim));
        }
        match self.ink {
            Some(palette) => (
                TIER_BLOCK,
                Some(mix(palette.surface, palette.accent, TIER_MIX[tier - 1])),
            ),
            None => (TIER_GLYPHS[tier], None),
        }
    }

    /// 月份行：首列标它所在的月，其后某列覆盖了「某月 1 日」时才标（且不与前一个标签挤在一起）。
    fn month_line(
        &self,
        first_monday: NaiveDate,
        offset: usize,
        shown: usize,
        cell_offset: usize,
        marker: bool,
    ) -> String {
        // 与格子行同宽；末列的月份标签可以写到格子区的右邻（只在这一种情形下更宽）。
        let grid = grid_width(shown, marker);
        let mut row: Vec<char> = vec![' '; self.width.max(grid)];
        let mut next_free_slot = 0usize;
        // 被间距规则挡住的标签**延后**到下一个可用列，而不是丢掉：一个月份的标注机会
        // 通常只有一次（含它 1 号的那一周），丢掉就等于整月无名。
        let mut pending: Option<NaiveDate> = None;
        for slot in 0..shown {
            let monday = first_monday + Duration::days(((offset + slot) * 7) as i64);
            // 首列也优先标「这一周里的 1 号」：窗口从月末开始时（`--since 7-31`），
            // 标 `Jul` 会让占满整列的八月无名——那一列真正属于八月。
            let in_week = (0..7)
                .map(|day| monday + Duration::days(day))
                .find(|day| day.day() == 1);
            if let Some(anchor) = in_week.or_else(|| (slot == 0).then_some(monday))
                && pending.is_none()
            {
                pending = Some(anchor);
            }
            if slot < next_free_slot {
                continue;
            }
            let Some(anchor) = pending.take() else {
                continue;
            };
            let start = GUTTER + cell_offset + slot * 2;
            let label = MONTH_NAMES[anchor.month0() as usize];
            if start + label.chars().count() > row.len() {
                // 装不下就丢弃（这一列太靠右），但别拖住后面的标签。
                next_free_slot = slot + 1;
                continue;
            }
            for (index, ch) in label.chars().enumerate() {
                row[start + index] = ch;
            }
            next_free_slot = slot + MONTH_SLOT_GAP;
        }
        let end = row
            .iter()
            .rposition(|ch| *ch != ' ')
            .map_or(0, |index| index + 1)
            .max(grid);
        row.truncate(end);
        row.into_iter().collect::<String>()
    }

    /// 图例行：`less · ░ ▒ ▓ █ more`（有颜色时同样着色）。
    ///
    /// 固定宽度的一句话：窄到装不下就整行不画（半截图例比没有更误导）。
    fn legend_line(&self, shown: usize, marker: bool) -> String {
        /// `less ` + 5 格 + 4 个分隔 + ` more`（每个字形都是 1 个显示列）。
        const PLAIN_WIDTH: usize = GUTTER + 5 + 5 + 4 + 5;
        if self.width < PLAIN_WIDTH {
            return String::new();
        }
        let cells: Vec<(char, Option<Color>)> = (0..5)
            .map(|tier| {
                if tier == 0 {
                    (TIER_GLYPHS[0], self.ink.map(|palette| palette.dim))
                } else {
                    match self.ink {
                        Some(palette) => (
                            TIER_BLOCK,
                            Some(mix(palette.surface, palette.accent, TIER_MIX[tier - 1])),
                        ),
                        None => (TIER_GLYPHS[tier], None),
                    }
                }
            })
            .collect();
        let text = format!(
            "{}less {} more",
            " ".repeat(GUTTER),
            join_cells(&cells, self.ink.is_some())
        );
        // 与格子行同宽（等宽是本模块的契约；尾部空格在终端上看不见）。
        let grid = grid_width(shown, marker);
        format!(
            "{text}{}",
            " ".repeat(grid.saturating_sub(visible_chars(&text)))
        )
    }
}

/// `d` 所在 ISO 周的周一。
fn week_start(day: NaiveDate) -> NaiveDate {
    day - Duration::days(day.weekday().num_days_from_monday() as i64)
}

/// 宽度能装下多少列（每列 2 个显示列、列间 1 空格；至少要 1 列）。
fn max_columns(width: usize) -> usize {
    width.saturating_sub(GUTTER).saturating_add(1) / 2
}

/// 整行格子的显示宽度（含左侧 `…` 占位，不含 gutter）。
fn cell_row_width(slots: usize) -> usize {
    slots.saturating_mul(2).saturating_sub(1)
}

/// 整块热力图的显示宽度（gutter + 格子区）。
fn grid_width(slots: usize, marker: bool) -> usize {
    GUTTER + cell_row_width(slots + usize::from(marker))
}

/// 一串纯文本的显示列数（这里只有 ASCII 与单列字形，字符数即列数）。
fn visible_chars(text: &str) -> usize {
    text.chars().count()
}

/// 行标签（`Mon` / `Wed` / `Fri` / `Sun`），其余行留空。
fn row_label(row: usize) -> &'static str {
    match row {
        0 => "  Mon ",
        2 => "  Wed ",
        4 => "  Fri ",
        6 => "  Sun ",
        _ => "      ",
    }
}

/// 非零值的四分位阈值：`q_k = v[(n-1)*k/4]`（升序）。空输入 → 全 0（没有非零日）。
///
/// 取 `(n-1)*k/4` 而不是 `n*k/4`：后者在 `n=4` 时把 `q3` 顶到最大值，于是 `█` 永远
/// 画不出来（`x > q3` 无解）。同值同档（比较用 `<=`），全等值 → 全部 1 档。
fn quartile_thresholds(sorted: &[i64]) -> [i64; 3] {
    let last = sorted.len().saturating_sub(1);
    [1, 2, 3].map(|k| sorted.get(last * k / 4).copied().unwrap_or(0))
}

/// 值 → 档位（0 = 零值 / 1..=4）。
fn tier_of(value: i64, thresholds: &[i64; 3]) -> usize {
    if value <= 0 {
        return 0;
    }
    1 + thresholds.iter().filter(|q| **q < value).count()
}

/// 两色之间按 `t`（0..=1）线性混色；任一侧不是 RGB 时退回 `accent`（主题色本身）。
///
/// `ui::shimmer::mix` 是同一份线性数学的另一份（依赖方向 `ui → render`，所以那边用不了
/// 这里）：要合并得把原语下移到 `render`，那之前两处的 doc 互相点名。
fn mix(surface: Color, accent: Color, t: f64) -> Color {
    let (Some((sr, sg, sb)), Some((ar, ag, ab))) = (rgb(surface), rgb(accent)) else {
        return accent;
    };
    let lerp = |from: u8, to: u8| (from as f64 + (to as f64 - from as f64) * t).round() as u8;
    Color::Rgb(lerp(sr, ar).min(ar.max(sr)), lerp(sg, ag), lerp(sb, ab))
}

fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// 一格一行：空格分隔（每列 2 个显示列），有颜色时逐格着色。
fn join_cells(cells: &[(char, Option<Color>)], color: bool) -> String {
    cells
        .iter()
        .map(|(glyph, ink)| {
            let text = glyph.to_string();
            match ink {
                Some(ink) => paint(Style::new().fg(*ink), &text, color),
                None => text,
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::{strip_ansi, visible_width};

    fn date(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    /// 把每天的值画成一张图（宽 120 列 = 足够 12 周）。
    fn render(since: &str, until: &str, values: &[(NaiveDate, i64)]) -> HeatmapLines {
        Heatmap {
            since: date(since),
            until: date(until),
            values,
            width: 120,
            ink: None,
        }
        .render()
    }

    /// 一天一行（去掉 gutter 与空格后的字形串）。
    fn rows(lines: &[String]) -> Vec<String> {
        lines[1..8]
            .iter()
            .map(|line| {
                line.chars()
                    .skip(GUTTER)
                    .filter(|c| !c.is_whitespace())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_grid_starts_on_the_monday_of_the_since_week_and_ends_on_the_until_sunday() {
        // 2026-10-21 是周三：首列必须是 10-19 那一周（周一起）。
        let lines = render("2026-10-21", "2026-10-21", &[(date("2026-10-21"), 5)]);
        assert_eq!(lines.columns, 1);
        let rows = rows(&lines.lines);
        // 只有周三那一行有读数；同一周里窗口外的日子照样占一个格子（画 0 档），
        // 所以每一行的格子数都一样（= 列数），行首不会缩进。
        assert_eq!(rows[2], "░", "周三的格子落在第 3 行：{rows:?}");
        assert!(rows.iter().all(|row| row.chars().count() == 1), "{rows:?}");
        // 四行标尺在（整行没有格子时行尾空格被裁掉，所以不锚定分隔空格）。
        assert!(lines.lines[1].starts_with("  Mon"));
        assert!(lines.lines[3].starts_with("  Wed"));
        assert!(lines.lines[5].starts_with("  Fri"));
        assert!(lines.lines[7].starts_with("  Sun"));
    }

    /// 范围外 = 空格；范围内没活动 = `·`。两者不能同形。
    #[test]
    fn quiet_days_and_days_outside_the_window_share_the_lightest_cell() {
        let lines = render("2026-10-19", "2026-10-25", &[(date("2026-10-20"), 7)]);
        assert_eq!(lines.columns, 1);
        let rows = rows(&lines.lines);
        assert_eq!(rows[1], "░", "周二有活动：{rows:?}");
        // 周三..周日：窗口内、没活动。
        assert_eq!(rows[2], "·", "{rows:?}");
        assert_eq!(rows[6], "·", "{rows:?}");
        // 周一在窗口内、同样没活动。
        assert_eq!(rows[0], "·", "{rows:?}");
    }

    /// 布局契约（渲染契约，不是视觉偏好）：**每一行共用同一个 gutter 与同一套格子槽位**。
    ///
    /// 窗口从周中开始时（这里从周五起），首个窗口外的那几天也必须占满自己的槽位——否则
    /// 行首缩进不同，整张图看起来是错位的（这正是把窗口外画成空格时的样子）。
    #[test]
    fn every_row_shares_one_gutter_and_one_cell_geometry() {
        // 2026-07-31 是周五：首列（周一起）有 4 天在窗口外。
        let lines = render("2026-07-31", "2026-10-23", &[(date("2026-10-23"), 9)]);
        assert_eq!(lines.columns, 13);
        let all: Vec<&str> = lines.lines.iter().map(String::as_str).collect();

        // 7 行格子 + 月份行 + 图例：首字符都从第 0 列起，第一格（或第一段文字）都在 gutter 之后。
        let first_glyph = |line: &str| -> usize {
            line.chars()
                .enumerate()
                .find(|(_, ch)| !ch.is_whitespace())
                .map_or_else(|| line.chars().count(), |(column, _)| column)
        };
        // 7 行格子的显示宽度完全相等（unicode-width，不是字节数）……
        let widths: Vec<usize> = all[1..8].iter().map(|line| visible_width(line)).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "行宽必须一致：{widths:?}\n{all:#?}"
        );
        // ……而格子区恒从 gutter 起（行宽 - 格子区宽度 == gutter），与行标签长短无关。
        let cells_width = cell_row_width(lines.columns);
        for line in &all[1..8] {
            assert_eq!(
                visible_width(line) - cells_width,
                GUTTER,
                "第一格起始列必须一致：{line:?}"
            );
        }
        // 月份行与图例也共用同一个 gutter（前导空白不会早于它），并与格子行等宽
        // （月份标签只在末列月份写到格子区右邻时才更宽）。
        assert!(first_glyph(all[0]) >= GUTTER, "{:?}", all[0]);
        assert_eq!(
            visible_width(all[0]),
            widths[0],
            "月份行与格子行等宽：{all:#?}"
        );
        assert_eq!(first_glyph(all[8]), GUTTER, "{:?}", all[8]);
        assert_eq!(
            visible_width(all[8]),
            widths[0],
            "图例与格子行等宽：{all:#?}"
        );
    }

    #[test]
    fn the_window_spans_iso_weeks_and_counts_them() {
        // 2026-08-03（周一）→ 2026-10-25（周日）正好 12 个 ISO 周。
        let lines = render("2026-08-03", "2026-10-25", &[(date("2026-08-03"), 1)]);
        assert_eq!(lines.columns, 12);
        assert_eq!(rows(&lines.lines)[0].chars().count(), 12, "周一那行 12 格");
    }

    /// 月份标被间距规则挡住时要**延后**到下一个可用列，不能整月无名
    /// （`--since 2026-10-19` 的窗口从十月末开始，十一月那几列必须还有 `Nov`）。
    #[test]
    fn a_month_label_blocked_by_the_gap_rule_moves_to_the_next_column() {
        let lines = render("2026-10-19", "2026-11-30", &[(date("2026-11-03"), 5)]);
        let month = &lines.lines[0];
        assert!(month.contains("Oct"), "首列标它所在的月：{month:?}");
        assert!(month.contains("Nov"), "十一月不能被挤掉：{month:?}");
        // 两个标签不重叠（起点至少隔两个格子 = 4 列，于是中间至少空一列）。
        let oct = month.find("Oct").unwrap();
        let nov = month.find("Nov").unwrap();
        assert!(nov >= oct + 4, "标签至少隔两个格子：{month:?}");
    }

    /// 裁剪这件事由渲染结果自己说：`total` 是窗口横跨的列数，`columns` 是画出来的。
    #[test]
    fn the_result_reports_both_the_window_span_and_the_drawn_columns() {
        let long: Vec<(NaiveDate, i64)> = (0..84)
            .map(|offset| (date("2026-08-03") + Duration::days(offset), 1 + offset % 3))
            .collect();
        let narrow = Heatmap {
            since: date("2026-08-03"),
            until: date("2026-10-25"),
            values: &long,
            width: 27,
            ink: None,
        }
        .render();
        assert_eq!(narrow.total, 12, "窗口横跨 12 周");
        assert_eq!(narrow.columns, 10);
        assert!(narrow.truncated());

        // 宽度够 → 不裁剪。
        let wide = Heatmap {
            since: date("2026-08-03"),
            until: date("2026-10-25"),
            values: &long,
            width: 120,
            ink: None,
        }
        .render();
        assert_eq!(wide.columns, wide.total);
        assert!(!wide.truncated());
    }

    /// 跨年：12 月 29 日那一周跨到 1 月，月份行必须出现两个年份标签而不是一坨。
    #[test]
    fn a_year_boundary_keeps_one_label_per_month() {
        let lines = render("2026-12-20", "2027-01-10", &[(date("2026-12-25"), 3)]);
        let month = &lines.lines[0];
        assert!(month.contains("Dec"), "{month:?}");
        assert!(month.contains("Jan"), "{month:?}");
        assert!(!month.contains("Feb"), "{month:?}");
    }

    /// 全零（范围内一次都没动）：所有格都是 0 档，绝不出货任何非零字形。
    #[test]
    fn an_all_zero_window_draws_only_the_light_dot() {
        let lines = render("2026-10-19", "2026-10-25", &[]);
        for row in rows(&lines.lines) {
            assert_eq!(row, "·", "{row:?}");
        }
        assert!(!rows(&lines.lines).iter().any(|row| row.contains('█')));
    }

    /// 全等值：四分位阈值退化成同一个数 → 所有非零日同档（1 档），不会有一个独占 `█`。
    #[test]
    fn equal_values_share_one_tier() {
        let values = [
            (date("2026-10-19"), 9),
            (date("2026-10-20"), 9),
            (date("2026-10-21"), 9),
            (date("2026-10-22"), 9),
        ];
        let lines = render("2026-10-19", "2026-10-25", &values);
        let grid = rows(&lines.lines);
        for row in grid.iter().take(4) {
            assert_eq!(row, "░", "{grid:?}");
        }
        assert!(!grid.iter().any(|row| row.contains('▒')));
    }

    /// 分档的四条边界：最小的一天 1 档、最大的一天 4 档（`n=4` 时也要画得出 `█`）。
    #[test]
    fn quartiles_reach_the_top_tier() {
        let values = [
            (date("2026-10-19"), 10),
            (date("2026-10-20"), 20),
            (date("2026-10-21"), 30),
            (date("2026-10-22"), 40),
        ];
        let sorted = [10, 20, 30, 40];
        let thresholds = quartile_thresholds(&sorted);
        assert_eq!(thresholds, [10, 20, 30]);
        assert_eq!(tier_of(10, &thresholds), 1);
        assert_eq!(tier_of(20, &thresholds), 2);
        assert_eq!(tier_of(30, &thresholds), 3);
        assert_eq!(tier_of(40, &thresholds), 4);
        assert_eq!(tier_of(0, &thresholds), 0);
        let rows = rows(&render("2026-10-19", "2026-10-25", &values).lines);
        assert_eq!(rows[0], "░");
        assert_eq!(rows[3], "█");
    }

    /// 单非零日：n=1 时三个阈值都等于它本身 → 1 档（不是 `█`）。
    #[test]
    fn a_single_non_zero_day_is_the_lightest_tier() {
        let thresholds = quartile_thresholds(&[42]);
        assert_eq!(thresholds, [42, 42, 42]);
        assert_eq!(tier_of(42, &thresholds), 1);
        assert_eq!(tier_of(43, &thresholds), 4);
    }

    /// 宽度不够时只画**最近**的列，最左一列变成 `…`，且整行不超预算。
    #[test]
    fn a_narrow_terminal_keeps_the_most_recent_columns_and_marks_the_cut() {
        let values = [(date("2026-08-03"), 1), (date("2026-10-19"), 100)];
        let lines = Heatmap {
            since: date("2026-08-03"),
            until: date("2026-10-25"),
            values: &values,
            width: 40,
            ink: None,
        }
        .render();
        // (40 - 6 + 1) / 2 = 17 列装得下；12 周的窗口用不上裁剪。
        assert_eq!(lines.columns, 12);
        assert!(!lines.lines[1].contains(ELISION));

        // 12 周（宽 120 装得下 57 列）→ 强行压到 20 列：裁剪 + 8 列。
        let long: Vec<(NaiveDate, i64)> = (0..84)
            .map(|offset| (date("2026-08-03") + Duration::days(offset), 1 + offset % 3))
            .collect();
        let narrow = Heatmap {
            since: date("2026-08-03"),
            until: date("2026-10-25"),
            values: &long,
            width: 27,
            ink: None,
        }
        .render();
        assert_eq!(narrow.columns, 10, "(27-6+1)/2 = 11 槽，留一槽画 …");
        assert!(narrow.lines[1].contains(ELISION), "{:?}", narrow.lines);
        for line in &narrow.lines {
            assert!(
                visible_width(line) <= 27,
                "不越预算: {line:?} ({} > 27)",
                visible_width(line)
            );
        }
    }

    /// 极窄（只够一列）：不画 `…`（没有槽位），仍然不越预算。
    #[test]
    fn a_single_column_budget_does_not_reserve_the_elision_slot() {
        let lines = Heatmap {
            since: date("2026-08-03"),
            until: date("2026-10-25"),
            values: &[],
            width: GUTTER + 2,
            ink: None,
        }
        .render();
        assert_eq!(lines.columns, 1);
        assert!(!lines.lines[1].contains(ELISION));
        for line in &lines.lines {
            assert!(visible_width(line) <= GUTTER + 2, "{line:?}");
        }
    }

    /// 图例与月份行都在（图例是「档位是什么意思」的唯一说明）。
    #[test]
    fn the_legend_names_the_five_tiers() {
        let lines = render("2026-10-19", "2026-10-25", &[(date("2026-10-20"), 3)]);
        let legend = lines.lines.last().unwrap();
        // 图例是真句话 + 补齐到格子行的宽度（等宽契约）。
        assert!(
            legend.starts_with("      less · ░ ▒ ▓ █ more"),
            "{legend:?}"
        );
        assert!(lines.lines[0].contains("Oct"), "{:?}", lines.lines[0]);
    }

    /// 有颜色时格子统一是 `█`（明度表达档位），0 档仍是 `·`，且都带 SGR。
    #[test]
    fn colour_mode_paints_solid_blocks_with_five_distinct_inks() {
        let palette = palette();
        let values = [
            (date("2026-10-19"), 10),
            (date("2026-10-20"), 20),
            (date("2026-10-21"), 30),
            (date("2026-10-22"), 40),
        ];
        let lines = Heatmap {
            since: date("2026-10-19"),
            until: date("2026-10-25"),
            values: &values,
            width: 120,
            ink: Some(&palette),
        }
        .render();
        let plain = lines
            .lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<String>>();
        let glyphs: String = plain[1..8]
            .iter()
            .flat_map(|line| line.chars().skip(GUTTER))
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(glyphs, "████···", "{plain:?}");
        // 4 个非零档渲染出 4 种不同的 SGR（0 档那三种共用 dim）；末档就是 accent 本身。
        let sgr = |line: &str| {
            line.split_once("\u{1b}[")
                .and_then(|(_, rest)| rest.split_once('m'))
                .map(|(codes, _)| codes.to_string())
        };
        let tier_inks: Vec<String> = (1..8)
            .filter_map(|index| sgr(&lines.lines[index]))
            .collect();
        assert_eq!(tier_inks.len(), 7, "每格都带 SGR：{:?}", lines.lines);
        let distinct: std::collections::BTreeSet<&String> = tier_inks.iter().collect();
        assert_eq!(distinct.len(), 5, "4 档明度 + dim：{tier_inks:?}");
        assert_eq!(
            mix(palette.surface, palette.accent, TIER_MIX[3]),
            palette.accent,
            "4 档 = accent 本身"
        );
        assert_ne!(
            mix(palette.surface, palette.accent, TIER_MIX[0]),
            palette.accent,
            "1 档要离 accent 足够远"
        );
        assert!(lines.lines[1].contains("\u{1b}["), "有颜色时有 SGR");

        // 关掉颜色（None）时一个转义都不该有。
        assert!(!lines.lines[0].contains('\u{1b}'), "月份行从不着色");
    }

    /// 非 RGB 主题墨色不 panic，退回 accent（配色是用户可改的，混色不能是崩溃点）。
    #[test]
    fn non_rgb_inks_fall_back_to_the_accent() {
        assert_eq!(
            mix(Color::Black, Color::Cyan, 0.5),
            Color::Cyan,
            "非 RGB 一侧 → 直接用 accent"
        );
        assert_eq!(
            mix(Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255), 0.5),
            Color::Rgb(128, 128, 128)
        );
        assert_eq!(
            mix(Color::Rgb(10, 20, 30), Color::Rgb(10, 20, 30), 1.0),
            Color::Rgb(10, 20, 30)
        );
    }
}
