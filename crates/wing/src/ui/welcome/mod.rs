//! 欢迎屏 —— 空会话时 chat 顶部那一段（随会话滚走）。
//!
//! 三样东西拼成：像素 W 标记（[`art`]）、右侧文字列（版本 / 键位 / 轮换 tip），
//! 以及开屏时扫过标记与 wordmark 的一道高光。
//!
//! **内容只放"永远是事实"的东西**：版本号、commit、键位、tips 池里抽的一条。
//! 这里以前挂着一个硬编码的 "What's new" 盒子，敏捷开发下必然陈旧（那份文案
//! 从闭源时代起就没再动过），所以整块删掉了 —— 想告诉用户新能力，就写进
//! `shared::tips` 池，那里只讲稳定能力。
//!
//! 窄终端按宽度档位降级：整块 → 只文字列 → 一行 wordmark，任何宽度都不截断。
//! 状态（tip 抽签、扫光时钟、上次构建宽度）在 [`Welcome`] 里；App 只在需要时
//! 重建 header（见 `App::sync_welcome`）。

pub mod art;

use std::time::Duration;
use std::time::Instant;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::shared::constants::TIPS_COMMAND;
use crate::shared::tips;

/// 扫光时长：开屏扫 2 秒，然后永久定格（之后不再为它重建 / 重绘）。
pub const SWEEP_MS: u64 = 2_000;

/// 扫光帧间隔（≈25fps。100ms 的系统 tick 太粗，扫光会一顿一顿）。
const FRAME_MS: u64 = 40;

/// 光带半宽（列）——光带全宽 = 2 × 它。
const SWEEP_RADIUS: f32 = 9.0;

/// 整块布局需要的最小列数：标记 + 间隔 + 文字列至少 30 列。
const FULL_MIN: u16 = (art::ART_COLS + art::ART_GAP + 30) as u16;

/// 文字列布局需要的最小列数（再窄就只剩一行 wordmark）。
const COMPACT_MIN: u16 = 40;

/// 文字列相对标记的竖排起点：把 5 行文字塞进 7 行字形里居中。
const TEXT_OFFSET: usize = 1;

/// 开屏扫光：一道竖直光带横扫整块 header。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Sweep {
    /// 进度 0..1（0 = 光带还在左侧屏外，1 = 已经扫出去）。
    pub(super) phase: f32,
    /// 光带走过的列数（= 整块宽度）。
    pub(super) span: f32,
}

impl Sweep {
    /// 某一列的高光强度：0 = 不在光带里，1 = 光带正中。
    fn at(&self, col: f32) -> f32 {
        let center = self.phase * (self.span + 2.0 * SWEEP_RADIUS) - SWEEP_RADIUS;
        let distance = (col - center).abs();
        if distance >= SWEEP_RADIUS {
            return 0.0;
        }
        (1.0 - distance / SWEEP_RADIUS).powf(1.6)
    }
}

/// header 的宽度档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// 标记 + 文字列。
    Full,
    /// 只有文字列（窄屏先撤标记：标记被截只是一块色块，文字被截才是读不出来的句子）。
    Compact,
    /// 只有一行 wordmark。
    Minimal,
}

fn layout_for(width: u16) -> Layout {
    if width >= FULL_MIN {
        Layout::Full
    } else if width >= COMPACT_MIN {
        Layout::Compact
    } else {
        Layout::Minimal
    }
}

/// 欢迎屏状态 —— tip 抽签、扫光时钟、上次构建宽度。
///
/// tip 在构造时抽定：整个进程一条，重绘 / 缩放 / 重建都不会换行（换行会显得
/// 界面在自己抖）。
#[derive(Debug, Clone)]
pub struct Welcome {
    /// 抽中的那条 tip（`shared::tips` 池内，整个进程不变）。
    tip: &'static tips::Tip,
    /// 扫光起点。
    started: Instant,
    /// 上次构建用的终端宽度（0 = 还没构建过）。
    built_width: u16,
    /// 上次构建的是不是"定格版"（无扫光）。
    settled_built: bool,
}

impl Welcome {
    /// `seed` 决定抽中哪条 tip（`tips::seed_now()` 给生产用，测试传常数）。
    pub fn new(seed: u64, now: Instant) -> Self {
        Self {
            tip: tips::pick(seed),
            started: now,
            built_width: 0,
            settled_built: false,
        }
    }

