//! `thinking_preview` — 折叠思考行（刷光标签）的创作期预览。
//!
//! 调刷光手感 / 措辞 / 秒数时用它，不用起网关、不用开 TUI：
//!
//! ```text
//! # 默认画廊：几个相位 × 几个状态（进行中 / 定格 / 无计时），带一段上下文
//! cargo run -p wing --example thinking_preview
//!
//! # 动起来（真终端里看持续刷光，Ctrl+C / 到时退出）
//! cargo run -p wing --example thinking_preview -- --animate
//!
//! # 单张：某个相位、某个宽度
//! cargo run -p wing --example thinking_preview -- --phase 0.5 --width 60
//!
//! # 不带颜色（贴 issue / 对文案）
//! cargo run -p wing --example thinking_preview -- --plain
//! ```
//!
//! 画的就是 TUI 里那一份：`ThinkingBlock::to_lines`（折叠行 + 上下文真 cell），
//! 不会出现"预览好看、真机不一样"。`cargo test` 会编译这个 example（不运行），
//! 所以它不会因为没人跑而腐掉。

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar as _;
use wing::config::LayoutConfig;
use wing::config::ThemePalette;
use wing::config::rendering::ThinkingMode;
use wing::render::markdown::ImageOpts;
use wing::render::renderable::CellContext;
use wing::ui::cells::thinking::ThinkingBlock;
use wing::ui::cells::tool_call::ToolCallBlock;
use wing::ui::cells::tool_call::ToolStatus;
use wing::ui::chat_view::ChatCell;

/// 前 / 背景的 SGR 参数（`38;` / `48;` 之后接的那一段）。
fn sgr(color: Option<Color>) -> Option<String> {
    match color? {
        Color::Rgb(r, g, b) => Some(format!("2;{r};{g};{b}")),
        Color::Reset => None,
        other => Some(ansi_index(other).to_string()),
    }
}

/// 命名色的 ANSI 索引（16 色表）。
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

/// 一行 ANSI 编码：按 span 输出（真终端就是这么画的），超宽部分截掉 ——
/// 超宽在预览里就该看出来。
fn render_line(line: &Line<'_>, width: u16, plain: bool) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    let mut open = false;
    for span in &line.spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width as usize {
                break;
            }
            text.push(ch);
            used += w;
        }
        if text.is_empty() {
            continue;
        }
        if plain {
            out.push_str(&text);
            continue;
        }
        let style = span.style;
        out.push_str("\x1b[0m");
        if style.add_modifier.contains(Modifier::BOLD) {
            out.push_str("\x1b[1m");
        }
        if style.add_modifier.contains(Modifier::DIM) {
            out.push_str("\x1b[2m");
        }
        if let Some(code) = sgr(Some(style.fg.unwrap_or(Color::Reset))) {
            let _ = write!(out, "\x1b[38;{code}m");
        }
        out.push_str(&text);
        open = true;
    }
    if open && !plain {
        out.push_str("\x1b[0m");
    }
    out
}

fn palette() -> ThemePalette {
    ThemePalette::default()
}

/// 一段假对话：工具调用（真 cell）→ 折叠的思考行 → 助手回复（真 cell）。
fn transcript(
    block: &ThinkingBlock,
    width: u16,
    plain: bool,
    palette: &ThemePalette,
) -> (Vec<String>, usize) {
    let layout = LayoutConfig::default();
    let images = ImageOpts::off();
    let ctx = CellContext {
        palette,
        thinking_mode: ThinkingMode::Hidden,
        layout: &layout,
        images,
    };
    let mut call = ToolCallBlock::new(
        "Bash".into(),
        serde_json::json!({ "command": "cargo test" }),
        "call-1".into(),
    );
    call.status = ToolStatus::Success;
    call.set_result("ok. 12 passed".into(), true);
    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.extend(ChatCell::ToolCall(call).to_lines(width, &ctx));
    lines.extend(block.to_lines(palette, ThinkingMode::Hidden, width, images));
    lines.extend(
        ChatCell::AssistantMessage("测试通过，接着改渲染层。".into()).to_lines(width, &ctx),
    );

    let rendered: Vec<String> = lines
        .iter()
        .map(|line| render_line(line, width, plain))
        .collect();
    let height = rendered.len();
    (rendered, height)
}

/// 一块思考行在给定状态下的样子。
fn block_at(width_phase: f32, state: LabelState) -> ThinkingBlock {
    let mut block = ThinkingBlock::new();
    block.append("（正文只在展开时可见，折叠行不显示。）");
    let now = Instant::now();
    match state {
        LabelState::Active(elapsed) => {
            block.start(now);
            block.tick(now + elapsed);
            // 相位直接摆到要看的时刻（真机里由帧 tick 按周期推进）。
            block.set_sweep_phase(width_phase);
        }
        LabelState::Done(elapsed) => {
            block.start(now);
            block.finish(now + elapsed);
        }
        LabelState::Untimed => {}
    }
    block
}

