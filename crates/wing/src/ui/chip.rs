//! 带底色的文字块（键帽 / 徽章）—— 宽度、内边距降级、明暗两套底色的**唯一**实现。
//!
//! 开屏名牌用它把「可敲」画成一个色块：键帽（[`Chip::key`]）安静，`Tip` 那枚标签
//! 也是这一档（提示是用来读的，不是按钮）。消费方：开屏名牌的键帽与 `Tip` 标签、
//! 状态栏的 pin 星标（自带状态墨）与模型段、输入卡空草稿时顶栏右端那枚 `Enter`
//! 键帽 —— 各写一份的下场是宽度与降级规则对不上（一边 5 列收边距、另一边 4 列才
//! 收）。另一档 [`Chip::tag`]（`accent` 底）当前没有生产消费者，它的降级与兜底
//! 规则由自己的单测守着。
//!
//! 与输入卡的 `chip`（`input_area` 的粘贴 / 图片原子，无底色）不是一回事：
//! 这里的 `Chip` 是**面**，它存在的意义就是那块底。
//!
//! ## 一块是什么
//!
//! 一个 span：`[内边距][文字][内边距]`，底色铺满整个 span（拆成两个 span 会在
//! 两侧留出终端默认底色的一道缝）。宽度 = 文字宽（CJK 记 2 列）+ 2 × 内边距，
//! **纯函数、不看调色板**：底色不可用时退路不改变宽度，行不会重排。
//!
//! ## 底与墨怎么取
//!
//! 两档，都来自调色板，没有写死的颜色：
//!
//! | 档 | 底 | 墨（有底） | 墨（无底） | 暗色墨/底 | 亮色墨/底 |
//! |---|---|---|---|---|---|
//! | 安静（键帽） | `surface` | `tool_result` | 同左 | 4.35:1 | 4.64:1 |
//! | 强调（徽章） | `accent` | `surface`（纸色） | `text` | 6.77:1 | 4.51:1 |
//!
//! **底**有两道闸门（[`Tint::band`]）：底是 `Color::Reset`（那就是终端默认底色，
//! 画了等于没画）、底浮在纸面上不到 [`MIN_BAND_CONTRAST`]（看不见这块面）。
//! 亮色终端上 `surface` 与白纸只有 1.19:1，正是过不了后一道 —— 所以安静档按
//! [`Appearance`] 再往 `text` 压 8%（1.38:1）。这个 8% 留了余量：墨的交界在
//! 9.6% 附近（8% 时 4.64、12% 时 4.29，测试钉住这两点）。命名色的 `surface` 不压
//! （跟终端主题走，与 `config::stretch` 同一条规矩），方向不对（`text` 比 `surface`
//! 还亮）也不压。
//!
//! `reset` 在两类槽上含义不同（[`ink_rgb`]）：前景（`text` / `tool_result` /
//! 徽章的墨）是**终端默认前景**，按纸面的反面估；背景（`surface` / `accent`）是
//! **纸面本身**，那种底被第一道闸门拦掉。两者都不当成"白"。`surface: reset`
//! 同时是"没有纸色可画"和"没有纸色可作墨"：徽章退回正文色（有意的降级）。
//!
//! **墨**随后按「它压在哪里」挑（[`Tint::default_ink`]）：有底时强调档的纸色墨要过
//! [`MIN_INK_CONTRAST`]，过不了就换正文色（面留着、字换掉）；没有底时纸色墨等于
//! 写在纸面上，直接退正文色。最后 [`Chip::span`] 再兜一次：挑中的墨压在挑中的面上
//! 仍然读不出，就连底一起不要 —— 落到纸面上用正文色。
//!
//! 调用方可以给一块换**状态墨**（[`Chip::ink`]）：状态栏的 pin 要在同一块面上说
//! 开 / 关，而面不能跟着状态变宽变窄。状态墨仍然过同一道闸门 —— 压在面上读不出就
//! 退回本档自己的墨，没有面时干脆不用它（那时墨落在纸面上，是纸面的账）。开放的
//! 是墨，不是底：底一旦能被调用方挑，明暗对账就散回每个调用点了。
//!
//! 无论走哪条退路，内边距与宽度都不变（见上一节）：退路只换颜色，不换几何。

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::Appearance;
use crate::config::ThemePalette;
use crate::ui::shimmer::Rgb;
use crate::ui::shimmer::mix;
use crate::ui::shimmer::to_rgb;