    /// 抽中的那条 tip（首帧那一行与测试读它）。
    pub fn tip(&self) -> &'static tips::Tip {
        self.tip
    }

    /// 扫光是否还在跑。
    fn animating(&self, now: Instant) -> bool {
        now.duration_since(self.started) < Duration::from_millis(SWEEP_MS)
    }

    /// 需要重建 header 吗 —— 宽度档变了，或者扫光还在跑（每帧一个新进度）。
    ///
    /// 定格之后 `settled_built` 立住，宽度不变就再也不重建：静态 header 一次
    /// 构建、之后每帧只是重画。
    pub fn needs_rebuild(&self, width: u16, now: Instant) -> bool {
        if self.built_width != width {
            return true;
        }
        !self.settled_built || self.animating(now)
    }

    /// 扫光下一帧的**绝对**截止时刻（`None` = 已定格，不再需要 tick 驱动）。
    ///
    /// 绝对时刻而不是"睡 40ms"：事件循环里任何事件都会重建这个 future，相对
    /// sleep 会被输入 / 流式事件无限推后，扫光就卡死在原地。
    pub fn next_frame(&self, now: Instant) -> Option<Instant> {
        let deadline = self.started + Duration::from_millis(SWEEP_MS);
        if now >= deadline {
            return None;
        }
        let elapsed = now.duration_since(self.started).as_millis() as u64;
        let frame = (elapsed / FRAME_MS + 1) * FRAME_MS;
        Some(self.started + Duration::from_millis(frame.min(SWEEP_MS)))
    }

    /// 构建 header 行。调用方负责把它交给 chat view（见 `App::sync_welcome`）。
    pub fn build(
        &mut self,
        palette: &ThemePalette,
        width: u16,
        now: Instant,
    ) -> Vec<Line<'static>> {
        let layout = layout_for(width);
        // 扫光按整块的绝对列走：整块布局里 wordmark 在标记右侧，窄屏布局里它在
        // 最左边 —— 光带走过的距离因此跟着布局变。
        let (col_offset, span) = match layout {
            Layout::Full => (
                art::ART_COLS + art::ART_GAP,
                art::ART_COLS + art::ART_GAP + WORDMARK_WIDTH,
            ),
            Layout::Compact | Layout::Minimal => (0, WORDMARK_WIDTH),
        };
        let sweep = self.animating(now).then(|| Sweep {
            phase: (now.duration_since(self.started).as_millis() as f32 / SWEEP_MS as f32)
                .clamp(0.0, 1.0),
            span: span as f32,
        });

        self.built_width = width;
        self.settled_built = sweep.is_none();

        let tip = self.tip().text;
        match layout {
            Layout::Full => {
                let left = col_offset;
                let text = text_column(
                    palette,
                    width.saturating_sub(left as u16 + 1) as usize,
                    tip,
                    sweep,
                    left,
                );
                let art_rows = art::lines(palette.accent, sweep.as_ref());
                let mut lines = Vec::with_capacity(art::ART_ROWS + 2);
                // 上留白 —— 和状态栏拉开一点距离。
                lines.push(Line::from(""));
                for (row, art_line) in art_rows.into_iter().enumerate() {
                    let mut spans: Vec<Span<'static>> = art_line.spans;
                    let used: usize = spans.iter().map(|s| s.content.width()).sum();
                    spans.push(Span::raw(" ".repeat(left.saturating_sub(used))));
                    if let Some(text_line) = row.checked_sub(TEXT_OFFSET)
                        && let Some(line) = text.get(text_line)
                    {
                        spans.extend(line.spans.iter().cloned());
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
                lines
            }
            Layout::Compact => {
                let mut lines = vec![Line::from("")];
                lines.extend(text_column(
                    palette,
                    width.saturating_sub(1) as usize,
                    tip,
                    sweep,
                    col_offset,
                ));
                lines.push(Line::from(""));
                lines
            }
            Layout::Minimal => vec![
                Line::from(""),
                wordmark_line(
                    palette,
                    width.saturating_sub(1) as usize,
                    col_offset as f32,
                    sweep,
                ),
                Line::from(""),
            ],
        }
    }
}

/// wordmark 文案（`✦ wing`）的显示宽度 —— 扫光要扫过它。
const WORDMARK: &str = "✦ wing";
const WORDMARK_WIDTH: usize = 6;

/// 右侧文字列：wordmark / 键位 / tip / 入口，共 5 行（wordmark 后留一空行），
/// 放进 [`art::ART_ROWS`] 行里由 [`TEXT_OFFSET`] 居中。
///
/// `col_offset` 是文字列在整块里的起始列 —— 扫光按绝对列走，两半才算同一个
/// 光带扫过去。
fn text_column(
    palette: &ThemePalette,
    width: usize,
    tip: &str,
    sweep: Option<Sweep>,
    col_offset: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    vec![
        wordmark_line(palette, width, col_offset as f32, sweep),
        Line::from(""),
        dim_line(KEYS, width, dim),
        tip_line(tip, width, dim, text),
        dim_line(&hints_text(), width, dim),
    ]
}

