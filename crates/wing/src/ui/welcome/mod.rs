//! 欢迎屏 —— 空会话时 chat 顶部那一段（随会话滚走）。
//!
//! 拼成它的四样东西：
//!
//! * **海鸥**（[`art`] 的字母网格 + [`sprite`] 的半格渲染）：待机是站姿 chibi，
//!   agent 干活时切成飞行扇翅 —— "它在飞" = "它在干活"；
//! * **wordmark**（[`wordmark`]）：手绘 5 行像素大字 `WING`，开屏一道扫光扫过；
//! * 右侧文字列：版本 / 键位 / 轮换 tip；
//! * **动作规划器**（[`motion`]）：纯 deadline 状态机，眨眼 / 抖翅 / 跳各管各的。
//!
//! **内容只放"永远是事实"的东西**：版本号、commit、键位、tips 池里抽的一条。
//! 想告诉用户新能力，就写进 `shared::tips` 池，那里只讲稳定能力。
//!
//! 窄终端按宽度档位降级：整块 → 只文字列 → 一行 wordmark，任何宽度都不截断。
//!
//! ## 重绘成本
//!
//! 海鸥是常驻 idle 循环（与 dsh 的像素鲸鱼同语义），但**只在欢迎屏真的在视口里
//! 时才走时钟**：会话一旦长出消息、header 被滚出视口，`needs_rebuild` /
//! `next_frame` 全部短路，run loop 的那个 select 臂停摆 —— 看不见的东西不花钱。
//! 用户滚回顶部看历史时，动作自然续上。

pub mod art;
pub mod motion;
pub mod sprite;
pub mod wordmark;

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
use crate::ui::shimmer::is_light_theme;
use crate::ui::shimmer::to_rgb;
use art::FLY_0;
use art::FLY_0_COLS;
use art::FLY_1;
use art::FLY_2;
use art::FLY_3;
use art::FLY_4;
use art::FLY_5;
use art::PERCHED_BLINK;
use art::PERCHED_FLUTTER1;
use art::PERCHED_FLUTTER2;
use art::PERCHED_IDLE;
use art::PERCHED_IDLE_COLS;
use motion::Motion;
use motion::PerchedFrame;
use motion::Pose;

/// 开屏扫光时长：扫完永久定格（之后不再为它重建 / 重绘）。
pub const SWEEP_MS: u64 = 2_400;

/// 扫光帧间隔（≈25fps）。
///
/// 扫光期必须**逐帧**排 tick：空闲会话没有别的重绘驱动（100ms 的系统 tick 只在
/// 干活时才画），而待机的动作 deadline 下界是 `BLINK_GAP_MS = 2600 > SWEEP_MS`
/// —— 只按动作 deadline 排的话，2.4s 窗口里一次都不会醒，光带从头到尾不出现。
const FRAME_MS: u64 = 40;

/// 海鸥在 header 里占的终端行数（两个姿态对齐到同一高度，切换不跳版）。
const ART_TERM_ROWS: usize = 13;

/// 站姿在盒子里默认下移的像素行 —— 头顶余量：抬升（呼吸 / 跳）把整帧上移，
/// 没有余量就会裁掉头冠的像素行，读起来像被压扁而不是跳起来。
const PERCHED_TOP_PAD: usize = 2;

/// 海鸥与右侧文字列之间的空列数。
const ART_GAP: usize = 3;

/// 海鸥的最宽列数（两个姿态取大）。
const ART_COLS: usize = if PERCHED_IDLE_COLS > FLY_0_COLS {
    PERCHED_IDLE_COLS
} else {
    FLY_0_COLS
};

/// 整块布局需要的最小列数：海鸥 + 间隔 + 文字预算。
///
/// 文字预算刻意压得比"最长一行"窄 —— 键位 / tip / 入口行允许省略，海鸥不能让
/// 出去：`App::draw` 传进来的是**内容宽**（终端宽减滚动条 gutter 2），这条线要
/// 让 80 列（多数终端的默认宽）也落在整块档里。
const FULL_MIN: u16 = (ART_COLS + ART_GAP + TEXT_MIN) as u16;

/// 整块档里右列的文字预算（列）。最长的键位行约 40 列，超出按显示宽度省略。
const TEXT_MIN: usize = 34;

/// 文字列布局需要的最小列数（再窄就只剩一行 wordmark）。
const COMPACT_MIN: u16 = 40;

