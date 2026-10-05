//! `theme_preview` — 配色画廊：聊天里所有 cell 用当前 palette 铺一条假 transcript。
//!
//! 调色板 / 层级时用它，不用起网关、不用开 TUI：
//!
//! ```text
//! # 默认：读真实配置（$WING_HOME/tui/config.yaml），ANSI 输出到终端
//! cargo run -p wing --example theme_preview
//!
//! # 换预设对比：wing（设计版，默认）/ terminal（跟随终端 ANSI 色）
//! cargo run -p wing --example theme_preview -- --preset terminal
//!
//! # 导出 HTML（浏览器里对照 / 截图贴 issue）
//! cargo run -p wing --example theme_preview -- --html /tmp/theme.html
//!
//! # 不带颜色（对文案 / 贴纯文本）
//! cargo run -p wing --example theme_preview -- --plain
//! ```
//!
//! 纯预设（不含个人覆盖）可以把 `WING_HOME` 指到空目录再跑：
//! `WING_HOME=/tmp/empty-wing cargo run -p wing --example theme_preview`。
//!
//! 画的就是 TUI 里那一份：cell 全部走真渲染（`ChatCell` / `ToolCallBlock` /
//! `ThinkingBlock` / `DiffView` / `AskMessage` / 状态栏），palette 走
//! `AppConfig::load()` → `ThemePalette::from_config` —— 不会出现"预览好看、
//! 真机不一样"。唯一的近似在用户消息卡：卡的几何（铺底 / 内衬）在 viewport
//! 层，这里按同样的做法补上，文本行仍出自真 cell。`cargo test` 会编译这个
//! example（不运行），所以它不会因为没人跑而腐掉。

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use wing::config::AppConfig;
use wing::config::ColorPreset;
use wing::config::LayoutConfig;
use wing::config::ThemePalette;
use wing::config::rendering::ThinkingMode;
use wing::protocol::AskOption;
use wing::protocol::AskQuestion;
use wing::render::markdown::ImageOpts;
use wing::render::renderable::CellContext;
use wing::shared::panels::ask::AskPanel;
use wing::ui::cells::ask_msg::AskMessage;
use wing::ui::cells::diff_view::DiffView;
use wing::ui::cells::thinking::ThinkingBlock;
use wing::ui::cells::todo_msg::TodoItem;
use wing::ui::cells::todo_msg::TodoMessage;
use wing::ui::cells::tool_call::ToolCallBlock;
use wing::ui::chat_view::ChatCell;
use wing::ui::shimmer::to_rgb;
use wing::ui::status_bar::StatusBar;
use wing::ui::status_bar::StatusData;

// ── 组装：一条假 transcript，全部走真 cell ──────────────────────

