//! `welcome_preview` — 把欢迎屏按指定宽度 / 扫光进度打印出来。
//!
//! 调字形、排版、渐变时用它，不用起网关、不用开 TUI：
//!
//! ```text
//! # 默认画廊：几档宽度 × 几个扫光时刻
//! cargo run -p wing --example welcome_preview
//!
//! # 单张：120 列、扫光走到 40%
//! cargo run -p wing --example welcome_preview -- --width 120 --phase 0.4
//!
//! # 看窄屏降级：40 / 20 列
//! cargo run -p wing --example welcome_preview -- --width 40
//!
//! # 不带颜色（贴 issue / 对比字形）
//! cargo run -p wing --example welcome_preview -- --plain
//!
//! # 会话事实行的两种样子：`2 skills · 1 rule`（默认）/ 什么都没加载
//! cargo run -p wing --example welcome_preview -- --facts 2,1
//! cargo run -p wing --example welcome_preview -- --facts 0,0
//! ```
//!
//! 画的就是 TUI 里那一份：与 `App::sync_welcome` 同一个 `Welcome::build`，
//! 连样式都原样带出来，不会出现"预览好看、真机不一样"。
//!
//! `cargo test` 会**编译** example（只是不运行），所以它不会因为没人跑而腐掉。

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;
use wing::config::ThemePalette;
use wing::ui::welcome::SWEEP_MS;
use wing::ui::welcome::SessionFacts;
use wing::ui::welcome::Welcome;

/// 前景 / 背景的 SGR 参数（`38;` / `48;` 之后接的那一段）。
fn sgr(color: Option<Color>) -> Option<String> {
    match color? {
        // 38;2;r;g;b / 48;2;r;g;b —— 真彩。
        Color::Rgb(r, g, b) => Some(format!("2;{r};{g};{b}")),
        // 默认色：不设色彩，让终端用它的默认前景 / 背景。
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

/// 一行 ANSI 编码：按 span 输出（真终端就是这么画的），行首补 1 格边距，
/// 超过 `width` 的部分截掉 —— 超宽在预览里就该看出来。
fn render_line(line: &Line<'_>, width: u16, plain: bool) -> String {
    let mut out = String::new();
    let mut used = 1usize; // 左边距
    let mut open = false;
    for span in &line.spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = ch.to_string().width();
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
            // 无色预览里 `▀` + 背景色 = 实心格：不还原成 `█` 的话，画出来的
            // 字形会整片瘦一半，没法拿来对形状。
            if span.style.bg.is_some() {
                text = text.replace('▀', "█");
            }
            out.push_str(&text);
            continue;
        }
        let style = span.style;
        out.push_str("\x1b[0m");
        if style.add_modifier.contains(Modifier::BOLD) {
            out.push_str("\x1b[1m");
        }
        if let Some(code) = sgr(Some(style.fg.unwrap_or(Color::Reset))) {
            out.push_str(&format!("\x1b[38;{code}m"));
        }
        if let Some(code) = sgr(Some(style.bg.unwrap_or(Color::Reset))) {
            out.push_str(&format!("\x1b[48;{code}m"));
        }
        out.push_str(&text);
        open = true;
    }
    if open && !plain {
        out.push_str("\x1b[0m");
    }
    out.insert(0, ' ');
    out
}

/// 画一块：宽度 `width`，扫光进度 `phase`（`None` = 已定格）。
///
/// `working` = agent 在干活（海鸥切飞行扇翅）；`ms` = 开屏后多久（落在哪个
/// 动作帧上，方便把眨眼 / 抖翅 / 跳单独打出来看）；`facts` = 名牌右列会话事实
/// 行（`None` = 这个会话什么都没加载 / 还没同步到）。
#[allow(clippy::too_many_arguments)] // 预览的参数就是真 `build` 的参数 + 打印开关
fn draw(
    width: u16,
    phase: Option<f32>,
    working: bool,
    ms: u64,
    plain: bool,
    facts: Option<SessionFacts>,
    out: &mut String,
) {
    let palette = ThemePalette::default();
    let started = Instant::now();
    let mut welcome = Welcome::new(2, started);
    // 不给 `--phase` 就是"已定格"：此刻直接按 `--ms` 取帧（早先这里固定钳到
    // SWEEP_MS + 1，于是 2400ms 之前的帧根本打不出来，标签还显示原值）。
    let at = match phase {
        Some(p) => ((p * SWEEP_MS as f32) as u64).max(ms),
        None => ms,
    };
    let now = started + Duration::from_millis(at);
    let lines = welcome.build(&palette, width, now, working, true, facts);

    let label = match phase {
        Some(p) => format!("width={width} sweep={:.0}%", p * 100.0),
        None => format!("width={width} settled @{ms}ms"),
    };
    let label = if working {
        format!("{label} working")
    } else {
        label
    };
    let dashes = "─".repeat((width as usize).saturating_sub(label.len() + 4));
    let _ = writeln!(out, "── {label} {dashes}");
    for line in &lines {
        out.push_str(&render_line(line, width, plain));
        out.push('\n');
    }
    out.push('\n');
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1).cloned()
}

/// `--facts 2,1` —— 会话事实（skills,rules）。没给、或给了但解析不出来（这是
/// 调试入口，不为参数报错）→ 默认 `2,1`；`0,0` 是"什么都没加载"：名牌上那一行
/// 是空白占位（槽位恒在）。
fn facts_argument() -> Option<SessionFacts> {
    const DEFAULT: Option<SessionFacts> = Some(SessionFacts {
        skills: 2,
        rules: 1,
    });
    let Some(raw) = argument("--facts") else {
        return DEFAULT;
    };
    let parsed = raw.split_once(',').and_then(|(skills, rules)| {
        Some(SessionFacts {
            skills: skills.trim().parse().ok()?,
            rules: rules.trim().parse().ok()?,
        })
    });
    parsed.or(DEFAULT)
}

fn main() {
    let plain = std::env::args().any(|a| a == "--plain");
    let working = std::env::args().any(|a| a == "--working");
    let width = argument("--width").and_then(|v| v.parse::<u16>().ok());
    let phase = argument("--phase").and_then(|v| v.parse::<f32>().ok());
    let ms = argument("--ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let facts = facts_argument();

    let mut out = String::new();
    match width {
        Some(width) => draw(width, phase, working, ms, plain, facts, &mut out),
        None => {
            for width in [120u16, 96, 80, 70, 60, 40, 24] {
                draw(width, None, false, 0, plain, facts, &mut out);
            }
            for phase in [0.0f32, 0.25, 0.5, 0.75] {
                draw(96, Some(phase), false, 0, plain, facts, &mut out);
            }
            // 两个姿态各来一张：待机标准姿势 / 干活飞行。
            draw(96, None, false, 0, plain, facts, &mut out);
            draw(96, None, true, 0, plain, facts, &mut out);
            // 事实行缺席的样子（0/0 或老网关）。
            draw(96, None, false, 0, plain, None, &mut out);
        }
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}