/// 键位提示（一行）。
const KEYS: &str = "Esc 中断 · Shift+Enter 换行 · Ctrl+C ×2 退出";

/// 入口提示（一行）。
///
/// `/tips` 从常量拼，命令改名时这一行跟着走；「输入 / 看命令」不带命令名 ——
/// 命令表来自网关（用户自己的 prompt 命令），写死某一个名字迟早会是假的。
fn hints_text() -> String {
    format!("{} 全部提示 · 输入 / 看全部命令", TIPS_COMMAND)
}

/// `Tip  <正文>` —— 标签暗、正文正常色。
fn tip_line(tip: &str, width: usize, dim: Style, text: Style) -> Line<'static> {
    let label = "Tip  ";
    let room = width.saturating_sub(label.width());
    Line::from(vec![
        Span::styled(label, dim),
        Span::styled(elide(tip, room), text),
    ])
}

/// wordmark 行：`✦ wing` 带扫光，后面跟版本 + commit（暗色）。
///
/// 整块布局里它在第 `col_offset` 列（标记右侧），窄屏布局里它自己在最左边 ——
/// 扫光按绝对列走，所以这个偏移要传进来。
fn wordmark_line(
    palette: &ThemePalette,
    width: usize,
    col_offset: f32,
    sweep: Option<Sweep>,
) -> Line<'static> {
    let base = art::to_rgb(palette.accent);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(WORDMARK.len() + 2);
    let mut used = 0;
    for (index, ch) in WORDMARK.chars().enumerate() {
        let glyph_width = ch.to_string().width();
        if used + glyph_width > width {
            // 窄到连 wordmark 都放不下：截在这儿，版本号整段让位。
            return Line::from(spans);
        }
        let strength = sweep.map_or(0.0, |s| s.at(col_offset + index as f32));
        let rgb = art::mix(base, (255, 255, 255), 0.55 * strength);
        spans.push(Span::styled(
            ch.to_string(),
            Style::default()
                .fg(ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2))
                .add_modifier(Modifier::BOLD),
        ));
        used += glyph_width;
    }
    let version = version_label();
    if used + 2 <= width {
        let tail = if used + 2 + version.width() <= width {
            format!("  {version}")
        } else {
            format!("  {}", elide(&version, width - used - 2))
        };
        spans.push(Span::styled(tail, Style::default().fg(palette.dim)));
    }
    Line::from(spans)
}

/// 一行单色文字，按可用宽度截断。
fn dim_line(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(elide(text, width), style))
}

