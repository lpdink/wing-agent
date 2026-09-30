//! 欢迎屏的像素 "W" 标记。
//!
//! 字形是手绘的半格像素画：每个字符格装上下两个像素，`▀` 只画上半、`▄` 只画
//! 下半、`█` 两个都画、空格透明。每格的前景 / 背景各取一个像素的颜色，于是
//! **颜色也按半格走**——这正是它比"纯色 `█` 拼字" 精致的原因：对角线平滑，
//! 颜色沿对角线从亮到暗铺开。
//!
//! 改字形只需要动 `WING_ART` 这几行字符串（列数 = `ART_COLS`）。

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::Sweep;

/// 字形：7 行 × 26 列。空格 = 透明，`▀` / `▄` / `█` = 上半 / 下半 / 整格像素。
const WING_ART: &[&str] = &[
    " ▄█▄                   ▄█▄",
    " ▀███▄      ▄█▄      ▄███▀",
    "   ███▄    █████    ▄███",
    "    ▀███  ███▀███  ███▀",
    "     ▀███▄███ ███▄███▀",
    "      ▀█████   █████▀",
    "        ▀█▀     ▀█▀",
];

/// 标记占用的列数（比最宽的字形行多留一列，右侧文字列的起点因此写死）。
pub const ART_COLS: usize = 27;

/// 标记占用的行数。
pub const ART_ROWS: usize = WING_ART.len();

/// 标记与右侧文字列之间的空列数。
pub const ART_GAP: usize = 3;

/// 一格的墨迹：上半 / 下半像素在不在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    upper: bool,
    lower: bool,
}

impl Cell {
    /// 字符 → 墨迹。`None` = 未知字符（字形写错了，测试会拦）。
    fn parse(ch: char) -> Option<Self> {
        match ch {
            ' ' => Some(Self {
                upper: false,
                lower: false,
            }),
            '▀' => Some(Self {
                upper: true,
                lower: false,
            }),
            '▄' => Some(Self {
                upper: false,
                lower: true,
            }),
            '█' => Some(Self {
                upper: true,
                lower: true,
            }),
            _ => None,
        }
    }

    /// 该格画出来的字符：整格也走 `▀` —— 上下两个像素各拿一个颜色，格子内部的
    /// 渐变因此是半格粒度（`█` 会把下半像素的颜色吃掉）。
    fn glyph(self) -> char {
        match (self.upper, self.lower) {
            (true, _) => '▀',
            (false, true) => '▄',
            (false, false) => ' ',
        }
    }
}

/// RGB 三元组。
pub(super) type Rgb = (u8, u8, u8);

/// 纯白——扫光高光的目标色。
const WHITE: Rgb = (255, 255, 255);

/// 渐变的中间停靠点：对角线位置 0 到它之间从高亮色收敛到主题色，再往后一路
/// 压暗到 `gradient_dark`。
const GRADIENT_MID: f32 = 0.55;

/// 高亮端混白多少。三个数（亮端 / 暗端 / 权重）就是整个标记的"力度"旋钮 ——
/// 太小时看上去仍像一块纯色。
const GRADIENT_LIGHT_BLEND: f32 = 0.55;

/// 压暗端混黑多少。
const GRADIENT_DARK_BLEND: f32 = 0.40;

/// 对角线的横向权重（纵向 = 1 - 它）。横向多一点，左上角那束光才明显。
const GRADIENT_AXIS_X: f32 = 0.60;

/// 高亮端：主题色往白里提一档。
fn gradient_light(accent: Rgb) -> Rgb {
    mix(accent, WHITE, GRADIENT_LIGHT_BLEND)
}

/// 压暗端：主题色往黑里压一档。
fn gradient_dark(accent: Rgb) -> Rgb {
    mix(accent, (0, 0, 0), GRADIENT_DARK_BLEND)
}

/// 两个颜色按 `t`（0 = a，1 = b）线性混合。
pub(super) fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// 主题色 → RGB。命名色按 xterm 调色板的近似值展开：渐变需要数值，而
/// `Color::Rgb` 是 ratatui 唯一能表达中间色的形式。
pub(super) fn to_rgb(color: Color) -> Rgb {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (229, 229, 229),
        Color::DarkGray => (102, 102, 102),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (255, 255, 255),
        // Reset / 索引色没有可用的数值：当作白，渐变退化成单色，不至于画不出来。
        _ => (255, 255, 255),
    }
}

