//! `TestBackend` 帧测试：整套 overlay 的屏幕断言。
//!
//! 三条铁律（任务书 §Verification）：
//!
//! ① **不溢出**：40/60/80/100/110/140/200 列 × 10/24/50 行全覆盖，且渲染进「内缩区域」时
//!    区域外一格不许被写（用 `X` 哨兵钉住）；
//! ② **密文红线**：编辑密钥时屏幕缓冲里不含明文子串；
//! ③ 帧断言写**具体屏幕内容**，不只是「不 panic」。

use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::config::ThemePalette;
use crate::shared::panels::settings::Focus;
use crate::shared::panels::settings::SettingsAction;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;
use wing_api_client::models::SettingNode;

use super::SettingsCatalogs;
use super::SettingsOverlay;
use super::anchors_viewport_rows;
use super::has_anchor_column;
use super::test_support as fx;
use super::tree_viewport_rows;

// ── 夹具 ────────────────────────────────────────────────────

/// 一次渲染需要的三件套（catalog 必须与面板构造时同源）。
struct Harness {
    gateway: SettingNode,
    interface: Option<SettingNode>,
    panel: SettingsPanel,
}

impl Harness {
    fn new() -> Self {
        Self::with_interface(false)
    }

    fn with_interface(interface: bool) -> Self {
        let gateway = fx::sample_catalog();
        let interface = interface.then(fx::interface_catalog);
        let panel = SettingsPanel::new(
            &fx::schema(&gateway),
            fx::state(fx::sample_values()),
            interface.clone().map(fx::interface_source),
            View::Tree,
        );
        Self {
            gateway,
            interface,
            panel,
        }
    }

    fn catalogs(&self) -> SettingsCatalogs<'_> {
        SettingsCatalogs::new(&self.gateway, self.interface.as_ref())
    }

    /// 渲染一整屏（区域内缩 0，`TestBackend` 的尺寸就是 overlay 的尺寸）。
    fn frame(&self, width: u16, height: u16) -> String {
        render(&self.panel, self.catalogs(), width, height)
    }

    /// 只画一屏的 `Buffer`（按行号定位断言用）。
    fn buffer(&self, width: u16, height: u16) -> Buffer {
        render_buffer(&self.panel, self.catalogs(), width, height)
    }
}

fn palette() -> ThemePalette {
    ThemePalette::default()
}

fn render(
    panel: &SettingsPanel,
    catalogs: SettingsCatalogs<'_>,
    width: u16,
    height: u16,
) -> String {
    let buf = render_buffer(panel, catalogs, width, height);
    let mut out = String::new();
    for y in 0..buf.area.height {
        out.push_str(&row_text(&buf, y));
        out.push('\n');
    }
    out
}

