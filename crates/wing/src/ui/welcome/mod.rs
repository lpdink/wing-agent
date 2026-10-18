//! 欢迎屏 —— 空会话时 chat 顶部那一段（随会话滚走）。
//!
//! 拼成它的五样东西：
//!
//! * **海鸥**（[`art`] 的字母网格 + [`sprite`] 的半格渲染）：待机是站姿 chibi，
//!   agent 干活时切成飞行扇翅 —— "它在飞" = "它在干活"；
//! * **wordmark**（[`wordmark`]）：手绘 5 行像素大字 `WING`，开屏一道扫光扫过；
//!   按信息列宽度 1x / 2x 两档（半格像素只能整数倍放大，见 [`wordmark::Scale`]）；
//! * **名牌右列**：版本 / 会话事实 / **一条分节线** / 键位 / 轮换 tip / 入口。
//!   没有框 —— 这是信笺（logo 下面是文字），不是一张表：框把两簇内容圈成了
//!   "一张有六个格子的表"，还让分隔用的空行看起来像"数据缺了一行"；
//! * **会话事实**：[`SessionFacts`]（`2 skills · 1 rule`）—— 这个会话加载了什么，
//!   随 SyncSession 到达；0/0 时槽位留空（行数因此恒定，见 [`info_rows`]）；
//! * **动作规划器**（[`motion`]）：纯 deadline 状态机，眨眼 / 抖翅 / 跳各管各的。
//!
//! **内容只放"永远是事实"的东西**：版本号、commit、会话的加载事实、键位、tips
//! 池里抽的一条。想告诉用户新能力，就写进 `shared::tips` 池，那里只讲稳定能力。
//!
//! ## 版面：空会话时是一屏开屏，不是一段贴顶的日志
//!
//! transcript 还空着（[`Viewport::empty`]）时整块**水平居中**，竖直位置交给
//! App 的开屏版式（[`splash_pad`]）：牌子与输入卡作为一个整体停在视口的光学中心，
//! 下面留一片空气吸收弹层与草稿长大。一旦有了消息就退回左对齐 + 一格留白 ——
//! header 从此只是会话的开头，跟消息一起滚走，插在 chat band 的最上面。
//!
//! 窄终端按宽度档位降级：整块（大字 + 名牌）→ 只名牌 → 一行 wordmark，任何宽度
//! 都不截断、不折行、不溢出。
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
use crate::render::table::split_str_by_width;
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

/// 空会话时整块停在视口高度的这个百分比处 —— **光学中心**略高于几何中心：
/// 一屏里重的东西（图标、标题）稍稍上移比正中摆着更稳（老印刷排版的老规矩）。
const HERO_CENTER_PCT: u16 = 45;

/// 名牌右列里分节线的位置：版本 / 会话事实之后（0 起的行号 + 1）。
///
/// 线上是"这是什么"（身份），线下是"你能做什么"（键位 + tip + 入口）。
const RULE_AFTER_ROW: usize = 2;

/// 分节线的字形（细横线；重框退役了，见模块文档）。
const RULE: char = '─';

/// 这一帧给欢迎屏的版面输入。
///
/// 两个都是**内容的输入**（不是位置的）：宽度决定档位（[`Layout`]），`empty` 决定
/// 是"开屏"（水平居中、竖直位置由 App 的版式给）还是"会话的开头"（左对齐 + 一格
/// 留白）。两者一起进 [`Welcome::needs_rebuild`] 的键 —— 只记宽度的话，第一条消息
/// 落地不会重排。
///
/// 竖直位置**不在这里**：开屏那一块要和输入卡一起算光学中心（见 [`splash_pad`]），
/// 而输入卡是 App 的版块 —— 由 App 排版比让 header 自己塞空行更诚实。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    /// 内容宽（开屏 = 终端整宽，没有滚动条；会话里 = band 减滚动条 gutter）。
    pub width: u16,
    /// transcript 里除了这块 header 什么都没有。
    pub empty: bool,
}

impl Viewport {
    /// 会话里的构造（不居中，等同于"已经有消息了"）。
    pub fn width_only(width: u16) -> Self {
        Self {
            width,
            empty: false,
        }
    }
}

/// 开屏整块上方的空气行数：把「牌子 + 一格间隔 + 输入卡」当一个整体放在**光学
/// 中心**（略高于几何中心，见 [`HERO_CENTER_PCT`]）。
///
/// `reserved` 是牌子下面那些行的**基线**高度（输入卡单行 + 间隔）—— 弹层与草稿
/// 长大**不**进这个账：它们由牌子下面的空气吸收（App 的 `frame_layout`）。否则用户
/// 每敲一个 `/` 打开命令列表，整块都会往上跳几行。
pub fn splash_pad(screen_h: u16, block_h: u16, reserved: u16) -> u16 {
    let slack = screen_h
        .saturating_sub(1) // 状态栏
        .saturating_sub(block_h)
        .saturating_sub(reserved);
    (slack as u32 * HERO_CENTER_PCT as u32 / 100) as u16
}