/// 按**显示宽度**截断（CJK 记 2 列），截断处补 `…`。
fn elide(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1); // 给 `…` 留一列
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.to_string().width();
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// 版本行尾巴：`dev · <commit>` —— workspace 的占位版本 0.0.0 是开发构建
/// （发版时由 `scripts/sync_version.py` 换成真版本号），展示成 `dev`；
/// `v0.0.0` 只会让人以为装错了包。
fn version_label() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("WING_COMMIT_HASH");
    if version == "0.0.0" {
        format!("dev · {commit}")
    } else {
        format!("v{version} · {commit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn welcome() -> Welcome {
        Welcome::new(3, Instant::now())
    }

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    /// 一行的显示宽度。
    fn line_width(line: &Line<'_>) -> usize {
        line.spans.iter().map(|s| s.content.width()).sum()
    }

    #[test]
    fn layout_ladder() {
        assert_eq!(layout_for(200), Layout::Full);
        assert_eq!(layout_for(FULL_MIN), Layout::Full);
        assert_eq!(layout_for(FULL_MIN - 1), Layout::Compact);
        assert_eq!(layout_for(COMPACT_MIN), Layout::Compact);
        assert_eq!(layout_for(COMPACT_MIN - 1), Layout::Minimal);
        assert_eq!(layout_for(10), Layout::Minimal);
    }

    #[test]
    fn every_line_fits_every_width() {
        // 核心不变量：任何宽度下都不能有行超出（Paragraph 不换行，超了就是被切）。
        let palette = palette();
        for width in 2u16..200 {
            let mut welcome = welcome();
            for phase_ms in [0u64, 500, 1_000, 1_999, 2_000] {
                let now = welcome.started + Duration::from_millis(phase_ms);
                let lines = welcome.build(&palette, width, now);
                for line in &lines {
                    assert!(
                        line_width(line) <= width as usize,
                        "宽 {width} 时第 {phase_ms}ms 的行超宽 {}：{:?}",
                        line_width(line),
                        line
                    );
                }
            }
        }
    }

    #[test]
    fn full_layout_puts_the_tip_beside_the_art() {
        let mut welcome = welcome();
        let lines = welcome.build(&palette(), 120, welcome.started);
        // 上留白 + 7 行标记 + 下留白。
        assert_eq!(lines.len(), art::ART_ROWS + 2);
        let tip = welcome.tip().text;
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains(tip), "tip 应当在整块布局里：{joined}");
        assert!(joined.contains("✦ wing"), "wordmark 在：{joined}");
        assert!(joined.contains("dev"), "开发构建显示 dev：{joined}");
        assert!(joined.contains("/tips"), "入口提示在：{joined}");
    }

    #[test]
    fn compact_and_minimal_have_no_art() {
        let mut welcome = welcome();
        let compact = welcome.build(&palette(), FULL_MIN - 1, welcome.started);
        assert!(!compact.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains('█') || s.content.contains('▀'))
        }));

        let minimal = welcome.build(&palette(), 20, welcome.started);
        assert_eq!(minimal.len(), 3);
        let wordmark: String = minimal[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(wordmark.contains("wing"), "{wordmark}");
    }

    #[test]
    fn settles_after_the_sweep_and_stops_rebuilding() {
        let mut welcome = welcome();
        let palette = palette();
        let width = 120;

        assert!(welcome.needs_rebuild(width, welcome.started), "首帧要构建");
        welcome.build(&palette, width, welcome.started);
        assert!(
            welcome.needs_rebuild(width, welcome.started + Duration::from_millis(40)),
            "扫光期间逐帧重建"
        );

        let settled_at = welcome.started + Duration::from_millis(SWEEP_MS);
        welcome.build(&palette, width, settled_at);
        assert!(!welcome.settled_built || !welcome.animating(settled_at));
        assert!(
            !welcome.needs_rebuild(width, settled_at + Duration::from_secs(5)),
            "定格后宽度不变就不重建"
        );
        assert!(
            welcome.needs_rebuild(width + 10, settled_at + Duration::from_secs(5)),
            "缩放要重建"
        );
    }

    #[test]
    fn sweep_frames_are_absolute_and_monotonic() {
        let welcome = welcome();
        let mut now = welcome.started;
        let mut frames = Vec::new();
        while let Some(next) = welcome.next_frame(now) {
            assert!(next > now, "截止时刻必须向前");
            frames.push(next);
            now = next;
        }
        // 2 秒 / 40ms ≈ 50 帧，末帧正好落在定格时刻。
        assert!(frames.len() >= 49, "帧数太少：{}", frames.len());
        assert_eq!(
            *frames.last().expect("末帧"),
            welcome.started + Duration::from_millis(SWEEP_MS)
        );
        assert_eq!(
            welcome.next_frame(welcome.started + Duration::from_secs(60)),
            None
        );
    }

    #[test]
    fn sweep_progresses_across_the_block() {
        let sweep = Sweep {
            phase: 0.5,
            span: 60.0,
        };
        assert!(sweep.at(30.0) > 0.9, "光带正中接近满强度");
        assert_eq!(sweep.at(200.0), 0.0, "光带之外是 0");
        assert!(sweep.at(33.0) < sweep.at(30.0), "离开中心强度单调降");
    }

    #[test]
    fn elide_respects_display_width() {
        assert_eq!(elide("abc", 10), "abc");
        assert_eq!(elide("abcdef", 4), "abc…");
        assert_eq!(elide("中文中文", 4), "中…");
        assert_eq!(elide("中文", 5), "中文");
        assert_eq!(elide("anything", 0), "");
    }

    #[test]
    fn dev_builds_show_dev_not_the_placeholder_version() {
        let label = version_label();
        assert!(
            !label.contains("0.0.0") || !cfg!(debug_assertions),
            "占位版本不该露出 v0.0.0：{label}"
        );
        assert!(label.contains("dev") || label.starts_with('v'), "{label}");
    }

    #[test]
    fn tip_is_picked_once_per_process() {
        let mut welcome = welcome();
        let first = welcome.tip().text;
        for _ in 0..5 {
            welcome.build(
                &palette(),
                120,
                welcome.started + Duration::from_millis(100),
            );
            assert_eq!(welcome.tip().text, first, "重绘不该换 tip");
        }
    }

    #[test]
    fn wordmark_width_constant_matches_the_text() {
        assert_eq!(
            WORDMARK.width(),
            WORDMARK_WIDTH,
            "扫光按常量算列，常量必须跟着文案走"
        );
    }

    #[test]
    fn wordmark_keeps_the_accent_hue_without_sweep() {
        let palette = ThemePalette::default();
        let line = wordmark_line(&palette, 80, 0.0, None);
        let first = line.spans[0].style.fg.expect("fg");
        assert!(matches!(first, Color::Rgb(..)), "{first:?}");
    }
}
