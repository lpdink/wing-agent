//! wordmark —— 手绘 5 行像素大字 `WING` + 横向渐变 + 一道扫光。
//!
//! 字形数据在 [`super::art::WORDMARK`]（`#` = 墨迹）。渐变方向跟主题走：
//! 暗底从"冰白混 accent"收到 accent（左亮右稳），亮底反过来从 accent 压到
//! 深一档 —— 冰白端在亮底上会淡得看不见（创作期画廊 G2 验过）。
//!
//! 扫光是开屏那一道：`phase` 为 `None` 时定格（无高光），之后不再为它重建。
//!
//! 半格像素只能**整数倍**放大，所以字号是档位而不是比例：[`Scale::X2`] 把每个
//! 源像素画成 2×2（46 列 × 5 终端行），[`Scale::X1`] 是原尺寸（23 × 3）。档位由
//! [`scale_for`] 按信息列的可用列数选：放得下 2x 就放大，放不下回退 1x ——
//! 80 列终端（信息列只有 38 列）因此保持原尺寸，不挤不截。

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::art::WORDMARK;
use super::art::WORDMARK_COLS;
use crate::ui::shimmer::Rgb;
use crate::ui::shimmer::mix;
use crate::ui::shimmer::sweep_intensity;

/// 扫光光带半宽（列，源像素口径）。
pub const SWEEP_RADIUS: f32 = 6.0;

/// 高光混白强度（光带正中）。
const SWEEP_WHITE: f32 = 0.65;

/// 暗底渐变的亮端：accent 往白里提这么多。
const DARK_THEME_LIGHT_BLEND: f32 = 0.75;

/// 亮底渐变的暗端：accent 往黑里压这么多。
const LIGHT_THEME_DARK_BLEND: f32 = 0.45;

/// 放大档位。半格像素只能整数倍放大，所以这里是离散的两档，不是比例。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    /// 原尺寸：每个源像素一个半格（23 列 × 3 终端行）。
    X1,
    /// 双倍像素：每个源像素 2×2（46 列 × 5 终端行）。
    X2,
}

impl Scale {
    /// 一个源像素放大成几格（宽高同一倍数 —— 像素保持正方）。
    pub const fn factor(self) -> usize {
        match self {
            Scale::X1 => 1,
            Scale::X2 => 2,
        }
    }

    /// 渲染后的列数。
    pub const fn columns(self) -> usize {
        WORDMARK_COLS * self.factor()
    }

    /// 渲染后的终端行数（5 个源像素行 → 半格两行一格）。
    pub const fn rows(self) -> usize {
        (WORDMARK.len() * self.factor()).div_ceil(2)
    }
}

/// 可用列数 `max_cols` 下该用哪一档：放得下 [`Scale::X2`] 就放大，否则原尺寸。
///
/// 调用方只在信息列真的空着时让大字长大：80 列终端给信息列留 38 列（海鸥 37 +
/// 间隔 3 之外），1x 的 23 列是最坏情况也放得下的宽度 —— 所以每一档都不需要
/// 再截字形。
pub fn scale_for(max_cols: usize) -> Scale {
    if max_cols >= Scale::X2.columns() {
        Scale::X2
    } else {
        Scale::X1
    }
}

/// 第 `col` 列（**源像素**列）的渐变基色（`light` = 亮底主题）。
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

/// 扫光在 `col` 列（**渲染**列）的高光强度（0 = 不在光带里，1 = 正中）。
///
/// 跨度与光带半宽都按档位放大：光带扫过整幅大字的节奏与 1x 一致，放大后不会
/// 显得"光带没变、字变宽了"。通用的那份数学在
/// [`crate::ui::shimmer::sweep_intensity`]（聊天里的思考行也用它）。
fn sweep_at(col: usize, phase: f32, scale: Scale) -> f32 {
    let factor = scale.factor();
    sweep_intensity(
        col as f32,
        phase,
        WORDMARK_COLS as f32 * factor as f32,
        SWEEP_RADIUS * factor as f32,
    )
}

