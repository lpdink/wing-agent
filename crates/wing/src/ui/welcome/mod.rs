//! 欢迎屏 —— 空会话时 chat 顶部那一段（随会话滚走）。
//!
//! 拼成它的五样东西：
//!
//! * **海鸥**（[`art`] 的字母网格 + [`sprite`] 的半格渲染）：待机是站姿 chibi，
//!   agent 干活时切成飞行扇翅 —— "它在飞" = "它在干活"；
//! * **wordmark**（[`wordmark`]）：手绘 5 行像素大字 `WING`，开屏一道扫光扫过；
//!   按信息列宽度 1x / 2x 两档（半格像素只能整数倍放大，见 [`wordmark::Scale`]）；
//! * **名牌右列**：身份（版本 + commit + 会话事实，一行）/ **一条分节线** / 键位 /
//!   轮换 tip / 入口 —— 词标下与分节线上各留一格空气，线下面紧贴键位（表头）。
//!   没有框 —— 这是信笺（logo 下面是文字），不是一张表：框把两簇内容圈成了
//!   "一张有六个格子的表"，还让分隔用的空行看起来像"数据缺了一行"；
//! * **会话事实**：[`SessionFacts`]（`2 skills · 1 rule`）—— 这个会话加载了什么，
//!   随 SyncSession 到达，并进身份行（0/0 时整段消失，行数仍恒定，见 [`identity_cluster`]）；
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
use crate::ui::chip;
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
///
/// `pub(crate)`：首次运行向导的背板（`cmd::setup`）按同一份几何排版。
pub(crate) const ART_TERM_ROWS: usize = 13;

/// 站姿在盒子里默认下移的像素行 —— 头顶余量：抬升（呼吸 / 跳）把整帧上移，
/// 没有余量就会裁掉头冠的像素行，读起来像被压扁而不是跳起来。
pub(crate) const PERCHED_TOP_PAD: usize = 2;

/// 海鸥与右侧文字列之间的空列数。
pub(crate) const ART_GAP: usize = 3;

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
/// 名牌右列的**环境事实**（`2 skills · 1 rule`）：它随每次 SyncSession 到达，
/// 曾经是一条渲染在 transcript 顶部的 system 消息（`⦁ system` + 日志式英文），
/// 开屏时紧贴在像素名牌下面 —— 那里没有它的位置。
///
/// 现在它是**身份行的一段**（[`identity_line`]）：它和版本 / commit 同为「这个会话
/// 是什么」，所以同在一行。什么都没加载（0/0、老网关）时它整段消失，身份行退成
/// `wing · dev · <commit>` —— 行数不变，名牌高度因此仍不随 sync 变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFacts {
    /// 已加载的 skill 数（`AgentInfo.skills`）。
    pub skills: usize,
    /// 已加载的 rule 文件数（`AgentInfo.rules`）。
    pub rules: usize,
}

impl SessionFacts {
    /// 事实的各段文案（`["2 skills", "1 rule"]`）；什么都没加载时为空。
    ///
    /// 分段而不是拼好一整句：身份行在窄档要**按段**丢尾巴（先 `1 rule`、再
    /// `2 skills`，见 [`identity_line`]），切整句只会切出 `2 skil…`。
    ///
    /// 复数按英语习惯走（`1 rule`，不是 `1 rules` —— 旧 banner 的复数错就错在
    /// 数字在前、名词固定复数）。
    fn parts(self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.skills > 0 {
            parts.push(format!("{} skill{}", self.skills, plural(self.skills)));
        }
        if self.rules > 0 {
            parts.push(format!("{} rule{}", self.rules, plural(self.rules)));
        }
        parts
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
        let light = palette.appearance == crate::config::Appearance::Light;
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
                let art = gull_lines(pose, accent, palette.appearance);
                // 右列竖直居中：2x 大字（5 + 7 行 = 12）比盒子矮 1 行，1x（3 + 7 = 10）矮 3 行。
                // 余数行给**上方**（向上取整）：两档的大字首行因此留在**原位**（2x 盒内第 1 行、
                // 1x 第 2 行 —— 与名牌加空气之前取整到的地方相同），品牌锁合不动；向下取整会
                // 把整列上移一行。代价是名牌末行与海鸥的脚共用盒底那一行。
                let offset = ART_TERM_ROWS.saturating_sub(column.len()).div_ceil(2);
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
fn gull_lines(
    pose: Pose,
    accent: sprite::Rgb,
    appearance: crate::config::Appearance,
) -> Vec<Line<'static>> {
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
                appearance,
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
                appearance,
            )
        }
    }
}