fn render_buffer(
    panel: &SettingsPanel,
    catalogs: SettingsCatalogs<'_>,
    width: u16,
    height: u16,
) -> Buffer {
    let palette = palette();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            SettingsOverlay::new(panel, catalogs, &palette).render(area, frame.buffer_mut());
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// 取某一行的文本（去掉尾随空格）。
///
/// 双宽字符（CJK）在 ratatui 的缓冲里占两格：第二个格子是它 reset 出来的空格，
/// 直接逐格拼会得到「每 个 汉 字 之 间 一 个 空 格」的假象 —— 这里按显示宽度跳过占位格。
fn row_text(buf: &Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut x = 0u16;
    while x < buf.area.width {
        let symbol = buf[(x, y)].symbol();
        out.push_str(symbol);
        let width = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
        x += width;
    }
    out.trim_end().to_string()
}

/// 取某一行的行内文本（去掉左右两条边框）。
fn inner_text(buf: &Buffer, y: u16) -> String {
    let row = row_text(buf, y);
    let chars: Vec<char> = row.chars().collect();
    if chars.len() >= 2 {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        row
    }
}

/// 找到含 `needle` 的行号。
fn find_row(buf: &Buffer, needle: &str) -> Option<u16> {
    (0..buf.area.height).find(|&y| row_text(buf, y).contains(needle))
}

// ── 宽屏 / 窄屏完整帧 ────────────────────────────────────────

#[test]
fn wide_frame_has_title_subheader_two_columns_and_hints() {
    let mut harness = Harness::with_interface(true);
    // 让详情栏有内容：光标落到 timeout_first_chunk 上（搜索会顺带把焦点放进右栏）。
    fx::goto(&mut harness.panel, "timeout_first_chunk");
    let buf = harness.buffer(120, 24);
    let text = render(&harness.panel, harness.catalogs(), 120, 24);

    // 标题栏：⚙ Settings + 配置路径 + valid（搜索态不再顶掉它）。
    let title = row_text(&buf, 0);
    assert!(title.starts_with("┌─ ⚙ Settings"), "{title:?}");
    assert!(
        title.contains("/home/u/.wing/core/config.yaml"),
        "{title:?}"
    );
    assert!(title.contains("valid"), "{title:?}");
    assert!(title.ends_with('┐'), "{title:?}");

    // 标题下面那一行是搜索位的常驻占位。
    assert!(
        row_text(&buf, 1).contains("/ 搜索…"),
        "{}",
        row_text(&buf, 1)
    );

    // 左栏：四个锚点（Gateway 的三组 + Interface）。
    for anchor in ["Providers", "Net", "Misc", "Interface"] {
        let row = find_row(&buf, anchor).unwrap_or_else(|| panic!("锚点 {anchor}：{text}"));
        assert!(row_text(&buf, row).contains(anchor), "{anchor}");
    }
    // 左栏与右栏之间有一条竖分隔；右栏够宽时再竖切出详情栏（共两条）。
    // 取组头**下面**一行：组头横跨整个右栏（那里只看得见左栏的分隔）。
    let body_row = find_row(&buf, "providers").expect("providers 行") + 1;
    let dividers: Vec<u16> = (2..118)
        .filter(|x| buf[(*x, body_row)].symbol() == "│")
        .collect();
    assert_eq!(dividers.len(), 2, "左栏分隔 + 详情栏分隔：{dividers:?}");
    assert!(text.contains("流式首块超时"), "详情栏的 doc：{text}");
    assert!(text.contains("路径"), "详情栏的路径段：{text}");

    // 分隔线：左交点 + 左栏分隔处的 ┴ + 右交点。
    let separator_y = 24 - 1 - 1 - 1; // 下边框 + 键位栏 1 行 + 分隔线
    assert_eq!(buf[(0, separator_y)].symbol(), "├", "分隔线左交点");
    assert_eq!(
        buf[(dividers[0], separator_y)].symbol(),
        "┴",
        "分隔线在左栏分隔处断开"
    );
    assert_eq!(buf[(119, separator_y)].symbol(), "┤", "分隔线右交点");

    // 键位栏（07 的上下文提示）。
    assert!(text.contains("Esc 回左栏"), "右栏焦点的 Esc 语义：{text}");
    assert!(text.contains("Tab/←→ 切栏"), "{text}");
    assert_eq!(buf[(0, 23)].symbol(), "└", "下边框");
    assert_eq!(buf[(119, 23)].symbol(), "┘");
}

#[test]
fn narrow_frame_drops_the_detail_pane_and_keeps_the_hint_band() {
    let mut harness = Harness::with_interface(true);
    fx::goto(&mut harness.panel, "timeout_first_chunk");
    let buf = harness.buffer(80, 24);
    let text = render(&harness.panel, harness.catalogs(), 80, 24);

    // 左栏照旧（锚点是 v2 的导航，不分宽窄），但**只有一条**竖分隔：详情栏退场。
    let body_row = find_row(&buf, "Providers").expect("Providers 行");
    let dividers: Vec<u16> = (2..78)
        .filter(|x| buf[(*x, body_row)].symbol() == "│")
        .collect();
    assert_eq!(dividers.len(), 1, "只有左栏分隔：{dividers:?}");
    assert!(
        !text.contains(
            "流式首块超时（秒）
"
        ),
        "详情栏的整段 doc 不该出现：{text}"
    );
    // doc 提示行接替详情栏（右栏最后一行）。
    assert!(text.contains("流式首块超时"), "doc 提示行：{text}");
    assert!(text.contains("Esc"), "{text}");
}

// ── 标记 / 值形态 ────────────────────────────────────────────

#[test]
fn dirty_marker_marks_the_row() {
    let mut harness = Harness::new();
    fx::goto(&mut harness.panel, "gateway.auth.enabled");
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Char(' '));
    assert!(harness.panel.dirty_count() > 0, "切换 bool 即脏");
    let buf = harness.buffer(100, 24);
    let text = render(&harness.panel, harness.catalogs(), 100, 24);
    assert!(text.contains("●"), "脏标记：{text}");
    let row = find_row(&buf, "enabled").expect("enabled 行");
    let line = row_text(&buf, row);
    assert!(
        line.contains('●') && line.contains("enabled"),
        "脏标记在该行：{line:?}"
    );
    // 标题栏出现未保存计数。
    assert!(
        row_text(&buf, 0).contains("unsaved"),
        "{}",
        row_text(&buf, 0)
    );
}

#[test]
fn problem_rows_render_the_bang_marker() {
    let gateway = fx::sample_catalog();
    let mut state = fx::state(fx::sample_values());
    state.problems = vec![wing_api_client::models::SettingProblem {
        path: Some("gateway.port".into()),
        kind: "invalid_value".into(),
        message: "端口非法".into(),
        hint: None,
    }];
    let mut panel = SettingsPanel::new(&fx::schema(&gateway), state, None, View::Tree);
    fx::goto(&mut panel, "gateway.port");
    let buf = render_buffer(&panel, SettingsCatalogs::new(&gateway, None), 100, 24);
    let row = find_row(&buf, "port").expect("port 行");
    assert!(
        inner_text(&buf, row).trim_end().ends_with('!'),
        "problem 标记：{:?}",
        row_text(&buf, row)
    );
    // 标题栏报问题数。
    assert!(
        row_text(&buf, 0).contains("1 problems"),
        "{}",
        row_text(&buf, 0)
    );
}

#[test]
fn secret_value_renders_bullets_and_the_hint() {
    let mut harness = Harness::new();
    fx::goto(&mut harness.panel, "api_key");
    let buf = harness.buffer(100, 30);
    let text = harness.frame(100, 30);
    assert!(text.contains("••••••••"), "{text}");
    assert!(text.contains("•••••••• ab12"), "hint 是末 4 位：{text}");
    let row = find_row(&buf, "api_key").expect("api_key 行");
    let line = row_text(&buf, row);
    assert!(line.contains("•••••••• ab12"), "{line:?}");
    assert!(!line.contains("sk-"), "{line:?}");
}

