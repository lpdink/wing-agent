//! 整屏取样器 —— 创作期视觉工具的库侧（命令行在 `examples/splash_preview.rs`）。
//!
//! 一帧「状态栏 + 开屏牌子 + 输入卡」只有一条真路径：`App::draw` —— 它读 App 的
//! 私有状态（欢迎屏时钟、turn、版式探针缓存），把三块画进同一块 `Buffer`。
//! example 是独立 crate、只见公共 API：够不着 `draw`，也够不着 `handle_event`。
//! 把两者变 `pub` 等于把「一帧怎么画」和「事件入口」一起变成公共契约；取样器留在
//! `app` 子树里，`App` 的公共面一动不动。
//!
//! **像素不是仿画**：`sample` = 造一个真 App → 用真事件把 fixture 状态灌进去 →
//! `App::draw` 画进 `TestBackend` → 读回那块 `Buffer`。预览里每个格子都是 TUI 会
//! 画出的格子，「预览好看、真机不一样」因此不可能发生。
//!
//! 时间只在一处被拨动：欢迎屏的时钟（扫光相位 / 动作帧）。tip 用固定种子 ——
//! 同一份代码两次运行逐字节相同，视觉改动才 diff 得出来。唯一的例外是 `working`
//! 帧里活动栏的秒数（它读墙钟，跨整数秒边界会变一位）。
//!
//! **取样面**：开屏整块。弹层 / 设置面板 / toast / 图片不在 fixture 里，画不出来。

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::style::Style;
use wing_api_client::models::SessionUsage;

use super::App;
use crate::config::AppConfig;
use crate::config::Appearance;
use crate::config::ColorPreset;
use crate::config::ThemePalette;
use crate::protocol::AgentInfo;
use crate::protocol::EventMeta;
use crate::protocol::SessionStatus;
use crate::protocol::WingEvent;
use crate::ui::html;
use crate::ui::welcome::SWEEP_MS;
use crate::ui::welcome::SessionFacts;
use crate::ui::welcome::Welcome;

/// 取样用的会话 id（与 `ui::status_bar` / app 测试同一份样例 id：24 字符，
/// 宽终端画全、窄终端退化尾 8 位 —— 两种形态都能在预览里看到）。
const SESSION_ID: &str = "20261005-213012-ab12cd34";

/// 名牌 tip 的抽签种子：固定值（`tips::seed_now()` 会让两次运行不同）。
const TIP_SEED: u64 = 7;

/// 定格帧的取样时刻：扫光刚结束 1ms。
///
/// 不能用「扫光 + 1s」这类更省的数：动作规划器的第一次眨眼在
/// `BLINK_GAP_MS = 2600`，落在那一帧上会画出闭眼的海鸥 —— 稳态的默认帧应当是
/// 站定的那一帧（扫光已过，动作还没到点）。
const SETTLED_AGE_MS: u64 = SWEEP_MS + 1;

/// 状态栏的模型段与输入卡 meta rail 的样例（与 `theme_preview` 同一份会话）。
const MODEL_NAME: &str = "deepseek-v4-flash-0731";
const MODEL_ID: &str = "deepseek-flash";
const MODEL_LABEL: &str = "DeepSeek-Flash";
const PROVIDER: &str = "dashscope";
/// 固定工作目录：不读 `$HOME`（路径不该是「这台机器的事实」），预览要能逐字节 diff。
const WORKDIR: &str = "~/ws/wing";

/// 一次取样的输入。
#[derive(Debug, Clone)]
pub struct Spec {
    /// 终端列数（整屏，不是内容宽）。
    pub width: u16,
    /// 终端行数 —— 开屏的竖直位置由它决定（`ui::welcome::splash_pad` 的光学中心：
    /// 只给宽度是看不出整块停在哪里的）。
    pub height: u16,
    /// 强制明暗档；`None` = 沿用配置（`colors.preset`；`auto` 且没有探测结果时是 dark）。
    pub preset: Option<ColorPreset>,
    /// 输入卡里的草稿（`None` = 空草稿，显示占位文案）。
    pub draft: Option<String>,
    /// agent 在干活：活动栏 + 飞行姿态的海鸥。
    pub working: bool,
    /// 名牌右列的会话事实（skills / rules 计数）；`0,0` = 这一行留空
    /// （什么都没加载，槽位仍在 —— 名牌高度因此不随它变）。
    pub facts: SessionFacts,
    /// 扫光相位 `0.0..=1.0`；`None` = 已定格（默认，稳态那一帧）。
    pub phase: Option<f32>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            width: 100,
            height: 30,
            preset: None,
            draft: None,
            working: false,
            // 与 `welcome_preview --facts` 的默认同一份样例：名牌右列不是半空的。
            facts: SessionFacts {
                skills: 2,
                rules: 1,
            },
            phase: None,
        }
    }
}