/// 会话事实 —— 这个会话加载了多少 skills / rules。
///
/// 名牌右列的**环境事实**行（`2 skills · 1 rule`）：它随每次 SyncSession 到达，
/// 曾经是一条渲染在 transcript 顶部的 system 消息（`⦁ system` + 日志式英文），
/// 开屏时紧贴在像素名牌下面 —— 那里没有它的位置。
///
/// 0/0（或老网关）时这一行是**空白占位**、不是缺席：槽位恒在，名牌高度才不随
/// sync 变化（见 [`text_column`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFacts {
    /// 已加载的 skill 数（`AgentInfo.skills`）。
    pub skills: usize,
    /// 已加载的 rule 文件数（`AgentInfo.rules`）。
    pub rules: usize,
}

impl SessionFacts {
    /// 事实行文案（`2 skills · 1 rule`）；什么都没加载时为 `None`。
    ///
    /// 复数按英语习惯走（`1 rule`，不是 `1 rules` —— 旧 banner 的复数错就错在
    /// 数字在前、名词固定复数）。
    fn line(self) -> Option<String> {
        let mut parts = Vec::new();
        if self.skills > 0 {
            parts.push(format!("{} skill{}", self.skills, plural(self.skills)));
        }
        if self.rules > 0 {
            parts.push(format!("{} rule{}", self.rules, plural(self.rules)));
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join(" · "))
    }
}

/// 英语复数后缀（`1` → 空）。
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

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
    /// 上次构建用的版面输入（`None` = 还没构建过）。
    built_view: Option<Viewport>,
    /// 上次构建时欢迎屏在不在视口里。
    built_visible: bool,
    /// 上次构建时 agent 是不是在干活。
    built_working: bool,
    /// 上次构建画的姿态（`None` = 还没构建过）。
    built_pose: Option<Pose>,
    /// 上次构建的是不是"定格版"（开屏扫光已结束）。
    settled_built: bool,
    /// 上次构建用到的会话事实（`None` = 无 / 还没同步到）。
    built_facts: Option<SessionFacts>,
    /// 上次构建出来的牌子宽度（不含居中位移）。
    block_width: usize,
    /// 构建次数（只给测试：钉住"一帧最多建一次"）。
    #[cfg(test)]
    builds: u32,
}