/// 亮色终端上安静档的底再往 `text` 压这么多（推导见模块文档）。
const LIGHT_BAND_DEEPEN: f32 = 0.08;

/// 墨压在底上的最小对比度：3:1 是控件线（WCAG 非文本）。低于它就是糊成一团。
///
/// 不取正文线 4.5：暗色预设的键帽墨（`tool_result` on `surface`）只有 4.35，
/// 而它是有意的取舍 —— 键帽是控件标签，可读性由「面」承担；换成 `text` 能到
/// 11.4，但那会把「键」提到与正文同亮，三层寄存器退化成两层。
///
/// `pub(crate)` 是给渲染侧的断言用的（状态栏 pin 面的两态墨）：判据与阈值都只有
/// 这一份，测试不另抄一个 3.0。
pub(crate) const MIN_INK_CONTRAST: f32 = 3.0;

/// 底浮在纸面上的最小对比度。这不是可读性标准，只要求「看得出是一块」——
/// 亮色预设的 `surface`（1.19）过不了，压深之后（1.38）才过。
const MIN_BAND_CONTRAST: f32 = 1.2;

/// 一个**前景**色的数值代理：`Reset` = 终端默认前景，按纸面的反面估（亮色终端当作
/// 黑、暗色终端当作白）。混色方向与"墨读不读得出"都走它 —— `to_rgb` 把 `Reset` 当白，
/// 在亮色终端上会把方向判反、把可读的墨当成不可读。
///
/// 只用于前景。「底 = `Reset`」的含义是纸面本身，那种底已经被闸门拦掉了。
fn ink_rgb(color: Color, appearance: Appearance) -> Rgb {
    match color {
        Color::Reset => match appearance {
            Appearance::Dark => (255, 255, 255),
            Appearance::Light => (0, 0, 0),
        },
        other => to_rgb(other),
    }
}

/// 纸面：两套预设各自的**校准基准**（亮色按白、暗色按黑，与 light preset 的对比度单测
/// 同一口径）。它只服务本文件的闸门 —— 用户把终端背景改成别的颜色时它是个近似值；
/// 想知道"这一屏实际画在什么色上"用 `ui::html::page_colors`（`#0b0f16` / `#ffffff`）。
fn paper(appearance: Appearance) -> Rgb {
    match appearance {
        Appearance::Dark => (0, 0, 0),
        Appearance::Light => (255, 255, 255),
    }
}

