//! 整屏取样器 —— 创作期视觉工具的库侧（命令行在 `examples/splash_preview.rs`）。
//!
//! 一帧「状态栏 + 开屏牌子 + 输入卡」只有一条真路径：[`App::draw`] —— 它读 App 的
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
//! 同一份代码两次运行逐字节相同，视觉改动才 diff 得出来。
//!
//! **取样面**：开屏整块。弹层 / 设置面板 / toast / 图片不在 fixture 里，画不出来。

use std::fmt::Write as _;
use std::time::Duration;
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::style::Modifier;
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
use crate::ui::shimmer::to_rgb;
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
    /// 名牌右列的会话事实；`None` = 这一行留空（老网关 / 什么都没加载）。
    pub facts: Option<SessionFacts>,
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
            facts: Some(SessionFacts {
                skills: 2,
                rules: 1,
            }),
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

    /// 真彩 ANSI（一行一条，行尾复位）：直接贴到终端里就是真机上那一帧。
    pub fn ansi(&self) -> String {
        let mut out = String::new();
        for y in self.buffer.area.y..self.buffer.area.bottom() {
            for (style, text) in self.runs(y) {
                push_ansi(&mut out, style, &text);
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }

    /// 独立 HTML 文档：`<pre>` 里逐段 span，页面底色由调用方给。
    ///
    /// App 不画终端背景（那是终端自己的事），所以页面必须自己补底色：深色 palette
    /// 落在浏览器默认白底上，截图就成了「全白」。行高是 1 —— 半格像素靠上下两半
    /// 拼实心，行高一多就会出现横缝。
    pub fn html(&self, caption: &str, background: &str, foreground: &str) -> String {
        let mut out = String::new();
        out.push_str("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n");
        let _ = writeln!(out, "<title>{}</title>", escape(caption));
        out.push_str("<style>\nhtml,body{margin:0;padding:0}\n");
        let _ = writeln!(
            out,
            "body{{background:{background};color:{foreground};padding:22px 26px}}"
        );
        out.push_str(".cap{font:12px/1.4 Menlo,\"SF Mono\",\"DejaVu Sans Mono\",monospace;opacity:.55;margin:0 0 10px}\n");
        out.push_str("pre{margin:0;font:14px/1 Menlo,\"SF Mono\",\"DejaVu Sans Mono\",Consolas,monospace;white-space:pre}\n");
        out.push_str("</style></head><body>\n");
        let _ = writeln!(out, "<div class=\"cap\">{}</div>", escape(caption));
        out.push_str("<pre>\n");
        for y in self.buffer.area.y..self.buffer.area.bottom() {
            for (style, text) in self.runs(y) {
                push_html(&mut out, style, &text);
            }
            out.push('\n');
        }
        out.push_str("</pre></body></html>\n");
        out
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

/// HTML 页面的默认底色 / 前景 —— 与这套 palette 配对的墙面颜色。
pub fn page_colors(palette: &ThemePalette) -> (&'static str, &'static str) {
    match palette.appearance {
        Appearance::Dark => ("#0b0f16", "#c9ced9"),
        Appearance::Light => ("#ffffff", "#1f2328"),
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
    let (skills, rules) = spec
        .facts
        .map_or((0, 0), |facts| (facts.skills, facts.rules));
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

fn cell_style(cell: &ratatui::buffer::Cell) -> Style {
    Style::default()
        .fg(cell.fg)
        .bg(cell.bg)
        .add_modifier(cell.modifier)
}

/// `--plain` 的一个格子：带背景色的半格还原成实心块，其余原样（见 [`Frame::plain`]）。
fn plain_symbol(cell: &ratatui::buffer::Cell) -> &str {
    if cell.bg != Color::Reset && matches!(cell.symbol(), "▀" | "▄") {
        "█"
    } else {
        cell.symbol()
    }
}

fn ansi_index(color: Color) -> u8 {
    match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        _ => 7,
    }
}

/// 一段文本的 SGR 序列（`Style::default()` → 空串）。
fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    let modifier = style.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_string());
        }
    }
    if let Some(code) = sgr_color(style.fg, 38) {
        codes.push(code);
    }
    if let Some(code) = sgr_color(style.bg, 48) {
        codes.push(code);
    }
    if codes.is_empty() {
        return String::new();
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn sgr_color(color: Option<Color>, layer: u8) -> Option<String> {
    match color? {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(format!("{layer};2;{r};{g};{b}")),
        Color::Indexed(i) => Some(format!("{layer};5;{i}")),
        named => Some(format!("{layer};5;{}", ansi_index(named))),
    }
}

fn push_ansi(out: &mut String, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    let sgr = sgr(style);
    if !sgr.is_empty() {
        out.push_str("\x1b[0m");
        out.push_str(&sgr);
    }
    out.push_str(text);
}

fn push_html(out: &mut String, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    let css = css(style);
    if css.is_empty() {
        out.push_str(&escape(text));
    } else {
        let _ = write!(out, "<span style=\"{css}\">{}</span>", escape(text));
    }
}

fn css(style: Style) -> String {
    let mut decls: Vec<String> = Vec::new();
    let reversed = style.add_modifier.contains(Modifier::REVERSED);
    let fg = style.fg.filter(|color| *color != Color::Reset);
    let bg = style.bg.filter(|color| *color != Color::Reset);
    let (fg, bg) = if reversed { (bg, fg) } else { (fg, bg) };
    if let Some(color) = fg {
        decls.push(format!("color:{}", hex(color)));
    }
    if let Some(color) = bg {
        decls.push(format!("background-color:{}", hex(color)));
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        decls.push("font-weight:700".into());
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        decls.push("font-style:italic".into());
    }
    let mut decoration: Vec<&str> = Vec::new();
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        decoration.push("underline");
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        decoration.push("line-through");
    }
    if !decoration.is_empty() {
        decls.push(format!("text-decoration:{}", decoration.join(" ")));
    }
    if style.add_modifier.contains(Modifier::DIM) {
        decls.push("opacity:.62".into());
    }
    decls.join(";")
}

fn hex(color: Color) -> String {
    let (r, g, b) = to_rgb(color);
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_frame_is_as_big_as_the_spec_says() {
        for (width, height) in [(120u16, 40u16), (80, 24), (40, 12), (1, 1), (20, 3)] {
            let frame = render(&Spec {
                width,
                height,
                ..spec()
            });
            let rows = frame.rows();
            assert_eq!(rows.len(), height as usize);
            // 格子投影：一个格子一个字符（宽字形的续格单独算一格），所以数**字符**。
            for row in &rows {
                assert!(
                    row.chars().count() <= width as usize,
                    "{width}x{height} 的格子行超出宽度：{row:?}"
                );
            }
            // 文本投影：宽字形真的占两列，所以按显示宽度量。
            for line in frame.plain().lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width as usize,
                    "{width}x{height} 的文本行超出宽度：{line:?}"
                );
            }
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
        assert_ne!(dark.ansi(), light.ansi());
        assert_ne!(page_colors(dark.palette()), page_colors(light.palette()));
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

    #[test]
    fn ansi_carries_colour_and_ends_each_row_with_a_reset() {
        let frame = render(&spec());
        let ansi = frame.ansi();
        assert!(ansi.contains("38;2;"), "真彩前景在：{ansi:.200}");
        assert!(
            ansi.contains("48;2;"),
            "真彩背景在（半格像素）：{ansi:.200}"
        );
        for line in ansi.lines() {
            assert!(line.ends_with("\x1b[0m"), "每行都以复位收尾：{line:?}");
        }
        assert_eq!(ansi.lines().count(), spec().height as usize);
    }

    #[test]
    fn html_is_a_styled_page_that_escapes_its_text() {
        let frame = render(&spec());
        let document = frame.html("120×40 · dark", "#0b0f16", "#c9ced9");
        assert!(document.starts_with("<!doctype html>"), "{document:.80}");
        assert!(document.contains("background:#0b0f16"), "页面底色要补上");
        assert!(document.contains("<span style=\"color:#"), "每段带真彩");
        assert!(document.contains("line-height") || document.contains("/1 Menlo"));
        // 行高必须正好一格（半格像素拼实心），不能是 1.35 那种。
        assert!(!document.contains("1.35"), "半格像素的行高必须是 1");
        assert!(document.contains("120×40 · dark"), "标题与说明都带 caption");
        assert_eq!(escape("<a & b>"), "&lt;a &amp; b&gt;");
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
    fn facts_are_optional() {
        let none = render(&Spec {
            facts: None,
            ..spec()
        });
        let some = render(&spec());
        assert!(some.rows().join("\n").contains("2 skills · 1 rule"));
        assert!(!none.rows().join("\n").contains("skills"));
        // 槽位恒在：有无事实，帧高与输入卡位置都不变。
        assert_eq!(none.rows().len(), some.rows().len());
    }
}