impl Welcome {
    /// `seed` 决定抽中哪条 tip（`tips::seed_now()` 给生产用，测试传常数）。
    pub fn new(seed: u64, now: Instant) -> Self {
        Self {
            tip: tips::pick(seed),
            epoch: now,
            motion: Motion::new(0, seed),
            built_view: None,
            built_visible: false,
            built_working: false,
            built_pose: None,
            settled_built: false,
            built_facts: None,
            block_width: 0,
            #[cfg(test)]
            builds: 0,
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
    /// 短路 —— 常驻 idle 循环的前提是"看不见就不花钱"。`facts` 是这一帧的会话
    /// 事实（sync 到达时变化）；不可见时我们不重建它，但也不记进 `built_facts`
    /// —— 回到视口那一下的 `!built_visible` 会带着最新事实重建。
    pub fn needs_rebuild(
        &mut self,
        view: Viewport,
        now: Instant,
        working: bool,
        visible: bool,
        facts: Option<SessionFacts>,
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
        let rebuild = self.built_view != Some(view)
            || !self.built_visible
            || self.built_working != working
            || self.built_pose != Some(pose)
            || self.built_facts != facts
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
        view: Viewport,
        now: Instant,
        working: bool,
        visible: bool,
        facts: Option<SessionFacts>,
    ) -> Vec<Line<'static>> {
        let ms = self.ms(now);
        self.motion.advance(ms, working);
        let pose = self.motion.pose(ms, working);
        let sweep = self.sweeping(now).then(|| ms as f32 / SWEEP_MS as f32);

        #[cfg(test)]
        {
            self.builds += 1;
        }
        self.built_view = Some(view);
        self.built_visible = visible;
        self.built_working = working;
        self.built_pose = Some(pose);
        self.settled_built = sweep.is_none();
        self.built_facts = facts;

        let accent = to_rgb(palette.accent);
        let light = is_light_theme(to_rgb(palette.text));
        let width = view.width;

        // 每一档都产出（行, 块宽）：块宽决定水平居中的位移，行自己只管左对齐。
        let (mut lines, block) = match layout_for(width) {
            Layout::Full => {
                let left = ART_COLS + ART_GAP;
                let (column, column_width) = text_column(
                    palette,
                    width.saturating_sub(left as u16) as usize,
                    sweep,
                    accent,
                    light,
                    self.tip.text,
                    facts,
                );
                let art = gull_lines(pose, accent);
                // 右列竖直居中：2x 大字（5 + 6 行）正好填满海鸥盒，1x（3 + 6
                // 行）上下各留一格。
                let offset = ART_TERM_ROWS.saturating_sub(column.len()) / 2;
                let mut lines = Vec::with_capacity(ART_TERM_ROWS);
                for (row, art_line) in art.into_iter().enumerate() {
                    let mut spans: Vec<Span<'static>> = art_line.spans;
                    let used = width_of_spans(&spans);
                    spans.push(Span::raw(" ".repeat(left.saturating_sub(used))));
                    if let Some(text_line) = row.checked_sub(offset)
                        && let Some(line) = column.get(text_line)
                    {
                        spans.extend(line.spans.iter().cloned());
                    }
                    lines.push(Line::from(spans));
                }
                (lines, left + column_width)
            }
            Layout::Compact => {
                let (column, column_width) = text_column(
                    palette,
                    width.saturating_sub(1) as usize,
                    sweep,
                    accent,
                    light,
                    self.tip.text,
                    facts,
                );
                (column, column_width)
            }
            Layout::Minimal => {
                let line = wordmark_line(palette, width.saturating_sub(1) as usize);
                let block = width_of(&line);
                (vec![line], block)
            }
        };

        self.block_width = block;

        // 水平居中：整块居中的看着才像"一屏"，逐行居中会让名牌左右乱窜。
        // 只对空会话生效 —— 有了消息，它就是 transcript 的第一段，左对齐。
        let x_pad = if view.empty {
            (width as usize).saturating_sub(block) / 2
        } else {
            0
        };
        if x_pad > 0 {
            for line in &mut lines {
                if width_of(line) > 0 {
                    line.spans.insert(0, Span::raw(" ".repeat(x_pad)));
                }
            }
        }

        let mut out = Vec::with_capacity(lines.len() + 2);
        // 会话里（顶对齐）留一格呼吸；开屏的空气由 App 的版式给（它要和输入卡
        // 一起算光学中心），header 自己不再塞空行。
        if !view.empty {
            out.push(Line::from(""));
        }
        out.append(&mut lines);
        out.push(Line::from(""));
        out
    }

    /// 构建过多少次（测试用：钉住"一帧最多建一次"）。
    #[cfg(test)]
    pub(crate) fn builds(&self) -> u32 {
        self.builds
    }

    /// 上一帧那块牌子的宽度（**不含**水平居中塞进去的空列）。
    ///
    /// 开屏版式要用它来定输入卡的宽度：输入卡与牌子同宽居中，读起来是一块东西。
    pub fn block_width(&self) -> u16 {
        self.block_width.min(u16::MAX as usize) as u16
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

/// 右侧文字列：wordmark（1x 3 行 / 2x 5 行）+ 名牌（版本 / 事实 / 分节线 / 键位 /
/// tip / 入口）。
///
/// 返回（行, 块宽）：块宽 = `max(大字宽, 最宽的一行)`，不超过可用列数。分节线按
/// 块宽画满 —— 它就是这块文字右缘的定义，没有框之后，线是唯一的收边。
///
/// 事实只在这个会话真的有加载物时出现；它是**环境事实**，所以和版本行同簇、
/// 同 dim 寄存器 —— 曾经它是 transcript 顶部的 system 消息，开屏时贴在像素名牌
/// 下面，读起来像一行日志。
fn text_column(
    palette: &ThemePalette,
    width: usize,
    sweep: Option<f32>,
    accent: sprite::Rgb,
    light: bool,
    tip: &str,
    facts: Option<SessionFacts>,
) -> (Vec<Line<'static>>, usize) {
    let scale = wordmark::scale_for(width);
    let mut out = wordmark::lines(scale, sweep, accent, light);

    let rows = info_rows(palette, tip, facts);
    let widest = rows.iter().map(width_of).max().unwrap_or(0);
    let block = widest.max(scale.columns()).min(width);

    let dim = Style::default().fg(palette.dim);
    let mut body: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 1);
    for (index, row) in rows.iter().enumerate() {
        if index == RULE_AFTER_ROW {
            body.push(rule_line(block, dim));
        }
        body.push(fit_line(row, block));
    }
    out.extend(body);
    (out, block)
}

/// 分节线：`block` 列细横线。
fn rule_line(block: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(RULE.to_string().repeat(block), style))
}

/// 名牌的行：版本 / 会话事实 / 键位 / tip / 入口（分节线在 [`RULE_AFTER_ROW`] 处插入）。
///
/// 像素大字是品牌形，但终端里还得有**文本**形态的 brand（grep / 读屏 / 测试
/// 都读像素）—— 版本行带上它：`wing · dev · <commit>`。
///
/// 事实槽位**恒在**：缺席（0/0 / 老网关）时留一行空白 —— 右列行数因此在两个
/// 布局档里都固定。Full 档本来就骑在海鸥的 13 行盒子里（高度由画盒定），
/// Compact 档文字列**就是**整块 header，多一行会让下方内容跳一行。
fn info_rows(palette: &ThemePalette, tip: &str, facts: Option<SessionFacts>) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    let key = Style::default().fg(palette.tool_result);
    let cmd = Style::default().fg(palette.accent);
    let facts = match facts.and_then(SessionFacts::line) {
        Some(line) => Line::from(Span::styled(line, dim)),
        None => Line::from(""),
    };
    vec![
        Line::from(Span::styled(format!("wing · {}", version_label()), dim)),
        facts,
        keys_line(key, dim),
        tip_line(tip, dim, text, cmd),
        hints_line(dim, cmd),
    ]
}

/// 键位提示（一行）：**按键亮、动作暗** —— 先看见敲什么，再读它做什么。
///
/// 换行写 `Ctrl+J` 而不是 `Shift+Enter`：后者能不能到达取决于终端有没有
/// 实现 kitty 键盘协议（见 `tui::push_keyboard_enhancement`），在 header 上
/// 当既成事实写死就是在骗人 —— tips 池里有那条"看终端"的提示，这里只放
/// 任何终端都成立的键。
const KEYS: &[(&str, &str)] = &[("Esc", "中断"), ("Ctrl+J", "换行"), ("Ctrl+C×2", "退出")];