/// 两色的对比度（WCAG 相对亮度口径）。
///
/// 与 `config` / `toast` 的单测同一套公式：那两份是各自测试模块里的私有助手，
/// 这里是渲染真要用的判据，所以公式有第三份。单测用黑白 21:1 给它上锚。
///
/// `pub(crate)` 是给渲染侧的寄存器断言用的（`ui::input_area::chrome` 的三档轮廓、
/// `app::tests::composer` 的"卡内最响的是谁"）—— 判据只有这一份，测试不另抄。
/// 注意它们的纸面取 `ui::html::page_colors`（那一屏实际画在什么色上），不是本文件的
/// [`paper`]（闸门的黑白校准）；两者数值不同，别混用。
pub(crate) fn contrast(a: Rgb, b: Rgb) -> f32 {
    fn channel(v: u8) -> f32 {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    let luma = |c: Rgb| 0.2126 * channel(c.0) + 0.7152 * channel(c.1) + 0.0722 * channel(c.2);
    let (la, lb) = (luma(a), luma(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// 块的内边距：左右各几列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    /// 左右各一格 —— 键帽的常规形态（` Esc `）。
    One,
    /// 没有内边距 —— 行不够宽时的第一档降级（`Esc`）。
    Zero,
}

impl Pad {
    /// 从宽到窄。放不下一档就往下走，走完才由调用方退回纯文本。
    pub const LADDER: [Pad; 2] = [Pad::One, Pad::Zero];

    /// 左右各几列。
    fn columns(self) -> usize {
        match self {
            Self::One => 1,
            Self::Zero => 0,
        }
    }
}

/// 底色档位。私有：外部只有「键帽 / 徽章」两个意图。
///
/// **底不可由调用方挑** —— 那会把明暗对账散回每个调用点；将来真需要第三档（比如
/// `warning` 底）时加一个构造器 + 一个变体。能让调用方换的只有**墨**
/// （[`Chip::ink`]），而且它仍然过 [`MIN_INK_CONTRAST`] 这道闸门，所以"开墨不开底"
/// 不是一句口号：可读性判据只有这一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tint {
    /// `surface` 底 + `tool_result` 墨。
    Quiet,
    /// `accent` 底 + 纸色墨。
    Strong,
}

impl Tint {
    /// 墨：压在这块（或这张纸）上读得出字的那一档。
    ///
    /// 键帽的墨是前景槽，纸面上本来就在用它（纯文本档一直是 `tool_result`），
    /// 所以「落在纸面上」那一支不需要在这里挑。强调档两处都不是无条件成立的：
    /// 纸色墨只在「字压在品牌色上」时成立，无底时它落在纸面上等于消失 ——
    /// 于是退回正文色（预设保证它在纸面上可读）。
    fn default_ink(self, palette: &ThemePalette, band: Option<Color>) -> Color {
        match (self, band) {
            (Self::Quiet, _) => palette.tool_result,
            (Self::Strong, Some(face))
                if contrast(ink_rgb(palette.surface, palette.appearance), to_rgb(face))
                    >= MIN_INK_CONTRAST =>
            {
                palette.surface
            }
            (Self::Strong, _) => palette.text,
        }
    }

    /// 底；不可用时 `None`。两道闸门见模块文档。
    fn band(self, palette: &ThemePalette) -> Option<Color> {
        let band = match self {
            Self::Quiet => quiet_band(palette),
            Self::Strong => palette.accent,
        };
        if band == Color::Reset {
            return None; // 终端默认底色：`to_rgb` 会把它当成白，别拿去算
        }
        (contrast(to_rgb(band), paper(palette.appearance)) >= MIN_BAND_CONTRAST).then_some(band)
    }
}

/// 安静档的底：`surface`；亮色终端压深 [`LIGHT_BAND_DEEPEN`]。
///
/// 两个前置条件，任一不满足就原样用（一次都不推）：`surface` 是 RGB
/// （命名色跟终端主题走，不固化 —— 与 `config::stretch` 同一条规矩；`text` 只是
/// 混色的方向，命名色按 `to_rgb` 的近似值用）、压深真的让它离纸面更远
/// （`text` 比 `surface` 亮时会推反）。
fn quiet_band(palette: &ThemePalette) -> Color {
    let Color::Rgb(..) = palette.surface else {
        return palette.surface;
    };
    if palette.appearance == Appearance::Dark {
        return palette.surface;
    }
    let band = to_rgb(palette.surface);
    let ink = ink_rgb(palette.text, palette.appearance);
    let deepened = mix(band, ink, LIGHT_BAND_DEEPEN);
    // 方向检查：压深必须让它离纸面更远（`text` 比 `surface` 亮时会推反）。
    let paper = paper(palette.appearance);
    if contrast(deepened, paper) <= contrast(band, paper) {
        return palette.surface;
    }
    Color::Rgb(deepened.0, deepened.1, deepened.2)
}

/// 一块：文字 + 底色档位 + （可选的）状态墨。
///
/// 用法是「先量后画」，量的是行自己的预算，不是 chip 的（伪代码，真身见
/// `ui::welcome` 的 `keys_line`）：
///
/// ```ignore
/// for pad in Pad::LADDER {
///     let row = row_of(vec![Chip::key("Esc").span(palette, pad), …]);
///     if row.width() <= room { return row; }
/// }
/// plain_row() // 一档都放不下：回到没有色块的排法，省略交给调用方的 fit
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Chip<'a> {
    text: &'a str,
    tint: Tint,
    /// 调用方要的状态墨（见 [`Chip::ink`]）；`None` = 本档自己的墨。
    state_ink: Option<Color>,
}

impl<'a> Chip<'a> {
    /// 键帽（安静的一档）：` Esc `。
    pub fn key(text: &'a str) -> Self {
        Self {
            text,
            tint: Tint::Quiet,
            state_ink: None,
        }
    }

    /// 徽章（强调的一档）：`accent` 底 + 纸色墨。标签，不是键。
    pub fn tag(text: &'a str) -> Self {
        Self {
            text,
            tint: Tint::Strong,
            state_ink: None,
        }
    }

    /// 换一档**状态墨**：同一块面、不同语义（状态栏的 pin：置顶 `warning` /
    /// 未置顶留本档的墨）。
    ///
    /// 墨换了，闸门不换：压在面上不到 [`MIN_INK_CONTRAST`] 就退回本档自己的墨；
    /// 底被闸门拿掉时也不用它 —— 状态墨落到纸面上是纸面的账，不是这块的。
    #[must_use]
    pub fn ink(mut self, ink: Color) -> Self {
        self.state_ink = Some(ink);
        self
    }

    /// 显示宽度（CJK 记 2 列），含内边距。**不看调色板**：底色不可用时宽度不变。
    pub fn width(&self, pad: Pad) -> usize {
        self.text.width() + pad.columns() * 2
    }

    /// 画成 span（内边距与文字同一个 span，底色才连成一块）。
    pub fn span(&self, palette: &ThemePalette, pad: Pad) -> Span<'static> {
        let air = " ".repeat(pad.columns());
        let mut band = self.tint.band(palette);
        let mut ink = self.resolved_ink(palette, band);
        // 墨压在这块上仍然读不出（用户挑了两个亮度贴近的槽）：宁可不要这块面，
        // 落到纸面上用正文色 —— 比留一块读不出字的色块诚实。
        if let Some(face) = band
            && contrast(ink_rgb(ink, palette.appearance), to_rgb(face)) < MIN_INK_CONTRAST
        {
            band = None;
            ink = self.resolved_ink(palette, None);
        }
        let mut style = Style::default().fg(ink);
        if let Some(band) = band {
            style = style.bg(band);
        }
        Span::styled(format!("{air}{}{air}", self.text), style)
    }

    /// 墨：状态墨（有面且读得出才用）→ 否则本档自己的墨。
    fn resolved_ink(&self, palette: &ThemePalette, band: Option<Color>) -> Color {
        match (self.state_ink, band) {
            (Some(ink), Some(face))
                if contrast(ink_rgb(ink, palette.appearance), to_rgb(face)) >= MIN_INK_CONTRAST =>
            {
                ink
            }
            _ => self.tint.default_ink(palette, band),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ColorPreset;
    use crate::config::ColorsConfig;

    fn dark() -> ThemePalette {
        ThemePalette::default()
    }

    fn light() -> ThemePalette {
        ThemePalette::from_config(&ColorsConfig {
            preset: ColorPreset::Light,
            ..ColorsConfig::default()
        })
    }

    /// 一套预设 + 若干槽位覆盖（用户改配色的正经用法）。
    fn preset_with(preset: ColorPreset, slot: impl Fn(&mut ColorsConfig)) -> ThemePalette {
        let mut cfg = ColorsConfig {
            preset,
            ..ColorsConfig::default()
        };
        slot(&mut cfg);
        ThemePalette::from_config(&cfg)
    }

    fn paper_of(palette: &ThemePalette) -> Rgb {
        paper(palette.appearance)
    }

    fn palette_with(surface: Color) -> ThemePalette {
        ThemePalette {
            surface,
            ..ThemePalette::default()
        }
    }

    #[test]
    fn the_contrast_formula_is_anchored() {
        // 下面所有闸门都用 `contrast`，它自己得有个不依赖调色板的锚：黑白 21:1，
        // 中灰对白 3.95:1（闸门用的就是这种量级）。
        assert!((contrast((255, 255, 255), (0, 0, 0)) - 21.0).abs() < 0.01);
        assert!((contrast((128, 128, 128), (255, 255, 255)) - 3.95).abs() < 0.05);
        assert!((contrast((0x87, 0x91, 0xa7), (0x23, 0x2d, 0x42)) - 4.35).abs() < 0.05);
    }

    #[test]
    fn width_is_display_columns_plus_the_padding() {
        let chip = Chip::key("Esc");
        assert_eq!(chip.width(Pad::One), 5);
        assert_eq!(chip.width(Pad::Zero), 3);
        // CJK 记 2 列（状态栏将来要放「模式」「思考」这类词）。
        assert_eq!(Chip::key("模式").width(Pad::Zero), 4);
        assert_eq!(Chip::key("模式").width(Pad::One), 6);
        // 量的与画的是同一份数学：span 的显示宽度等于 `width`。
        let palette = dark();
        for pad in Pad::LADDER {
            assert_eq!(chip.span(&palette, pad).content.width(), chip.width(pad));
        }
    }

    #[test]
    fn the_ladder_drops_air_never_the_text() {
        // 降级的代价是内边距，不是文字：一块窄下去时字还在。
        for text in ["Esc", "Ctrl+C×2", "模式"] {
            let chip = Chip::key(text);
            assert_eq!(chip.width(Pad::One), chip.width(Pad::Zero) + 2);
            assert!(chip.span(&dark(), Pad::Zero).content.contains(text));
            assert!(chip.span(&dark(), Pad::One).content.contains(text));
        }
        assert_eq!(Pad::LADDER, [Pad::One, Pad::Zero], "先宽后窄");
    }

    #[test]
    fn a_chip_is_one_span_and_the_band_covers_the_padding() {
        let palette = dark();
        let span = Chip::key("Esc").span(&palette, Pad::One);
        assert_eq!(span.content.as_ref(), " Esc ");
        assert_eq!(
            span.style.bg,
            Some(palette.surface),
            "底色铺满整块（含内边距）"
        );
        assert_eq!(span.style.fg, Some(palette.tool_result), "键帽的墨");
    }

    #[test]
    fn the_badge_is_the_accent_fill_with_the_paper_ink() {
        for palette in [dark(), light()] {
            let span = Chip::tag("Tip").span(&palette, Pad::One);
            assert_eq!(span.style.bg, Some(palette.accent), "徽章的底是品牌色");
            assert_eq!(span.style.fg, Some(palette.surface), "墨是纸色");
        }
    }

    #[test]
    fn no_band_when_the_surface_is_the_terminal_default() {
        // `surface: reset` = 终端自己的底色：画了等于没画，宁可退回裸墨。
        let palette = palette_with(Color::Reset);
        let span = Chip::key("Esc").span(&palette, Pad::One);
        assert_eq!(span.style.bg, None);
        assert_eq!(span.style.fg, Some(palette.tool_result), "键的寄存器照旧");
        assert_eq!(span.content.as_ref(), " Esc ", "内边距照旧（几何不变）");
    }

    #[test]
    fn a_badge_whose_fill_is_invisible_falls_back_to_body_text() {
        // 强调档的墨是纸色 —— 只在有底时成立。底被闸门拿掉时必须换墨，
        // 否则就是纸色字写在纸面上。
        let palette = preset_with(ColorPreset::Light, |cfg| {
            cfg.accent = Some("#ffffff".to_string()); // 白底上的白块
        });
        let span = Chip::tag("Tip").span(&palette, Pad::One);
        assert_eq!(span.style.bg, None, "看不见的底不画");
        assert_eq!(
            span.style.fg,
            Some(palette.text),
            "无底的徽章退回正文色（唯一无条件可读的前景），而不是纸色"
        );
        assert!(contrast(to_rgb(palette.text), paper_of(&palette)) >= 4.5);

        // `accent: reset` 走同一条退路。
        let palette = preset_with(ColorPreset::Dark, |cfg| {
            cfg.accent = Some("reset".to_string());
        });
        let span = Chip::tag("Tip").span(&palette, Pad::One);
        assert_eq!(span.style.bg, None);
        assert_eq!(span.style.fg, Some(palette.text));
    }

    #[test]
    fn a_badge_whose_paper_ink_is_lost_swaps_the_ink_not_the_fill() {
        // 底还在（离纸面够远），但「纸色墨压在它上面」读不出：换墨，不换底 ——
        // 徽章的身份是那块面，能留就留。
        let palette = preset_with(ColorPreset::Light, |cfg| {
            cfg.accent = Some("#eab308".to_string()); // 明黄：纸色墨淹在里面
        });
        let span = Chip::tag("Tip").span(&palette, Pad::One);
        assert_eq!(span.style.bg, Some(palette.accent), "面留着");
        assert_eq!(span.style.fg, Some(palette.text), "墨换成正文色");
        let fg = span.style.fg.map(to_rgb).unwrap_or_default();
        assert!(
            contrast(fg, to_rgb(palette.accent)) >= MIN_INK_CONTRAST,
            "换过的墨要在面上读得出"
        );
    }

    #[test]
    fn a_face_nobody_can_read_on_is_dropped_whole() {
        // 最后一道兜底：底看得见、但两种墨压在它上面都读不出（用户把 `surface`
        // 与 `text` 都调成了同一档灰）→ 连底一起不要，落回纸面用正文色。
        let palette = preset_with(ColorPreset::Dark, |cfg| {
            cfg.surface = Some("#7f7f7f".to_string());
            cfg.text = Some("#8f8f8f".to_string());
        });
        let span = Chip::key("Esc").span(&palette, Pad::One);
        assert_eq!(span.style.bg, None, "读不出字的色块不如没有");
        // 键帽的最后退路是它自己的寄存器（`tool_result`）—— 也就是纯文本档的样子。
        assert_eq!(span.style.fg, Some(palette.tool_result));
        assert!(contrast(to_rgb(palette.tool_result), paper_of(&palette)) >= 4.5);
    }

    #[test]
    fn a_badge_face_no_ink_can_live_on_is_dropped_whole() {
        // 强调档的最后兜底：面看得见，但纸色墨与正文色压在它上面都读不出 ——
        // 连底一起不要。两套预设的 `surface` 与 `text` 相距 11:1 以上，构造不出
        // 这种面，所以用一个故意挑坏的调色板（三个槽挤在同一档灰里）。
        let palette = preset_with(ColorPreset::Light, |cfg| {
            cfg.surface = Some("#8a8a8a".to_string());
            cfg.text = Some("#8f8f8f".to_string());
            cfg.accent = Some("#a0a0a0".to_string());
        });
        let span = Chip::tag("Tip").span(&palette, Pad::One);
        assert_eq!(span.style.bg, None, "没有一种墨活得了的面不画");
        assert_eq!(span.style.fg, Some(palette.text));
    }

    #[test]
    fn no_band_when_the_ink_sinks_into_it() {
        // 用户把 surface 调成接近 tool_result 的灰：块会糊成一团，退回裸墨。
        let palette = palette_with(Color::Rgb(0x87, 0x91, 0xa7));
        assert_eq!(Chip::key("Esc").span(&palette, Pad::One).style.bg, None);
        // 反过来：预设里的面过得了闸门（否则上面那条就没有意义）。
        assert_eq!(
            Chip::key("Esc").span(&dark(), Pad::One).style.bg,
            Some(dark().surface)
        );
    }

    #[test]
    fn no_band_when_the_band_hides_in_the_paper() {
        // 底与纸面分不开（只是"看不见一块面"）：同样不画。
        let palette = preset_with(ColorPreset::Light, |cfg| {
            cfg.surface = Some("#ffffff".to_string());
        });
        assert_eq!(
            Chip::key("Esc").span(&palette, Pad::One).style.bg,
            None,
            "白底上的白块"
        );
    }

    #[test]
    fn a_reset_foreground_is_the_default_foreground_not_white() {
        // `reset` 是合法的槽值：它的意思是"终端默认前景"，按纸面的反面估（见
        // `ink_rgb`）。前景槽（`text` 负责混色方向、`tool_result` 是键帽的墨）都
        // 靠这个代理，两个配置各断言一次：**色块还在**，且墨在它上面 ≥ 3:1。
        let cases = [
            (
                "text: reset（负责混色方向）",
                preset_with(ColorPreset::Light, |cfg| {
                    cfg.text = Some("reset".to_string());
                }),
                Chip::key("Esc"),
            ),
            (
                "tool_result: reset（键帽的墨）",
                preset_with(ColorPreset::Light, |cfg| {
                    cfg.tool_result = Some("reset".to_string());
                }),
                Chip::key("Esc"),
            ),
        ];
        for (name, palette, chip) in cases {
            let span = chip.span(&palette, Pad::One);
            let face = span
                .style
                .bg
                .unwrap_or_else(|| panic!("{name}：色块不该消失"));
            let ink = span.style.fg.expect("总有墨");
            let ratio = contrast(ink_rgb(ink, palette.appearance), to_rgb(face));
            assert!(ratio >= MIN_INK_CONTRAST, "{name}：墨/底 {ratio:.2}:1");
        }

        // 背景槽的 `reset` 是另一回事：它就是纸面本身（底被拦掉），而强调档的墨
        // 也取自 `surface` —— 于是它同时失去墨源，徽章退回正文色。这是**有意的**
        // 降级，不是"看不见的字"：文档（模块头 / `welcome-mascot.md`）写的就是这条。
        let no_paper = preset_with(ColorPreset::Dark, |cfg| {
            cfg.surface = Some("reset".to_string());
        });
        let span = Chip::tag("Tip").span(&no_paper, Pad::One);
        assert_eq!(span.style.bg, None, "`surface: reset` 没有纸色可画");
        assert_eq!(span.style.fg, Some(no_paper.text), "退回正文色");
        let ink = span.style.fg.map(|c| ink_rgb(c, no_paper.appearance));
        assert!(
            contrast(ink.unwrap_or_default(), paper_of(&no_paper)) >= 4.5,
            "退回的墨要在纸面上可读"
        );
        // `text: reset` 走的确实是"压深"，不是"没压深"。
        let text = preset_with(ColorPreset::Light, |cfg| {
            cfg.text = Some("reset".to_string());
        });
        assert_ne!(quiet_band(&text), text.surface, "方向不该判反");
    }

    #[test]
    fn a_state_ink_rides_the_same_face_and_the_same_gate() {
        // 状态墨在本档自己的墨之外换一档，但面、内边距、宽度都不动。
        for palette in [dark(), light()] {
            let chip = Chip::key("Esc");
            let plain = chip.span(&palette, Pad::One);
            let marked = chip.ink(palette.warning).span(&palette, Pad::One);
            assert_eq!(marked.style.fg, Some(palette.warning));
            assert_eq!(marked.style.bg, plain.style.bg, "同一块面");
            assert_eq!(marked.content, plain.content, "同样的字与气口");
            assert_eq!(marked.content.width(), chip.width(Pad::One));
            assert!(
                contrast(to_rgb(palette.warning), to_rgb(plain.style.bg.expect("面")))
                    >= MIN_INK_CONTRAST,
                "{:?}: 状态墨压在面上读得出",
                palette.appearance
            );
        }
    }

    #[test]
    fn a_state_ink_that_sinks_into_the_face_is_not_used() {
        // 用户把状态槽调到与 `surface` 同一档灰：那一档读不出，退回键帽自己的墨
        // —— 面留着、字换掉，不退化成"看不见的状态"。
        let palette = preset_with(ColorPreset::Dark, |cfg| {
            cfg.warning = Some("#2b3548".to_string()); // ≈ surface
        });
        let span = Chip::key("Esc")
            .ink(palette.warning)
            .span(&palette, Pad::One);
        assert_eq!(span.style.fg, Some(palette.tool_result), "退回本档的墨");
        assert_eq!(span.style.bg, Some(palette.surface), "面还在");
        assert_eq!(span.content.as_ref(), " Esc ", "几何不变");
    }

    #[test]
    fn a_state_ink_never_lands_on_the_paper() {
        // 面被闸门拿掉时状态墨不接管：此时墨压在纸面上，那是本档墨的账。否则
        // `surface: reset` 的终端上会冒出一块没有任何底气支撑的状态色。
        let palette = palette_with(Color::Reset);
        let span = Chip::key("Esc")
            .ink(palette.warning)
            .span(&palette, Pad::One);
        assert_eq!(span.style.bg, None);
        assert_eq!(span.style.fg, Some(palette.tool_result));
    }

    #[test]
    fn the_badge_keeps_its_fill_at_any_padding() {
        // 徽章也会被降级到 `Pad::Zero`（状态栏那种宽度急的时候）：形状与底照旧，
        // 只少气口。
        let palette = dark();
        let tight = Chip::tag("Tip").span(&palette, Pad::Zero);
        assert_eq!(tight.content.as_ref(), "Tip");
        assert_eq!(tight.content.width(), Chip::tag("Tip").width(Pad::Zero));
        assert_eq!(tight.style.bg, Some(palette.accent));
        assert_eq!(tight.style.fg, Some(palette.surface));
    }

    #[test]
    fn only_the_light_band_is_deepened_and_only_when_that_is_the_right_way() {
        // 暗色终端：`surface` 已比纸面亮一档，原样用。
        assert_eq!(quiet_band(&dark()), dark().surface);

        let palette = light();
        let band = quiet_band(&palette);
        assert_ne!(band, palette.surface, "亮色终端要压深");
        assert!(
            contrast(to_rgb(band), paper_of(&palette))
                > contrast(to_rgb(palette.surface), paper_of(&palette)),
            "压深是为了离纸面更远"
        );

        // 方向不对（`text` 比 `surface` 亮）：一次都不推，原样返回。
        let inverted = preset_with(ColorPreset::Light, |cfg| {
            cfg.text = Some("#f7f9fc".to_string());
        });
        assert_eq!(quiet_band(&inverted), inverted.surface, "推反了就不推");

        // 命名色跟终端主题走，不固化（与 `config::stretch` 同一条规矩）。
        let named = preset_with(ColorPreset::Light, |cfg| {
            cfg.surface = Some("gray".to_string());
        });
        assert_eq!(quiet_band(&named), Color::Gray);

        // 只有 *被推的那个* 槽要求是 RGB：`text` 只是方向，命名色按近似值用。
        let named_ink = preset_with(ColorPreset::Light, |cfg| {
            cfg.text = Some("black".to_string());
        });
        assert_ne!(
            quiet_band(&named_ink),
            named_ink.surface,
            "`text: black` 不妨碍压深"
        );
    }

    #[test]
    fn the_deepen_is_a_bounded_step_the_ink_allows() {
        // 「8% 留了余量」得自己被钉住：8% 时墨还在 4.5 以上，12% 已破。
        let palette = light();
        let ink = Tint::Quiet.default_ink(&palette, None);
        let band = quiet_band(&palette);
        assert!(
            contrast(to_rgb(ink), to_rgb(band)) >= 4.5,
            "亮色键帽的墨要在正文线上：{:.2}",
            contrast(to_rgb(ink), to_rgb(band))
        );
        for deeper in [0.12f32, 0.2] {
            let deeper_band = mix(to_rgb(palette.surface), to_rgb(palette.text), deeper);
            assert!(
                contrast(to_rgb(ink), deeper_band) < 4.5,
                "{:.0}% 应当已经破线",
                deeper * 100.0
            );
        }
    }

    #[test]
    fn both_presets_keep_the_ink_off_the_band_and_the_band_off_the_paper() {
        // 块内对比度是这里唯一能算准的东西（块外是终端底色，未知）。
        // 键帽的墨：亮色 4.64 过正文线；暗色 4.35 只过控件线 —— 那是刻意的取舍
        // （见 `MIN_INK_CONTRAST`），所以分档断言，别把两条线混成一个数。
        for (palette, key_floor) in [(dark(), 4.0), (light(), 4.5)] {
            let quiet = Tint::Quiet.band(&palette).expect("安静档的底可用");
            let strong = Tint::Strong.band(&palette).expect("强调档的底可用");
            let key = contrast(
                to_rgb(Tint::Quiet.default_ink(&palette, Some(quiet))),
                to_rgb(quiet),
            );
            let badge = contrast(
                to_rgb(Tint::Strong.default_ink(&palette, Some(strong))),
                to_rgb(strong),
            );
            assert!(
                key >= key_floor,
                "{:?} 键帽的墨 {key:.2}:1 < {key_floor}",
                palette.appearance
            );
            assert!(
                badge >= 4.5,
                "{:?} 徽章的墨 {badge:.2}:1",
                palette.appearance
            );
            // 块要浮在纸面上：暗色 1.53、亮色 1.38。
            let float = contrast(to_rgb(quiet), paper_of(&palette));
            assert!(
                float >= 1.3,
                "{:?} 块与纸面 {float:.2}:1",
                palette.appearance
            );
            // 无底时两种墨都落在纸面上，都要读得出。
            for (tint, expected) in [
                (Tint::Quiet, palette.tool_result),
                (Tint::Strong, palette.text),
            ] {
                let ink = tint.default_ink(&palette, None);
                assert_eq!(ink, expected);
                assert!(
                    contrast(to_rgb(ink), paper_of(&palette)) >= 4.5,
                    "{:?} 无底时的墨要在纸面上可读",
                    palette.appearance
                );
            }
        }
    }
}