#[test]
fn default_and_inherited_value_texts_differ() {
    // `providers[0].timeout_first_chunk` 缺席 → 展示声明默认值。
    let mut harness = Harness::new();
    let values = serde_json::json!({
        "providers": [{
            "name": "default",
            "protocol": "openai",
            "base_url": "https://api.example.com",
            "api_key": null,
            "models": ["ds-flash"]
        }]
    });
    let mut get = fx::state(values);
    // 没有密文表 → 密文 `null` 走 `Inherited`（保留磁盘原值）。
    get.secrets.clear();
    harness.panel = SettingsPanel::new(&fx::schema(&harness.gateway), get, None, View::Tree);
    fx::goto(&mut harness.panel, "timeout_first_chunk");
    let text = render(&harness.panel, harness.catalogs(), 110, 30);
    let buf = harness.buffer(110, 30);
    let row = find_row(&buf, "(default)").expect("默认值行");
    let line = row_text(&buf, row);
    assert!(
        line.contains("300 (default)") || line.contains("300.0 (default)"),
        "缺席 → 声明默认值：{line:?}"
    );
    assert!(text.contains("(unchanged)"), "密文保留原值：{text}");
}

#[test]
fn color_swatch_marks_a_color_value_in_the_interface_root() {
    let mut harness = Harness::with_interface(true);
    fx::goto(&mut harness.panel, "colors.accent");
    let buf = harness.buffer(120, 24);
    let row = find_row(&buf, "■ cyan").expect("accent 行");
    let line = row_text(&buf, row);
    assert!(line.contains("■ cyan"), "色块 + 值：{line:?}");
    // 色块的着色就是 cyan（不是 palette 的 accent）。
    let cell = (0..buf.area.width)
        .find(|&x| buf[(x, row)].symbol() == "■")
        .expect("色块格子");
    assert_eq!(
        buf[(cell, row)].style().fg,
        Some(ratatui::style::Color::Cyan),
        "色块用该颜色着色"
    );
}

// ── 编辑器 ──────────────────────────────────────────────────

#[test]
fn editor_row_shows_the_cursor_block_and_the_error_line() {
    let mut harness = Harness::new();
    fx::goto(&mut harness.panel, "gateway.port");
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Enter);
    let buf = harness.buffer(100, 24);
    let row = find_row(&buf, "port").expect("port 行");
    assert!(
        row_text(&buf, row).contains("[32523"),
        "{:?}",
        row_text(&buf, row)
    );
    // 光标块：反显格子（样式断言，符号仍是空格）。
    let cursor_cell = (0..buf.area.width)
        .find(|&x| {
            buf[(x, row)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        })
        .expect("反显光标块");
    assert_eq!(buf[(cursor_cell, row)].symbol(), " ");

    // 提交非法值 → 下一行出现 `↳ 需要一个整数`。
    fx::press_with(
        &mut harness.panel,
        crossterm::event::KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    fx::type_text(&mut harness.panel, "abc");
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Enter);
    let text = render(&harness.panel, harness.catalogs(), 100, 24);
    assert!(text.contains("↳ 需要一个整数"), "{text}");
    // 错误行占的是「编辑器行的下一行」，所以编辑器行仍在窗口里、光标也还在。
    let buf = harness.buffer(100, 24);
    assert!(find_row(&buf, "↳ 需要一个整数").is_some());
    assert!(find_row(&buf, "[abc").is_some(), "编辑器行还在");
}

/// 红线：编辑密钥时，屏幕上不许出现明文密钥（文档里真的存了明文）。
#[test]
fn red_line_secret_edit_never_paints_plaintext() {
    let gateway = fx::sample_catalog();
    let values = serde_json::json!({
        "providers": [{
            "name": "default",
            "protocol": "openai",
            "base_url": "https://api.example.com",
            "api_key": "sk-super-secret-value-1234",
            "models": ["ds-flash"]
        }],
        "tools": ["Bash"]
    });
    let mut panel = SettingsPanel::new(&fx::schema(&gateway), fx::state(values), None, View::Tree);

    // 进入密文编辑态并输入新密钥。
    fx::open_editor(&mut panel, "api_key");
    let state = panel.edit_state().expect("编辑器开着");
    assert!(state.is_secret(), "这是密文编辑器");
    fx::type_text(&mut panel, "sk-typed-secret-0000");
    assert_eq!(
        panel.edit_state().map(|state| state.buffer_len()),
        Some(20),
        "缓冲里有 20 个字符"
    );

    let text = render(&panel, SettingsCatalogs::new(&gateway, None), 120, 30);
    assert!(
        !text.contains("super-secret"),
        "屏幕里出现了磁盘上的明文密钥：\n{text}"
    );
    assert!(!text.contains("sk-super"), "屏幕里出现了明文前缀：\n{text}");
    assert!(
        !text.contains("typed-secret"),
        "屏幕里出现了正在输入的明文：\n{text}"
    );
    assert!(!text.contains("0000"), "屏幕里出现了输入的后缀：\n{text}");
    assert!(text.contains("••••"), "密文只以掩码出现：\n{text}");
    assert!(text.contains("api_key"), "行还在：\n{text}");
}

// ── 问题清单 / 帮助 ─────────────────────────────────────────