fn build(config: &AppConfig, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
    let layout = LayoutConfig::default();
    let images = ImageOpts::off();
    let ctx = CellContext {
        palette,
        thinking_mode: ThinkingMode::Hidden,
        thinking_expanded: None,
        layout: &layout,
        images,
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // 槽位表：名字 × 覆盖值 × 解析后的颜色 —— 抬头与图例共用一份，别再手抄两遍。
    let slots: [(&str, &Option<String>, Color); 14] = [
        ("accent", &config.colors.accent, palette.accent),
        ("text", &config.colors.text, palette.text),
        ("thinking", &config.colors.thinking, palette.thinking),
        (
            "tool_result",
            &config.colors.tool_result,
            palette.tool_result,
        ),
        ("dim", &config.colors.dim, palette.dim),
        ("success", &config.colors.success, palette.success),
        ("warning", &config.colors.warning, palette.warning),
        ("danger", &config.colors.danger, palette.danger),
        ("math", &config.colors.math, palette.math),
        ("surface", &config.colors.surface, palette.surface),
        (
            "diff_add_bg",
            &config.colors.diff_add_bg,
            palette.diff_add_bg,
        ),
        (
            "diff_del_bg",
            &config.colors.diff_del_bg,
            palette.diff_del_bg,
        ),
        (
            "diff_add_bg_strong",
            &config.colors.diff_add_bg_strong,
            palette.diff_add_bg_strong,
        ),
        (
            "diff_del_bg_strong",
            &config.colors.diff_del_bg_strong,
            palette.diff_del_bg_strong,
        ),
    ];
    let overrides: Vec<&str> = slots
        .iter()
        .filter(|(_, value, _)| value.is_some())
        .map(|(name, _, _)| *name)
        .collect();
    lines.push(Line::from(vec![
        Span::styled(
            "wing theme preview".to_string(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  preset={:?}  width={width}", config.colors.preset),
            Style::default().fg(palette.dim),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        if overrides.is_empty() {
            "config: 无逐槽覆盖（纯预设）".to_string()
        } else {
            format!("config 覆盖: {}", overrides.join(", "))
        },
        Style::default().fg(palette.dim),
    )));
    legend(&mut lines, &slots, palette);

    // ── chrome · 状态栏 ─────────────────────────────────────────
    section(&mut lines, "chrome · 状态栏", width, palette);
    lines.extend(status_bar_lines(width, palette));

    // ── 用户消息：已提交 / pending / discarded ───────────────────
    section(&mut lines, "user · 三种状态", width, palette);
    let cards = [
        (
            "已提交",
            ChatCell::UserMessage("把模型输出里的 diff 底色统一一下。".into()),
        ),
        (
            "pending（已发出、模型还没取）",
            ChatCell::PendingUserMessage("顺便看看 200 行以外的那个 tab。".into()),
        ),
        (
            "discarded（被中断丢弃）",
            ChatCell::DiscardedUserMessage("这条没送出去。".into()),
        ),
    ];
    for (caption, cell) in cards {
        label(&mut lines, caption, palette);
        user_card(&mut lines, &cell, &ctx, width, palette);
    }

    // ── 助手 · markdown 全要素 ──────────────────────────────────
    section(&mut lines, "assistant · markdown", width, palette);
    lines.extend(ChatCell::AssistantMessage(SAMPLE_MD.into()).to_lines(width, &ctx));

    // ── system / warning / error 三种消息 ────────────────────────
    section(
        &mut lines,
        "messages · system / warning / error",
        width,
        palette,
    );
    for cell in [
        ChatCell::SystemMessage("system: 工具集已更新 — 链空冷切换，冻结 declared 视图".into()),
        ChatCell::WarningMessage("notice: provider 限流，6s 后重试".into()),
        ChatCell::ErrorMessage("error: connection closed (1006)".into()),
    ] {
        lines.extend(cell.to_lines(width, &ctx));
    }

    // ── thinking：折叠（进行中刷光 / 定格）/ 展开 ────────────────
    section(
        &mut lines,
        "thinking · 折叠（进行中 · 相位 0.45）",
        width,
        palette,
    );
    let now = Instant::now();
    let mut active = ThinkingBlock::new();
    active.append("正文只在展开时可见。");
    active.start(now - Duration::from_secs(4));
    active.tick(now);
    active.set_sweep_phase(0.45);
    lines.extend(active.to_lines(palette, ThinkingMode::Hidden, None, width, images));

    label(&mut lines, "折叠 · 已定格", palette);
    let mut done = ThinkingBlock::new();
    done.append("正文不显示。");
    done.start(now - Duration::from_secs(12));
    done.finish(now);
    lines.extend(done.to_lines(palette, ThinkingMode::Hidden, None, width, images));

    section(&mut lines, "thinking · 展开（Ctrl+O）", width, palette);
    let mut expanded = ThinkingBlock::new();
    expanded.append(
        "先确认槽位语义。`dim` 归 chrome —— 边框、gutter、计时都走它，不能再兼作正文的次级色。\n\n",
    );
    expanded.append(
        "灰阶四档：\n\n- text：正文\n- thinking：推理\n- tool_result：数据\n- dim：chrome\n\n",
    );
    expanded.append("warning 往琥珀收，别和 accent 撞。");
    expanded.start(now - Duration::from_secs(12));
    expanded.finish(now);
    lines.extend(expanded.to_lines(palette, ThinkingMode::Hidden, Some(true), width, images));

    // ── 工具调用：各状态 × 各渲染策略 ────────────────────────────
    section(&mut lines, "tool calls", width, palette);
    tools(&mut lines, &ctx, width, now);

    // ── todo / diff / ask ───────────────────────────────────────
    section(&mut lines, "todo", width, palette);
    lines.extend(ChatCell::Todo(sample_todo()).to_lines(width, &ctx));

    section(&mut lines, "diff", width, palette);
    lines.extend(ChatCell::Diff(sample_diff()).to_lines(width, &ctx));

    section(&mut lines, "ask · AskUserQuestion", width, palette);
    lines.extend(ChatCell::Ask(AskMessage::new(sample_panel())).to_lines(width, &ctx));

    // ── 分隔线 ──────────────────────────────────────────────────
    section(&mut lines, "separator", width, palette);
    lines.extend(ChatCell::Separator.to_lines(width, &ctx));

    lines
}

/// 灰阶 / 色相一览：`██ 槽位`，被 config 覆盖的槽位带 `*`。
fn legend(
    lines: &mut Vec<Line<'static>>,
    slots: &[(&str, &Option<String>, Color)],
    p: &ThemePalette,
) {
    for row in slots.chunks(4) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (name, value, color) in row {
            spans.push(Span::styled("██ ", Style::default().fg(*color)));
            let mark = if value.is_some() { '*' } else { ' ' };
            spans.push(Span::styled(
                format!("{name}{mark}  "),
                Style::default().fg(p.dim),
            ));
        }
        lines.push(Line::from(spans));
    }
}

fn section(lines: &mut Vec<Line<'static>>, title: &str, width: u16, p: &ThemePalette) {
    lines.push(Line::from(""));
    let dashes = (width as usize).saturating_sub(UnicodeWidthStr::width(title) + 4);
    lines.push(Line::from(vec![
        Span::styled("── ", Style::default().fg(p.dim)),
        Span::styled(
            title.to_string(),
            Style::default().fg(p.dim).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", "─".repeat(dashes)),
            Style::default().fg(p.dim),
        ),
    ]));
}

fn label(lines: &mut Vec<Line<'static>>, text: &str, p: &ThemePalette) {
    lines.push(Line::from(Span::styled(
        format!("  [{text}]"),
        Style::default().fg(p.dim),
    )));
}

/// 一张用户消息卡：文本行走**真 cell**（正常 / dim / dim+划掉的样式全部来自
/// `ChatCell`），只有卡的几何是这里补的 —— 铺满整行、左 2 内衬、上下空行，
/// 与 `chat_view` viewport 的做法一致（几何在 viewport 层，不在 cell）。
fn user_card(
    lines: &mut Vec<Line<'static>>,
    cell: &ChatCell,
    ctx: &CellContext<'_>,
    width: u16,
    p: &ThemePalette,
) {
    let text_lines = cell.to_lines(width, ctx);
    let style = text_lines
        .first()
        .and_then(|line| line.spans.first())
        .map(|span| span.style)
        .unwrap_or_else(|| Style::default().fg(p.text).bg(p.surface));
    let blank = Line::from(Span::styled(" ".repeat(width as usize), style));
    lines.push(blank.clone());
    for line in text_lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let mut row = format!("  {text}");
        let used = UnicodeWidthStr::width(row.as_str());
        if used < width as usize {
            row.push_str(&" ".repeat(width as usize - used));
        }
        lines.push(Line::from(Span::styled(row, style)));
    }
    lines.push(blank);
}

fn tools(lines: &mut Vec<Line<'static>>, ctx: &CellContext<'_>, width: u16, now: Instant) {
    // Bash：成功 + 计时 + 结果输出。
    let mut ok = ToolCallBlock::new(
        "Bash".into(),
        serde_json::json!({ "command": "cargo test -p wing --lib config::" }),
        "call-bash-ok".into(),
    );
    ok.start_timer(now - Duration::from_secs(3));
    ok.set_result("ok. 24 passed; 0 failed".into(), true);
    lines.extend(ChatCell::ToolCall(ok).to_lines(width, ctx));

    // Read：结果被隐藏（成功只留头行）。
    let mut read = ToolCallBlock::new(
        "Read".into(),
        serde_json::json!({ "path": "crates/wing/src/config/mod.rs" }),
        "call-read".into(),
    );
    read.set_result("(1-88/537)".into(), true);
    lines.extend(ChatCell::ToolCall(read).to_lines(width, ctx));

    // Bash：失败 + 计时 + 截断的错误输出。
    let mut fail = ToolCallBlock::new(
        "Bash".into(),
        serde_json::json!({ "command": "cargo clippy -- -D warnings" }),
        "call-bash-fail".into(),
    );
    fail.start_timer(now - Duration::from_secs(9));
    fail.set_result(
        "error: unused variable `args`\n  --> src/theme.rs:12:9".into(),
        false,
    );
    lines.extend(ChatCell::ToolCall(fail).to_lines(width, ctx));

    // Bash：参数还在流式下发（◌，args 局部 JSON）。
    let mut stream = ToolCallBlock::new_streaming("Bash".into(), "call-bash-stream".into());
    stream.append_args_fragment(
        r#"{"command": "cargo run --example theme_preview -- --html /tmp/wing"#,
    );
    stream.flush_pending_args();
    lines.extend(ChatCell::ToolCall(stream).to_lines(width, ctx));

    // Write：内容流式预览（语法高亮 + 行首缩进 gutter）。
    let mut write = ToolCallBlock::new_streaming("Write".into(), "call-write".into());
    write.append_args_fragment(
        r#"{"path": "crates/wing/src/ui/shimmer.rs", "content": "/// 两个颜色按 t 线性混合。\npub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {\n    let t = t.clamp(0.0, 1.0);\n    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))\n}\n"}"#,
    );
    write.flush_pending_args();
    lines.extend(ChatCell::ToolCall(write).to_lines(width, ctx));

    // Edit：实时 diff 预览（旧行删除底、新行新增底、语法高亮）。
    let mut edit = ToolCallBlock::new_streaming("Edit".into(), "call-edit".into());
    edit.append_args_fragment(
        r#"{"path": "crates/wing/src/ui/cells/tool_call.rs", "old_string": "let bold = Style::default().add_modifier(Modifier::BOLD);\nlet dim = Style::default().fg(palette.dim);", "new_string": "let name_style = Style::default().fg(palette.text).add_modifier(Modifier::BOLD);\nlet args_style = Style::default().fg(palette.dim);"}"#,
    );
    edit.flush_pending_args();
    lines.extend(ChatCell::ToolCall(edit).to_lines(width, ctx));
}

fn sample_todo() -> TodoMessage {
    TodoMessage::new(vec![
        TodoItem {
            content: "收敛灰阶 ramp".into(),
            status: "completed".into(),
            active_form: None,
        },
        TodoItem {
            content: "tool 行三级层级".into(),
            status: "in_progress".into(),
            active_form: Some("拆 tool 行层级".into()),
        },
        TodoItem {
            content: "gallery 自检".into(),
            status: "pending".into(),
            active_form: None,
        },
    ])
}

fn sample_diff() -> DiffView {
    DiffView::new(
        "crates/wing/src/ui/cells/tool_call.rs".into(),
        Some(
            "let status = self.status;\nlet bold = Style::default().add_modifier(Modifier::BOLD);\nlet dim = Style::default().fg(palette.dim);\nlet args_part = renderer.header_args(&self.tool_args);"
                .into(),
        ),
        "let status = self.status;\nlet name_style = Style::default().fg(palette.text).add_modifier(Modifier::BOLD);\nlet args_style = Style::default().fg(palette.dim);\nlet args_part = renderer.header_args(&self.tool_args);"
            .into(),
        560,
        560,
    )
}

fn sample_panel() -> AskPanel {
    AskPanel::new(
        "ask-1".into(),
        vec![AskQuestion {
            id: "palette-1".into(),
            header: "配色".into(),
            question: "warning 往琥珀色收（和品牌喙色一致），可以吗？".into(),
            multi_select: false,
            options: vec![
                AskOption {
                    label: "可以".into(),
                    description: "琥珀更暖，也不和 accent 撞".into(),
                },
                AskOption {
                    label: "再想想".into(),
                    description: "先保持纯黄，之后再定".into(),
                },
            ],
            choices: vec![],
        }],
    )
}

/// 状态栏走真 widget（Buffer 渲染），再还原成一行 span。
fn status_bar_lines(width: u16, p: &ThemePalette) -> Vec<Line<'static>> {
    let data = StatusData {
        model: "deepseek-v4-flash-0731".into(),
        model_display_name: Some("DeepSeek-Flash".into()),
        provider: Some("dashscope".into()),
        total_tokens: 45_200,
        context_window_tokens: 200_000,
        thinking: true,
        reasoning_effort: Some("medium".into()),
        session_prompt_tokens: 132_400,
        session_completion_tokens: 24_800,
        session_cached_tokens: 98_100,
        connected: true,
        ..StatusData::default()
    };
    let area = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(area);
    StatusBar::new(&data, true, p).render(area, &mut buf);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run: Option<(Style, String)> = None;
    for x in 0..width {
        let cell = &buf[(x, 0)];
        let style = Style::default()
            .fg(cell.fg)
            .bg(cell.bg)
            .add_modifier(cell.modifier);
        match &mut run {
            Some((current, text)) if *current == style => text.push_str(cell.symbol()),
            Some(_) => {
                let (previous, text) = run.take().expect("run 在手");
                spans.push(Span::styled(text, previous));
                run = Some((style, cell.symbol().to_string()));
            }
            None => run = Some((style, cell.symbol().to_string())),
        }
    }
    if let Some((style, text)) = run {
        spans.push(Span::styled(text, style));
    }
    vec![Line::from(spans)]
}