/// 右列行数（wordmark 3 + 版本 + 空 + 键位 + tip + 入口）。
const TEXT_ROWS: usize = 8;

/// 右列在 12 行海鸥里的竖排起点（居中）。
const TEXT_OFFSET: usize = (ART_TERM_ROWS - TEXT_ROWS) / 2;

/// header 的宽度档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// 海鸥 + 文字列。
    Full,
    /// 只有文字列（窄屏先撤海鸥：海鸥被截只是一块色块，文字被截才是读不出来的句子）。
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

/// 欢迎屏状态。
///
/// tip 在构造时抽定：整个进程一条，重绘 / 缩放 / 重建都不会换行（换行会显得
/// 界面在自己抖）。
#[derive(Debug, Clone)]
pub struct Welcome {
    /// 抽中的那条 tip（`shared::tips` 池内，整个进程不变）。
    tip: &'static tips::Tip,
    /// 一切时间刻度的原点（开屏时刻）。
    epoch: Instant,
    /// 动作规划器。
    motion: Motion,
    /// 上次构建用的终端宽度（0 = 还没构建过）。
    built_width: u16,
    /// 上次构建时欢迎屏在不在视口里。
    built_visible: bool,
    /// 上次构建时 agent 是不是在干活。
    built_working: bool,
    /// 上次构建画的姿态（`None` = 还没构建过）。
    built_pose: Option<Pose>,
    /// 上次构建的是不是"定格版"（开屏扫光已结束）。
    settled_built: bool,
}

impl Welcome {
    /// `seed` 决定抽中哪条 tip（`tips::seed_now()` 给生产用，测试传常数）。
    pub fn new(seed: u64, now: Instant) -> Self {
        Self {
            tip: tips::pick(seed),
            epoch: now,
            motion: Motion::new(0, seed),
            built_width: 0,
            built_visible: false,
            built_working: false,
            built_pose: None,
            settled_built: false,
        }
    }

    /// 抽中的那条 tip（首帧那一行与测试读它）。
    pub fn tip(&self) -> &'static tips::Tip {
        self.tip
    }

    /// 开屏以来的毫秒刻度。
    fn ms(&self, now: Instant) -> u64 {
        now.duration_since(self.epoch).as_millis() as u64
    }

    /// 开屏扫光是否还在跑。
    fn sweeping(&self, now: Instant) -> bool {
        self.ms(now) < SWEEP_MS
    }

    /// 需要重建 header 吗。
    ///
    /// `visible` = 欢迎屏还在视口里（见 `ChatView::header_in_view`）：不在就一切
    /// 短路 —— 常驻 idle 循环的前提是"看不见就不花钱"。
    pub fn needs_rebuild(
        &mut self,
        width: u16,
        now: Instant,
        working: bool,
        visible: bool,
    ) -> bool {
        if !visible {
            self.built_visible = false;
            return false;
        }
        let ms = self.ms(now);
        // 从视口外回来：待机节奏从此刻重排，否则四个平面同时到点（回屏第一帧
        // 会叠成"眨眼 + 跳 + 呼吸"）。
        if visible && !self.built_visible {
            self.motion.resume(ms, working);
        }
        // 先推进规划器再问姿态：姿态变化是 advance 的**产物**，不推进就永远
        // 看到旧姿态 -> 不重建 -> 不 advance 的死锁（真 TUI 里动画会定格）。
        self.motion.advance(ms, working);
        let pose = self.motion.pose(ms, working);
        // 扫光期逐帧重建；**扫光结束那一刻也要重建一次** —— 否则最后一帧的
        // 高光会留在屏上，直到下一个动作 deadline（可达 ~2.6s）。
        let sweeping = self.sweeping(now);
        let rebuild = self.built_width != width
            || !self.built_visible
            || self.built_working != working
            || self.built_pose != Some(pose)
            || (!self.settled_built && sweeping)
            || self.settled_built == sweeping;
        self.built_visible = true;
        rebuild
    }

    /// 下一帧的**绝对**截止时刻（`None` = 不需要 tick：定格且不在视口外无事可做）。
    ///
    /// 绝对时刻而不是"睡 40ms"：事件循环里任何事件都会重建这个 future，相对
    /// sleep 会被输入 / 流式事件无限推后，动画就卡死在原地。
    pub fn next_frame(&self, now: Instant, working: bool, visible: bool) -> Option<Instant> {
        if !visible {
            return None;
        }
        let ms = self.ms(now);
        let mut due = self.motion.next_due(working);
        if self.sweeping(now) {
            // 扫光按帧节奏走，并且不越过扫光结束点。
            let frame = (ms / FRAME_MS + 1) * FRAME_MS;
            due = due.min(frame.min(SWEEP_MS));
        }
        // 至少往前 1ms：deadline 落在过去会让 select 臂空转。
        Some(self.epoch + Duration::from_millis(due.max(ms.saturating_add(1))))
    }

    /// 构建 header 行。调用方负责把它交给 chat view（见 `App::sync_welcome`）。
    pub fn build(
        &mut self,
        palette: &ThemePalette,
        width: u16,
        now: Instant,
        working: bool,
        visible: bool,
    ) -> Vec<Line<'static>> {
        let ms = self.ms(now);
        self.motion.advance(ms, working);
        let pose = self.motion.pose(ms, working);
        let sweep = self.sweeping(now).then(|| ms as f32 / SWEEP_MS as f32);

        self.built_width = width;
        self.built_visible = visible;
        self.built_working = working;
        self.built_pose = Some(pose);
        self.settled_built = sweep.is_none();

        let accent = to_rgb(palette.accent);
        let light = is_light_theme(to_rgb(palette.text));
        let wm = wordmark::lines(sweep, accent, light);

        match layout_for(width) {
            Layout::Full => {
                let left = ART_COLS + ART_GAP;
                let text = text_column(
                    palette,
                    width.saturating_sub(left as u16) as usize,
                    wm,
                    self.tip.text,
                );
                let art = gull_lines(pose, accent);
                let mut lines = Vec::with_capacity(ART_TERM_ROWS + 2);
                lines.push(Line::from(""));
                for (row, art_line) in art.into_iter().enumerate() {
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
                    wm,
                    self.tip.text,
                ));
                lines.push(Line::from(""));
                lines
            }
            Layout::Minimal => vec![
                Line::from(""),
                wordmark_line(palette, width.saturating_sub(1) as usize),
                Line::from(""),
            ],
        }
    }
}