/// 右侧文字列：wordmark（1x 3 行 / 2x 5 行）+ 名牌（身份 / 分节线 / 键位 / tip /
/// 入口）。
///
/// 返回（行, 块宽）：块宽 = `max(大字宽, 操作簇里最宽的一行)`，不超过可用列数。
/// 分节线按块宽画满 —— 它就是这块文字右缘的定义，没有框之后，线是唯一的收边。
///
/// 块宽**不含身份行**：合并后的身份行（40 列）比键位行（38 列）宽，若准它参与，
/// 80 列终端上的块会从 78 撑到 80（输入卡跟着变宽，那是几何改动）。它按段丢到
/// 放得下为止（[`identity_line`]）。
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

    // 色块的**行预算**只属于 2x 档：那一档的块宽被大字钉死在 46，行宽长了也不动几何。
    // 1x 档（信息列 34–45）的块宽由内容决定，加宽的键位行（40 列）在那里比 80 列
    // 终端的信息列（38）还宽 —— 那一档继续用纯文本（见 `docs/dev/welcome-mascot.md`）。
    let two_x = scale == wordmark::Scale::X2;
    let chip_room = two_x.then(|| scale.columns());
    let actions = action_rows(palette, tip, chip_room);
    let widest = actions.iter().map(width_of).max().unwrap_or(0);
    let block = widest.max(scale.columns()).min(width);

    let dim = Style::default().fg(palette.dim);
    let identity = identity_cluster(palette, facts, block);
    let mut body: Vec<Line<'static>> = Vec::with_capacity(identity.len() + actions.len() + 1);
    // 线上是「这是什么」，线下是「你能做什么」：分节线就是这两簇之间的那条缝，
    // 它上面一格空气、下面紧贴键位（表头）。
    body.extend(identity.iter().map(|row| fit_line(row, block)));
    body.push(rule_line(block, dim));
    body.extend(actions.iter().map(|row| fit_line(row, block)));
    out.extend(body);
    (out, block)
}

/// 分节线：`block` 列细横线。
fn rule_line(block: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(RULE.to_string().repeat(block), style))
}

/// 分节线上面那一簇：空气 / 身份 / 空气。
///
/// 纵向节奏是这里的全部内容：**空气在块内**。词标下面一格把身份从大字上脱开，
/// 身份下面一格把分节线从字里救出来。块外的空气不动 —— 省下的行来自合并的身份行
/// （版本 + 事实 → 一行），不来自屏幕。`block` 是这一行必须装进去的块宽（见
/// [`text_column`]：块宽不含身份行，所以这里得先知道它）。
fn identity_cluster(
    palette: &ThemePalette,
    facts: Option<SessionFacts>,
    block: usize,
) -> [Line<'static>; 3] {
    [
        Line::from(""),
        identity_line(palette, facts, block),
        Line::from(""),
    ]
}

/// 分节线下面那三行：键位 / tip / 入口。1x 档的块宽由它们定（键位行的纯文本形态
/// 是最宽的一行），所以它们的行预算就是可用列数。
///
/// `chip_room` 是键帽 / `Tip` 的**行预算**（`None` = 这一档不画色块）：它只会是
/// `Scale::X2.columns()`（2x 档的块宽 = 46），带色块的行在预算内先宽后窄地降级；
/// 一档都放不下就退回纯文本 —— 那是**样子**的退路，宽度再超就交给 `fit_line`。
fn action_rows(palette: &ThemePalette, tip: &str, chip_room: Option<usize>) -> [Line<'static>; 3] {
    [
        keys_line(palette, chip_room),
        tip_line(tip, palette, chip_room.is_some()),
        hints_line(palette),
    ]
}

/// 身份行：`wing · dev · <commit> · 2 skills · 1 rule`。
///
/// 三段都是**环境事实**、同一个 `dim` 寄存器（「这是哪个 wing、装了什么」），所以
/// 同行 —— 分成两行只剩下一件事：把名牌读成一堵墙。像素大字是品牌形，但终端里
/// 还得有**文本**形态的 brand（grep / 读屏 / 测试都读像素），它就在这一行的开头。
///
/// 窄档从**尾巴整段**丢（先 `1 rule`、再 `2 skills`）：丢一段仍是完整的身份，拦腰
/// 切一刀只会得到 `2 skil…`。两段都丢完还放不下（只有超长 commit 会）才交给
/// `fit_line` 的省略。
fn identity_line(
    palette: &ThemePalette,
    facts: Option<SessionFacts>,
    block: usize,
) -> Line<'static> {
    const SEP: &str = " · ";
    let dim = Style::default().fg(palette.dim);
    let brand = format!("wing · {}", version_label());
    let mut facts = facts.map(SessionFacts::parts).unwrap_or_default();
    let width = |facts: &[String]| {
        brand.width()
            + facts
                .iter()
                .map(|part| SEP.width() + part.width())
                .sum::<usize>()
    };
    while !facts.is_empty() && width(&facts) > block {
        facts.pop();
    }
    let mut text = brand;
    for part in &facts {
        text.push_str(SEP);
        text.push_str(part);
    }
    Line::from(Span::styled(text, dim))
}

