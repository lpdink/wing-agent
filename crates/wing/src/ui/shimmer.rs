//! 扫光 / 混色原语 —— 开屏 wordmark 与聊天里的思考行共用的一份数学。
//!
//! 「一道光带扫过文本」= 逐列算强度 + 朝高光色混色。强度曲线（[`sweep_intensity`]）、
//! 混色（[`mix`]）、主题明暗判断（[`is_light_theme`]）收在这里，两个消费者只提供
//! 自己的「跨度 / 半径 / 高光色」。各写一份的下场是手感对不上：一边调了另一边不跟。

use ratatui::style::Color;

/// RGB 三元组。
pub type Rgb = (u8, u8, u8);

/// 两个颜色按 `t`（0 = a，1 = b）线性混合。
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// 相对亮度（0..1 近似）—— 判断主题是亮是暗。
pub fn luminance(rgb: Rgb) -> f32 {
    (0.2126 * rgb.0 as f32 + 0.7152 * rgb.1 as f32 + 0.0722 * rgb.2 as f32) / 255.0
}

/// 主题是不是亮底（按 `text` 色亮度判：亮底主题的正文字是深的）。
///
/// 扫光方向的唯一判据：暗底朝白提，亮底朝深压 —— 亮底上白光看不见。
pub fn is_light_theme(text: Rgb) -> bool {
    luminance(text) < 0.5
}

/// 扫光在 `col` 列的高光强度（0 = 不在光带里，1 = 正中）。
///
/// 光带从 `-radius` 扫到 `span + radius`：`phase` 0..1 走完全程，两端各留一个
/// 半径的出入场；`span` 是被扫文本的显示宽度（列）。
pub fn sweep_intensity(col: f32, phase: f32, span: f32, radius: f32) -> f32 {
    if radius <= 0.0 {
        return 0.0;
    }
    let center = phase * (span + 2.0 * radius) - radius;
    let d = (col - center).abs();
    if d >= radius {
        return 0.0;
    }
    (1.0 - d / radius).powf(1.5)
}

/// ratatui 颜色 → RGB。
///
/// 命名色取一组固定的近似值（终端 16 色没有标准 RGB，这组是常见主题的中庸解）：
/// 混色需要数值，而主题色可能只是 `"gray"` 这样的名字。`Reset` / 索引色没有可用
/// 数值：当作白，渐变退化成单色，不至于画不出来。
pub fn to_rgb(color: Color) -> Rgb {
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
        _ => (255, 255, 255),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_endpoints_and_midpoint() {
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0.0), (0, 0, 0));
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 1.0), (255, 255, 255));
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0.5), (128, 128, 128));
        // 越界参数钳到 0..1，不是断言失败。
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 2.0), (255, 255, 255));
    }

    #[test]
    fn sweep_starts_and_ends_outside_the_span() {
        let (span, radius) = (10.0, 5.0);
        // 光带完全在左边界外 / 右边界外：整行无高光。
        assert_eq!(sweep_intensity(5.0, 0.0, span, radius), 0.0);
        assert_eq!(sweep_intensity(5.0, 1.0, span, radius), 0.0);
        // 正中：强度 1。
        assert_eq!(sweep_intensity(5.0, 0.5, span, radius), 1.0);
        // 光带边缘上：0；带内随距离衰减。
        assert_eq!(sweep_intensity(0.0, 0.5, span, radius), 0.0);
        let edge_inside = sweep_intensity(2.0, 0.5, span, radius);
        assert!(edge_inside > 0.0 && edge_inside < 1.0, "{edge_inside}");
    }

    #[test]
    fn zero_radius_is_dark() {
        assert_eq!(sweep_intensity(1.0, 0.5, 10.0, 0.0), 0.0);
    }

    #[test]
    fn light_theme_reads_the_text_colour() {
        assert!(is_light_theme((20, 20, 20)), "深色正文 = 亮底主题");
        assert!(!is_light_theme((229, 229, 229)), "浅色正文 = 暗底主题");
    }

    #[test]
    fn to_rgb_maps_named_colours() {
        assert_eq!(to_rgb(Color::Cyan), (17, 168, 205));
        assert_eq!(to_rgb(Color::Rgb(1, 2, 3)), (1, 2, 3));
        assert_eq!(to_rgb(Color::Reset), (255, 255, 255));
    }
}