#[test]
fn problems_view_renders_the_wireframe() {
    let (gateway, panel) = fx::panel_from_problems(vec![
        wing_api_client::models::SettingProblem {
            path: Some("providers".into()),
            kind: "empty_list".into(),
            message: "providers 不得为空".into(),
            hint: Some("至少声明一个 provider".into()),
        },
        wing_api_client::models::SettingProblem {
            path: None,
            kind: "invalid_value".into(),
            message: "配置里出现了未知键".into(),
            hint: None,
        },
    ]);
    let buf = render_buffer(&panel, SettingsCatalogs::new(&gateway, None), 80, 24);
    let text = render(&panel, SettingsCatalogs::new(&gateway, None), 80, 24);
    assert!(
        row_text(&buf, 0).contains("2 problems"),
        "{}",
        row_text(&buf, 0)
    );
    assert!(text.contains("待修复"), "{text}");
    assert!(text.contains("❯ 1  providers 不得为空"), "{text}");
    assert!(
        text.contains("路径 providers · 至少声明一个 provider"),
        "{text}"
    );
    assert!(
        text.contains("这个问题无法定位到单个字段"),
        "文档级问题：{text}"
    );
    assert!(text.contains("config.yaml"), "文件路径：{text}");
    assert!(
        text.contains("改完按 s 保存；保存失败会自动回到这里。"),
        "{text}"
    );
    // 问题清单视图没有详情栏。
    let row = find_row(&buf, "待修复").expect("待修复");
    for x in 2..78 {
        assert_ne!(buf[(x, row)].symbol(), "│", "问题清单不竖切");
    }
}

/// setup 首屏（design §14.1 场景 1）：问题清单是第一个视图，标题栏标 setup mode。
#[test]
fn setup_first_screen_lands_on_the_problem_list() {
    let (gateway, panel) = fx::setup_panel();
    assert_eq!(panel.view(), View::Problems);
    let text = render(&panel, SettingsCatalogs::new(&gateway, None), 80, 24);
    assert!(text.contains("setup mode"), "标题栏：{text}");
    assert!(text.contains("待修复"), "{text}");
    assert!(
        text.contains("修完全部问题后网关自动进入正常模式"),
        "setup 说明行：{text}"
    );
    assert!(text.contains("不得为空"), "{text}");
    assert!(
        text.contains("这个问题无法定位到单个字段"),
        "文档级问题：{text}"
    );
}

#[test]
fn help_overlay_lists_every_key_and_the_save_note() {
    let mut harness = Harness::new();
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Char('?'));
    assert_eq!(harness.panel.view(), View::Help);
    let text = render(&harness.panel, harness.catalogs(), 100, 50);
    assert!(text.contains("┌ 帮助"), "浮层标题：{text}");
    assert!(text.contains("改动只在按 s 保存后才落盘。"), "{text}");
    for key in ["Tab", "Ctrl+R", "J K", "Space", "PgUp PgDn"] {
        assert!(text.contains(key), "帮助里缺少 {key}：{text}");
    }
    // Esc 关掉之后回到树。
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Esc);
    assert_eq!(harness.panel.view(), View::Tree);
}

#[test]
fn the_subheader_row_becomes_the_search_prompt_and_keeps_the_cursor_visible() {
    let mut harness = Harness::new();
    let title_before = row_text(&harness.buffer(80, 20), 0);
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Char('/'));
    fx::type_text(&mut harness.panel, "timeout_first_chunk");
    let buf = harness.buffer(80, 20);
    // v2：搜索有自己的常驻行（标题栏底下那一行），标题栏不再被顶掉。
    assert_eq!(row_text(&buf, 0), title_before, "标题栏稳定");
    let prompt = row_text(&buf, 1);
    assert!(prompt.contains("搜索: timeout_first_chunk▏"), "{prompt:?}");
    assert!(prompt.contains("命中"), "命中数在同一行：{prompt:?}");

    // 长 query（比那一行还长）→ 左截断，光标 `▏` 仍在。
    fx::type_text(&mut harness.panel, &"很长".repeat(40));
    let prompt = row_text(&harness.buffer(60, 20), 1);
    assert!(prompt.contains('▏'), "光标还在：{prompt:?}");
    assert!(prompt.contains('…'), "长 query 左截断：{prompt:?}");
    assert!(!prompt.contains("搜索: "), "头部让位给尾部：{prompt:?}");
}

#[test]
fn prompt_overlay_renders_options_and_cursor() {
    let mut harness = Harness::new();
    // `d` on 标量字段行 → 二次确认提示（纯确认）。
    fx::goto(&mut harness.panel, "base_url");
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Char('d'));
    let prompt = harness.panel.prompt().expect("提示开着");
    let text = render(&harness.panel, harness.catalogs(), 100, 30);
    assert!(text.contains(&prompt.title), "提示标题：{text}");
    assert!(text.contains("Enter 确认 · Esc 取消"), "{text}");
    // Esc 取消后回到树。
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Esc);
    assert!(harness.panel.prompt().is_none());
}

// ── 窗口滚动 ────────────────────────────────────────────────