/// 这一帧的海鸥：姿态 → 字母网格 + 竖直偏移，统一补到 [`ART_TERM_ROWS`] 行。
fn gull_lines(pose: Pose, accent: sprite::Rgb) -> Vec<Line<'static>> {
    match pose {
        Pose::Perched { frame, lift } => {
            let grid = match frame {
                PerchedFrame::Idle => PERCHED_IDLE,
                PerchedFrame::Blink => PERCHED_BLINK,
                PerchedFrame::Flutter1 => PERCHED_FLUTTER1,
                PerchedFrame::Flutter2 => PERCHED_FLUTTER2,
            };
            // 头顶先补透明行再抬升（lift 0 站定 / 1 呼吸半格 / 2 跳起一格）：
            // 余量在网格里，抬升因此不裁头冠。
            let mut padded: Vec<&str> = vec![""; PERCHED_TOP_PAD];
            padded.extend_from_slice(grid);
            let rows = padded.len().div_ceil(2);
            sprite::lines_padded(
                &padded,
                accent,
                -(lift as i32),
                ART_TERM_ROWS.saturating_sub(rows),
                ART_TERM_ROWS,
            )
        }
        Pose::Flying { frame } => {
            let grid = match frame % 6 {
                0 => FLY_0,
                1 => FLY_1,
                2 => FLY_2,
                3 => FLY_3,
                4 => FLY_4,
                _ => FLY_5,
            };
            let rows = grid.len().div_ceil(2);
            // 居中：飞行姿态比盒子矮，上下各留一点比贴顶好看。
            sprite::lines_padded(
                grid,
                accent,
                0,
                ART_TERM_ROWS.saturating_sub(rows) / 2,
                ART_TERM_ROWS,
            )
        }
    }
}

/// 右侧文字列：wordmark（3 行）/ 版本 / 空 / 键位 / tip / 入口。
fn text_column(
    palette: &ThemePalette,
    width: usize,
    wm: Vec<Line<'static>>,
    tip: &str,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    let mut out = Vec::with_capacity(TEXT_ROWS);
    for line in wm {
        out.push(elide_line(line, width));
    }
    // 像素大字是品牌形，但终端里还得有**文本**形态的 brand（grep / 读屏 / 测试
    // 都读像素）—— 版本行带上它：`wing · dev · <commit>`。
    out.push(dim_line(&format!("wing · {}", version_label()), width, dim));
    out.push(Line::from(""));
    out.push(dim_line(KEYS, width, dim));
    out.push(tip_line(tip, width, dim, text));
    out.push(dim_line(&hints_text(), width, dim));
    out
}

