//! 日历热力图：7 行（周一..周日）× N 列（ISO 周）的格子网格。
//!
//! 纯渲染：输入一段闭区间与「每天的一个数」，输出可直接打印的等宽文本行（可测、无 IO）。
//! 两个产品口径钉在这里：
//!
//! - **档位是相对量**（非零日的四分位，GitHub 同款思想）：一天跑了 200 次调用不会把其余
//!   全部压成最浅一档。绝对值永远可从 `wing usage --by day` / `--json` 读到；
//! - **范围外与「范围内但没活动」不能同形**：前者是空格（那一天不属于这次查询），后者是
//!   最浅档 `·`。把两者画成一样会让「我的范围选对了吗」无从判断。
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

/// 渲染结果：行 + 实际画出的列数（供调用方注明「只画了最近 N 周」）。
pub struct HeatmapLines {
    pub lines: Vec<String>,
    pub columns: usize,
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
                let value = (date >= self.since && date <= self.until)
                    .then(|| by_day.get(&date).copied().unwrap_or(0));
                cells.push(self.cell(value, &thresholds));
            }
            // 行尾空格没有意义（末列落在范围外时尤其明显）：裁掉，行宽仍由格子数决定。
            lines.push(
                format!(
                    "{}{}",
                    row_label(row),
                    join_cells(&cells, self.ink.is_some())
                )
                .trim_end()
                .to_string(),
            );
        }
        let legend = self.legend_line();
        if !legend.is_empty() {
            lines.push(legend);
        }
        HeatmapLines {
            lines,
            columns: shown,
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
    fn cell(&self, value: Option<i64>, thresholds: &[i64; 3]) -> (char, Option<Color>) {
        let Some(value) = value else {
            return (' ', None);
        };
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
        // 月份标签可以长得比格子宽（末列的那一个月就是这种情况），只要不出终端预算。
        let cells_width = cell_row_width(shown + usize::from(marker));
        let mut row: Vec<char> = vec![' '; self.width.max(GUTTER + cells_width)];
        let mut next_free_slot = 0usize;
        for slot in 0..shown {
            let monday = first_monday + Duration::days(((offset + slot) * 7) as i64);
            // 首列也优先标「这一周里的 1 号」：窗口从月末开始时（`--since 7-31`），
            // 标 `Jul` 会让占满整列的八月无名——那一列真正属于八月。
            let in_week = (0..7)
                .map(|day| monday + Duration::days(day))
                .find(|day| day.day() == 1);
            let anchor = in_week.or_else(|| (slot == 0).then_some(monday));
            let Some(anchor) = anchor else { continue };
            if slot < next_free_slot {
                continue;
            }
            let start = GUTTER + cell_offset + slot * 2;
            let label = MONTH_NAMES[anchor.month0() as usize];
            if start + label.chars().count() > row.len() {
                continue;
            }
            for (index, ch) in label.chars().enumerate() {
                row[start + index] = ch;
            }
            next_free_slot = slot + MONTH_SLOT_GAP;
        }
        row.into_iter().collect::<String>().trim_end().to_string()
    }

    /// 图例行：`less · ░ ▒ ▓ █ more`（有颜色时同样着色）。
    ///
    /// 固定宽度的一句话：窄到装不下就整行不画（半截图例比没有更误导）。
    fn legend_line(&self) -> String {
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
        format!(
            "{}less {} more",
            " ".repeat(GUTTER),
            join_cells(&cells, self.ink.is_some())
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
        // 只有周三那一行有格子（其余是范围外的空格）。
        assert_eq!(rows[0], "", "{rows:?}");
        assert_eq!(rows[2], "░", "周三的格子落在第 3 行：{rows:?}");
        // 其余行是范围外（空格），不是 `·`。
        assert!(rows.iter().all(|row| row != "·"), "{rows:?}");
        // 四行标尺在（整行没有格子时行尾空格被裁掉，所以不锚定分隔空格）。
        assert!(lines.lines[1].starts_with("  Mon"));
        assert!(lines.lines[3].starts_with("  Wed"));
        assert!(lines.lines[5].starts_with("  Fri"));
        assert!(lines.lines[7].starts_with("  Sun"));
    }

    /// 范围外 = 空格；范围内没活动 = `·`。两者不能同形。
    #[test]
    fn out_of_range_cells_are_blank_while_quiet_days_are_the_light_dot() {
        let lines = render("2026-10-19", "2026-10-25", &[(date("2026-10-20"), 7)]);
        assert_eq!(lines.columns, 1);
        let rows = rows(&lines.lines);
        assert_eq!(rows[1], "░", "周二有活动：{rows:?}");
        // 周三..周日都在范围内但没有活动 → `·`。
        assert_eq!(rows[2], "·", "{rows:?}");
        assert_eq!(rows[6], "·", "{rows:?}");
        // 周一在范围内同样没活动（不是空格）。
        assert_eq!(rows[0], "·", "{rows:?}");
    }

    #[test]
    fn the_window_spans_iso_weeks_and_counts_them() {
        // 2026-08-03（周一）→ 2026-10-25（周日）正好 12 个 ISO 周。
        let lines = render("2026-08-03", "2026-10-25", &[(date("2026-08-03"), 1)]);
        assert_eq!(lines.columns, 12);
        assert_eq!(rows(&lines.lines)[0].chars().count(), 12, "周一那行 12 格");
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
                unicode_width(line) <= 27,
                "不越预算: {line:?} ({} > 27)",
                unicode_width(line)
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
            assert!(unicode_width(line) <= GUTTER + 2, "{line:?}");
        }
    }

    /// 图例与月份行都在（图例是「档位是什么意思」的唯一说明）。
    #[test]
    fn the_legend_names_the_five_tiers() {
        let lines = render("2026-10-19", "2026-10-25", &[(date("2026-10-20"), 3)]);
        let legend = lines.lines.last().unwrap();
        assert_eq!(legend, "      less · ░ ▒ ▓ █ more");
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

    fn unicode_width(text: &str) -> usize {
        use unicode_width::UnicodeWidthStr;
        UnicodeWidthStr::width(strip_ansi(text).as_str())
    }

    /// 去掉 SGR 转义（只用于断言可见宽度与字形）。
    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch != '\u{1b}' {
                out.push(ch);
                continue;
            }
            // `\u{1b}[<codes>m`：吞到 `m` 为止。
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
        }
        out
    }
}