#[test]
fn long_trees_scroll_with_a_centered_cursor_and_clamped_ends() {
    let (gateway, mut panel) = fx::big_list(30);
    // 搜索「items」→ 命中列表节点（搜索态会连带展示它的直接子节点）。
    fx::goto(&mut panel, "items");
    fx::press(&mut panel, crossterm::event::KeyCode::Enter);
    fx::press(&mut panel, crossterm::event::KeyCode::Right);
    // 光标居中：窗口里的行数 = tree_viewport_rows，且光标不在窗口边缘。
    let area = Rect::new(0, 0, 80, 14);
    let rows = tree_viewport_rows(&panel, area);
    // 80×14：边框 2 → 12；副标题 1 → 11；键位 2 + 分隔 1 → 8（主体）；
    // 组头 1 → 7；doc 提示 1 → 6。
    assert_eq!(rows, 6, "窄屏矮终端：{rows}");
    for _ in 0..11 {
        fx::press(&mut panel, crossterm::event::KeyCode::Down);
    }
    let buf = render_buffer(&panel, SettingsCatalogs::new(&gateway, None), 80, 14);
    let cursor_row = panel.rows()[panel.cursor()].label.clone();
    let text = render(&panel, SettingsCatalogs::new(&gateway, None), 80, 14);
    assert!(text.contains(&cursor_row), "光标行可见：{text}");
    // 光标居中：窗口的第一行不应是 item00（已经滚过去了）。
    assert!(!text.contains("item00"), "窗口滚动后看不到第一项：{text}");
    assert!(text.contains('❯'), "光标标记：{text}");
    let y = find_row(&buf, &cursor_row).expect("光标行");
    assert!(y > 1 && y < 12, "光标不在窗口边缘（y={y}）：{text}");
    assert!(row_text(&buf, y).contains('❯'), "光标标记在该行");

    // 末端钳制：End → 末项可见，且窗口不动了。
    fx::press(&mut panel, crossterm::event::KeyCode::End);
    let text = render(&panel, SettingsCatalogs::new(&gateway, None), 80, 14);
    assert!(text.contains("item29"), "末项可见：{text}");
}

// ── 不溢出自查 ──────────────────────────────────────────────

/// 在 `width × height` 的内缩区域里渲染一次，返回最终缓冲。
///
/// 填充 `X` 哨兵：区域外的每一格都必须是 `X`（没被写），区域内的四角与四边必须是框。
fn render_inset(
    panel: &SettingsPanel,
    catalogs: SettingsCatalogs<'_>,
    width: u16,
    height: u16,
) -> (Buffer, Rect) {
    let palette = palette();
    let outer = Rect::new(0, 0, width + 4, height + 4);
    let area = Rect::new(2, 2, width, height);
    let mut terminal = Terminal::new(TestBackend::new(outer.width, outer.height)).expect("term");
    terminal
        .draw(|frame| {
            let full = frame.area();
            let buf = frame.buffer_mut();
            for y in full.y..full.bottom() {
                for x in full.x..full.right() {
                    buf[(x, y)].set_symbol("X");
                }
            }
            SettingsOverlay::new(panel, catalogs, &palette).render(area, buf);
        })
        .expect("draw");
    (terminal.backend().buffer().clone(), area)
}

/// 断言的公共部分：区域外未被写 + 边框完整。
fn assert_frame_intact(buf: &Buffer, area: Rect, label: &str) {
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if !area.contains(Position::new(x, y)) {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    "X",
                    "{label}: 区域外 ({x},{y}) 被写了"
                );
            }
        }
    }
    let symbol = |x: u16, y: u16| buf[(x, y)].symbol().to_string();
    assert_eq!(symbol(area.x, area.y), "┌", "{label}: 左上角");
    assert_eq!(symbol(area.right() - 1, area.y), "┐", "{label}: 右上角");
    assert_eq!(symbol(area.x, area.bottom() - 1), "└", "{label}: 左下角");
    assert_eq!(
        symbol(area.right() - 1, area.bottom() - 1),
        "┘",
        "{label}: 右下角"
    );
    if area.height < 2 || area.width < 2 {
        return;
    }
    // 左右两条边：每个内部行都是框线（分隔线行是 ├ / ┤）。
    for y in area.y + 1..area.bottom() - 1 {
        let left = symbol(area.x, y);
        let right = symbol(area.right() - 1, y);
        assert!(
            left == "│" || left == "├",
            "{label}: 左边框 y={y} 是 {left:?}"
        );
        assert!(
            right == "│" || right == "┤",
            "{label}: 右边框 y={y} 是 {right:?}"
        );
    }
    // 下边框：整条都是 ─。
    for x in area.x + 1..area.right() - 1 {
        assert_eq!(symbol(x, area.bottom() - 1), "─", "{label}: 下边框 x={x}");
    }
}

#[test]
fn the_overlay_never_draws_outside_its_area_at_any_size() {
    let mut harness = Harness::with_interface(true);
    fx::goto(&mut harness.panel, "timeout_first_chunk");
    for width in [40u16, 60, 80, 100, 110, 140, 200] {
        for height in [10u16, 24, 50] {
            let label = format!("{width}x{height}");
            let (buf, area) = render_inset(&harness.panel, harness.catalogs(), width, height);
            assert_frame_intact(&buf, area, &label);
            // 标题栏在那个尺寸下仍然能看见面板是设置面板。
            let title = row_text(&buf, area.y);
            assert!(title.contains("⚙ Settings"), "{label}: 标题 {title:?}");
            // 键位栏（两行整体：折行位置随宽窄变化，内容不该丢）。
            if height >= 8 {
                let hints = format!(
                    "{} {}",
                    row_text(&buf, area.bottom() - 3),
                    row_text(&buf, area.bottom() - 2)
                );
                assert!(hints.contains("Esc"), "{label}: 键位栏 {hints:?}");
            }
        }
    }
}