/// 键位提示（一行）。
///
/// 换行写 `Ctrl+J` 而不是 `Shift+Enter`：后者能不能到达取决于终端有没有
/// 实现 kitty 键盘协议（见 `tui::push_keyboard_enhancement`），在 header 上
/// 当既成事实写死就是在骗人 —— tips 池里有那条"看终端"的提示，这里只放
/// 任何终端都成立的键。
const KEYS: &str = "Esc 中断 · Ctrl+J 换行 · Ctrl+C×2 退出";

/// 入口提示（一行）。
///
/// `/tips` 从常量拼，命令改名时这一行跟着走；「输入 / 看命令」不带命令名 ——
/// 命令表来自网关（用户自己的 prompt 命令），写死某一个名字迟早会是假的。
fn hints_text() -> String {
    format!("{TIPS_COMMAND} 全部提示 · 输入 / 看全部命令")
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

/// 一行单色文字，按可用宽度截断。
fn dim_line(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(elide(text, width), style))
}

/// wordmark 按宽度截断（窄到放不下就截字形，不换行）。
fn elide_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if used <= width {
        return line;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut budget = width;
    for span in line.spans {
        let w = span.content.width();
        if w <= budget {
            budget -= w;
            out.push(span);
        } else {
            let kept: String = span
                .content
                .chars()
                .scan(0usize, |acc, ch| {
                    let cw = ch.to_string().width();
                    if *acc + cw <= budget {
                        *acc += cw;
                        Some(ch)
                    } else {
                        None
                    }
                })
                .collect();
            out.push(Span::styled(kept, span.style));
            break;
        }
    }
    Line::from(out)
}