/// 键位提示（一行）：**按键亮、动作暗** —— 先看见敲什么，再读它做什么。
///
/// 换行写 `Ctrl+J` 而不是 `Shift+Enter`：后者能不能到达取决于终端有没有
/// 实现 kitty 键盘协议（见 `tui::push_keyboard_enhancement`），在 header 上
/// 当既成事实写死就是在骗人 —— tips 池里有那条"看终端"的提示，这里只放
/// 任何终端都成立的键。
const KEYS: &[(&str, &str)] = &[("Esc", "中断"), ("Ctrl+J", "换行"), ("Ctrl+C×2", "退出")];

/// 键位行。`chip_room` 是行预算：`Some(room)` 画键帽（内边距逐档降级，见
/// [`keycap_line`]），`None` 是纯文本档。
///
/// 纯文本档比"收掉内边距的键帽"还宽（38 > 34），所以退回纯文本时仍可能超预算 ——
/// 那一步交给 [`fit_line`] 的省略，这里保证的只是"色块不会以半块出现"。
fn keys_line(palette: &ThemePalette, chip_room: Option<usize>) -> Line<'static> {
    if let Some(room) = chip_room {
        for pad in chip::Pad::LADDER {
            let line = keycap_line(palette, pad);
            if width_of(&line) <= room {
                return line;
            }
        }
    }
    plain_keys_line(palette)
}

/// 键帽档：` Esc  中断 Ctrl+J  换行 …`。
///
/// 键帽之间**不放 `·`**：块自己就是分隔，色块 + 符号是双重分隔。两处 ` · ` 换成
/// 一格空格省下四列，抵掉六个内边距列，整行 40 列（纯文本档 38）—— 离 46 还有
/// 六列余量，把 `Ctrl+J` 换成更宽的键名不至于立刻撑破块宽。
fn keycap_line(palette: &ThemePalette, pad: chip::Pad) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(KEYS.len() * 3 + 2);
    for (index, (keys, verb)) in KEYS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ", dim));
        }
        spans.push(chip::Chip::key(keys).span(palette, pad));
        spans.push(Span::styled(format!(" {verb}"), dim));
    }
    Line::from(spans)
}

/// 纯文本档：今天的样子（1x 档的全部宽度、以及键帽放不下时的退路）。
fn plain_keys_line(palette: &ThemePalette) -> Line<'static> {
    let key = Style::default().fg(palette.tool_result);
    let dim = Style::default().fg(palette.dim);
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
fn hints_line(palette: &ThemePalette) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let cmd = Style::default().fg(palette.accent);
    Line::from(vec![
        Span::styled(TIPS_COMMAND, cmd),
        Span::styled(" 全部提示", dim),
        Span::styled(" · ", dim),
        Span::styled("输入 ", dim),
        Span::styled("/", cmd),
        Span::styled(" 看全部命令", dim),
    ])
}

/// `Tip  <正文>` —— 标签是一块与键帽同档的色块（`banded` 时），正文 `tool_result`，
/// 正文里的 `/命令` verbatim 成 accent。
///
/// 提示是**读的**、不是敲的：所以它走安静档（`surface` 底 + `tool_result` 墨，和键帽
/// 同一套归 `ui::chip` 管），而不是实心品牌色。名牌里 `accent` 只属于能照着敲的命令
/// （`/tips`、`/`）—— 形状 = 可敲，一次启动随机换的一条提示拿不走那个形状。
///
/// 色块后面多一格气口把块与正文分开。不再按宽度预截断：调用方按块宽统一省略
/// （[`fit_line`]），截断只发生一次（两处各截一次就会多丢几个字）。
fn tip_line(tip: &str, palette: &ThemePalette, banded: bool) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.tool_result);
    let cmd = Style::default().fg(palette.accent);
    let mut spans = if banded {
        vec![
            chip::Chip::key("Tip").span(palette, chip::Pad::One),
            Span::styled(" ", dim),
        ]
    } else {
        vec![Span::styled("Tip  ", dim)]
    };
    spans.extend(with_commands(tip, text, cmd));
    Line::from(spans)
}

/// 把 `/xxx` 形态的词染成 accent（palette 的约定：accent = logo / 命令 / 链接；输入卡的
/// `❯` 与粘贴 / 图片 chip 也用它，品牌图形与 spinner 另算 —— 见 `docs/dev/tui-input.md`）。
/// 名牌里若是 dim，命令就成了看不见的字。
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