#[test]
fn overlays_and_degenerate_panels_stay_inside_the_frame() {
    // 帮助浮层 + 模态提示 + 问题清单，都在内缩区域里再跑一遍。
    let mut harness = Harness::new();
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Char('?'));
    for width in [40u16, 80, 120, 200] {
        for height in [10u16, 24] {
            let (buf, area) = render_inset(&harness.panel, harness.catalogs(), width, height);
            assert_frame_intact(&buf, area, &format!("help {width}x{height}"));
        }
    }

    // 退化输入：空目录 / 单节点 / 空文档。
    let empty = fx::root(vec![]);
    let panel = SettingsPanel::new(
        &fx::schema(&empty),
        fx::state(serde_json::json!({})),
        None,
        View::Tree,
    );
    for width in [40u16, 80, 140, 200] {
        for height in [10u16, 24, 50] {
            let (buf, area) =
                render_inset(&panel, SettingsCatalogs::new(&empty, None), width, height);
            assert_frame_intact(&buf, area, &format!("empty {width}x{height}"));
        }
        // 空目录：没有锚点 ⇒ 不留空白左栏，右栏一行占位（退化输入照常可读）。
        let text = render(&panel, SettingsCatalogs::new(&empty, None), width, 24);
        assert!(text.contains("(空目录)"), "{width}: {text}");
        assert!(text.contains("⚙ Settings"), "{width}: {text}");
    }

    let single = fx::root(vec![fx::str_field("only")]);
    let panel = SettingsPanel::new(
        &fx::schema(&single),
        fx::state(serde_json::json!({"only": "v"})),
        None,
        View::Problems,
    );
    for width in [40u16, 80, 140] {
        let (buf, area) = render_inset(&panel, SettingsCatalogs::new(&single, None), width, 10);
        assert_frame_intact(&buf, area, &format!("single {width}"));
    }
}

#[test]
fn long_docs_values_and_cjk_never_overflow_the_frame() {
    let gateway = fx::root(vec![
        {
            let mut node = fx::str_field("huge");
            node.doc = "很长的中文说明".repeat(20);
            node.title = "一个非常长的字段标题".repeat(4);
            node.notes = vec!["多行详解".repeat(30)];
            node.example = Some("#abcdef".repeat(20));
            node
        },
        {
            let mut node = fx::str_field("cjk");
            node.doc = "中文 doc".into();
            node
        },
    ]);
    let values = serde_json::json!({
        "huge": "值".repeat(200),
        "cjk": "中文值",
    });
    let mut panel = SettingsPanel::new(&fx::schema(&gateway), fx::state(values), None, View::Tree);
    fx::goto(&mut panel, "huge");
    for width in [40u16, 60, 80, 100, 110, 140, 200] {
        for height in [10u16, 24, 50] {
            let (buf, area) =
                render_inset(&panel, SettingsCatalogs::new(&gateway, None), width, height);
            assert_frame_intact(&buf, area, &format!("long {width}x{height}"));
        }
        let text = render(&panel, SettingsCatalogs::new(&gateway, None), width, 24);
        assert!(text.contains('中') || width < 10, "{width}: 中文照样能画");
    }
}

// ── 视图行数契约 ────────────────────────────────────────────

#[test]
fn tree_viewport_rows_matches_what_is_drawn() {
    let harness = Harness::new();
    // 80×24：边框 2 → 22；副标题 1 → 21；键位 2 + 分隔 1 → 18（主体）；
    //        组头 1 → 17；doc 提示 1 → 16。
    assert_eq!(
        tree_viewport_rows(&harness.panel, Rect::new(0, 0, 80, 24)),
        16
    );
    // 140×24：右栏 ≥ 78 列 ⇒ 详情栏接替 doc 提示行；键位装得下 1 行 ⇒ 主体多一行。
    assert_eq!(
        tree_viewport_rows(&harness.panel, Rect::new(0, 0, 140, 24)),
        18
    );
    // 10 行的小终端：22 → 8（边框）→ 7（副标题）→ 4（键位 2 + 分隔）→ 3（组头）→ 2。
    assert_eq!(
        tree_viewport_rows(&harness.panel, Rect::new(0, 0, 80, 10)),
        2
    );
    // 左栏的可见行数与主体同高（锚点自己滚动）。
    assert_eq!(
        anchors_viewport_rows(&harness.panel, Rect::new(0, 0, 80, 24)),
        18
    );
    // 退化尺寸不 panic。
    assert_eq!(tree_viewport_rows(&harness.panel, Rect::new(0, 0, 1, 1)), 0);
    assert_eq!(tree_viewport_rows(&harness.panel, Rect::new(0, 0, 0, 0)), 0);
    assert_eq!(
        anchors_viewport_rows(&harness.panel, Rect::new(0, 0, 1, 1)),
        0
    );
}