/// 画一帧要用到的常量：渐变的三段色 + 像素尺寸。
///
/// 打包成一个值，是为了让"取某一格颜色"只需要坐标 —— 否则每层调用都要把五个
/// 颜色 / 尺寸参数一路传下去。
#[derive(Debug, Clone, Copy)]
struct Canvas {
    accent: Rgb,
    light: Rgb,
    dark: Rgb,
    /// 像素列数。
    width: usize,
    /// 像素行数（= 显示行 × 2）。
    height: usize,
}

impl Canvas {
    /// 按主题色铺开三段渐变：高亮（左上）→ 主题色 → 压暗（右下）。
    fn new(accent: Color) -> Self {
        let accent = to_rgb(accent);
        Self {
            accent,
            light: gradient_light(accent),
            dark: gradient_dark(accent),
            width: ART_COLS,
            height: ART_ROWS * 2,
        }
    }

    /// 对角渐变的位置参数（0 = 左上，1 = 右下）。
    ///
    /// 竖直方向按**像素行**归一化：半格像素只占半行，这样铺出来的才是视觉上的
    /// 对角线（不然渐变会明显偏扁）。
    fn diagonal(&self, col: usize, pixel_row: usize) -> f32 {
        let x = col as f32 / (self.width - 1).max(1) as f32;
        let y = pixel_row as f32 / (self.height - 1).max(1) as f32;
        GRADIENT_AXIS_X * x + (1.0 - GRADIENT_AXIS_X) * y
    }

    /// 一个像素的颜色：对角线渐变 + 叠扫光高光。
    fn pixel(&self, col: usize, pixel_row: usize, sweep: Option<&Sweep>) -> Color {
        let t = self.diagonal(col, pixel_row);
        let mut rgb = if t < GRADIENT_MID {
            mix(self.light, self.accent, t / GRADIENT_MID)
        } else {
            mix(
                self.accent,
                self.dark,
                (t - GRADIENT_MID) / (1.0 - GRADIENT_MID),
            )
        };
        if let Some(sweep) = sweep {
            let strength = sweep.at(col as f32);
            if strength > 0.0 {
                rgb = mix(rgb, WHITE, 0.55 * strength);
            }
        }
        Color::Rgb(rgb.0, rgb.1, rgb.2)
    }

    /// 一格的样式：上面那半取像素行 `pixel_row`、下面那半取 `pixel_row + 1`。
    ///
    /// `█` 走出来是 `▀`：上半像素当前景、下半像素当背景 —— 半格渐变就藏在这里。
    fn cell(&self, cell: &Cell, col: usize, pixel_row: usize, sweep: Option<&Sweep>) -> Style {
        if !cell.upper && !cell.lower {
            return Style::default();
        }
        let upper = self.pixel(col, pixel_row, sweep);
        let lower = self.pixel(col, pixel_row + 1, sweep);
        if cell.lower {
            Style::default().fg(upper).bg(lower)
        } else {
            Style::default().fg(upper)
        }
    }
}

