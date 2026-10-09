//! `TestBackend` 帧测试：整套 overlay 的屏幕断言。
//!
//! 三条铁律（任务书 §Verification）：
//!
//! ① **不溢出**：40/60/80/100/110/140/200 列 × 10/24/50 行全覆盖，且渲染进「内缩区域」时
//!    区域外一格不许被写（用 `X` 哨兵钉住）；
//! ② **密文红线**：编辑密钥时屏幕缓冲里不含明文子串；
//! ③ 帧断言写**具体屏幕内容**，不只是「不 panic」。

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::config::ThemePalette;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;
use wing_api_client::models::SettingNode;

use super::SettingsCatalogs;
use super::SettingsOverlay;
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
fn wide_frame_has_title_body_divider_and_hints() {
    let mut harness = Harness::with_interface(true);
    // 让详情栏有内容：光标落到 timeout_first_chunk 上。
    fx::goto(&mut harness.panel, "timeout_first_chunk");
    let buf = harness.buffer(120, 24);
    let text = render(&harness.panel, harness.catalogs(), 120, 24);

    // 标题栏：⚙ Settings + 配置路径 + valid。
    let title = row_text(&buf, 0);
    assert!(title.starts_with("┌─ ⚙ Settings"), "{title:?}");
    assert!(
        title.contains("/home/u/.wing/core/config.yaml"),
        "{title:?}"
    );
    assert!(title.contains("valid"), "{title:?}");
    assert!(title.ends_with('┐'), "{title:?}");

    // 两个根都在。
    assert!(text.contains("Gateway"), "{text}");
    assert!(text.contains("Interface"), "{text}");

    // 竖分隔 + 详情栏（宽屏专属）。
    let divider_x = 1 + (118 * 55 / 100); // inner.x + tree_width
    let body_row = find_row(&buf, "Gateway").expect("Gateway 行");
    assert_eq!(buf[(divider_x, body_row)].symbol(), "│", "竖分隔列");
    assert!(text.contains("流式首块超时"), "详情栏的 doc：{text}");
    assert!(text.contains("路径"), "详情栏的路径段：{text}");

    // 分隔线：左中右三个交点。
    let separator_y = 24 - 4;
    assert_eq!(buf[(0, separator_y)].symbol(), "├", "分隔线左交点");
    assert_eq!(buf[(divider_x, separator_y)].symbol(), "┴", "分隔线交点");
    assert_eq!(buf[(119, separator_y)].symbol(), "┤", "分隔线右交点");

    // 键位栏（两行，内含 07 的上下文提示）。
    assert!(
        text.contains("Esc 关闭") || text.contains("Esc 放弃改动"),
        "{text}"
    );
    assert_eq!(buf[(0, 23)].symbol(), "└", "下边框");
    assert_eq!(buf[(119, 23)].symbol(), "┘");
}

#[test]
fn narrow_frame_drops_the_detail_pane_and_keeps_the_hint_band() {
    let mut harness = Harness::with_interface(true);
    fx::goto(&mut harness.panel, "timeout_first_chunk");
    let buf = harness.buffer(80, 24);
    let text = render(&harness.panel, harness.catalogs(), 80, 24);

    // 没有竖分隔列：任何一行都不该在 44 列附近有 `│`（边框除外）。
    let body_row = find_row(&buf, "Gateway").expect("Gateway 行");
    for x in 2..78 {
        assert_ne!(
            buf[(x, body_row)].symbol(),
            "│",
            "窄屏不该有竖分隔（x={x}）"
        );
    }
    // doc 提示行在树下方（分隔线上方一行）。
    let hint_row = row_text(&buf, 24 - 5);
    assert!(
        hint_row.contains("流式首块超时"),
        "doc 提示行：{hint_row:?}"
    );
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
        text.contains("这个问题无法定位到单个字段，请检查文件：/home/u/.wing/core/config.yaml"),
        "{text}"
    );
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
fn title_bar_becomes_the_search_prompt_and_keeps_the_cursor_visible() {
    let mut harness = Harness::new();
    fx::press(&mut harness.panel, crossterm::event::KeyCode::Char('/'));
    fx::type_text(&mut harness.panel, "timeout_first_chunk");
    let title = row_text(&harness.buffer(80, 20), 0);
    assert!(title.contains("搜索: timeout_first_chunk▏"), "{title:?}");

    // 长 query（比标题栏还长）→ 左截断，光标 `▏` 仍在。
    fx::type_text(&mut harness.panel, &"很长".repeat(40));
    let title = row_text(&harness.buffer(60, 20), 0);
    assert!(title.contains('▏'), "光标还在：{title:?}");
    assert!(title.contains('…'), "长 query 左截断：{title:?}");
    assert!(!title.contains("搜索: "), "头部让位给尾部：{title:?}");
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
    assert_eq!(rows, 14 - 5 - 1, "窄屏：h - 5 - doc 提示行");
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
        // 空目录仍有根头行（`flatten` 一定产出根行）—— 退化输入照常可读。
        let text = render(&panel, SettingsCatalogs::new(&empty, None), width, 24);
        assert!(text.contains("Gateway"), "{width}: {text}");
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
    // 窄屏：h - 5（边框 2 + 分隔 1 + 键位 2）- 1（doc 提示行）。
    assert_eq!(
        tree_viewport_rows(&harness.panel, Rect::new(0, 0, 80, 24)),
        18
    );
    // 宽屏：没有 doc 提示行。
    assert_eq!(
        tree_viewport_rows(&harness.panel, Rect::new(0, 0, 140, 24)),
        19
    );
    // 10 行的小终端。
    assert_eq!(
        tree_viewport_rows(&harness.panel, Rect::new(0, 0, 80, 10)),
        4
    );
    // 退化尺寸不 panic。
    assert_eq!(tree_viewport_rows(&harness.panel, Rect::new(0, 0, 1, 1)), 0);
    assert_eq!(tree_viewport_rows(&harness.panel, Rect::new(0, 0, 0, 0)), 0);
}