#[test]
fn the_viewport_contract_holds_at_every_size() {
    // 翻页步长必须等于实际画出来的行数：两者一旦漂移，PageDown 就会跳错。
    let (gateway, interface, panel) = fx::product_panel();
    let catalogs = SettingsCatalogs::new(&gateway, Some(&interface));
    for width in [40u16, 60, 80, 100, 110, 120, 140, 200] {
        for height in [10u16, 16, 24, 30, 40, 50] {
            let rows = tree_viewport_rows(&panel, Rect::new(0, 0, width, height));
            let anchors = anchors_viewport_rows(&panel, Rect::new(0, 0, width, height));
            assert!(rows <= height.saturating_sub(2), "{width}x{height}: {rows}");
            assert!(
                anchors <= height.saturating_sub(2),
                "{width}x{height}: {anchors}"
            );
            // 画出来不 panic、不溢出（哨兵检查在 render_inset 里）。
            let (buf, area) = render_inset(&panel, catalogs, width, height);
            assert_frame_intact(&buf, area, &format!("{width}x{height}"));
        }
    }
}

// ── 产品口径的帧证据（三个尺寸）─────────────────────────────

/// 在一台 `term_w × term_h` 的终端里按**产品路径**渲染：`card_area` 定卡片、
/// 卡片之外填 `X` 哨兵（证明浮层不越界、也不抹掉四周的背景）。
fn render_card(
    panel: &SettingsPanel,
    catalogs: SettingsCatalogs<'_>,
    term_w: u16,
    term_h: u16,
) -> (Buffer, Rect) {
    let palette = palette();
    let card = super::card_area(Rect::new(0, 0, term_w, term_h));
    let mut terminal = Terminal::new(TestBackend::new(term_w, term_h)).expect("terminal");
    terminal
        .draw(|frame| {
            let buf = frame.buffer_mut();
            for y in 0..term_h {
                for x in 0..term_w {
                    buf[(x, y)].set_symbol("X");
                }
            }
            SettingsOverlay::new(panel, catalogs, &palette).render(card, buf);
        })
        .expect("draw");
    (terminal.backend().buffer().clone(), card)
}

/// 卡片外必须全是哨兵（浮层不越界，也不全屏 Clear）。
fn assert_outside_untouched(buf: &Buffer, card: Rect, label: &str) {
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if !card.contains(Position::new(x, y)) {
                assert_eq!(buf[(x, y)].symbol(), "X", "{label}: ({x},{y}) 被写了");
            }
        }
    }
}

/// 三个产品尺寸的渲染帧（PR 的证据；`WING_DUMP_SETTINGS_FRAMES=1` 时打到 stdout）。
///
/// `println!` 是这条用例的**产物**（帧证据要能复制进 PR），不是调试残留：默认不打印。
///
/// 断言的是 v2 的四条几何/内容契约：卡片尺寸与居中、8 个锚点全在左栏、右栏是当前组的
/// 设置项、卡片外一格不写。
#[test]
#[allow(clippy::print_stdout)]
fn product_frames_hold_their_geometry_at_the_three_sizes() {
    let (gateway, interface, mut panel) = fx::product_panel();
    let catalogs = SettingsCatalogs::new(&gateway, Some(&interface));
    // 产品口径的第一屏：左栏焦点 + 第一个锚点（Providers）的右栏。
    fx::goto(&mut panel, "timeout_first_chunk");

    let cases = [
        // (终端, 期望卡片)
        ((80u16, 24u16), Rect::new(2, 2, 76, 20)),
        ((100, 30), Rect::new(2, 2, 96, 26)),
        ((120, 40), Rect::new(5, 4, 110, 32)),
    ];
    for ((term_w, term_h), expected) in cases {
        let label = format!("{term_w}x{term_h}");
        let (buf, card) = render_card(&panel, catalogs, term_w, term_h);
        assert_eq!(card, expected, "{label}: 卡片几何");
        assert_outside_untouched(&buf, card, &label);

        let text = buffer_text(&buf);
        // 8 个锚点（后端 7 组 + Interface）全在左栏。
        for anchor in [
            "Providers",
            "Agents",
            "Behavior",
            "Images",
            "Sessions",
            "Gateway",
            "Advanced",
            "Interface",
        ] {
            assert!(
                text.contains(anchor),
                "{label}: 左栏缺锚点 {anchor}\n{text}"
            );
        }
        // 标题栏 / 副标题行 / 键位栏。
        assert!(text.contains("⚙ Settings"), "{label}: {text}");
        assert!(text.contains("Tab/←→ 切栏"), "{label}: {text}");
        // 右栏是当前组（Providers）的设置项：密文只以掩码出现。
        assert!(text.contains("providers"), "{label}: {text}");
        assert!(text.contains("••••••••"), "{label}: 密文掩码：{text}");
        assert!(
            !text.contains("sk-") && !text.contains("api_key: "),
            "{label}: 密文不外泄：{text}"
        );
        // 组头（右栏第一行）带着分组说明。
        assert!(text.contains("LLM provider"), "{label}: 组头说明：{text}");

        if term_w >= 120 {
            // 宽屏：右栏再竖切出详情栏（两条竖分隔）。
            let row = find_row(&buf, "timeout_first_chunk").expect("行");
            let dividers = (card.x + 1..card.right() - 1)
                .filter(|x| buf[(*x, row)].symbol() == "│")
                .count();
            assert_eq!(dividers, 2, "{label}: 左栏 + 详情栏两条分隔");
            assert!(text.contains("生效"), "{label}: 详情栏在场：{text}");
        }

        if std::env::var("WING_DUMP_SETTINGS_FRAMES").is_ok() {
            println!(
                "=== terminal {term_w}x{term_h} → card {}x{} @({},{})",
                card.width, card.height, card.x, card.y
            );
            for y in 0..term_h {
                println!("|{}|", row_text(&buf, y));
            }
        }
    }
}

