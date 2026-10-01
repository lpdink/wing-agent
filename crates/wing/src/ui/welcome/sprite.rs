//! 半格像素精灵渲染器 —— 字母网格 → 终端 `Line`。
//!
//! 与开屏旧版像素 W 同一套半格语义（见仓库历史）：一个字符格装上下两个像素，
//! **上像素 = 前景、下像素 = 背景**；`▀` 画上半、`▄` 画下半、两像素都在也写 `▀`
//! （上下各拿一个颜色，格子内部的色彩过渡因此是半格粒度）。只画半格的格子
//! **只设前景** —— 给 `▄` 配背景会把本该透明的上半整格填成对面那半的颜色。
//!
//! 调色板是**固定品牌色**（白羽 / 石板灰 / 深藏青描边 / 琥珀喙），不跟主题走 ——
//! 海鸥之所以是海鸥，靠的就是这身颜色；唯一跟主题呼吸的是 `A`（accent 点缀），
//! 以及 wordmark 的渐变（见 [`super::wordmark`]）。亮色终端下深藏青描边替暗底
//! 干活，剪影照样立得住（创作期画廊验过）。

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// RGB 三元组。
pub type Rgb = (u8, u8, u8);

/// 字母 → 品牌色。`None` = 透明（`.`）。`A` = 主题 accent，渲染期填入。
///
/// 未知字母返回 `None`：帧数据写错时画成透明，比 panic 或画成黑块体面；
/// 帧数据的字母表由单测钉死（见 `mod.rs` 的 `art_letters_are_in_palette`）。
pub fn brand(letter: char, accent: Rgb) -> Option<Rgb> {
    Some(match letter {
        '.' => return None,
        'O' => (27, 35, 56),    // 描边：深藏青，不是纯黑
        'W' => (247, 249, 252), // 羽白
        'G' => (201, 210, 224), // 浅灰（mantle / 折翼）
        'S' => (142, 155, 176), // 石板灰（翼下缘 / 尾下）
        'D' => (58, 71, 99),    // 深石板（翼尖 / 尾尖）
        'B' => (245, 169, 60),  // 喙 / 脚：琥珀
        'M' => (199, 126, 34),  // 琥珀暗面
        'E' => (20, 28, 51),    // 眼
        'H' => (255, 255, 255), // 眼神光
        'R' => (224, 82, 63),   // 喙上红点（预留）
        'A' => accent,
        _ => return None,
    })
}

/// 一帧字母网格 → 终端行，带补行：
///
/// * `pad_top`：顶部补这么多**终端行**的透明（飞行姿态比站姿矮，居中靠它）；
/// * `term_rows`：总终端行数（不足在底部补透明）—— 两个姿态对齐到同一高度，
///   姿态切换时 header 不跳版。
pub fn lines_padded(
    grid: &[&str],
    accent: Rgb,
    row_shift: i32,
    pad_top: usize,
    term_rows: usize,
) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(""); pad_top.min(term_rows)];
    out.extend(lines(grid, accent, row_shift));
    out.resize_with(term_rows, Line::default);
    out
}