/// 画 wordmark：源网格 → 档位放大的半格渲染。
///
/// `phase`：扫光进度 0..1；`None` = 定格无高光。
pub fn lines(scale: Scale, phase: Option<f32>, accent: Rgb, light: bool) -> Vec<Line<'static>> {
    let factor = scale.factor();
    let rows: Vec<Vec<bool>> = WORDMARK
        .iter()
        .map(|r| r.chars().map(|c| c == '#').collect())
        .collect();
    let mut out = Vec::with_capacity(scale.rows());
    for term_row in 0..scale.rows() {
        // 渲染像素坐标 → 源像素坐标：一个源像素占 `factor` 个渲染像素。
        let up = term_row * 2;
        let lo = up + 1;
        // 行尾透明格不画（与 `sprite::lines` 同一条瘦身）：不裁的话每行都是满宽
        // 空格，字形右边界就测不出来了。
        let ink_end = (0..scale.columns())
            .rev()
            .find(|&col| ink_at(&rows, col, up, factor) || ink_at(&rows, col, lo, factor))
            .map_or(0, |col| col + 1);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run: Option<(Style, String)> = None;
        for col in 0..ink_end {
            let u = ink_at(&rows, col, up, factor);
            let l = ink_at(&rows, col, lo, factor);
            if !u && !l {
                // 透明格不并入 run（它没有样式可言）。
                if let Some((style, text)) = run.take() {
                    spans.push(Span::styled(text, style));
                }
                spans.push(Span::raw(" "));
                continue;
            }
            let mut rgb = column_color(col / factor, accent, light);
            if let Some(phase) = phase {
                let s = sweep_at(col, phase, scale);
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

/// 渲染像素 `(px_col, px_row)` 处有没有墨迹（`factor` = 一个源像素几格）。
fn ink_at(rows: &[Vec<bool>], px_col: usize, px_row: usize, factor: usize) -> bool {
    rows.get(px_row / factor)
        .and_then(|r| r.get(px_col / factor))
        .copied()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::shimmer::luminance;

    /// 一行的显示宽度（半格：每格 1 列）。
    fn line_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    /// 画出来的源像素数（半格渲染里：带背景的格 = 上下两个像素，只带前景 = 1）。
    fn rendered_pixels(lines: &[Line<'_>]) -> usize {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| {
                        let ink = s.content.chars().filter(|c| *c != ' ').count();
                        ink * if s.style.bg.is_some() { 2 } else { 1 }
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    /// 源网格的墨迹像素数。
    fn source_ink() -> usize {
        WORDMARK
            .iter()
            .map(|r| r.chars().filter(|c| *c == '#').count())
            .sum()
    }

    #[test]
    fn scale_ladder_picks_the_largest_that_fits() {
        // 1x 的 23 列，2x 的 46 列；一个源像素放大成 factor 格。
        assert_eq!(scale_for(22), Scale::X1);
        assert_eq!(scale_for(Scale::X1.columns()), Scale::X1);
        assert_eq!(scale_for(Scale::X2.columns() - 1), Scale::X1);
        assert_eq!(scale_for(Scale::X2.columns()), Scale::X2);
        assert_eq!(scale_for(200), Scale::X2);

        assert_eq!(Scale::X1.columns(), WORDMARK_COLS);
        assert_eq!(Scale::X2.columns(), WORDMARK_COLS * 2);
        assert_eq!(Scale::X1.rows(), 3);
        assert_eq!(Scale::X2.rows(), 5);
    }

    #[test]
    fn three_terminal_rows_at_1x_and_five_at_2x() {
        assert_eq!(lines(Scale::X1, None, (34, 211, 238), false).len(), 3);
        assert_eq!(lines(Scale::X1, Some(0.5), (34, 211, 238), true).len(), 3);
        assert_eq!(lines(Scale::X2, None, (34, 211, 238), false).len(), 5);
        assert_eq!(lines(Scale::X2, Some(0.5), (34, 211, 238), true).len(), 5);
    }

    #[test]
    fn every_source_pixel_survives_the_scaling() {
        // 放大是"每个源像素画成 factor×factor"，不是重画：把画出来的源像素数
        // （带背景的格算两个）钉死 —— 少一个就是丢像素。
        assert_eq!(
            rendered_pixels(&lines(Scale::X1, None, (34, 211, 238), false)),
            source_ink()
        );
        assert_eq!(
            rendered_pixels(&lines(Scale::X2, None, (34, 211, 238), false)),
            source_ink() * 4,
            "2x 每个源像素该占 2×2"
        );
    }

    #[test]
    fn scaled_rows_are_exactly_the_scaled_glyph_width() {
        // 行尾透明格裁掉（与 `sprite` 同一瘦身）：最宽的一行必须铺满档位宽度。
        // 只断言行数会漏掉"横向没放大"—— 所以口径是**墨迹右边界**，不是行数。
        for scale in [Scale::X1, Scale::X2] {
            let lines = lines(scale, None, (34, 211, 238), false);
            let widest = lines.iter().map(line_width).max().unwrap_or(0);
            assert_eq!(widest, scale.columns(), "{scale:?}: 字形没有铺满档位宽度");
            for line in &lines {
                assert!(
                    line_width(line) <= scale.columns(),
                    "{scale:?}: 行宽超出档位"
                );
            }
        }
    }

    #[test]
    fn sweep_brightens_the_band() {
        // 口径必须是**求和**：单行最大 luma 恒真是因为渐变亮端（第 0 列）本来
        // 就是全行最亮，光带中心追不上它 —— 那样扫光坏了测试也不会红。
        for scale in [Scale::X1, Scale::X2] {
            let accent = (34, 211, 238);
            let settled = lines(scale, None, accent, false);
            let swept = lines(scale, Some(0.5), accent, false);
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
                    "{scale:?} 第 {row} 行：光带扫过时整行应当更亮（{} vs {}）",
                    total(&swept[row]),
                    total(&settled[row])
                );
            }
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