/// 一帧整屏：真帧的格子 + 画它用的 palette。
pub struct Frame {
    buffer: Buffer,
    palette: ThemePalette,
}

impl Frame {
    /// 画这一帧用的 palette（明暗档 / 页面底色由它决定）。
    pub fn palette(&self) -> &ThemePalette {
        &self.palette
    }

    /// 帧里出现过的颜色（前景与背景，去重；默认色不算）：颜色断言用它，
    /// 不必去解析某种输出记号。
    pub fn colors(&self) -> Vec<Color> {
        let mut out: Vec<Color> = Vec::new();
        for y in self.buffer.area.y..self.buffer.area.bottom() {
            for (_, cell) in self.glyphs(y) {
                for color in [cell.fg, cell.bg] {
                    if color != Color::Reset && !out.contains(&color) {
                        out.push(color);
                    }
                }
            }
        }
        out
    }

    /// 每行按**格子**取原文，行尾的默认样式空白裁掉。
    ///
    /// 一个格子一条：宽字形（CJK）的续格在 `Buffer` 里是一个空格，所以字符串下标
    /// 就是**屏幕列号**（几何断言用它）。要"看起来像终端"的文本，用 [`Frame::plain`]
    /// / [`Frame::ansi`]：那里续格被收掉。
    ///
    /// 裁剪只发生在「空格 + 前景/背景都是默认色 + 没有修饰」的格子上：有背景色的
    /// 空格是**画的**（铺底），留着它才看得见卡的边缘。
    pub fn rows(&self) -> Vec<String> {
        let area = self.buffer.area;
        (area.y..area.bottom())
            .map(|y| {
                let end = self.ink_end(y);
                (area.x..end)
                    .map(|x| self.buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    /// `--plain` 形态：无颜色的纯文本，一行一条。
    ///
    /// 与 [`Frame::rows`] 的两点差别就是"文本"与"格子"的差别：宽字形的续格收掉
    /// （CJK 不会每字之间夹一个空格），带背景色的 `▀` / `▄` 还原成 `█` —— 半格
    /// 像素的「实心」来自前景画上半、背景画下半，没有颜色时那一格会瘦一半（像素
    /// 大字与海鸥会整片看着缺一半）。只替换半格字形，不碰空格 —— 输入卡与状态栏
    /// 的底色不会被涂成实心块。
    pub fn plain(&self) -> String {
        let area = self.buffer.area;
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for (_, cell) in self.glyphs(y) {
                out.push_str(plain_symbol(cell));
            }
            out.push('\n');
        }
        out
    }

    /// 真彩 ANSI（一行一条）：直接贴到终端里就是真机上那一帧。
    ///
    /// 每段都由 [`crate::render::ansi::paint`] 自闭合交出 —— 默认样式的段是裸文本，
    /// 但它前面那段已经复位，空白格不会继承墨迹的颜色。行尾不再补一次复位（每一段
    /// 已经关干净了）。
    pub fn ansi(&self) -> String {
        let mut out = String::new();
        for y in self.buffer.area.y..self.buffer.area.bottom() {
            for (style, text) in self.runs(y) {
                out.push_str(&crate::render::ansi::paint(style, &text));
            }
            out.push('\n');
        }
        out
    }

    /// 独立 HTML 文档：`<pre>` 里逐段 span，页面底色由调用方给。
    ///
    /// App 不画终端背景（那是终端自己的事），所以页面必须自己补底色：深色 palette
    /// 落在浏览器默认白底上，截图就成了「全白」。行高是 1 —— 半格像素靠上下两半拼
    /// 实心，行高一多就会出现横缝。
    pub fn html(&self, caption: &str, background: &str, foreground: &str) -> String {
        let mut body = String::new();
        for y in self.buffer.area.y..self.buffer.area.bottom() {
            for (style, text) in self.runs(y) {
                let css = html::css(style);
                if css.is_empty() {
                    body.push_str(&html::escape(&text));
                } else {
                    let _ = write!(body, "<span style=\"{css}\">{}</span>", html::escape(&text));
                }
            }
            body.push('\n');
        }
        html::document(&html::Page {
            title: caption,
            caption: Some(caption),
            background,
            foreground,
            padding: "22px 26px",
            font: "font:14px/1 Menlo,\"SF Mono\",\"DejaVu Sans Mono\",Consolas,monospace",
            body: &body,
        })
    }

    /// 一行里真正承载字形的格子（宽字形的续格被跳过），到行尾的可见列为上。
    fn glyphs(&self, y: u16) -> Vec<(u16, &ratatui::buffer::Cell)> {
        let area = self.buffer.area;
        let end = self.ink_end(y);
        let mut out = Vec::new();
        let mut x = area.x;
        while x < end {
            let cell = &self.buffer[(x, y)];
            out.push((x, cell));
            let width = unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
            x += width;
        }
        out
    }

    /// 一行按样式合并后的文本段（[`Frame::ansi`] / [`Frame::html`] 共用）。
    fn runs(&self, y: u16) -> Vec<(Style, String)> {
        let mut runs: Vec<(Style, String)> = Vec::new();
        for (_, cell) in self.glyphs(y) {
            let style = cell_style(cell);
            match runs.last_mut() {
                Some((current, text)) if *current == style => text.push_str(cell.symbol()),
                _ => runs.push((style, cell.symbol().to_string())),
            }
        }
        runs
    }

    /// 这一行最后一个「看得见」的列（见 [`Frame::rows`] 的裁剪规则）。
    fn ink_end(&self, y: u16) -> u16 {
        let area = self.buffer.area;
        let mut end = area.x + area.width;
        while end > area.x {
            let cell = &self.buffer[(end - 1, y)];
            let blank = cell.symbol() == " "
                && cell.fg == Color::Reset
                && cell.bg == Color::Reset
                && cell.modifier.is_empty();
            if !blank {
                break;
            }
            end -= 1;
        }
        end
    }
}

/// 取样一帧：调色板读真实配置（用户覆盖的颜色跟真机走，与 `theme_preview` 一致）。
pub fn sample(spec: &Spec) -> Frame {
    sample_with(AppConfig::load(), spec)
}

/// 同上，配置由调用方给（测试注入默认配置：不碰文件系统、不依赖 `$WING_HOME`）。
pub fn sample_with(mut config: AppConfig, spec: &Spec) -> Frame {
    if let Some(preset) = spec.preset {
        config.colors.preset = preset;
    }
    config.resolve();

    let mut app = App::new(SESSION_ID.to_string(), config, None);
    // 欢迎屏：固定 tip 种子 + 扫光时钟拨到指定相位。`App::new` 只给「此刻」，
    // 相位没有事件入口（也不该有）：取样器直接换掉那个 `Welcome`。
    let age = match spec.phase {
        Some(phase) => Duration::from_millis((phase.clamp(0.0, 1.0) * SWEEP_MS as f32) as u64),
        None => Duration::from_millis(SETTLED_AGE_MS),
    };
    let epoch = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
    app.welcome = Some(Welcome::new(TIP_SEED, epoch));

    // 状态走真事件投影（见模块文档）：快照给出会话 / 模型 / 事实 / 草稿 / turn 在飞，
    // 其余三条补上状态栏与输入卡的读数。
    app.handle_event(sync_event(spec));
    if spec.working {
        app.handle_event(call_metrics());
    }
    app.handle_event(context_stats());
    app.handle_event(state_changed());

    let mut terminal = Terminal::new(TestBackend::new(spec.width.max(1), spec.height.max(1)))
        .expect("test terminal");
    app.draw(&mut terminal).expect("frame");
    Frame {
        buffer: terminal.backend().buffer().clone(),
        palette: app.palette,
    }
}

fn meta() -> EventMeta {
    EventMeta {
        created_at: "2026-01-01T00:00:00+00:00".into(),
        session_id: Some(SESSION_ID.into()),
        request_id: "preview".into(),
    }
}

/// 会话快照：模型段 / 工作目录 / 事实行 / 草稿 / turn 在飞都从这一条进 App。
fn sync_event(spec: &Spec) -> WingEvent {
    let (skills, rules) = (spec.facts.skills, spec.facts.rules);
    WingEvent::SyncSession {
        session_id: SESSION_ID.into(),
        messages: Vec::new(),
        uncommitted: None,
        uncommitted_tools: Vec::new(),
        events: Vec::new(),
        // 快照的 status 就是 turn 在不在飞（读取，不推断）。
        status: if spec.working {
            SessionStatus::Working
        } else {
            SessionStatus::Idle
        },
        turn_started_at: spec
            .working
            .then(|| (chrono::Utc::now() - chrono::Duration::seconds(3)).to_rfc3339()),
        agent: Some(Box::new(AgentInfo {
            model_name: MODEL_NAME.into(),
            model_id: Some(MODEL_ID.into()),
            system_prompt: None,
            tools: Vec::new(),
            skills: (0..skills).map(|i| format!("skill-{i}")).collect(),
            rules: (0..rules).map(|i| format!("rule-{i}")).collect(),
            workspace: Some(WORKDIR.into()),
            provider_name: Some(PROVIDER.into()),
            model_display_name: Some(MODEL_LABEL.into()),
        })),
        name: None,
        draft: spec.draft.clone(),
        usage: Some(SessionUsage {
            user_messages: 4,
            llm_calls: 11,
            tool_calls: 9,
            turns: 3,
            prompt_tokens: 132_400,
            completion_tokens: 24_800,
            cached_tokens: 98_100,
            ..SessionUsage::default()
        }),
        meta: meta(),
    }
}

/// 这一轮的一次 LLM 调用：输入卡 meta rail 的读数（也累加进会话账本）。
fn call_metrics() -> WingEvent {
    WingEvent::LlmCallMetrics {
        model: MODEL_NAME.into(),
        prompt_tokens: 1_200,
        completion_tokens: 340,
        cached_tokens: 800,
        first_chunk_rt_ms: 320.0,
        tokens_per_sec: 42.5,
        stop_reason: Some("tool_use".into()),
        cache_expected_tokens: Some(1_056),
        cache_verdict: "hit".into(),
        cache_diverged: String::new(),
        meta: meta(),
    }
}

/// 上下文水位：状态栏右上的进度条与读数。
fn context_stats() -> WingEvent {
    WingEvent::ContextStats {
        message_count: 6,
        total_tokens: 45_200,
        context_window_tokens: 200_000,
        system_prompt_parts: Vec::new(),
        meta: meta(),
    }
}

/// thinking / 推理档位：状态栏模型段之后的 `· think:medium`。
fn state_changed() -> WingEvent {
    WingEvent::SessionStateChanged {
        model: Some(MODEL_NAME.into()),
        model_id: Some(MODEL_ID.into()),
        provider_name: Some(PROVIDER.into()),
        model_display_name: Some(MODEL_LABEL.into()),
        thinking: Some(true),
        reasoning_effort: Some("medium".into()),
        yolo: Some(false),
        title: None,
        agent: None,
        meta: meta(),
    }
}

/// 一格的样式（默认色不写进 `Style`：写进去会让「无样式」与「显式复位」分成两段，
/// 白白多出一串 `0` 码）。
fn cell_style(cell: &ratatui::buffer::Cell) -> Style {
    let mut style = Style::default().add_modifier(cell.modifier);
    if cell.fg != Color::Reset {
        style = style.fg(cell.fg);
    }
    if cell.bg != Color::Reset {
        style = style.bg(cell.bg);
    }
    style
}

/// `--plain` 的一个格子：带背景色的半格还原成实心块，其余原样（见 [`Frame::plain`]）。
fn plain_symbol(cell: &ratatui::buffer::Cell) -> &str {
    if cell.bg != Color::Reset && matches!(cell.symbol(), "▀" | "▄") {
        "█"
    } else {
        cell.symbol()
    }
}

// ── 命令行输入（example 只剩输出） ──────────────────────────────

/// `splash_preview` 的帮助文本。
pub const USAGE: &str = "\
splash_preview — 开屏整块的真帧（状态栏 + 牌子 + 输入卡）

USAGE:
    cargo run -p wing --example splash_preview [OPTIONS]

OPTIONS:
    --width <N>         终端列数（默认 100，上限 1000）
    --height <N>        终端行数（默认 30，上限 1000）—— 开屏的竖直位置由它决定
    --preset <NAME>     auto | dark | light（默认沿用配置）
    --draft <TEXT>      输入卡里放一段草稿（默认空：显示占位文案）
    --working           agent 在干活：活动栏 + 飞行姿态 + 这一轮的用量
    --facts <S,R>       名牌右列的会话事实（默认 2,1；0,0 = 这一行留空）
    --phase <0..1>      开屏扫光的相位（默认已定格；1 = 已扫完，同定格）
    --plain             无颜色输出（半格像素还原成实心块）
    --html <PATH>       写出 HTML（浏览器里对照 / 截图），与 --plain 互斥
    --bg <#RRGGBB>      HTML 页面底色（默认按明暗档）
    --fg <#RRGGBB>      HTML 页面前景色
    -h, --help          这一页

    `--flag=value` 是等号形态：值本身以 `--` 开头时（例如草稿里带 `--`）用它。
";

/// 命令行输入：取样参数 + 输出后端。
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// 取样参数。
    pub spec: Spec,
    /// 无颜色输出。
    pub plain: bool,
    /// HTML 输出路径（与 [`Options::plain`] 互斥）。
    pub html: Option<PathBuf>,
    /// HTML 页面底色（`None` = 按明暗档）。
    pub background: Option<String>,
    /// HTML 页面前景色（`None` = 按明暗档）。
    pub foreground: Option<String>,
    /// 只打帮助。
    pub help: bool,
}

impl Options {
    /// 解析 `args`（不含程序名）。
    ///
    /// `--flag value` 与 `--flag=value` 两种形态。`--flag --other` 是**用法错误**而不是
    /// 「把 `--other` 当值」（那会静默写出一个名叫 `--plain` 的文件）；值本身真以 `--`
    /// 开头时用等号形态。错误消息是给人看的，所以由单测钉住。
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
                _ => (arg, None),
            };
            let value = |index: &mut usize| -> Result<String, String> {
                if let Some(value) = inline.clone() {
                    return Ok(value);
                }
                match args.get(*index + 1) {
                    Some(next) if !next.starts_with("--") => {
                        *index += 1;
                        Ok(next.clone())
                    }
                    Some(next) => Err(format!(
                        "{flag} needs a value, got the flag '{next}' (write {flag}=<value> \
                         when the value itself starts with --)"
                    )),
                    None => Err(format!("{flag} needs a value")),
                }
            };
            match flag {
                "--help" | "-h" => options.help = true,
                "--plain" => options.plain = true,
                "--working" => options.spec.working = true,
                "--width" => options.spec.width = number(&value(&mut index)?, flag)?,
                "--height" => options.spec.height = number(&value(&mut index)?, flag)?,
                "--preset" => {
                    let raw = value(&mut index)?;
                    options.spec.preset = Some(ColorPreset::parse(&raw).ok_or_else(|| {
                        format!(
                            "unknown preset '{raw}' (known: {})",
                            ColorPreset::ALL
                                .iter()
                                .map(|preset| preset.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?);
                }
                "--draft" => options.spec.draft = Some(value(&mut index)?),
                "--facts" => options.spec.facts = facts(&value(&mut index)?)?,
                "--phase" => {
                    let raw = value(&mut index)?;
                    let phase = raw
                        .parse::<f32>()
                        .ok()
                        .filter(|phase| (0.0..=1.0).contains(phase))
                        .ok_or_else(|| format!("--phase expects a number in 0..=1, got '{raw}'"))?;
                    options.spec.phase = Some(phase);
                }
                "--html" => options.html = Some(PathBuf::from(value(&mut index)?)),
                "--bg" => options.background = Some(value(&mut index)?),
                "--fg" => options.foreground = Some(value(&mut index)?),
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag '{other}' (see --help)"));
                }
                other => return Err(format!("unexpected argument '{other}' (see --help)")),
            }
            index += 1;
        }

        if options.plain && options.html.is_some() {
            return Err(
                "--plain and --html are different outputs: write the frame to stdout or to a \
                 styled HTML file, not both"
                    .into(),
            );
        }
        if (options.background.is_some() || options.foreground.is_some()) && options.html.is_none()
        {
            return Err("--bg / --fg only apply to --html".into());
        }
        Ok(options)
    }

    /// 一帧的说明行：尺寸 / 明暗 / 相位 / 状态（终端 stderr 与 HTML 标题共用）。
    pub fn caption(&self, frame: &Frame) -> String {
        let appearance = match frame.palette().appearance {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        };
        let mut parts = vec![
            format!("{}×{}", self.spec.width, self.spec.height),
            appearance.to_string(),
            // `phase = 1` 与定格是同一帧（扫光窗口已经走完），说明行不该说"100%"。
            match self.spec.phase {
                Some(phase) if phase < 1.0 => format!("sweep {:.0}%", phase * 100.0),
                _ => "settled".to_string(),
            },
            format!("facts {},{}", self.spec.facts.skills, self.spec.facts.rules),
        ];
        if self.spec.draft.is_some() {
            parts.push("draft".to_string());
        }
        if self.spec.working {
            parts.push("working".to_string());
        }
        parts.join(" · ")
    }
}

/// 正整数的列 / 行数。上界是防呆：`--width 65535 --height 65535` 会让取样器去分配
/// 四十亿个格子（卡死或 OOM），而不是报错。
fn number(raw: &str, flag: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .ok()
        .filter(|value| (1..=1000).contains(value))
        .ok_or_else(|| format!("{flag} expects a positive integer (max 1000), got '{raw}'"))
}

/// `--facts 2,1` → 会话事实（skills,rules）。上界同 [`number`]：计数只出现在名牌上。
fn facts(raw: &str) -> Result<SessionFacts, String> {
    let (skills, rules) = raw
        .split_once(',')
        .ok_or_else(|| format!("--facts expects '<skills>,<rules>', got '{raw}'"))?;
    let parse = |text: &str, name: &str| -> Result<usize, String> {
        let value = text
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("--facts: {name} expects a count, got '{text}'"))?;
        (value <= 999)
            .then_some(value)
            .ok_or_else(|| format!("--facts: {name} must be <= 999, got {value}"))
    };
    Ok(SessionFacts {
        skills: parse(skills, "skills")?,
        rules: parse(rules, "rules")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Appearance;

    fn spec() -> Spec {
        Spec {
            width: 100,
            height: 30,
            preset: Some(ColorPreset::Dark),
            ..Spec::default()
        }
    }

    fn render(spec: &Spec) -> Frame {
        sample_with(AppConfig::default(), spec)
    }

    #[test]
    fn any_size_renders_a_frame_of_that_size() {
        // 极小的尺寸也得活着（工具会被随手试到 1×1 / 20×3）；整屏的"不溢出"
        // 由 `app::tests::splash_frame` 钉在拼装后的帧上。
        for (width, height) in [(120u16, 40u16), (80, 24), (40, 12), (1, 1), (20, 3)] {
            let frame = render(&Spec {
                width,
                height,
                ..spec()
            });
            assert_eq!(frame.rows().len(), height as usize);
        }
    }

    #[test]
    fn the_frame_is_deterministic() {
        // 视觉改动的 diff 必须只反映改动：tip 抽签与死时钟都固定在同一处。
        let spec = spec();
        assert_eq!(render(&spec).plain(), render(&spec).plain());
    }

    #[test]
    fn the_sweep_phase_moves_the_highlight_not_the_text() {
        let settled = render(&spec());
        let sweeping = render(&Spec {
            phase: Some(0.4),
            ..spec()
        });
        assert_eq!(settled.rows(), sweeping.rows(), "扫光只动颜色，不动字形");
        assert_ne!(settled.ansi(), sweeping.ansi(), "扫光得真的出现在帧里");
    }

    #[test]
    fn light_and_dark_share_the_text_but_not_the_ink() {
        let dark = render(&spec());
        let light = render(&Spec {
            preset: Some(ColorPreset::Light),
            ..spec()
        });
        assert_eq!(dark.rows(), light.rows(), "明暗只换颜色，不换排版");
        assert_eq!(dark.palette().appearance, Appearance::Dark);
        assert_eq!(light.palette().appearance, Appearance::Light);
        assert_ne!(dark.colors(), light.colors());
        assert_ne!(
            html::page_colors(dark.palette()),
            html::page_colors(light.palette())
        );
    }

    #[test]
    fn plain_turns_a_backed_half_block_into_a_solid_one() {
        // 没有颜色时 `▀`（前景上半 + 背景下半）只画出上半：像素大字与海鸥会瘦一半。
        let mut cell = ratatui::buffer::Cell::default();
        cell.set_symbol("▀");
        cell.set_bg(Color::Rgb(1, 2, 3));
        assert_eq!(plain_symbol(&cell), "█");
        cell.set_symbol("▄");
        assert_eq!(plain_symbol(&cell), "█");
        cell.set_bg(Color::Reset);
        assert_eq!(plain_symbol(&cell), "▄", "没有背景色的半格不动");
        // 空格永远不动：带背景的空格是铺底，不能涂成实心块。
        cell.set_symbol(" ");
        cell.set_bg(Color::Rgb(1, 2, 3));
        assert_eq!(plain_symbol(&cell), " ");
    }

    #[test]
    fn a_wide_glyph_keeps_one_cell_in_the_text_projections() {
        // `Buffer` 把宽字形的第二格写成空格：格子里看是「读 系 统」，文本投影里必须
        // 是「读系统」（终端就是这么画的）。
        let frame = render(&spec());
        let raw = frame.rows().join("\n");
        let plain = frame.plain();
        let text = plain.lines().collect::<Vec<_>>().join("\n");
        assert!(
            raw.contains("中 断"),
            "格子里宽字形后跟一个续格：{raw:.200}"
        );
        assert!(
            text.contains("中断 · Ctrl+J 换行"),
            "文本投影收掉续格：{text:.300}"
        );
    }

    /// 把 `Frame::ansi()` 的样式解回「每格的有效前景 / 背景」。
    ///
    /// 只认本帧会出现的记号：复位、六个修饰位、`38;2;r;g;b` / `48;2;r;g;b`（调色板
    /// 全是真彩）。解不出来就直接失败 —— 那说明编码器长出了测试没跟上的记法。
    fn parsed_styles(frame: &Frame) -> Vec<Vec<(Color, Color)>> {
        let mut rows = Vec::new();
        for line in frame.ansi().lines() {
            let mut row: Vec<(Color, Color)> = Vec::new();
            let (mut fg, mut bg) = (Color::Reset, Color::Reset);
            let mut rest = line;
            while let Some(at) = rest.find('\x1b') {
                let (text, tail) = rest.split_at(at);
                row.extend(std::iter::repeat_n((fg, bg), text.chars().count()));
                let end = tail.find('m').expect("SGR 以 m 结尾");
                let codes: Vec<&str> = tail[2..end].split(';').collect();
                let mut code = 0;
                while code < codes.len() {
                    match codes[code] {
                        "0" => {
                            fg = Color::Reset;
                            bg = Color::Reset;
                            code += 1;
                        }
                        layer @ ("38" | "48") => {
                            let rgb = match codes.get(code + 1) {
                                Some(&"2") => Color::Rgb(
                                    codes[code + 2].parse().expect("r"),
                                    codes[code + 3].parse().expect("g"),
                                    codes[code + 4].parse().expect("b"),
                                ),
                                other => panic!("测试帧里只有真彩，得到 {layer};{other:?}"),
                            };
                            if layer == "38" {
                                fg = rgb;
                            } else {
                                bg = rgb;
                            }
                            code += 5;
                        }
                        "1" | "2" | "3" | "4" | "7" | "9" => code += 1,
                        unknown => panic!("编码器产出了测试没跟上的 SGR 参数 {unknown}"),
                    }
                }
                rest = &tail[end + 1..];
            }
            row.extend(std::iter::repeat_n((fg, bg), rest.chars().count()));
            rows.push(row);
        }
        rows
    }

    /// 这一帧每个字形的（前景, 背景），与 [`parsed_styles`] 同口径。
    fn expected_styles(frame: &Frame) -> Vec<Vec<(Color, Color)>> {
        (frame.buffer.area.y..frame.buffer.area.bottom())
            .map(|y| {
                frame
                    .glyphs(y)
                    .into_iter()
                    .flat_map(|(_, cell)| {
                        std::iter::repeat_n((cell.fg, cell.bg), cell.symbol().chars().count())
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_ansi_output_is_the_frame_and_nothing_else() {
        // 逐格还原：终端上看到的每个格子 == Buffer 里的那一格。默认样式的段不复位
        // （空白格继承上一段的底色）就会在这里红。
        for spec in [
            spec(),
            Spec {
                width: 80,
                height: 24,
                ..spec()
            },
        ] {
            let frame = render(&spec);
            assert_eq!(parsed_styles(&frame), expected_styles(&frame));
            assert!(
                !frame.ansi().contains("\x1b[0m\x1b[0m"),
                "每段自闭合即可，行尾不该再补一次复位"
            );
            // 两个后端同一帧：ANSI 里出现过的每个颜色，HTML 页面里也得有。
            let document = frame.html("reconcile", "#0b0f16", "#c9ced9");
            for color in frame.colors() {
                assert!(
                    document.contains(&html::hex(color)),
                    "HTML 少了颜色 {color:?}"
                );
            }
        }
    }

    #[test]
    fn html_is_a_styled_page_that_escapes_its_text() {
        let frame = render(&spec());
        let document = frame.html("120×40 · dark", "#0b0f16", "#c9ced9");
        assert!(document.starts_with("<!doctype html>"), "{document:.80}");
        assert!(document.contains("background:#0b0f16"), "页面底色要补上");
        assert!(document.contains("<span style=\"color:#"), "每段带真彩");
        // 行高必须正好一格（半格像素拼实心），不能是 1.35 那种。
        assert!(
            document.contains("font:14px/1 Menlo"),
            "行高 1：{document:.400}"
        );
        assert!(!document.contains("1.35"), "半格像素的行高必须是 1");
        assert!(document.contains("120×40 · dark"), "标题与说明都带 caption");
    }

    #[test]
    fn the_working_state_puts_the_activity_rail_on_the_card() {
        let idle = render(&spec());
        let working = render(&Spec {
            working: true,
            ..spec()
        });
        let idle_text = idle.rows().join("\n");
        let working_text = working.rows().join("\n");
        assert!(!idle_text.contains("Esc to interrupt"), "空闲卡没有活动栏");
        assert!(
            working_text.contains("Working..."),
            "干活时有活干：{working_text}"
        );
        assert!(
            working_text.contains("Esc to interrupt"),
            "活动栏带中断提示：{working_text}"
        );
        // 4.2k in 之类：这一轮的用量也进 meta rail。
        assert!(working_text.contains("1.2k in"), "meta rail 带本轮读数");
    }

    #[test]
    fn a_draft_lands_in_the_composer() {
        let frame = render(&Spec {
            draft: Some("refactor the splash".into()),
            ..spec()
        });
        assert!(
            frame.rows().join("\n").contains("refactor the splash"),
            "草稿在卡里"
        );
    }

    #[test]
    fn the_cli_reads_its_flags() {
        let args: Vec<String> = "--width 80 --height 24 --preset light --draft hi --working \
             --facts 0,3 --phase 0.4 --plain"
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let options = Options::parse(&args).expect("valid");
        assert_eq!(options.spec.width, 80);
        assert_eq!(options.spec.height, 24);
        assert_eq!(options.spec.preset, Some(ColorPreset::Light));
        assert_eq!(options.spec.draft.as_deref(), Some("hi"));
        assert!(options.spec.working);
        assert_eq!(
            options.spec.facts,
            SessionFacts {
                skills: 0,
                rules: 3
            }
        );
        assert_eq!(options.spec.phase, Some(0.4));
        assert!(options.plain);
        assert!(options.html.is_none());
        assert!(!options.help);

        // 等号形态：值本身以 `--` 开头。
        let options = Options::parse(&["--draft=--not-a-flag".to_string()]).expect("valid");
        assert_eq!(options.spec.draft.as_deref(), Some("--not-a-flag"));
    }

    #[test]
    fn the_cli_defaults_match_the_spec() {
        let options = Options::parse(&[]).expect("valid");
        assert_eq!(options.spec.width, 100);
        assert_eq!(options.spec.height, 30);
        assert_eq!(options.spec.preset, None);
        assert_eq!(
            options.spec.facts,
            SessionFacts {
                skills: 2,
                rules: 1
            }
        );
        assert_eq!(options.spec.phase, None);
        assert!(!options.plain && options.html.is_none() && !options.help);
        assert!(Options::parse(&["-h".to_string()]).expect("valid").help);
    }

    #[test]
    fn the_cli_refuses_ambiguous_or_broken_input() {
        for (args, expected) in [
            (
                vec!["--plain", "--html", "/tmp/x.html"],
                "different outputs",
            ),
            (vec!["--html", "--plain"], "needs a value"),
            (vec!["--width", "abc"], "positive integer"),
            (vec!["--width", "0"], "positive integer"),
            (vec!["--width", "2000"], "max 1000"),
            (vec!["--facts", "2"], "'<skills>,<rules>'"),
            (vec!["--facts", "1000,0"], "<= 999"),
            (vec!["--phase", "1.5"], "0..=1"),
            (vec!["--preset", "neon"], "unknown preset"),
            (vec!["--bg", "#000000"], "only apply to --html"),
            (vec!["--widht", "120"], "unknown flag"),
            (vec!["stray"], "unexpected argument"),
            (vec!["--draft", "--plain"], "needs a value"),
        ] {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            let error = Options::parse(&args).expect_err(&format!("{args:?} 应当报错"));
            assert!(
                error.contains(expected),
                "{args:?} 的报错应当提到 `{expected}`，实际：{error}"
            );
        }
    }

    #[test]
    fn the_caption_says_what_the_frame_shows() {
        let options = Options::parse(&[]).expect("valid");
        let frame = render(&options.spec);
        assert_eq!(
            options.caption(&frame),
            "100×30 · dark · settled · facts 2,1"
        );

        let options = Options::parse(
            &"--phase 0.4 --working --draft hi --facts 0,0 --preset light"
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .expect("valid");
        let frame = render(&options.spec);
        assert_eq!(
            options.caption(&frame),
            "100×30 · light · sweep 40% · facts 0,0 · draft · working"
        );

        // `phase = 1` 已经扫完，与定格同一帧 —— 说明行不能说 "sweep 100%"。
        let options = Options::parse(&["--phase=1".to_string()]).expect("valid");
        let frame = render(&options.spec);
        assert!(
            options.caption(&frame).contains("settled"),
            "{}",
            options.caption(&frame)
        );
    }

    #[test]
    fn facts_leave_a_blank_slot_when_nothing_was_loaded() {
        let none = render(&Spec {
            facts: SessionFacts {
                skills: 0,
                rules: 0,
            },
            ..spec()
        });
        let some = render(&spec());
        assert!(some.rows().join("\n").contains("2 skills · 1 rule"));
        assert!(!none.rows().join("\n").contains("skills"));
        assert!(!none.rows().join("\n").contains("rule"));
        // 槽位恒在：名牌高度不随事实变（下方内容不跳行）。
        assert_eq!(none.rows().len(), some.rows().len());
    }
}
