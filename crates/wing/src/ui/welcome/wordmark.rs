//! wordmark —— 手绘 5 行像素大字 `WING` + 横向渐变 + 一道扫光。
//!
//! 字形数据在 [`super::art::WORDMARK`]（`#` = 墨迹）。渐变方向跟主题走：
//! 暗底从"冰白混 accent"收到 accent（左亮右稳），亮底反过来从 accent 压到
//! 深一档 —— 冰白端在亮底上会淡得看不见（创作期画廊 G2 验过）。
//!
//! 扫光是开屏那一道：`phase` 为 `None` 时定格（无高光），之后不再为它重建。

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::art::WORDMARK;
use super::art::WORDMARK_COLS;
use crate::ui::shimmer::Rgb;
use crate::ui::shimmer::mix;
use crate::ui::shimmer::sweep_intensity;

/// 扫光光带半宽（列）。
pub const SWEEP_RADIUS: f32 = 6.0;

/// 高光混白强度（光带正中）。
const SWEEP_WHITE: f32 = 0.65;

/// 暗底渐变的亮端：accent 往白里提这么多。
const DARK_THEME_LIGHT_BLEND: f32 = 0.75;

/// 亮底渐变的暗端：accent 往黑里压这么多。
const LIGHT_THEME_DARK_BLEND: f32 = 0.45;

/// 第 `col` 列的渐变基色（`light` = 亮底主题）。
///
/// 公开给品牌资产导出器（`examples/export_logo.rs`）：SVG 里的 wordmark 与终端里
/// 的必须是同一套渐变，否则 README 与真机漂移。
pub fn column_color(col: usize, accent: Rgb, light: bool) -> Rgb {
    let t = col as f32 / (WORDMARK_COLS.saturating_sub(1)).max(1) as f32;
    if light {
        mix(accent, (10, 14, 24), LIGHT_THEME_DARK_BLEND * t)
    } else {
        mix(
            mix(accent, (255, 255, 255), DARK_THEME_LIGHT_BLEND),
            accent,
            t,
        )
    }
}

/// 扫光在 `col` 列的高光强度（0 = 不在光带里，1 = 正中）。
///
/// wordmark 的跨度就是字形宽度；通用的那份数学在
/// [`crate::ui::shimmer::sweep_intensity`]（聊天里的思考行也用它）。
fn sweep_at(col: usize, phase: f32) -> f32 {
    sweep_intensity(col as f32, phase, WORDMARK_COLS as f32, SWEEP_RADIUS)
}

/// 画 wordmark：5 像素行 → 3 终端行（半格）。
///
/// `phase`：扫光进度 0..1；`None` = 定格无高光。
pub fn lines(phase: Option<f32>, accent: Rgb, light: bool) -> Vec<Line<'static>> {
    let rows: Vec<Vec<bool>> = WORDMARK
        .iter()
        .map(|r| r.chars().map(|c| c == '#').collect())
        .collect();
    let term_rows = rows.len().div_ceil(2);
    let mut out = Vec::with_capacity(term_rows);
    for t in 0..term_rows {
        let up = rows.get(t * 2);
        let lo = rows.get(t * 2 + 1);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run: Option<(Style, String)> = None;
        for col in 0..WORDMARK_COLS {
            let u = up.is_some_and(|r| *r.get(col).unwrap_or(&false));
            let l = lo.is_some_and(|r| *r.get(col).unwrap_or(&false));
            if !u && !l {
                // 透明格不并入 run（它没有样式可言）。
                if let Some((style, text)) = run.take() {
                    spans.push(Span::styled(text, style));
                }
                spans.push(Span::raw(" "));
                continue;
            }
            let mut rgb = column_color(col, accent, light);
            if let Some(phase) = phase {
                let s = sweep_at(col, phase);
                if s > 0.0 {
                    rgb = mix(rgb, (255, 255, 255), SWEEP_WHITE * s);
                }
            }
            let fg = Color::Rgb(rgb.0, rgb.1, rgb.2);
            let style = match (u, l) {
                (true, true) => Style::default().fg(fg).bg(fg),
                (true, false) => Style::default().fg(fg),
                (false, true) => Style::default().fg(fg),
                (false, false) => unreachable!(),
            };
            let glyph = if l { '▄' } else { '▀' };
            match &mut run {
                Some((current, text)) if *current == style => text.push(glyph),
                Some(_) => {
                    let (done, text) = run.take().expect("run 在手");
                    spans.push(Span::styled(text, done));
                    run = Some((style, glyph.to_string()));
                }
                None => run = Some((style, glyph.to_string())),
            }
        }
        if let Some((style, text)) = run {
            spans.push(Span::styled(text, style));
        }
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::shimmer::luminance;

    #[test]
    fn three_terminal_rows() {
        assert_eq!(lines(None, (34, 211, 238), false).len(), 3);
        assert_eq!(lines(Some(0.5), (34, 211, 238), true).len(), 3);
    }

    #[test]
    fn sweep_brightens_the_band() {
        // 口径必须是**求和**：单行最大 luma 恒真是因为渐变亮端（第 0 列）本来
        // 就是全行最亮，光带中心追不上它 —— 那样扫光坏了测试也不会红。
        let accent = (34, 211, 238);
        let settled = lines(None, accent, false);
        let swept = lines(Some(0.5), accent, false);
        let total = |line: &Line<'_>| -> u32 {
            line.spans
                .iter()
                .filter_map(|s| s.style.fg)
                .map(|c| match c {
                    Color::Rgb(r, g, b) => r as u32 + g as u32 + b as u32,
                    _ => 0,
                })
                .sum()
        };
        for row in 0..settled.len() {
            assert!(
                total(&swept[row]) > total(&settled[row]),
                "第 {row} 行：光带扫过时整行应当更亮（{} vs {}）",
                total(&swept[row]),
                total(&settled[row])
            );
        }
    }

    #[test]
    fn light_theme_darkens_the_tail() {
        let accent = (34, 211, 238);
        let dark_last = column_color(WORDMARK_COLS - 1, accent, false);
        let light_last = column_color(WORDMARK_COLS - 1, accent, true);
        assert!(
            luminance(light_last) < luminance(dark_last),
            "亮底右端要压暗，否则淡得看不见"
        );
    }
}