/// 预览里要摆的三个状态。
#[derive(Debug, Clone, Copy)]
enum LabelState {
    /// 进行中（秒数 = 已耗）。
    Active(Duration),
    /// 已定格（秒数 = 时长）。
    Done(Duration),
    /// 没有计时（重放的历史块）。
    Untimed,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let plain = args.iter().any(|a| a == "--plain");
    let animate = args.iter().any(|a| a == "--animate");
    let width = value(&args, "--width")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(76);
    let phase = value(&args, "--phase").and_then(|v| v.parse::<f32>().ok());
    let seconds = value(&args, "--seconds")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(6);
    let palette = palette();

    let mut out = String::new();
    if animate {
        animate_loop(&mut out, &palette, width, plain, seconds);
    } else {
        match phase {
            Some(p) => draw_one(&mut out, width, p, plain, &palette),
            None => gallery(&mut out, width, plain, &palette),
        }
    }
    emit(&out);
}

/// 画廊：相位 × 状态，最后来一张带上下文的整屏。
fn gallery(out: &mut String, width: u16, plain: bool, palette: &ThemePalette) {
    let _ = writeln!(
        out,
        "\n折叠思考行画廊（width={width}，宽 {width} 列；真机里进行中那条会持续刷光）\n"
    );
    for (label, phase) in [
        ("相位 0.00", 0.0f32),
        ("相位 0.25", 0.25),
        ("相位 0.50", 0.5),
        ("相位 0.75", 0.75),
    ] {
        let _ = writeln!(out, "{label}");
        for state in [
            LabelState::Active(Duration::from_secs(4)),
            LabelState::Done(Duration::from_secs(12)),
            LabelState::Untimed,
        ] {
            let block = block_at(phase, state);
            for line in block.to_lines(palette, ThinkingMode::Hidden, width, ImageOpts::off()) {
                let _ = writeln!(out, "{}", render_line(&line, width, plain));
            }
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "上下文（真 cell：工具调用 / 思考行 / 回复）：");
    let block = block_at(0.4, LabelState::Active(Duration::from_secs(4)));
    let (lines, _) = transcript(&block, width, plain, palette);
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out);
}

/// 单张：一个相位。
fn draw_one(out: &mut String, width: u16, phase: f32, plain: bool, palette: &ThemePalette) {
    let _ = writeln!(out, "\n相位 {phase:.2}（width={width}）");
    for state in [
        LabelState::Active(Duration::from_secs(4)),
        LabelState::Done(Duration::from_secs(12)),
        LabelState::Untimed,
    ] {
        let block = block_at(phase, state);
        for line in block.to_lines(palette, ThinkingMode::Hidden, width, ImageOpts::off()) {
            let _ = writeln!(out, "{}", render_line(&line, width, plain));
        }
    }
}

/// 动画：整块重画（光标上移 + 清屏），≈25fps，与真机同一份渲染。
fn animate_loop(out: &mut String, palette: &ThemePalette, width: u16, plain: bool, seconds: u64) {
    let start = Instant::now();
    let limit = Duration::from_secs(seconds.max(1));
    // 0 = 还没画过正文块（首帧不移动光标：标题刚打完，光标就在块首）。
    let mut drawn = 0usize;
    let _ = writeln!(
        out,
        "\n持续刷光（{seconds}s，Ctrl+C 退出）—— 真机里这条只在一行里动\n"
    );
    loop {
        let elapsed = start.elapsed();
        if elapsed > limit {
            break;
        }
        let mut block = ThinkingBlock::new();
        block.append("正文");
        block.start(start);
        block.tick(start + elapsed);
        let (lines, height) = transcript(&block, width, plain, palette);
        // 光标移回块首 + 清到屏底，再整块重画。
        if drawn > 0 {
            let _ = write!(out, "\x1b[{drawn}A\x1b[J");
        }
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }
        drawn = height;
        // 一次性把这一帧写出去（不然 25fps 下 stdout 缓冲会攒成幻灯片）。
        emit(&*out);
        out.clear();
        std::thread::sleep(Duration::from_millis(40));
    }
    let mut tail = String::new();
    let _ = writeln!(tail, "\n（预览结束 —— 真机里这条只在聊天流里那一行动）");
    emit(&tail);
}

/// 一次性把一段字节写到 stdout。
fn emit(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// `--flag value`。
fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