fn keys_line(key: Style, dim: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(KEYS.len() * 3);
    for (index, (keys, verb)) in KEYS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", dim));
        }
        spans.push(Span::styled(*keys, key));
        spans.push(Span::styled(format!(" {verb}"), dim));
    }
    Line::from(spans)
}

/// 入口提示（一行）。
///
/// `/tips` 从常量拼，命令改名时这一行跟着走；「输入 / 看命令」不带命令名 ——
/// 命令表来自网关（用户自己的 prompt 命令），写死某一个名字迟早会是假的。
fn hints_line(dim: Style, cmd: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(TIPS_COMMAND, cmd),
        Span::styled(" 全部提示", dim),
        Span::styled(" · ", dim),
        Span::styled("输入 ", dim),
        Span::styled("/", cmd),
        Span::styled(" 看全部命令", dim),
    ])
}

/// `Tip  <正文>` —— 标签暗、正文正常色、正文里的 `/命令` verbatim 成 accent。
///
/// 不再按宽度预截断：调用方按块宽统一省略（[`fit_line`]），截断只发生一次
/// （两处各截一次就会多丢几个字）。
fn tip_line(tip: &str, dim: Style, text: Style, cmd: Style) -> Line<'static> {
    let mut spans = vec![Span::styled("Tip  ", dim)];
    spans.extend(with_commands(tip, text, cmd));
    Line::from(spans)
}

/// 把 `/xxx` 形态的词染成 accent：**命令是唯一可以照着敲的东西**，在整套 UI
/// 里都是 accent（popup 候选、行内代码、composer 的 workdir），名牌里若是 dim
/// 就成了看不见的字。
fn with_commands(text: &str, base: Style, cmds: Style) -> Vec<Span<'static>> {
    let flush = |out: &mut Vec<Span<'static>>, word: &mut String, is_cmd: bool| {
        if !word.is_empty() {
            out.push(Span::styled(
                std::mem::take(word),
                if is_cmd { cmds } else { base },
            ));
        }
    };
    let mut out: Vec<Span<'static>> = Vec::with_capacity(3);
    let mut word = String::new();
    let mut word_is_command = false;
    let mut at_word_start = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            flush(&mut out, &mut word, word_is_command);
            out.push(Span::styled(ch.to_string(), base));
            word_is_command = false;
            at_word_start = true;
            continue;
        }
        if at_word_start && ch == '/' {
            word_is_command = true;
        }
        at_word_start = false;
        word.push(ch);
    }
    flush(&mut out, &mut word, word_is_command);
    out
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

/// 一行的显示宽度（CJK 记 2 列）。
fn width_of(line: &Line<'_>) -> usize {
    width_of_spans(&line.spans)
}