/// 名牌**身份行**的文本（从 `wing · ` 起到行尾）—— 测试助手，`ui::welcome` 与
/// `app::tests::welcome` 两侧共用。
///
/// 断言走这一行而不是整块文本：tip 是按时间种子抽的（`tips::seed_now()`），池子里有
/// `/skills 看已装技能…` —— 任何对整帧做子串扫描的断言都会随进程抖动。这一行是确定性
/// 的：它只由版本与会话事实决定。
#[cfg(test)]
pub(crate) fn identity_text(lines: &[Line<'_>]) -> String {
    let row = lines
        .iter()
        .find(|line| line.spans.iter().any(|s| s.content.contains("wing · ")))
        .expect("身份行在名牌右列");
    let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
    // 身份行左边还有海鸥的字形（半格 `▀` 一个 3 字节），从品牌起切到行尾。
    text[text.find("wing · ").expect("身份行带 brand")..]
        .trim_end()
        .to_string()
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

/// 身份行的中段：`dev · <commit>` —— workspace 的占位版本 0.0.0 是开发构建
/// （发版时由 `scripts/sync_version.py` 换成真版本号），展示成 `dev`；
/// `v0.0.0` 只会让人以为装错了包。
///
/// `pub(crate)`：整屏指纹（`app::tests::splash_frame`）要按同一份标签拼期望值 ——
/// 它的宽度跟着构建走，写死会让发版构建上的断言红。
pub(crate) fn version_label() -> String {
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

    /// 一行的样式指纹：(前景, 底色, 内容) 逐 span。断言"两行是同一个东西"用它
    /// —— 只比文字会漏掉底色，只比长度会漏掉一切。
    fn fingerprint(line: &Line<'_>) -> Vec<(Option<Color>, Option<Color>, String)> {
        line.spans
            .iter()
            .map(|span| (span.style.fg, span.style.bg, span.content.to_string()))
            .collect()
    }

    /// header 的纯文本（断言文案用）。
    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn facts(skills: usize, rules: usize) -> Option<SessionFacts> {
        Some(SessionFacts { skills, rules })
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
    fn facts_render_their_counts_with_correct_plurals() {
        // 复数按英语习惯走（`1 rule`，不是 `1 rules`）。钉在分段上而不是整行上：
        // 一行能装下几段由块宽与版本号宽度决定（见下面那条测试）。
        let parts =
            |facts: Option<SessionFacts>| facts.map(SessionFacts::parts).unwrap_or_default();
        assert_eq!(parts(facts(2, 1)), ["2 skills", "1 rule"]);
        assert_eq!(parts(facts(1, 1)), ["1 skill", "1 rule"]);
        // 只有一类加载物时另一类不出现。
        assert_eq!(parts(facts(0, 3)), ["3 rules"]);
        assert_eq!(parts(facts(3, 1)), ["3 skills", "1 rule"]);
        assert!(parts(None).is_empty(), "老网关：没有段");
        assert!(parts(facts(0, 0)).is_empty(), "0/0：没有段");
    }

    #[test]
    fn the_identity_row_is_the_brand_plus_whole_facts_segments() {
        let mut w = welcome();
        let row = |w: &mut Welcome, facts| {
            identity_text(&w.build(
                &palette(),
                Viewport::width_only(120),
                Instant::now(),
                false,
                true,
                facts,
            ))
        };
        let brand = format!("wing · {}", version_label());
        // 2x 档的块宽被大字钉死在 46：放得下的段都在，且用 ` · ` 连着。
        for facts in [facts(2, 1), facts(1, 1), facts(0, 3), facts(3, 1)] {
            let got = row(&mut w, facts);
            let full = format!("{brand} · {}", facts.unwrap().parts().join(" · "));
            assert!(full.starts_with(&got), "整段拼接：{got}");
            assert!(got.starts_with(&brand), "以品牌开头：{got}");
            assert!(!got.contains('…'), "不许切词：{got}");
        }
        // 没有事实：身份行就是品牌 + 版本，尾巴上不留一个孤零零的 ` · `。
        assert_eq!(row(&mut w, None), brand, "无事实时不留分隔符");
        assert_eq!(row(&mut w, facts(0, 0)), brand, "0/0 时不留分隔符");
    }

    #[test]
    fn the_identity_row_never_widens_the_block() {
        // 合并后的身份行（40 列）比键位行（38 列）宽 —— 它若参与块宽，80 列终端上的
        // 块会从 78 撑到 80，输入卡跟着变宽（那是几何改动）。所以块宽只由**操作簇**
        // 与大字决定，身份行按段丢到放得下为止。
        let palette = palette();
        let brand = format!("wing · {}", version_label());
        let full = format!("{brand} · 2 skills · 1 rule");
        for (column, block) in [(34usize, 34usize), (39, 38), (40, 38), (45, 38), (60, 46)] {
            let (rows, got) = text_column(
                &palette,
                column,
                None,
                (34, 211, 238),
                false,
                "tip",
                facts(2, 1),
            );
            assert_eq!(got, block, "信息列 {column} 的块宽");
            let identity = identity_text(&rows);
            // 品牌段自己就超块宽时（超长版本号 / commit），退路是 `fit_line` 的省略 ——
            // 那条路上不保证整段，只保证不溢出（下面那条）。
            if brand.width() <= block {
                assert!(
                    full.starts_with(&identity),
                    "丢的是整段尾巴（信息列 {column}）：{identity}"
                );
                assert!(!identity.contains('…'), "不许切词：{identity}");
            }
            let row = rows
                .iter()
                .find(|row| row.to_string().contains("wing · "))
                .expect("身份行");
            assert!(
                width_of(row) <= block,
                "身份行 {} 列 > 块宽 {block}",
                width_of(row)
            );
        }
        // 窄档（上面 39 列那一档）丢到只剩品牌也成立 —— 留下的永远是整句的前缀，
        // 「整段丢、不切词」已经由循环里的两条断言钉住。
        // 宽到装得下就整句都在 —— 丢尾巴不是默认行为（品牌段的宽度跟版本号走，
        // 所以判据用整句自己的宽度，不写死品牌宽度）。
        let (rows, block) = text_column(
            &palette,
            78,
            None,
            (34, 211, 238),
            false,
            "tip",
            facts(2, 1),
        );
        if full.width() <= block {
            assert_eq!(identity_text(&rows), full, "放得下就得整句");
        }
    }

    #[test]
    fn nothing_loaded_keeps_the_header_height() {
        // 事实缺席（0/0 / 老网关）时身份行退成 `wing · dev · <commit>`：**行数不变**，
        // header 的高度因此在任何宽度档里都不随 sync 变化（Full 档由海鸥的画盒定，
        // Compact 档由文字列定），sync 到达时下方内容不会跳一行。
        let mut w = welcome();
        let now = Instant::now();
        for width in [120u16, 96, 80, 70, 60, 40] {
            let without = w.build(
                &palette(),
                Viewport::width_only(width),
                now,
                false,
                true,
                None,
            );
            let with = w.build(
                &palette(),
                Viewport::width_only(width),
                now,
                false,
                true,
                facts(2, 1),
            );
            assert_eq!(
                without.len(),
                with.len(),
                "width={width}：事实改变了 header 高度"
            );
            assert_eq!(
                identity_text(&without),
                format!("wing · {}", version_label()),
                "width={width}"
            );
        }
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
            "身份行得在"
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
        let keys = plain_keys_line(&palette());
        assert!(
            width_of(&keys) <= budget,
            "键位行 {} 列 > 80 列终端的预算 {budget} 列",
            width_of(&keys)
        );
        // 80 列是 1x 档：键位行走的就是这条纯文本路径（键帽只在 2x 出现）。
        assert_eq!(
            fingerprint(&keys_line(&palette(), None)),
            fingerprint(&keys),
            "1x 档的键位行就是纯文本档"
        );
    }

    // ── 名牌、分节线与大字档位 ────────────────────────────

    #[test]
    fn the_info_column_never_overflows_the_art_box() {
        // Full 档右列骑在海鸥的 13 行盒子里：列比盒子高时多出来的行会被静默丢掉
        // （不是截断，是整行消失）。2x 大字 5 行 + 名牌 7 行 = 12 是当前天花板 ——
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
        // （信息列 38 列）里，键位行与入口行整行放得下；最宽的 tip（池子上限 36 +
        // `Tip  ` 5 = 41）丢尾巴 —— 它由 `/tips` 兜底。
        //
        // 这条线同时也是产品约束：键位 / 入口行文案变长会在这里红（逼一次决定：
        // 缩文案，还是认了省略）。身份行不在这条线上 —— 它按**段**丢，见
        // `the_identity_row_never_widens_the_block`。
        let budget = 80 - 2 - (ART_COLS + ART_GAP);
        let [keys, tip, hints] = action_rows(&palette(), widest_tip(), None);
        // 除 tip 之外每条都过一遍（入口行也不例外：`TIPS_COMMAND` 改名、入口文案加列
        // 都会把这一行顶出预算）。
        for row in [keys, hints] {
            assert!(
                width_of(&row) <= budget,
                "80 列终端放得下整行：「{row}」{} 列 > {budget} 列",
                width_of(&row)
            );
        }
        // tip：放不下就丢尾巴（一次性），永远不长出第二行。
        let fitted = fit_line(&tip, budget);
        assert_eq!(width_of(&fitted), budget, "tip 吃到满宽就停");
        assert!(
            fitted
                .spans
                .last()
                .is_some_and(|s| s.content.ends_with('…')),
            "丢的是尾巴，且有省略号：「{fitted}」"
        );
    }

    #[test]
    fn the_two_x_nameplate_needs_no_elision() {
        // 2x 档反过来：块宽被大字钉死在 `Scale::X2.columns()`（46），键帽行（40 列）与
        // 最宽的 tip（`Tip` 色块 5 + 气口 1 + 池子上限 36 = 42）整行放得下 —— 操作簇
        // 那几条不会省略。身份行是另一回事：它由**品牌段 + 整段事实**拼出来，品牌段的
        // 宽度跟着版本号走（`dev` 20 列 / 发版更长），所以「整句都在」只在放得下时成立。
        let block = wordmark::Scale::X2.columns();
        let facts = Some(SessionFacts {
            skills: 999,
            rules: 999,
        });
        let [_, identity, _] = identity_cluster(&palette(), facts, block);
        let identity = identity.to_string();
        // 身份行是**整段**丢的：留着的必须是整句的前缀，且不许出现省略号。
        let full = format!("wing · {} · 999 skills · 999 rules", version_label());
        assert!(
            full.starts_with(&identity),
            "身份行是整句的整段前缀：「{identity}」"
        );
        assert!(!identity.contains('…'), "不许切词：「{identity}」");
        // 品牌段的宽度跟着版本号走（`dev` / `v0.x.y`；发布构建更长），所以「整句都在」
        // 只在整句真的放得下时成立 —— 判据用整句自己的宽度，不写死品牌宽度。
        if full.width() <= block {
            assert_eq!(identity, full, "放得下就得整句都在");
        }
        let [keys, tip, hints] = action_rows(&palette(), widest_tip(), Some(block));
        // 身份行已被 `identity_cluster` 按段排进块宽（上面三条断言），这里剩下的是
        // 操作簇：`fit_line` 只在超宽时动手，原样返回 ⟺ 这行整行放得下。
        for row in [keys, tip, hints] {
            assert_eq!(
                width_of(&fit_line(&row, block)),
                width_of(&row),
                "不该被省略：「{row}」{} 列",
                width_of(&row)
            );
        }
    }

    /// 池子里最宽的那条 tip（`MAX_TIP_WIDTH` 是它的上界）。
    fn widest_tip() -> &'static str {
        tips::TIPS
            .iter()
            .map(|tip| tip.text)
            .max_by_key(|text| text.width())
            .unwrap_or("")
    }

    /// 信息列里的 wordmark 宽度：前 `rows` 行是字形（行尾透明格已裁，取最宽的
    /// 一行 = 档位宽度）。
    fn wordmark_width(column: &[Line<'static>], rows: usize) -> usize {
        column.iter().take(rows).map(width_of).max().unwrap_or(0)
    }

    #[test]
    fn the_nameplate_is_a_letterhead_not_a_table() {
        // 框退役了：没有 `┏┓┃`，只有一条分节线。线夹在身份行之后、键位之前
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
            .expect("身份行");
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
        // `/命令` 在整套 UI 里都是 accent（palette 的约定：accent = logo / 命令 / 链接）——
        // 名牌里若是 dim，它就成了看不见的字。
        let palette = palette();
        for chip in [false, true] {
            let tip = tip_line("输入 /skills 看已装技能", &palette, chip);
            assert!(
                tip.spans
                    .iter()
                    .any(|s| s.content.as_ref() == "/skills" && s.style.fg == Some(palette.accent)),
                "tip 里的命令要染成 accent（chip={chip}）：{tip:?}"
            );
        }
        let hints = hints_line(&palette);
        assert_eq!(hints.spans[0].content.as_ref(), TIPS_COMMAND);
        assert_eq!(
            hints.spans[0].style.fg,
            Some(palette.accent),
            "入口行的命令名是 accent：{hints:?}"
        );
    }

    #[test]
    fn keys_split_into_keys_and_verbs() {
        // 一行里先看见敲什么，再读它做什么：按键比动作亮一档。两档都是这个寄存器，
        // 只是 2x 档的键多了一圈内边距与一块底（`Some(46)`）。
        let palette = palette();
        for (chip_room, keys) in [
            (None, ["Esc", "Ctrl+J", "Ctrl+C×2"]),
            (
                Some(wordmark::Scale::X2.columns()),
                [" Esc ", " Ctrl+J ", " Ctrl+C×2 "],
            ),
        ] {
            let line = keys_line(&palette, chip_room);
            let found: Vec<&str> = line
                .spans
                .iter()
                .filter(|s| s.style.fg == Some(palette.tool_result))
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(
                found, keys,
                "三个键各自成 span（chip_room={chip_room:?}）：{line:?}"
            );
            let verbs: Vec<&str> = line
                .spans
                .iter()
                .filter(|s| s.style.fg == Some(palette.dim))
                .map(|s| s.content.trim())
                .filter(|s| !s.is_empty() && *s != "·")
                .collect();
            assert_eq!(verbs, ["中断", "换行", "退出"], "动作在 dim 寄存器");
        }
    }

    #[test]
    fn the_key_row_ladder_drops_padding_then_the_chips() {
        // 三档顺序：常规内边距（40）→ 收掉内边距（34）→ 纯文本（38）。2x 档碰不到
        // 前两档（40 ≤ 46），但降级这件事本身得被钉住 —— 下一个消费者是变量
        // 宽度的状态栏。纯文本比"收掉内边距"还宽，所以第三档是**样子**的退路
        // （回到今天的排版），宽度兜底交给 `fit_line`。
        let palette = palette();
        let roomy = keys_line(&palette, Some(wordmark::Scale::X2.columns()));
        assert!(
            roomy
                .spans
                .iter()
                .any(|span| span.content.as_ref() == " Esc " && span.style.bg.is_some()),
            "常规档：键帽带内边距：{roomy:?}"
        );

        let tight = keys_line(&palette, Some(38));
        assert_eq!(width_of(&tight), 34, "收掉内边距：40 − 6");
        assert!(
            tight
                .spans
                .iter()
                .any(|span| span.content.as_ref() == "Esc" && span.style.bg.is_some()),
            "第二档只掉内边距，不掉字：{tight:?}"
        );

        let plain = keys_line(&palette, Some(33));
        assert!(
            plain.spans.iter().all(|span| span.style.bg.is_none()),
            "一档都放不下 → 纯文本（省略交给 fit_line）：{plain:?}"
        );
        assert_eq!(fingerprint(&plain), fingerprint(&plain_keys_line(&palette)));
    }

    #[test]
    fn keycaps_only_exist_in_the_two_x_tier() {
        // 两档哲学：1x 档（80 列终端的信息列 38）保持纯文本，2x 档（46 列）才有
        // 带底色的键帽。判据是字号档，不是"还塞得下就塞"。
        let palette = palette();
        let row = |column: usize, needle: &str| -> Line<'static> {
            let (lines, _) =
                text_column(&palette, column, None, (34, 211, 238), false, "tip", None);
            lines
                .into_iter()
                .find(|line| line.to_string().contains(needle))
                .unwrap_or_else(|| panic!("{needle} 那一行在（信息列 {column}）"))
        };

        let plain_keys = row(38, "Ctrl+J");
        assert!(
            plain_keys.spans.iter().all(|span| span.style.bg.is_none()),
            "1x 档的键位行是纯文本：{plain_keys:?}"
        );
        // 纯文本档就是今天的样子（`Esc 中断 · Ctrl+J 换行 · Ctrl+C×2 退出` = 38 列，
        // 正好用满 80 列终端的信息列 —— 加宽后的 40 列在那里放不下，这是两档的分界）。
        assert_eq!(width_of(&plain_keys), 38, "纯文本档的键位行宽度不变");

        let two_x = wordmark::Scale::X2.columns();
        let capped = row(two_x, "Ctrl+J");
        assert_eq!(
            capped
                .spans
                .iter()
                .filter(|span| span.style.bg.is_some())
                .count(),
            3,
            "2x 档三个键各是一块：{capped:?}"
        );
        // 键帽行 = 纯文本 38 + 六个内边距列 − 省下的四个分隔列 = 40 列。
        assert_eq!(width_of(&capped), 40, "键帽行的宽度");
        assert!(
            width_of(&capped) <= two_x,
            "键帽行装进块宽（{two_x} 列）：{}",
            width_of(&capped)
        );

        // `Tip` 的标签与键帽**同一套**（`Chip::key` 的安静档）：形状 = 「一块」。
        // 1x 档没有色块，标签退回纯文本。
        let plain_tip = row(38, "Tip");
        assert!(plain_tip.spans.iter().all(|span| span.style.bg.is_none()));
        let labelled = row(two_x, "Tip");
        assert_eq!(
            fingerprint(&Line::from(vec![labelled.spans[0].clone()])),
            fingerprint(&Line::from(vec![
                chip::Chip::key("Tip").span(&palette, chip::Pad::One)
            ])),
            "`Tip` 与键帽是同一块：{labelled:?}"
        );
    }

    #[test]
    fn the_tip_label_takes_the_same_room_in_both_tiers() {
        // 1x 档的标签是纯文本 `Tip  `、2x 档是 ` Tip ` 色块 + 一格气口：标签本身占同样宽
        // 的格子，换档时 tip 正文的起点只差"色块自带的那格气口"。标签变宽变窄会让同一条
        // tip 在两档断在不同位置（读起来像文案在两档不一样）。
        let palette = palette();
        let plain = tip_line("正文", &palette, false);
        let banded = tip_line("正文", &palette, true);
        assert_eq!(plain.spans[0].content.as_ref(), "Tip  ");
        assert_eq!(banded.spans[0].content.as_ref(), " Tip ");
        assert_eq!(
            width_of_spans(&plain.spans[..1]),
            width_of_spans(&banded.spans[..1]),
            "标签在两档占同样宽的格子"
        );
    }

    #[test]
    fn the_nameplate_spends_accent_only_on_commands() {
        // 名牌里的**形状**语法：能照着敲的东西 = `accent` 墨（`/tips`、`/`），没有一块
        // `accent` 的底。一条每次启动随机换的提示不该拿走整块最响的形状。
        let palette = palette();
        for column in [38usize, 46, 60] {
            let (lines, _) = text_column(
                &palette,
                column,
                None,
                (34, 211, 238),
                false,
                "输入 /skills 看已装技能",
                facts(2, 1),
            );
            let glyphs = wordmark::scale_for(column).rows();
            let nameplate: Vec<&Line<'static>> = lines[glyphs.min(lines.len())..].iter().collect();
            assert!(!nameplate.is_empty(), "信息列 {column}：名牌得有行");
            for line in &nameplate {
                for span in &line.spans {
                    assert_ne!(
                        span.style.bg,
                        Some(palette.accent),
                        "名牌里没有 accent 底色（信息列 {column}）：{line:?}"
                    );
                    if span.style.fg == Some(palette.accent) {
                        assert!(
                            span.content.starts_with('/'),
                            "accent 只给命令（信息列 {column}）：「{}」",
                            span.content
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_nameplate_has_its_air_inside() {
        // 名牌的纵向节奏：词标下面一格空把身份从大字上脱开，身份下面一格空把分节线
        // 从字里救出来，线下面紧贴键位。块高不变 —— 空气来自合并的身份行，不来自屏幕。
        let palette = palette();
        for (column, scale, height) in [
            (60usize, wordmark::Scale::X2, 12usize),
            (38, wordmark::Scale::X1, 10),
        ] {
            let (lines, _) = text_column(
                &palette,
                column,
                None,
                (34, 211, 238),
                false,
                "tip",
                facts(2, 1),
            );
            assert_eq!(lines.len(), height, "信息列 {column}：名牌的行数");
            assert_eq!(
                wordmark_width(&lines, scale.rows()),
                scale.columns(),
                "信息列 {column}：大字档"
            );
            let glyphs = scale.rows();
            assert_eq!(width_of(&lines[glyphs]), 0, "词标下面那一格是空的");
            assert!(
                lines[glyphs + 1].to_string().contains("wing · "),
                "身份行紧跟那格空气：{:?}",
                lines[glyphs + 1]
            );
            assert_eq!(width_of(&lines[glyphs + 2]), 0, "身份与分节线之间一格空");
            let rule = lines[glyphs + 3].to_string();
            assert!(
                !rule.is_empty() && rule.chars().all(|c| c == RULE),
                "分节线在身份下面那格空气之后：{rule:?}"
            );
            assert!(
                lines[glyphs + 4].to_string().contains("Esc"),
                "键位紧贴分节线下面：{:?}",
                lines[glyphs + 4]
            );
        }
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

        // 高度：1x 3 + 7 行 = 10；2x 5 + 7 行 = 12（名牌 = 空气 / 身份 / 空气 /
        // 分节线 / 键位 / tip / 入口）。
        assert_eq!(height(40), 10);
        assert_eq!(height(46), 10);
        assert_eq!(height(120), 12);
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
                        ch == '.'
                            || sprite::brand(ch, accent, crate::config::Appearance::Dark).is_some(),
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
                accent,
                crate::config::Appearance::Dark
            )
            .len(),
            ART_TERM_ROWS
        );
        assert_eq!(
            gull_lines(
                Pose::Flying { frame: 0 },
                accent,
                crate::config::Appearance::Dark
            )
            .len(),
            ART_TERM_ROWS
        );
        assert_eq!(
            gull_lines(
                Pose::Perched {
                    frame: PerchedFrame::Idle,
                    lift: 2
                },
                accent,
                crate::config::Appearance::Dark
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
            let lines = gull_lines(
                Pose::Perched { frame, lift: 0 },
                accent,
                crate::config::Appearance::Dark,
            );
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
            let lines = gull_lines(
                Pose::Flying { frame },
                accent,
                crate::config::Appearance::Dark,
            );
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
                crate::config::Appearance::Dark,
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
        assert_eq!(
            sprite::brand('A', (1, 2, 3), crate::config::Appearance::Dark),
            Some((1, 2, 3))
        );
        assert_eq!(
            sprite::brand('W', (1, 2, 3), crate::config::Appearance::Dark),
            sprite::brand('W', (9, 9, 9), crate::config::Appearance::Dark),
            "品牌色不该跟 accent 变"
        );
    }
}