/// 整屏文本（每行去掉尾随空格）。
fn buffer_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 左栏锚点在矮卡片里滚动（8 个锚点 > 可见行数时窗口跟着光标走）。
#[test]
fn the_anchor_column_scrolls_when_it_does_not_fit() {
    let (gateway, interface, mut panel) = fx::product_panel();
    let catalogs = SettingsCatalogs::new(&gateway, Some(&interface));
    // 一张矮卡片：主体只剩几行，8 个锚点装不下。
    let (buf, card) = render_card(&panel, catalogs, 80, 12);
    assert!(card.height <= 12, "矮卡片：{card:?}");
    let visible = anchors_viewport_rows(&panel, card) as usize;
    assert!(visible < panel.anchors().len(), "锚点确实装不下：{visible}");
    let text = buffer_text(&buf);
    assert!(text.contains("Providers"), "窗口从头开始：{text}");
    assert!(!text.contains("Interface"), "末尾的锚点还没进窗口：{text}");

    // 走到最后一个锚点 → 窗口跟着滚。
    for _ in 0..panel.anchors().len() {
        fx::press(&mut panel, crossterm::event::KeyCode::Down);
    }
    assert_eq!(panel.group_cursor(), panel.anchors().len() - 1);
    let (buf, _) = render_card(&panel, catalogs, 80, 12);
    let text = buffer_text(&buf);
    assert!(text.contains("Interface"), "滚到末尾：{text}");
    assert!(!text.contains("Providers"), "头一个锚点滚出窗口：{text}");
}

// ── 副标题行（审查 S2）：搜索回显不许被横幅顶掉 ─────────────

#[test]
fn the_search_line_wins_over_the_stale_banner() {
    let catalog = fx::product_catalog();
    let schema = fx::product_schema();
    let mut panel = SettingsPanel::new(&schema, fx::product_state(), None, View::Tree);
    // 过期口径走产品路径：`settings_changed` 事件带着一份与本地不同的指纹。
    panel.on_settings_changed("other-client-fp");
    assert!(panel.is_stale(), "夹具前提：指纹不一致");
    let catalogs = SettingsCatalogs::new(&catalog, None);

    // 没有搜索：横幅整条占着副标题行。
    let text = render(&panel, catalogs, 100, 30);
    assert!(text.contains("配置已被其它客户端修改"), "{text}");
    assert!(!text.contains("/ 搜索…"), "横幅在时不画占位：{text}");

    // 搜索激活：回显优先，横幅退化成同一行尾部的一段。
    fx::press(&mut panel, crossterm::event::KeyCode::Char('/'));
    fx::type_text(&mut panel, "log");
    let text = render(&panel, catalogs, 100, 30);
    assert!(text.contains("搜索: log"), "query 回显在场：{text}");
    assert!(text.contains("命中"), "命中数在场：{text}");
    assert!(
        text.contains("已被其它客户端修改"),
        "横幅没丢，只是让位给回显：{text}"
    );
}

#[test]
fn a_narrow_card_drops_the_anchor_column_and_keeps_focus_in_the_tree() {
    let catalog = fx::product_catalog();
    let schema = fx::product_schema();
    let mut panel = SettingsPanel::new(&schema, fx::product_state(), None, View::Tree);
    assert_eq!(panel.focus(), Focus::Groups);
    let catalogs = SettingsCatalogs::new(&catalog, None);

    // 40 列：卡片铺满，主体放不下"左栏 14 + 分隔 + 右栏 24"。
    let area = Rect::new(0, 0, 40, 24);
    assert!(!has_anchor_column(&panel, area), "窄卡片不出左栏");
    panel.set_viewport_rows(tree_viewport_rows(&panel, area) as usize);
    panel.set_anchors_visible(has_anchor_column(&panel, area));
    assert_eq!(
        panel.focus(),
        Focus::Items,
        "左栏不存在 ⇒ 焦点不许停在它上面（审查 S3）"
    );
    let text = render(&panel, catalogs, 40, 24);
    assert!(text.contains("Settings"), "{text}");
    assert!(text.contains("providers"), "右栏独占：{text}");
    // 左栏没画（别的锚点标题一个都不在），但**组头还在**：用户仍然知道自己站在哪一组里。
    assert!(!text.contains("Agents"), "左栏确实没画：{text}");
    assert!(
        text.contains("Providers · LLM provider"),
        "组头补上了左栏的那份信息：{text}"
    );
    // 键位栏不再提"切栏"（没得切），Esc 仍是关闭。
    assert!(!text.contains("切栏"), "{text}");
    assert!(text.contains("Esc 关闭"), "{text}");
    // 窄卡片仍然不溢出（①号铁律）：哨兵一格不动。
    let (buf, area) = render_inset(&panel, catalogs, 40, 24);
    assert_frame_intact(&buf, area, "40x24 无左栏");
    // `Tab` 无处可去；`Esc` 直接走关闭那两级（不经过一个看不见的栏）。
    assert_eq!(panel.focus(), Focus::Items);
    fx::press(&mut panel, crossterm::event::KeyCode::Tab);
    assert_eq!(panel.focus(), Focus::Items, "Tab 不切到不存在的栏");
    let esc = KeyEvent::new(crossterm::event::KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(
        panel.handle_key(esc),
        SettingsAction::Close { discard: false },
        "Esc 直接关（干净面板）"
    );
}