/// 一组 span 的显示宽度。
fn width_of_spans(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// 把一行按显示宽度装进 `max` 列，切了就补 `…`。
///
/// 整 span 先过；第一个越界的 span 按**字符边界**切（CJK 记 2 列）。每次切都
/// 有标记：省略号的那一列先从预算里扣掉，所以"恰好用尽预算"的行结尾仍是 `…`
/// （一行本来就放得下时原样返回 —— 恰好装下不是切）。
///
/// 名牌的每一行都要过它：行数是版面契约（宽度档内高度恒定，折行会让下面的
/// 一切都挪位），所以降级只许丢掉行尾，不许长大。
fn fit_line(row: &Line<'static>, max: usize) -> Line<'static> {
    if width_of(row) <= max {
        return row.clone();
    }
    if max == 0 {
        return Line::from(Vec::new());
    }
    let budget = max - 1;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &row.spans {
        let width = span.content.width();
        if used + width <= budget {
            used += width;
            out.push(span.clone());
            continue;
        }
        let (head, _) = split_str_by_width(&span.content, budget - used);
        if head.is_empty() {
            out.push(Span::styled("…".to_string(), span.style));
        } else {
            out.push(Span::styled(format!("{head}…"), span.style));
        }
        return Line::from(out);
    }
    out.push(Span::raw("…"));
    Line::from(out)
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

    #[allow(unused_imports)]
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

    /// header 的纯文本（断言文案用）。
    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 名牌里 tip 那一行的下标（分节线在 `RULE_AFTER_ROW` 处插入，行序见 `info_rows`）。
    const TIP_ROW: usize = 3;

    fn facts(skills: usize, rules: usize) -> Option<SessionFacts> {
        Some(SessionFacts { skills, rules })
    }

    /// 名牌右列的一行：取版本行 `wing · ` 的起始列之后的尾部（框已退役，
    /// 没有边线可切；那一行左边是海鸥的字形）。
    fn nameplate_tail(line: &Line<'_>, column: usize) -> String {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        text.chars()
            .skip(column)
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// 名牌右列的**事实槽位**：版本行的下一行（槽位缺席时空白）。
    ///
    /// 断言走槽位而不是整块文本：tip 是抽签来的（池子里有 `/skills 看已装技能`
    /// 这类含 "skill" 的句子），对整块文本做"不含某词"的断言迟早会随池子改动
    /// 变红。
    fn facts_slot(lines: &[Line<'static>]) -> String {
        let version = lines
            .iter()
            .position(|line| line.spans.iter().any(|s| s.content.contains("wing · ")))
            .expect("版本行在名牌右列");
        let text: String = lines[version]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        // `find` 给的是字节偏移（`▀` 是 3 字节），`nameplate_tail` 按字符跳。
        let byte = text.find("wing · ").expect("版本行带 brand");
        let column = text[..byte].chars().count();
        nameplate_tail(&lines[version + 1], column)
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
        // 两种版面各跑一遍：**有消息**（左对齐，x_pad = 0）与**空会话**（整块水平
        // 居中，x_pad > 0）—— 后者正是"块宽"与"每行宽度"分成两个量的地方，
        // 少测一遍就少了一层保护。
        let palette = palette();
        let views = |width: u16| [Viewport::width_only(width), Viewport { width, empty: true }];
        for width in 2u16..200 {
            let now = Instant::now();
            for working in [false, true] {
                for facts in [None, facts(2, 1), facts(12, 130)] {
                    for view in views(width) {
                        let mut w = welcome();
                        let lines = w.build(&palette, view, now, working, true, facts);
                        for line in &lines {
                            assert!(
                                line_width(line) <= width as usize,
                                "width={width} empty={} working={working} facts={facts:?} \
                                 行超宽：{line:?}",
                                view.empty
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn facts_line_carries_the_counts_with_correct_plurals() {
        let mut w = welcome();
        let line = |w: &mut Welcome, facts| {
            facts_slot(&w.build(
                &palette(),
                Viewport::width_only(120),
                Instant::now(),
                false,
                true,
                facts,
            ))
        };
        assert_eq!(line(&mut w, facts(2, 1)), "2 skills · 1 rule");
        assert_eq!(line(&mut w, facts(1, 1)), "1 skill · 1 rule");
        // 只有一类加载物时另一类不出现。
        assert_eq!(line(&mut w, facts(0, 3)), "3 rules");
        assert_eq!(line(&mut w, facts(3, 1)), "3 skills · 1 rule");
    }

    #[test]
    fn facts_line_is_absent_when_nothing_is_loaded() {
        let mut w = welcome();
        let now = Instant::now();
        let none = w.build(
            &palette(),
            Viewport::width_only(120),
            now,
            false,
            true,
            None,
        );
        let zero = w.build(
            &palette(),
            Viewport::width_only(120),
            now,
            false,
            true,
            facts(0, 0),
        );
        assert_eq!(facts_slot(&none), "", "无事实（老网关）时槽位留空");
        assert_eq!(facts_slot(&zero), "", "0/0 时槽位留空");
        // 槽位恒在：事实缺席时那一行是空白 —— header 的**高度**在任何宽度
        // 档里都不随事实变化（Full 档由海鸥的画盒定，Compact 档由占位行定），
        // 所以 sync 到达时下方内容不会跳一行。
        assert_eq!(none.len(), zero.len());
        for width in [120u16, 96, 80, 70, 60, 40] {
            let without = w
                .build(
                    &palette(),
                    Viewport::width_only(width),
                    now,
                    false,
                    true,
                    None,
                )
                .len();
            let with = w
                .build(
                    &palette(),
                    Viewport::width_only(width),
                    now,
                    false,
                    true,
                    facts(2, 1),
                )
                .len();
            assert_eq!(without, with, "width={width}：事实行改变了 header 高度");
        }
    }

    #[test]
    fn facts_sit_right_after_the_version_line() {
        let mut w = welcome();
        let lines = w.build(
            &palette(),
            Viewport::width_only(120),
            Instant::now(),
            false,
            true,
            facts(2, 1),
        );
        let text = text_of(&lines);
        let version = text.find("dev ·").expect("版本行");
        let facts = text.find("2 skills").expect("事实行");
        assert!(version < facts, "事实行要在版本行之后：{text}");
        assert!(
            text.find("Esc 中断").expect("键位行") > facts,
            "事实行要在键位行之前（环境簇 → 帮助簇）：{text}"
        );
        // 槽位就贴在版本行下面一行（不是"某处出现过"）。
        assert_eq!(facts_slot(&lines), "2 skills · 1 rule");
    }

    #[test]
    fn facts_change_rebuilds_the_header() {
        let mut w = welcome();
        let settled = Instant::now() + Duration::from_millis(SWEEP_MS + 500);
        w.build(
            &palette(),
            Viewport::width_only(120),
            settled,
            false,
            true,
            facts(2, 1),
        );
        // 同一事实、已定格、姿态未到点：不重建。
        assert!(!w.needs_rebuild(Viewport::width_only(120), settled, false, true, facts(2, 1)));
        // 事实变了（sync 到达 / 换会话）：必须重建。
        assert!(
            w.needs_rebuild(Viewport::width_only(120), settled, false, true, facts(3, 1)),
            "事实变化要重建"
        );
        assert!(
            w.needs_rebuild(Viewport::width_only(120), settled, false, true, None),
            "老网关 / 清空事实也要重建"
        );
    }

    #[test]
    fn full_layout_has_the_gull_and_the_wordmark() {
        let mut w = welcome();
        let lines = w.build(
            &palette(),
            Viewport::width_only(120),
            Instant::now(),
            false,
            true,
            None,
        );
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
            w.needs_rebuild(Viewport::width_only(100), now, false, true, None);
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
        // `elide` 服务最窄档的 `wordmark_line`：按显示宽度（CJK 记 2 列）
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
        let keys = keys_line(Style::default(), Style::default());
        assert!(
            width_of(&keys) <= budget,
            "键位行 {} 列 > 80 列终端的预算 {budget} 列",
            width_of(&keys)
        );
    }

    // ── 名牌、分节线与大字档位 ────────────────────────────

    #[test]
    fn the_info_column_never_overflows_the_art_box() {
        // Full 档右列骑在海鸥的 13 行盒子里：列比盒子高时多出来的行会被静默丢掉
        // （不是截断，是整行消失）。2x 大字 5 行 + 名牌 6 行 = 11 是当前天花板 ——
        // 名牌加行、大字加档都会撞到它，所以钉在每一个宽度上。
        let palette = palette();
        for width in 0..200usize {
            let (column, _) =
                text_column(&palette, width, None, (34, 211, 238), false, "tip", None);
            assert!(
                column.len() <= ART_TERM_ROWS,
                "信息列 {width} 列时高 {} 行 > 盒子的 {ART_TERM_ROWS} 行",
                column.len()
            );
        }
    }

    #[test]
    fn the_nameplate_degrades_by_losing_the_tip_tail_never_by_wrapping() {
        // 名牌是一行一条的：折行会把下面的东西全推下去（版面契约）。80 列终端
        // （信息列 38 列）里，除 tip 之外的每一条**整行**都放得下；最宽的 tip
        // （池子上限 36 + `Tip  ` 5 = 41）丢尾巴 —— 它由 `/tips` 兜底。
        //
        // 这条线同时也是产品约束：键位 / 入口行文案变长、事实位数变多，都会在
        // 这里红（逼一次决定：缩文案，还是认了省略）。
        let budget = 80 - 2 - (ART_COLS + ART_GAP);
        let widest_tip = tips::TIPS
            .iter()
            .map(|tip| tip.text)
            .max_by_key(|text| text.width())
            .unwrap_or("");
        let facts = Some(SessionFacts {
            skills: 999,
            rules: 999,
        });
        let rows = info_rows(&palette(), widest_tip, facts);
        // **每一行**都过一遍（入口行也不例外：`TIPS_COMMAND` 改名、入口文案加列
        // 都会把这一行顶出预算）。唯一的例外是最宽的那条 tip —— 它由 `/tips`
        // 兜底，放不下只丢尾巴。
        for (index, row) in rows.iter().enumerate() {
            if index == TIP_ROW {
                continue;
            }
            assert!(
                width_of(row) <= budget,
                "80 列终端放得下整行：「{row}」{} 列 > {budget} 列",
                width_of(row)
            );
        }
        // tip：放不下就丢尾巴（一次性），永远不长出第二行。
        let fitted = fit_line(&rows[TIP_ROW], budget);
        assert_eq!(width_of(&fitted), budget, "tip 吃到满宽就停");
        assert!(
            fitted
                .spans
                .last()
                .is_some_and(|s| s.content.ends_with('…')),
            "丢的是尾巴，且有省略号：「{fitted}」"
        );
    }

    /// 信息列里的 wordmark 宽度：前 `rows` 行是字形（行尾透明格已裁，取最宽的
    /// 一行 = 档位宽度）。
    fn wordmark_width(column: &[Line<'static>], rows: usize) -> usize {
        column.iter().take(rows).map(width_of).max().unwrap_or(0)
    }

    #[test]
    fn the_nameplate_is_a_letterhead_not_a_table() {
        // 框退役了：没有 `┏┓┃`，只有一条分节线。线夹在版本 / 事实之后、键位之前
        // —— 上是"这是什么"，下是"你能做什么"。
        let mut w = welcome();
        let lines = w.build(
            &palette(),
            Viewport::width_only(120),
            Instant::now(),
            false,
            true,
            facts(2, 1),
        );
        let text = text_of(&lines);
        for gone in ['┏', '┓', '┃', '┗', '┛'] {
            assert!(!text.contains(gone), "名牌不该再有框（{gone}）：{text}");
        }
        let rule = lines
            .iter()
            .position(|line| line.spans.iter().any(|s| s.content.contains(RULE)))
            .expect("分节线得在");
        let version = lines
            .iter()
            .position(|line| line.spans.iter().any(|s| s.content.contains("wing · ")))
            .expect("版本行");
        let keys = lines
            .iter()
            .position(|line| line.spans.iter().any(|s| s.content.contains("Esc")))
            .expect("键位行");
        assert!(
            version < rule && rule < keys,
            "分节线夹在身份与操作之间：{text}"
        );
        // 分节线画满块宽（它就是这块文字的右缘）：46 列 = 2x 大字的宽度。
        let line = lines[rule]
            .spans
            .iter()
            .find(|span| span.content.chars().all(|c| c == RULE))
            .expect("分节线自成 span");
        assert_eq!(
            line.content.chars().count(),
            46,
            "2x 档的块宽 = 大字宽：{text}"
        );
    }

    #[test]
    fn commands_in_the_nameplate_are_accent() {
        // 命令是唯一可以照着敲的东西 —— 整套 UI 里都是 accent（palette 的约定：
        // accent = logo / 命令 / 链接）。名牌里若是 dim，它就成了看不见的字。
        let accent = Style::default().fg(Color::Red);
        let tip = tip_line(
            "输入 /skills 看已装技能",
            Style::default(),
            Style::default(),
            accent,
        );
        assert!(
            tip.spans
                .iter()
                .any(|s| s.content.as_ref() == "/skills" && s.style.fg == Some(Color::Red)),
            "tip 里的命令要染成 accent：{tip:?}"
        );
        let hints = hints_line(Style::default(), accent);
        assert_eq!(hints.spans[0].content.as_ref(), TIPS_COMMAND);
        assert_eq!(
            hints.spans[0].style.fg,
            Some(Color::Red),
            "入口行的命令名是 accent：{hints:?}"
        );
    }

    #[test]
    fn keys_split_into_keys_and_verbs() {
        // 一行里先看见敲什么，再读它做什么：按键比动作亮一档。
        let key = Style::default().fg(Color::Red);
        let dim = Style::default().fg(Color::Blue);
        let line = keys_line(key, dim);
        let keys: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Red))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            keys,
            ["Esc", "Ctrl+J", "Ctrl+C×2"],
            "三个键各自成 span：{line:?}"
        );
    }

    // ── 版面：居中与档位 ──────────────────────────────────

    #[test]
    fn the_splash_block_is_centred_horizontally() {
        // 空会话整块水平居中（竖直位置是 App 的版式，见 `splash_pad`）。
        let view = Viewport {
            width: 120,
            empty: true,
        };
        let mut w = welcome();
        let lines = w.build(&palette(), view, Instant::now(), false, true, None);
        let lead = |line: &Line<'static>| -> usize {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            text.chars().take_while(|c| *c == ' ').count()
        };
        let ink: Vec<&Line<'static>> = lines.iter().filter(|l| width_of(l) > 0).collect();
        let left = ink.iter().map(|l| lead(l)).min().expect("有内容");
        // 左边距 = 最靠左的墨迹列；右边距 = 视口宽减**整行**宽度（`width_of`
        // 已经把前导空格算进去了，不能再减一次 lead）。
        let right = (view.width as usize)
            .saturating_sub(ink.iter().map(|l| width_of(l)).max().expect("有内容"));
        assert!(left > 0, "宽终端上整块水平居中，实际左留白 {left}");
        assert!(
            left.abs_diff(right) <= 1,
            "整块居中：左 {left} 右 {right}（差 ≤ 1 列）"
        );
        // 块宽本身不受居中位移影响（App 用它定输入卡的宽度）。
        assert_eq!(w.block_width(), 86, "2x 大字 + 40 列海鸥");
    }

    #[test]
    fn splash_pad_puts_the_unit_above_the_geometric_centre() {
        // 「牌子 + 间隔 + 输入卡」当一个整体：上方空气 = 余量的 45%（少于一半），
        // 下面留给空气 / 弹层。
        for (screen, block) in [(30u16, 14u16), (50, 14), (24, 9), (120, 14)] {
            let pad = splash_pad(screen, block, 4);
            let slack = screen - 1 - block - 4;
            assert_eq!(pad, (slack as u32 * HERO_CENTER_PCT as u32 / 100) as u16);
            assert!(pad * 2 < slack, "光学中心在上：{screen}x{block} pad={pad}");
        }
        // 装不下：不溢出，也不凭空多一行。
        assert_eq!(splash_pad(10, 14, 4), 0);
        assert_eq!(splash_pad(20, 14, 4), 0);
    }

    #[test]
    fn a_conversation_pins_the_block_to_the_top() {
        // 有了消息就不是开屏了：header 退回"会话的开头"（一格留白、左对齐）。
        let view = Viewport {
            width: 120,
            empty: false,
        };
        let mut w = welcome();
        let lines = w.build(&palette(), view, Instant::now(), false, true, None);
        let first_ink = lines
            .iter()
            .position(|line| width_of(line) > 0)
            .expect("总有内容");
        assert_eq!(first_ink, 1, "顶对齐只有一格呼吸");
        // 左对齐：最左墨迹列比空会话时小一个居中位移。
        let left_ink = |lines: &[Line<'static>]| -> usize {
            lines
                .iter()
                .filter_map(|line| {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    text.chars()
                        .any(|c| c != ' ')
                        .then(|| text.chars().take_while(|c| *c == ' ').count())
                })
                .min()
                .unwrap_or(0)
        };
        let centred = {
            let mut w = welcome();
            w.build(
                &palette(),
                Viewport {
                    empty: true,
                    ..view
                },
                Instant::now(),
                false,
                true,
                None,
            )
        };
        assert!(
            left_ink(&lines) < left_ink(&centred),
            "有消息时退回左对齐（最左墨迹 {} < {}）",
            left_ink(&lines),
            left_ink(&centred)
        );
    }

    #[test]
    fn every_layout_input_rebuilds() {
        // 版面的两件都进 `needs_rebuild` 的键：宽度（档位）与"空会话没有"（居中
        // vs 左对齐）。只记宽度的话，第一条消息落地时 header 不会重排。
        let mut w = welcome();
        let settled = Instant::now() + Duration::from_millis(SWEEP_MS + 500);
        let view = Viewport {
            width: 120,
            empty: true,
        };
        w.build(&palette(), view, settled, false, true, None);
        assert!(
            !w.needs_rebuild(view, settled, false, true, None),
            "没变化不重建"
        );
        for changed in [
            Viewport { width: 121, ..view },
            Viewport {
                empty: false,
                ..view
            },
        ] {
            assert!(
                w.needs_rebuild(changed, settled, false, true, None),
                "版面变了要重建：{changed:?}"
            );
        }
    }

    #[test]
    fn the_width_ladder_is_the_recorded_one() {
        // 档位表（**内容宽** → 形态）。Full 档一出现，海鸥就吃掉 40 列，比它窄的
        // Compact 档反而更宽 —— 这条"变宽反而变小"是 FULL_MIN 既定优先级的代价
        // （80 列必须落在整块档、海鸥不能让出去），表就是它的契约。
        let palette = palette();
        let column_of = |content: u16| -> usize {
            match layout_for(content) {
                Layout::Full => (content as usize).saturating_sub(ART_COLS + ART_GAP),
                Layout::Compact => (content as usize).saturating_sub(1),
                Layout::Minimal => 0,
            }
        };
        let column = |content: u16| {
            text_column(
                &palette,
                column_of(content),
                None,
                (34, 211, 238),
                false,
                "tip",
                None,
            )
        };
        // (是不是 2x 大字, 分节线在不在)
        let shape = |content: u16| -> (bool, bool) {
            let (lines, _) = column(content);
            let width = column_of(content);
            let rows = wordmark::scale_for(width).rows();
            (
                wordmark_width(&lines, rows) == 46,
                lines.iter().any(|l| l.to_string().contains(RULE)),
            )
        };
        let height = |content: u16| column(content).0.len();

        // Compact 档（无海鸥）：信息列 = 内容宽 − 1。
        assert_eq!(shape(COMPACT_MIN - 1), (false, false), "最窄档没列");
        assert_eq!(shape(COMPACT_MIN), (false, true), "内容 40：1x + 分节线");
        assert_eq!(shape(45), (false, true), "内容 45：1x（信息列 44 < 46）");
        assert_eq!(shape(46), (false, true), "内容 46：1x（信息列 45）");
        assert_eq!(shape(47), (true, true), "内容 47：2x 起（信息列 46）");
        assert_eq!(shape(FULL_MIN - 1), (true, true), "内容 73：2x");

        // Full 档（海鸥 + 间隔 40 列）：信息列从 34 列起步。
        assert_eq!(shape(FULL_MIN), (false, true), "内容 74：1x + 分节线");
        assert_eq!(shape(FULL_MIN + 11), (false, true), "内容 85：1x");
        assert_eq!(shape(FULL_MIN + 12), (true, true), "内容 86：2x");

        // 高度：1x 3 + 6 行 = 9；2x 5 + 6 行 = 11。
        assert_eq!(height(40), 9);
        assert_eq!(height(46), 9);
        assert_eq!(height(120), 11);
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
            w.build(
                &palette(),
                Viewport::width_only(width),
                now,
                working,
                true,
                None,
            );
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
        w.build(
            &palette(),
            Viewport::width_only(100),
            mid,
            false,
            true,
            None,
        );
        let just_after = start + Duration::from_millis(SWEEP_MS + 1);
        assert!(
            w.needs_rebuild(Viewport::width_only(100), just_after, false, true, None),
            "扫光结束那一刻必须重建一次"
        );
        // 重建之后（已定格）就不该再因为扫光反复重建。
        w.build(
            &palette(),
            Viewport::width_only(100),
            just_after,
            false,
            true,
            None,
        );
        assert!(!w.needs_rebuild(Viewport::width_only(100), just_after, false, true, None));
    }

    #[test]
    fn offscreen_welcome_costs_nothing() {
        let mut w = welcome();
        let now = Instant::now();
        w.build(
            &palette(),
            Viewport::width_only(120),
            now,
            false,
            true,
            None,
        );
        assert!(
            !w.needs_rebuild(Viewport::width_only(120), now, false, false, None),
            "滚出视口就不重建"
        );
        assert!(
            w.next_frame(now, false, false).is_none(),
            "滚出视口就不走时钟"
        );
    }

    #[test]
    fn working_switch_rebuilds() {
        let mut w = welcome();
        let now = Instant::now();
        w.build(
            &palette(),
            Viewport::width_only(120),
            now,
            false,
            true,
            None,
        );
        assert!(
            w.needs_rebuild(Viewport::width_only(120), now, true, true, None),
            "干活/闲切换要换姿态，必须重建"
        );
    }

    #[test]
    fn settled_welcome_stops_rebuilding() {
        let mut w = welcome();
        let start = Instant::now();
        let settled = start + Duration::from_millis(SWEEP_MS + 500);
        w.build(
            &palette(),
            Viewport::width_only(120),
            settled,
            false,
            true,
            None,
        );
        // 定格后、姿态没到点：不重建。
        assert!(!w.needs_rebuild(Viewport::width_only(120), settled, false, true, None));
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