/// markdown 全要素样例：标题层级 / 加粗斜体 / 行内代码 / 链接 / 列表 /
/// 引用 / 代码块 / 表格 / 公式。
const SAMPLE_MD: &str = r#"# 调色板自检

正文一句话：**加粗**、*斜体*、`行内代码` 和 [一个链接](https://example.com/wing)。行内代码与链接同色 —— 一个色相一个含义。

## 灰阶

- 四档：`text` / `thinking` / `tool_result` / `dim`
  - 嵌套一级，看 marker 与正文的层级
- 状态色只做语义：success / warning / danger

> 引用块：看边框与正文的安静程度。

### 代码

```rust
let p = ThemePalette::from_config(&config.colors);
let quiet = p.dim; // chrome 永远比内容安静
```

| 槽位 | 岗位 |
|------|------|
| text | 正文 |
| dim | chrome |

行内公式 $E = mc^2$，块级公式：

$$
\int_0^1 x^2\,dx = \frac{1}{3}
$$
"#;

// ── 输出：ANSI 到终端 / HTML 到文件 ─────────────────────────────

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

fn ansi(lines: &[Line<'_>], plain: bool) -> String {
    let mut out = String::new();
    for line in lines {
        if plain {
            for span in &line.spans {
                out.push_str(&span.content);
            }
            out.push('\n');
            continue;
        }
        for span in &line.spans {
            out.push_str("\x1b[0m");
            let m = span.style.add_modifier;
            if m.contains(Modifier::BOLD) {
                out.push_str("\x1b[1m");
            }
            if m.contains(Modifier::DIM) {
                out.push_str("\x1b[2m");
            }
            if m.contains(Modifier::ITALIC) {
                out.push_str("\x1b[3m");
            }
            if m.contains(Modifier::UNDERLINED) {
                out.push_str("\x1b[4m");
            }
            if m.contains(Modifier::REVERSED) {
                out.push_str("\x1b[7m");
            }
            if m.contains(Modifier::CROSSED_OUT) {
                out.push_str("\x1b[9m");
            }
            if let Some(code) = sgr(span.style.fg, true) {
                out.push_str(&code);
            }
            if let Some(code) = sgr(span.style.bg, false) {
                out.push_str(&code);
            }
            out.push_str(&span.content);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

fn sgr(color: Option<Color>, foreground: bool) -> Option<String> {
    let layer = if foreground { 38 } else { 48 };
    match color? {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(format!("\x1b[{layer};2;{r};{g};{b}m")),
        // 256 色索引原样下发；命名色查 16 色表。
        Color::Indexed(i) => Some(format!("\x1b[{layer};5;{i}m")),
        other => Some(format!("\x1b[{layer};5;{}m", ansi_index(other))),
    }
}

fn html(lines: &[Line<'_>], meta: &str) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n");
    let _ = writeln!(out, "<title>{}</title>", escape(meta));
    out.push_str("<style>\n");
    out.push_str("html,body{margin:0;padding:0}\n");
    out.push_str("body{background:#0b0f16;color:#c9ced9;padding:26px 30px}\n");
    out.push_str(
        "pre{margin:0;font:13px/1.35 Menlo,\"SF Mono\",\"DejaVu Sans Mono\",Consolas,monospace;white-space:pre}\n",
    );
    out.push_str("</style></head><body>\n");
    out.push_str("<pre>\n");
    for line in lines {
        for span in &line.spans {
            let css = span_css(&span.style);
            let text = escape(&span.content);
            if css.is_empty() {
                out.push_str(&text);
            } else {
                let _ = write!(out, "<span style=\"{css}\">{text}</span>");
            }
        }
        out.push('\n');
    }
    out.push_str("</pre>\n</body></html>\n");
    out
}

fn span_css(style: &Style) -> String {
    let mut decls: Vec<String> = Vec::new();
    let reversed = style.add_modifier.contains(Modifier::REVERSED);
    let fg = style.fg.filter(|c| *c != Color::Reset);
    let bg = style.bg.filter(|c| *c != Color::Reset);
    let (fg, bg) = if reversed { (bg, fg) } else { (fg, bg) };
    if let Some(c) = fg {
        decls.push(format!("color:{}", hex(c)));
    }
    if let Some(c) = bg {
        decls.push(format!("background-color:{}", hex(c)));
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        decls.push("font-weight:700".into());
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        decls.push("font-style:italic".into());
    }
    let mut deco: Vec<&str> = Vec::new();
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        deco.push("underline");
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        deco.push("line-through");
    }
    if !deco.is_empty() {
        decls.push(format!("text-decoration:{}", deco.join(" ")));
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

// ── CLI ─────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let plain = args.iter().any(|a| a == "--plain");
    let width = match value(&args, "--width") {
        None => 104,
        Some(v) => v
            .parse::<u16>()
            .ok()
            .filter(|w| *w >= 1)
            .unwrap_or_else(|| fail(&format!("--width expects a positive integer, got '{v}'"))),
    };
    let html_path = value(&args, "--html");
    let preset_override = value(&args, "--preset").map(|v| v.to_ascii_lowercase());

    let mut config = AppConfig::load();
    match preset_override.as_deref() {
        None => {}
        Some("wing") => config.colors.preset = ColorPreset::Wing,
        Some("terminal") => config.colors.preset = ColorPreset::Terminal,
        Some(other) => fail(&format!("unknown preset '{other}' (wing | terminal)")),
    }
    config.resolve();
    let palette = ThemePalette::from_config(&config.colors);
    let lines = build(&config, &palette, width);

    match html_path {
        Some(path) => {
            let meta = format!(
                "wing theme preview · preset={:?} · width={width}",
                config.colors.preset
            );
            let document = html(&lines, &meta);
            if let Err(e) = std::fs::write(&path, document) {
                fail(&format!("cannot write {path}: {e}"));
            }
        }
        None => emit(&ansi(&lines, plain)),
    }
}

/// `--flag value`。缺值、或下一个 token 是另一个 flag → 明确报错（以前会把
/// `--plain` 当成 `--html` 的值，写出一个名叫 `--plain` 的文件）。
fn value(args: &[String], flag: &str) -> Option<String> {
    let index = args.iter().position(|a| a == flag)?;
    match args.get(index + 1) {
        Some(v) if !v.starts_with("--") => Some(v.clone()),
        Some(v) => fail(&format!("{flag} expects a value, got the flag '{v}'")),
        None => fail(&format!("{flag} expects a value, none was given")),
    }
}

fn emit(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn fail(message: &str) -> ! {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "theme_preview: {message}");
    std::process::exit(2);
}