/// 最窄档：一行 `wing` + 版本。
fn wordmark_line(palette: &ThemePalette, width: usize) -> Line<'static> {
    let brand = elide("wing", width);
    let used = brand.width();
    let mut spans = vec![Span::styled(
        brand,
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )];
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
    use unicode_width::UnicodeWidthStr as _;

    #[allow(unused_imports)]
    use ratatui::style::Color as _Color;

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
            let mut w = welcome();
            let now = Instant::now();
            for working in [false, true] {
                let lines = w.build(&palette, width, now, working, true);
                for line in &lines {
                    assert!(
                        line_width(line) <= width as usize,
                        "width={width} working={working} 行超宽：{line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn full_layout_has_the_gull_and_the_wordmark() {
        let mut w = welcome();
        let lines = w.build(&palette(), 120, Instant::now(), false, true);
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // 用品牌色指纹判海鸥在场：wordmark 也画 `▀`/`▄`，只数半格字符的话
        // 把海鸥整个删掉断言照样绿。
        let amber = ratatui::style::Color::Rgb(245, 169, 60); // 喙 / 脚：只有海鸥用
        assert!(
            lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| s.style.fg == Some(amber) || s.style.bg == Some(amber))),
            "海鸥得画出来"
        );
        assert!(
            joined.contains("dev ·") || joined.contains("v0."),
            "版本行得在"
        );
    }

    #[test]
    fn sweep_ticks_on_the_frame_cadence() {
        // 回归点：只按动作 deadline 排 tick 的话，扫光窗口里几乎不醒（待机
        // deadline 下界 2600ms > SWEEP_MS），光带根本不会出现。
        let mut w = welcome();
        let start = w.epoch;
        let mut now = start;
        let mut ticks = 0;
        let mut worst_gap = 0u128;
        while let Some(due) = w.next_frame(now, false, true) {
            if due >= start + Duration::from_millis(SWEEP_MS) {
                break;
            }
            assert!(due > now, "deadline 必须严格递增：now={now:?} due={due:?}");
            worst_gap = worst_gap.max((due - now).as_millis());
            ticks += 1;
            now = due;
            // 生产口径：每次 tick 都会走一遍 draw -> sync_welcome ->
            // needs_rebuild（那里推进规划器）。不推进的话 `next_due` 冻结在
            // 过去的时刻，`max(ms+1)` 会以 1ms 步进"救火"凑出 tick 数 ——
            // 那样测的就不是帧节奏了。
            w.needs_rebuild(100, now, false, true);
        }
        // 2400/40 = 60 帧，外加动作 deadline 恰好插进帧网格的那一两次。
        assert!(
            (58..=62).contains(&ticks),
            "2.4s 扫光应当是 ~60 帧（每帧一 tick），实际 {ticks}"
        );
        assert!(
            worst_gap <= FRAME_MS as u128,
            "扫光的相邻 tick 间隔最长 {worst_gap}ms > 一帧 {FRAME_MS}ms"
        );
    }

    #[test]
    fn elide_respects_display_width() {
        // `elide` 服务 dim_line / wordmark_line 热路径：按显示宽度（CJK 记 2 列）
        // 截断，超宽补 `…`，不超宽原样返回。
        assert_eq!(elide("abc", 5), "abc");
        assert_eq!(elide("abcde", 5), "abcde");
        assert_eq!(elide("abcdef", 5), "abcd…");
        assert_eq!(elide("中文中文", 5), "中文…");
        assert_eq!(elide("中文中文", 4), "中…");
        assert_eq!(elide("abc", 0), "");
        for width in 0..12usize {
            assert!(elide("中abc中", width).width() <= width, "width={width}");
        }
    }

    #[test]
    fn keys_fit_an_80_column_terminal() {
        // 80 列（多数终端的默认宽）是最常见的档位：内容宽 = 80 - 滚动条 gutter(2)，
        // 减去海鸥与间隔就是文字预算。键位行差 1 列就会在最多人看到的宽度上省略。
        let budget = 80 - 2 - (ART_COLS + ART_GAP);
        assert!(
            KEYS.width() <= budget,
            "键位行 {} 列 > 80 列终端的预算 {budget} 列",
            KEYS.width()
        );
    }

    #[test]
    fn dev_builds_show_dev_not_the_placeholder_version() {
        // 产品不变量：占位版本 0.0.0 是开发构建，绝不能露成 "v0.0.0"（会让人
        // 以为装错了包）。发版时由 scripts/sync_version.py 换成真版本号。
        let label = version_label();
        if env!("CARGO_PKG_VERSION") == "0.0.0" {
            assert!(label.starts_with("dev · "), "开发构建：{label}");
        } else {
            assert!(label.starts_with('v'), "发版构建：{label}");
        }
        assert!(
            label.contains(env!("WING_COMMIT_HASH")),
            "带 commit：{label}"
        );
    }

    #[test]
    fn tip_is_picked_once_per_process() {
        // 重建 / 缩放不许换 tip：换行会显得界面在自己抖。
        let mut w = welcome();
        let tip = w.tip();
        let now = Instant::now();
        for (width, working) in [(120u16, false), (60, true), (30, false), (120, false)] {
            w.build(&palette(), width, now, working, true);
            assert!(std::ptr::eq(w.tip(), tip), "tip 不该在重建之间换");
        }
    }

    #[test]
    fn sweep_end_forces_one_last_rebuild() {
        // 扫光结束不是姿态变化，很容易漏掉：没有这一下，最后一帧的高光会留在
        // 屏上，直到下一个动作 deadline（可达 ~2.6s）。
        let mut w = welcome();
        let start = w.epoch;
        let mid = start + Duration::from_millis(SWEEP_MS - 300);
        w.build(&palette(), 100, mid, false, true);
        let just_after = start + Duration::from_millis(SWEEP_MS + 1);
        assert!(
            w.needs_rebuild(100, just_after, false, true),
            "扫光结束那一刻必须重建一次"
        );
        // 重建之后（已定格）就不该再因为扫光反复重建。
        w.build(&palette(), 100, just_after, false, true);
        assert!(!w.needs_rebuild(100, just_after, false, true));
    }

    #[test]
    fn offscreen_welcome_costs_nothing() {
        let mut w = welcome();
        let now = Instant::now();
        w.build(&palette(), 120, now, false, true);
        assert!(!w.needs_rebuild(120, now, false, false), "滚出视口就不重建");
        assert!(
            w.next_frame(now, false, false).is_none(),
            "滚出视口就不走时钟"
        );
    }

    #[test]
    fn working_switch_rebuilds() {
        let mut w = welcome();
        let now = Instant::now();
        w.build(&palette(), 120, now, false, true);
        assert!(
            w.needs_rebuild(120, now, true, true),
            "干活/闲切换要换姿态，必须重建"
        );
    }

    #[test]
    fn settled_welcome_stops_rebuilding() {
        let mut w = welcome();
        let start = Instant::now();
        let settled = start + Duration::from_millis(SWEEP_MS + 500);
        w.build(&palette(), 120, settled, false, true);
        // 定格后、姿态没到点：不重建。
        assert!(!w.needs_rebuild(120, settled, false, true));
    }

    #[test]
    fn art_letters_are_in_palette() {
        let accent = (34, 211, 238);
        let grids: &[&[&str]] = &[
            PERCHED_IDLE,
            PERCHED_BLINK,
            PERCHED_FLUTTER1,
            PERCHED_FLUTTER2,
            FLY_0,
            FLY_1,
            FLY_2,
            FLY_3,
            FLY_4,
            FLY_5,
        ];
        for grid in grids {
            for row in *grid {
                for ch in row.chars() {
                    assert!(
                        ch == '.' || sprite::brand(ch, accent).is_some(),
                        "帧数据里有调色板不认识的字母 {ch:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn both_poses_share_one_height() {
        let accent = (34, 211, 238);
        assert_eq!(
            gull_lines(
                Pose::Perched {
                    frame: PerchedFrame::Idle,
                    lift: 0
                },
                accent
            )
            .len(),
            ART_TERM_ROWS
        );
        assert_eq!(
            gull_lines(Pose::Flying { frame: 0 }, accent).len(),
            ART_TERM_ROWS
        );
        assert_eq!(
            gull_lines(
                Pose::Perched {
                    frame: PerchedFrame::Idle,
                    lift: 2
                },
                accent
            )
            .len(),
            ART_TERM_ROWS
        );
    }

    /// 渲染出来的源像素总数（半格：带背景的格 = 2 像素，只带前景 = 1）。
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

    #[test]
    fn no_frame_loses_a_pixel() {
        // 盒子装不下时会**静默截断**（`resize_with`，丢的是底行），只断言行数
        // 是测不出来的。这里把"渲染出的源像素总数 == 网格里的墨迹数"钉死在
        // 每一帧上：站姿四帧 + 飞行六帧。
        let accent = (34, 211, 238);
        let source_ink = |grid: &[&str]| -> usize {
            grid.iter()
                .map(|r| r.chars().filter(|c| *c != '.').count())
                .sum()
        };
        let perched = [
            (PerchedFrame::Idle, PERCHED_IDLE),
            (PerchedFrame::Blink, PERCHED_BLINK),
            (PerchedFrame::Flutter1, PERCHED_FLUTTER1),
            (PerchedFrame::Flutter2, PERCHED_FLUTTER2),
        ];
        for (frame, grid) in perched {
            let lines = gull_lines(Pose::Perched { frame, lift: 0 }, accent);
            assert_eq!(
                rendered_pixels(&lines),
                source_ink(grid),
                "{frame:?} 丢像素了"
            );
        }
        for (frame, grid) in [FLY_0, FLY_1, FLY_2, FLY_3, FLY_4, FLY_5]
            .iter()
            .enumerate()
        {
            let lines = gull_lines(Pose::Flying { frame }, accent);
            assert_eq!(
                rendered_pixels(&lines),
                source_ink(grid),
                "飞行第 {frame} 帧丢像素了"
            );
        }
    }

    #[test]
    fn lift_never_crops_the_sprite() {
        // 抬升是"整帧上移"：没有头顶余量就会裁掉头冠，读起来像被压扁。
        // 钉住**源像素总数**（半格渲染里：带背景的格 = 2 像素，只带前景 = 1）——
        // 抬升只该改变像素落在哪一格，不该让任何像素消失。
        let accent = (34, 211, 238);
        let pixels = |lift: u8| -> usize {
            gull_lines(
                Pose::Perched {
                    frame: PerchedFrame::Idle,
                    lift,
                },
                accent,
            )
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
        };
        let stand = pixels(0);
        assert!(stand > 200, "站姿该有一整只海鸥的墨迹，实际 {stand}");
        assert_eq!(pixels(1), stand, "呼吸抬半格丢像素了");
        assert_eq!(pixels(2), stand, "跳起抬一格丢像素了");
    }

    #[test]
    fn accent_letter_follows_the_theme() {
        // `A` 是唯一跟主题走的字母：换个 accent 就该换个颜色。
        assert_eq!(sprite::brand('A', (1, 2, 3)), Some((1, 2, 3)));
        assert_eq!(
            sprite::brand('W', (1, 2, 3)),
            sprite::brand('W', (9, 9, 9)),
            "品牌色不该跟主题变"
        );
    }
}