/// 画出标记：行尾的透明格截掉，相邻同色的格子合并成一个 span。
pub(super) fn lines(accent: Color, sweep: Option<&Sweep>) -> Vec<Line<'static>> {
    let canvas = Canvas::new(accent);
    let mut out = Vec::with_capacity(ART_ROWS);
    for (row, raw) in WING_ART.iter().enumerate() {
        let cells: Vec<Cell> = raw
            .chars()
            .map(|c| Cell::parse(c).expect("字形只允许 ' ' / '▀' / '▄' / '█'"))
            .collect();
        let ink_end = cells
            .iter()
            .rposition(|c| c.upper || c.lower)
            .map_or(0, |i| i + 1);

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run: Option<(Style, String)> = None;
        for (col, cell) in cells.iter().enumerate().take(ink_end) {
            let style = canvas.cell(cell, col, row * 2, sweep);
            match &mut run {
                Some((current, text)) if *current == style => text.push(cell.glyph()),
                Some(_) => {
                    let (style_done, text) = run.take().expect("run 在手");
                    spans.push(Span::styled(text, style_done));
                    run = Some((style, cell.glyph().to_string()));
                }
                None => run = Some((style, cell.glyph().to_string())),
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
    use ratatui::style::Color;

    fn accent() -> Color {
        Color::Rgb(0, 188, 212)
    }

    #[test]
    fn art_rows_are_inside_the_box() {
        for row in WING_ART {
            assert!(
                row.chars().count() <= ART_COLS,
                "字形行超出 {ART_COLS} 列：{row:?}"
            );
        }
        assert_eq!(ART_ROWS, 7, "布局里的文字列行号跟着字形行数走");
    }

    #[test]
    fn art_uses_only_known_glyphs() {
        for row in WING_ART {
            for ch in row.chars() {
                assert!(Cell::parse(ch).is_some(), "未知字形字符 {ch:?} in {row:?}");
            }
        }
    }

    #[test]
    fn art_has_no_trailing_ink_gaps() {
        // 每行画出来的宽度都等于最后一个墨迹格 —— 行尾的透明格会被截掉。
        let lines = lines(accent(), None);
        assert_eq!(lines.len(), ART_ROWS);
        let ink = |row: &str| match row.rfind(|c| c != ' ') {
            Some(byte) => row[..byte].chars().count() + 1,
            None => 0,
        };
        for (line, raw) in lines.iter().zip(WING_ART) {
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert_eq!(width, ink(raw), "行宽应当截到最后一个墨迹格");
        }
    }

    #[test]
    fn gradient_is_brighter_at_the_top_left() {
        let lines = lines(accent(), None);
        // 行首 / 行尾的透明格没有前景色 —— 取两端第一个着色的 span。
        let first = lines[0]
            .spans
            .iter()
            .find_map(|s| s.style.fg)
            .expect("左上角有墨迹");
        let last = lines[ART_ROWS - 1]
            .spans
            .iter()
            .rev()
            .find_map(|s| s.style.fg)
            .expect("右下角有墨迹");
        let luma = |c: Color| match c {
            Color::Rgb(r, g, b) => r as u32 + g as u32 + b as u32,
            other => panic!("渐变出的是 RGB：{other:?}"),
        };
        assert!(
            luma(first) > luma(last),
            "左上应当比右下亮：{first:?} vs {last:?}"
        );
    }

    #[test]
    fn solid_cells_carry_two_pixel_colors() {
        // `█` 的格子要同时给出前景（上半）与背景（下半），否则半格渐变没了。
        let lines = lines(accent(), None);
        let two_tone = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains('▀') && s.style.bg.is_some() && s.style.fg != s.style.bg);
        assert!(two_tone, "整格像素应当走双色半格");
    }

    #[test]
    fn sweep_brightens_the_illuminated_column() {
        let plain = lines(accent(), None);
        let lit = lines(
            accent(),
            Some(&Sweep {
                phase: 0.5,
                span: 60.0,
            }),
        );
        let sum = |lines: &[Line<'static>]| -> u32 {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .filter_map(|s| s.style.fg)
                .map(|c| match c {
                    Color::Rgb(r, g, b) => r as u32 + g as u32 + b as u32,
                    _ => 0,
                })
                .sum()
        };
        assert!(sum(&lit) > sum(&plain), "扫光扫过时整体应当更亮");
    }

    #[test]
    fn mix_clamps_and_hits_both_ends() {
        assert_eq!(mix((0, 0, 0), (100, 200, 40), 0.0), (0, 0, 0));
        assert_eq!(mix((0, 0, 0), (100, 200, 40), 1.0), (100, 200, 40));
        assert_eq!(mix((0, 0, 0), (100, 200, 40), 5.0), (100, 200, 40));
        assert_eq!(mix((10, 10, 10), (20, 20, 20), 0.5), (15, 15, 15));
    }

    #[test]
    fn named_colors_expand_to_xterm_values() {
        assert_eq!(to_rgb(Color::Cyan), (17, 168, 205));
        assert_eq!(to_rgb(Color::Rgb(1, 2, 3)), (1, 2, 3));
    }
}