/// 一帧字母网格 → 终端行（`grid.len()` 个像素行 → `ceil(len/2)` 个终端行）。
///
/// `row_shift`：整帧竖直偏移（**像素行**，负 = 上移）—— 跳起帧不额外存一份
/// 网格，把标准姿势抬几行就是跳。移出去的行丢弃，空出来的行补透明。
///
/// 行尾透明格截掉、相邻同色格合并成一个 span：与旧版像素 W 同样的两条瘦身。
pub fn lines(grid: &[&str], accent: Rgb, row_shift: i32) -> Vec<Line<'static>> {
    let term_rows = grid.len().div_ceil(2);
    let cols = grid.iter().map(|r| r.chars().count()).max().unwrap_or(0);
    let mut out = Vec::with_capacity(term_rows);
    for term_row in 0..term_rows {
        let up_row = term_row as i32 * 2 - row_shift;
        let lo_row = up_row + 1;
        // 本行最后一个墨迹格：行尾透明不画。
        let ink_end = (0..cols)
            .rev()
            .find(|&col| pixel(grid, col, up_row).is_some() || pixel(grid, col, lo_row).is_some())
            .map_or(0, |col| col + 1);

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run: Option<(Style, String)> = None;
        for col in 0..ink_end {
            let (glyph, style) = match (pixel(grid, col, up_row), pixel(grid, col, lo_row)) {
                (Some(u), Some(l)) => (
                    '▀',
                    Style::default().fg(color(u, accent)).bg(color(l, accent)),
                ),
                (Some(u), None) => ('▀', Style::default().fg(color(u, accent))),
                (None, Some(l)) => ('▄', Style::default().fg(color(l, accent))),
                (None, None) => (' ', Style::default()),
            };
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

/// 像素行 `row`（已含偏移）第 `col` 格的字母；越界 / 透明 = `None`。
fn pixel(grid: &[&str], col: usize, row: i32) -> Option<char> {
    let raw = grid.get(row as usize)?;
    let ch = raw.chars().nth(col)?;
    (ch != '.').then_some(ch)
}

fn color(letter: char, accent: Rgb) -> Color {
    match brand(letter, accent) {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Reset,
    }
}

/// 两个颜色按 `t`（0 = a，1 = b）线性混合。
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// 相对亮度（0..1 近似）—— 判断主题是亮是暗，wordmark 渐变方向跟着走。
pub fn luminance(rgb: Rgb) -> f32 {
    (0.2126 * rgb.0 as f32 + 0.7152 * rgb.1 as f32 + 0.0722 * rgb.2 as f32) / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: &[&str] = &[".WW.", ".WW.", "OOOO"];

    #[test]
    fn full_cell_uses_fg_and_bg() {
        let lines = lines(TINY, (0, 0, 0), 0);
        assert_eq!(lines.len(), 2, "3 像素行 -> 2 终端行");
        let span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains('▀'))
            .expect("有墨迹");
        assert_eq!(span.content.as_ref(), "▀▀", "同色合并成一个 span");
        assert!(matches!(span.style.fg, Some(Color::Rgb(247, 249, 252))));
        assert!(matches!(span.style.bg, Some(Color::Rgb(247, 249, 252))));
    }

    #[test]
    fn half_cell_has_no_background() {
        // 第 2 终端行：上像素 = 描边、下像素透明 -> 只有前景。
        let span = &lines(TINY, (0, 0, 0), 0)[1].spans[0];
        assert_eq!(span.content.as_ref(), "▀▀▀▀");
        assert!(span.style.bg.is_none(), "半格不能带背景");
    }

    #[test]
    fn row_shift_lifts_the_sprite() {
        let idle = lines(TINY, (0, 0, 0), 0);
        let hop = lines(TINY, (0, 0, 0), -2);
        assert_eq!(idle.len(), hop.len());
        // 上移两像素行：描边行顶到第 0 终端行的下半，末行全空。
        assert!(hop[1].spans.is_empty() || hop[1].spans.iter().all(|s| s.content.is_empty()));
        let first = &hop[0].spans[0];
        assert!(first.content.as_ref().contains('▄') || first.content.as_ref().contains('▀'));
    }

    #[test]
    fn trailing_transparent_is_trimmed() {
        let lines = lines(&[".W..", "...."], (0, 0, 0), 0);
        let width: usize = lines[0]
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(width, 2, "行宽截到最后一个墨迹格");
    }

    #[test]
    fn unknown_letters_are_transparent() {
        assert!(brand('?', (1, 2, 3)).is_none());
        assert!(brand('.', (1, 2, 3)).is_none());
        assert!(brand('W', (1, 2, 3)).is_some());
        assert_eq!(brand('A', (9, 8, 7)), Some((9, 8, 7)));
    }
}
