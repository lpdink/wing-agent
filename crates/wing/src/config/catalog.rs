//! Interface 根的设置目录（TUI 自己的配置）+ 规范形 YAML emitter。
//!
//! 设置面板是一棵树、两个根：`Gateway` 根的 catalog 来自后端
//! `GET /api/settings/schema`，`Interface` 根（`$WING_HOME/tui/config.yaml`）后端不知道，
//! 所以在这里声明——但用的是**同一个** [`SettingNode`] 类型，树 widget 因此零分支。
//!
//! 两条不变量：
//!
//! 1. **catalog 不许与 serde 漂移**：`interface_catalog_covers_every_serde_leaf` /
//!    `interface_catalog_declares_no_phantom_key` 双向对账（机制不是纪律）。给 `AppConfig`
//!    加字段忘了声明 → 红；声明了不存在的键 → 红。
//! 2. **`dump_config_yaml` 是 `(doc, catalog, mode)` 的纯函数**：不读 env、不写时间戳——round-trip
//!    （`dump → 解析 → 再 dump` 逐字相同）与"空文档解析回 `AppConfig::default()`"
//!    因此可以被单测机械证明。密文怎么发射由 [`DumpMode`] 参数决定（**保存路径传 `Raw`，
//!    展示路径传 `Masked`**）：漏传编译不过，不存在"忘了想"的路径。
//!
//! 文案口径：`doc` 照抄 [`crate::config`] 里现有的字段文档注释（英文，已经打磨过的口径），
//! 新增的 `notes` / `choices[].doc` / `section_doc` 用中文（TUI 的产品语言）。
//! enum 的 `choices[].value` 与 `default` 都用**小写**拼写（用户文档口径），文件里已写下的值
//! 原样输出、靠 `Deserialize` 的大小写不敏感 round-trip。

use ratatui::style::Color;
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SettingChoice;
use wing_api_client::models::SettingGroup;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;

use crate::config::LayoutConfig;
use crate::config::ThemePalette;

// ── 声明 ────────────────────────────────────────────────────────

/// 颜色槽位的取值文法（咨询性）：命名 ANSI 色或 `#RRGGBB`。
///
/// 写成 Python 语法（协议 `pattern` 字段的既定口径）。Rust 侧没有 regex 引擎也不为此加依赖，
/// 所以它是**面板的展示/本地校验提示**；权威校验始终是 [`parse_color`] 的 warn + 回落。
const COLOR_PATTERN: &str = "(?i)^(#([0-9a-f]{6})|black|red|green|yellow|blue|magenta|cyan|white|dark_?gray|dark_?grey|\
     gray|grey|light_?(red|green|yellow|blue|magenta|cyan)|reset)$";

const ROOT_DOC: &str = "TUI 自身的配置（`$WING_HOME/tui/config.yaml`）。";

const ROOT_NOTES: [&str; 2] = [
    "Gateway host:port is NOT stored here — it is read from the backend config \
     (`$WING_HOME/core/config.yaml`) via `backend_config::read_backend_gateway_config()`.",
    "改动即时预览（Esc 回退），`s` 保存；与 Gateway 根在同一个面板里编辑。",
];

const COLORS_SECTION_DOC: &str = "配色：`preset` 选定底色，其余每个槽位键覆盖该槽位。";
const COLORS_DOC: &str = "Semantic color palette — one slot per UI role.";
const COLORS_NOTES: [&str; 2] = [
    "`preset` picks the base palette; every other key overrides that one slot \
     (absent = follow the preset).",
    "命名 ANSI 色（`cyan` / `dark_gray` …）或 24-bit hex（`#3ac6e0`）都行；\
     非法值 warn 后回落该预设槽位。",
];

const PRESET_DOC: &str = "Base palette (see `ColorPreset`).";

/// 槽位说明 = 该槽位在 UI 里管什么（逐字照抄 `config/mod.rs` 的字段文档）。
const SLOT_DOCS: [(&str, &str); 14] = [
    (
        "accent",
        "Brand color: logo, commands, links, code, popups.",
    ),
    ("text", "Primary text: messages, model name, input."),
    (
        "thinking",
        "Thinking: agent reasoning content (more prominent than dim).",
    ),
    (
        "tool_result",
        "Tool result: tool call output (secondary data).",
    ),
    (
        "dim",
        "Quiet register: bullets, borders, gutters, separators — and \
         tool-call args / timers (secondary metadata).",
    ),
    ("success", "Success: diff+, blockquote, token low."),
    ("warning", "Warning: pending, token mid."),
    ("danger", "Danger: diff-, error, token high."),
    (
        "math",
        "Math: rendered formulas (character grids) and their literal fallback.",
    ),
    ("surface", "Surface: user message background."),
    (
        "diff_add_bg",
        "Background tint for diff additions (text keeps syntax colors).",
    ),
    ("diff_del_bg", "Background tint for diff deletions."),
    (
        "diff_add_bg_strong",
        "Background tint for changed words inside an added line.",
    ),
    (
        "diff_del_bg_strong",
        "Background tint for changed words inside a deleted line.",
    ),
];

const SLOT_NOTE: &str = "缺席 = 跟随 `preset`。";

const LAYOUT_SECTION_DOC: &str = "布局：输入区 / 弹窗 / 工具输出的行数上限。";
const LAYOUT_DOC: &str = "Layout configuration section.";

const RENDERING_SECTION_DOC: &str = "渲染：思考块 / 公式 / 图片。";
const RENDERING_DOC: &str = "Rendering configuration section.";
const THINKING_DOC: &str = "reasoning 块的默认呈现。";
const THINKING_NOTES: [&str; 2] = [
    "`visible` = 默认展开（标题行 + 正文）；`hidden` = 默认折叠（只剩标题行 \
     `⦁ 深度思考中 4s`，持续刷光、完成后定格时长）。",
    "标题行是所有思考块的固有部分；`Ctrl+O` 全局切换（所有轮一起、会话内保持）。",
];
const MATH_DOC: &str = "`text` (render formulas) or `off` (leave LaTeX source verbatim).";
const IMAGES_DOC: &str = "Markdown image rendering: `off` | `auto` (see `ImagesMode`).";

const GATEWAY_SECTION_DOC: &str = "网关连接：TUI 对着网关鉴权用。";
const API_KEY_DOC: &str = "API key for gateway authentication.";
const API_KEY_NOTES: [&str; 1] = ["Empty or None → no auth header sent."];
const API_KEY_EXAMPLE: &str = "sk-xxx";

/// Interface 根的设置目录（TUI 自己的配置）。
///
/// 结构与后端 catalog 同构：同一个 [`SettingNode`] 类型。覆盖 `AppConfig` 的**全部** YAML 键
/// （`colors.*` 15 + `layout.*` 3 + `rendering.*` 3 + `api_key`）。
///
/// 根节点的 `key` / `path` 是 `"interface"`（与后端根的 `"config"` 对称）；
/// 子节点的 `path` **不带根前缀**——它们就是稀疏文档（`settings/get` 的 `values`）里的真实键路径，
/// 直接喂给 [`SettingNode::node_at`]。`colors.math_mode` 是 `#[serde(skip)]` 的解析载体、
/// 不是 YAML 键，所以这里不声明它（门禁会替我们记住这一点）。
pub fn interface_catalog() -> SettingNode {
    let palette = ThemePalette::default();

    let mut colors = node(
        "colors",
        "colors",
        SettingKind::Object,
        COLORS_DOC,
        0,
        "Colors",
        COLORS_SECTION_DOC,
    );
    colors.notes = COLORS_NOTES.iter().map(|n| (*n).to_owned()).collect();
    colors.children.push(preset_node());
    for (index, (key, doc)) in SLOT_DOCS.iter().enumerate() {
        colors.children.push(slot_node(
            key,
            doc,
            &color_to_string(slot_color(&palette, key)),
            index as i64 + 1,
        ));
    }

    let mut layout = node(
        "layout",
        "layout",
        SettingKind::Object,
        LAYOUT_DOC,
        1,
        "Layout",
        LAYOUT_SECTION_DOC,
    );
    let layout_defaults = LayoutConfig::default();
    layout.children = [
        (
            "max_input_lines",
            "Maximum input area lines.",
            layout_defaults.max_input_lines,
        ),
        (
            "max_popup_rows",
            "Maximum visible rows in selection popup.",
            layout_defaults.max_popup_rows,
        ),
        (
            "tool_output_max",
            "Maximum tool result output lines.",
            layout_defaults.tool_output_max,
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (key, doc, default))| {
        let mut child = node(
            key,
            &format!("layout.{key}"),
            SettingKind::Int,
            doc,
            index as i64,
            "Layout",
            LAYOUT_SECTION_DOC,
        );
        child.default = Some(Value::from(default)); // usize → u64 → JSON
        child.has_default = true;
        child.min = Some(1.0);
        child
    })
    .collect();

    let mut rendering = node(
        "rendering",
        "rendering",
        SettingKind::Object,
        RENDERING_DOC,
        2,
        "Rendering",
        RENDERING_SECTION_DOC,
    );
    let thinking_node = {
        let mut node = enum_node(
            "thinking",
            "rendering.thinking",
            THINKING_DOC,
            0,
            &[
                ("visible", "默认展开：标题行 + 正文"),
                (
                    "hidden",
                    "默认折叠：只剩标题行（进行中持续刷光，结束后定格时长）",
                ),
            ],
            "visible",
        );
        node.notes = THINKING_NOTES.iter().map(|n| (*n).to_owned()).collect();
        node
    };
    rendering.children = vec![
        thinking_node,
        enum_node(
            "math",
            "rendering.math",
            MATH_DOC,
            1,
            &[
                (
                    "text",
                    "把 `$…$` / `$$…$$` / 裸 AMS 环境渲染成字符网格（渲染不了时显示完整 LaTeX 源码）",
                ),
                ("off", "完全不解析、不归一化（未引入公式渲染前的行为）"),
            ],
            "text",
        ),
        enum_node(
            "images",
            "rendering.images",
            IMAGES_DOC,
            2,
            &[
                (
                    "auto",
                    "启动时探测终端图形协议（kitty / sixel / iTerm2），支持就画真图",
                ),
                ("off", "不探测、不读盘、零开销：markdown 图片走链接路径"),
            ],
            "auto",
        ),
    ];

    let mut api_key = node(
        "api_key",
        "api_key",
        SettingKind::Secret,
        API_KEY_DOC,
        3,
        "Gateway",
        GATEWAY_SECTION_DOC,
    );
    api_key.notes = API_KEY_NOTES.iter().map(|n| (*n).to_owned()).collect();
    api_key.example = Some(API_KEY_EXAMPLE.to_owned());
    api_key.secret = true;
    api_key.nullable = true;
    // 只在建连时读一次（启动链）——保存它不会重连，所以不是 hot。
    api_key.apply = ApplyScope::Restart;

    let mut root = node(
        "interface",
        "interface",
        SettingKind::Object,
        ROOT_DOC,
        0,
        "",
        "",
    );
    root.title = "Interface".to_owned();
    root.notes = ROOT_NOTES.iter().map(|n| (*n).to_owned()).collect();
    root.section = None;
    root.section_doc = None;
    root.children = vec![colors, layout, rendering, api_key];
    root
}

/// Interface 根的**业务分组**（设置面板左列的最后一个锚点）。
///
/// 与后端 `config/groups.py` 的 `SETTING_GROUPS` 同一份建模、同一个 wire 类型
/// （[`SettingGroup`]），只是声明在 Rust 侧——TUI 自己的配置后端不知道。
/// 一个锚点装下全部顶层键：Interface 的四个键（`colors` / `layout` / `rendering` / `api_key`）
/// 是一件事（"这个终端长什么样"），拆成四个锚点只会把左列挤满。
///
/// 门禁（本文件的单测）：成员必须与 [`interface_catalog`] 的 root 子节点**双向对账**——
/// 加了字段忘了归类 ⇒ 它在面板里无处可去；归类了不存在的键 ⇒ 空锚点。
pub fn interface_groups() -> Vec<SettingGroup> {
    vec![SettingGroup {
        id: "interface".to_owned(),
        title: "Interface".to_owned(),
        // 这段 doc 会原样画在右栏的组头（纯文本，不渲染 markdown）：不写反引号。
        doc: "TUI 自身：配色 / 布局 / 渲染（改动即时预览，s 保存到 tui/config.yaml）".to_owned(),
        members: vec![
            "colors".to_owned(),
            "layout".to_owned(),
            "rendering".to_owned(),
            "api_key".to_owned(),
        ],
    }]
}

/// 一个只填了身份 / 类型 / 文档的节点（其余字段取协议默认值，见 §9）。
fn node(
    key: &str,
    path: &str,
    kind: SettingKind,
    doc: &str,
    order: i64,
    section: &str,
    section_doc: &str,
) -> SettingNode {
    SettingNode {
        key: key.to_owned(),
        path: path.to_owned(),
        title: String::new(),
        doc: doc.to_owned(),
        notes: Vec::new(),
        example: None,
        order,
        kind,
        required: false,
        nullable: false,
        default: None,
        has_default: false,
        min: None,
        max: None,
        exclusive_min: false,
        exclusive_max: false,
        min_length: None,
        pattern: None,
        choices: Vec::new(),
        min_items: None,
        max_items: None,
        secret: false,
        apply: ApplyScope::Hot,
        editable: true,
        deprecated: None,
        section: Some(section.to_owned()),
        section_doc: Some(section_doc.to_owned()),
        children: Vec::new(),
        element: None,
        variants: None,
        summary_fields: Vec::new(),
        value_hint: None,
    }
}

fn preset_node() -> SettingNode {
    let mut node = node(
        "preset",
        "colors.preset",
        SettingKind::Enum,
        PRESET_DOC,
        0,
        "Colors",
        COLORS_SECTION_DOC,
    );
    node.choices = vec![
        choice("wing", "为暗底终端设计的 hex 灰阶 + cyan accent（默认）"),
        choice(
            "terminal",
            "跟随终端自己的 ANSI 色（亮底终端与跟随系统主题的场景）",
        ),
    ];
    // 默认值用小写拼写（= choices 的口径）；`Deserialize` 大小写不敏感，两种都能读。
    node.default = Some(Value::from("wing"));
    node.has_default = true;
    node
}

fn slot_node(key: &str, doc: &str, default: &str, order: i64) -> SettingNode {
    let mut node = node(
        key,
        &format!("colors.{key}"),
        SettingKind::Str,
        doc,
        order,
        "Colors",
        COLORS_SECTION_DOC,
    );
    node.notes = vec![SLOT_NOTE.to_owned()];
    node.nullable = true;
    node.pattern = Some(COLOR_PATTERN.to_owned());
    node.default = Some(Value::from(default));
    node.has_default = true;
    node.value_hint = Some("color".to_owned());
    node
}

fn enum_node(
    key: &str,
    path: &str,
    doc: &str,
    order: i64,
    choices: &[(&str, &str)],
    default: &str,
) -> SettingNode {
    let mut node = node(
        key,
        path,
        SettingKind::Enum,
        doc,
        order,
        "Rendering",
        RENDERING_SECTION_DOC,
    );
    node.choices = choices.iter().map(|(v, d)| choice(v, d)).collect();
    node.default = Some(Value::from(default));
    node.has_default = true;
    node
}

fn choice(value: &str, doc: &str) -> SettingChoice {
    SettingChoice {
        value: value.to_owned(),
        doc: Some(doc.to_owned()),
    }
}

/// 预设调色板里的一个槽位（`theme_preview` 与这里共用同一份默认值）。
fn slot_color(palette: &ThemePalette, key: &str) -> Color {
    match key {
        "accent" => palette.accent,
        "text" => palette.text,
        "thinking" => palette.thinking,
        "tool_result" => palette.tool_result,
        "dim" => palette.dim,
        "success" => palette.success,
        "warning" => palette.warning,
        "danger" => palette.danger,
        "math" => palette.math,
        "surface" => palette.surface,
        "diff_add_bg" => palette.diff_add_bg,
        "diff_del_bg" => palette.diff_del_bg,
        "diff_add_bg_strong" => palette.diff_add_bg_strong,
        "diff_del_bg_strong" => palette.diff_del_bg_strong,
        other => unreachable!("unknown color slot: {other}"),
    }
}

/// `ratatui::style::Color` → 配置里能写回来的拼写（命名色或 `#rrggbb`）。
///
/// 只服务"注释掉的默认值"的展示与测试；预设里若出现 `parse_color` 表达不了的颜色
/// （今天没有），退化为 `Debug` 拼写（不可解析，但也不会写进文件——它只出现在注释里）。
fn color_to_string(color: Color) -> String {
    match color {
        Color::Reset => "reset".to_owned(),
        Color::Black => "black".to_owned(),
        Color::Red => "red".to_owned(),
        Color::Green => "green".to_owned(),
        Color::Yellow => "yellow".to_owned(),
        Color::Blue => "blue".to_owned(),
        Color::Magenta => "magenta".to_owned(),
        Color::Cyan => "cyan".to_owned(),
        Color::Gray => "gray".to_owned(),
        Color::DarkGray => "dark_gray".to_owned(),
        Color::LightRed => "light_red".to_owned(),
        Color::LightGreen => "light_green".to_owned(),
        Color::LightYellow => "light_yellow".to_owned(),
        Color::LightBlue => "light_blue".to_owned(),
        Color::LightMagenta => "light_magenta".to_owned(),
        Color::LightCyan => "light_cyan".to_owned(),
        Color::White => "white".to_owned(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    }
}

// ── emitter ─────────────────────────────────────────────────────

/// 注释列宽预算（含 `# ` 前缀）。
const COMMENT_WIDTH: usize = 78;
/// `# ── Section ──…` 分隔线的总列宽。
const SEPARATOR_WIDTH: usize = 66;

const HEADER: [&str; 4] = [
    "wing — TUI configuration (interface)",
    "File: $WING_HOME/tui/config.yaml (default: ~/.wing/tui/config.yaml)",
    "Prefer editing it in the TUI: `/settings` — one tree, both configs.",
    "A commented line is an unset default: uncomment it to pin the value.",
];

/// 密文叶子的发射模式——**同一个 emitter 的两个用途在这里分岔**。
///
/// 不是"默认值 + 例外"，而是两处调用点各自显式点名（Rust 没有默认参数，漏传 = 编译不过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpMode {
    /// 密文叶子**原样写值**——**保存路径专用**。
    ///
    /// 只有 [`crate::config::store::write_interface_doc`]（面板 `s` / setup 向导 / `app/runner.rs`
    /// 共用的唯一写盘实现）该传它：把用户密钥换成掩码字符串写进文件 = **数据损坏**。
    Raw,
    /// 密文叶子写 `null` + 掩码注释（`•••••••• 1234`，与 `wing config get` / 设置面板同一套语言）
    /// ——**展示路径专用**（`wing tui --dump-config` 的默认；`--show-secrets` 才给真值）。
    Masked,
}

/// `--show-secrets` 的完整拼写（掩码注释里的指引用它，测试也 grep 它）。
pub const SHOW_SECRETS_HINT: &str = "wing tui --dump-config --show-secrets";

/// 由 catalog 生成的规范形 YAML（带注释）。
///
/// `--dump-config` 与 [`crate::config::store::write_interface_doc`] 共用它：Rust 侧也只有一份声明。
/// 输出规则照 design.md §6.2（后端的同一套产品品味）：
///
/// 1. 文件头固定 4 行注释（产品名 / 文件位置 / "推荐用 `/settings` 编辑" / 无时间戳）；
/// 2. 按 `section` 分组，每组一个 `# ── Section ──…` 分隔行 + `section_doc`；
/// 3. 文档注释在上方（不用行尾注释）；值存在 → 原样写出；值缺席且有默认 → 写成注释掉的默认值；
///    `example` → `# e.g. …`；
/// 4. `secret` → `# 密钥：面板里只写不回显（末 4 位提示）`；[`DumpMode::Masked`] 下再跟一行
///    `# 已掩码（•••••••• 1234）——真值：…`，值本身写 `null`（见 [`DumpMode`]）；
/// 5. `apply == restart / next_session` → `# 生效：…`（hot 不加，避免噪声）；
/// 6. 引号能不加就不加（含特殊字符 / 看起来像别的类型 → 双引号 + 转义，非 ASCII 原样）；
/// 7. 缩进 2 空格；注释折行到 78 列（见模块内的列宽常量）。
///
/// 它仍是 `(doc, catalog, mode)` 的**纯函数**：不读 env、不写时间戳、不含主机信息——
/// `mode` 是参数不是环境，round-trip 与幂等照样可以被单测机械证明。
///
/// **与后端的差异**（见 `09_interface_catalog/design.md` 的差异清单）：未知键丢弃（既有
/// `--dump-config` 行为）、没有必填字段、`null` 与缺席等价，以及——
/// **一个子树没有任何用户写下的值时，整块（含容器行）注释掉**：`colors:` 带空 body 是 `null`，
/// 而 `AppConfig.colors` 不是 `Option`，裸容器行会让整份配置反序列化失败。
pub fn dump_config_yaml(doc: &Value, mode: DumpMode) -> String {
    emit_document(doc, &interface_catalog(), mode)
}

fn emit_document(doc: &Value, catalog: &SettingNode, mode: DumpMode) -> String {
    let mut out = String::new();
    for line in HEADER {
        out.push_str("# ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    let mut section: Option<&str> = None;
    for child in &catalog.children {
        if let Some(name) = child.section.as_deref()
            && section != Some(name)
        {
            // 文件头之后已经有一个空行了（N-1：第一个 section 前不再重复补，
            // 否则输出里会出现两个连着的空行）。
            if section.is_some() {
                out.push('\n');
            }
            section = Some(name);
            out.push_str(&section_header(name));
            out.push('\n');
            if let Some(section_doc) = child.section_doc.as_deref() {
                comment(&mut out, 0, section_doc);
            }
        }
        emit_node(&mut out, child, doc.get(&child.key), 0, false, mode);
    }
    out
}

/// 递归发射一个节点（及其子树）。
///
/// `commented` = 外层子树正在整块注释中（见 [`dump_config_yaml`] 的差异说明）：
/// 该模式下每一行都在缩进后加 `# `，取消注释即还原合法 YAML。
fn emit_node(
    out: &mut String,
    node: &SettingNode,
    value: Option<&Value>,
    indent: usize,
    commented: bool,
    mode: DumpMode,
) {
    let value = value.filter(|v| !v.is_null());
    if !node.doc.trim().is_empty() {
        comment(out, indent, &node.doc);
    }
    for note in &node.notes {
        if !note.trim().is_empty() {
            comment(out, indent, note);
        }
    }
    if let Some(example) = node.example.as_deref() {
        comment(out, indent, &format!("e.g. {example}"));
    }
    match node.apply {
        ApplyScope::Restart => comment(out, indent, "生效：重启 wing 后生效"),
        ApplyScope::NextSession => comment(out, indent, "生效：新会话"),
        _ => {}
    }
    if node.secret {
        comment(out, indent, "密钥：面板里只写不回显（末 4 位提示）");
        // 只有"真的掩了一个值"时才加这一行：文件里没有密文时 Masked 与 Raw 逐字节相同。
        if mode == DumpMode::Masked
            && let Some(note) = masked_note(value)
        {
            comment(out, indent, &note);
        }
    }

    match node.kind {
        SettingKind::Object => {
            let has_values = value.is_some_and(|v| v.is_object())
                && node
                    .children
                    .iter()
                    .any(|child| subtree_has_value(child, value.and_then(|v| v.get(&child.key))));
            let block = commented || !has_values;
            line(out, indent, &format!("{}:", node.key), block);
            for child in &node.children {
                emit_node(
                    out,
                    child,
                    value.and_then(|v| v.get(&child.key)),
                    indent + 2,
                    block,
                    mode,
                );
            }
        }
        _ => emit_leaf(out, node, value, indent, mode),
    }
}

/// 标量叶子（含未知 kind 的兜底）。
fn emit_leaf(
    out: &mut String,
    node: &SettingNode,
    value: Option<&Value>,
    indent: usize,
    mode: DumpMode,
) {
    let key = &node.key;
    if let Some(value) = value {
        if mode == DumpMode::Masked && node.secret && is_maskable(value) {
            // 掩码 = `null`（后端 `get` 对密文叶的口径），**不是**把 `•••••••• 1234` 当值写出去：
            // 那会是一份能解析、能写回的 YAML——重定向回来就等于把密钥替换成 8 个实心点。
            line(out, indent, &format!("{key}: null"), false);
            return;
        }
        match inline(value) {
            Some(text) => line(out, indent, &format!("{key}: {text}"), false),
            // 声明的标量拿到 list/map（手写文件的畸形值）：块输出，不静默丢数据。
            None => {
                line(out, indent, &format!("{key}:"), false);
                let rendered = serde_yaml::to_string(value).unwrap_or_default();
                for text in rendered.lines() {
                    line(out, indent + 2, text, false);
                }
            }
        }
        return;
    }
    if node.has_default
        && let Some(default) = node.default.as_ref().filter(|d| !d.is_null())
    {
        let text = inline(default).unwrap_or_default();
        line(out, indent, &format!("{key}: {text}"), true);
    } else if node.required {
        // 值缺席且必填 → 写出键 + 空值（后端 §6.2 规则 3；Interface 根今天没有这种字段）。
        let empty = empty_value(node);
        line(out, indent, &format!("{key}: {empty}"), false);
    } else {
        let empty = empty_value(node);
        line(out, indent, &format!("{key}: {empty}"), true);
    }
}

/// [`DumpMode::Masked`] 下密文叶的掩码注释；`None` = 这个值不需要掩（缺席 / `null` / 空串）。
///
/// 视觉语言与 `wing config get`（`cmd/config.rs::render_secret`）和设置面板的值列
/// （`shared/panels/settings/tree.rs::display_value` 生产 `ValueText::Masked`，`ui/settings/tree.rs` 画）
/// **同一套**：`•••••••• <末 4 位>`。
/// 短密钥（< 8）不给 hint——比例过高等于泄露（后端 `document.py::_state_of` 同一条规则）。
fn masked_note(value: Option<&Value>) -> Option<String> {
    let value = value.filter(|v| is_maskable(v))?;
    let hint = value.as_str().and_then(secret_hint);
    Some(match hint {
        Some(hint) => format!("已掩码（•••••••• {hint}）——真值：`{SHOW_SECRETS_HINT}`"),
        None => format!("已掩码（••••••••）——真值：`{SHOW_SECRETS_HINT}`"),
    })
}

/// 这个值需要掩吗？空串 / `null` 不携带密钥（原样写出去反而保住了"显式清空过"这个状态）；
/// 其余一律按密文处理（手写坏文件里的 `api_key: 123` 也掩）。
fn is_maskable(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// 密文只下发末 4 位 hint：值长度 ≥ 8 才有，否则 `None`（短密钥不给，避免泄露比例过高）。
///
/// 与后端 `config/document.py::_state_of`（`value[-4:] if len(value) >= 8 else None`）和
/// `wing-api-client` 的 `SecretState.hint` 文档是同一条规则；`DumpMode::Masked`、
/// `wing config get` 与设置面板共用这一份实现（改规则就改这里 + 后端那一处）。
pub fn secret_hint(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 8 {
        return None;
    }
    Some(chars[chars.len() - 4..].iter().collect())
}

/// 子树里有没有用户写下的值（决定 object 是正常发射还是整块注释）。
fn subtree_has_value(node: &SettingNode, value: Option<&Value>) -> bool {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return false;
    };
    match node.kind {
        SettingKind::Object => {
            value.is_object()
                && node
                    .children
                    .iter()
                    .any(|child| subtree_has_value(child, value.get(&child.key)))
        }
        SettingKind::List => value.as_array().is_some_and(|items| !items.is_empty()),
        _ => true,
    }
}

/// 标量的单行形态；json 的 list/map 返回 `None`（走块输出）。
fn inline(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(quote(s)),
        Value::Array(items) if items.is_empty() => Some("[]".to_owned()),
        Value::Object(map) if map.is_empty() => Some("{}".to_owned()),
        _ => None,
    }
}

/// 缺席 + 无默认时占位：类型的"空"（取消注释后仍是一份合法配置）。
fn empty_value(node: &SettingNode) -> &'static str {
    match node.kind {
        SettingKind::Int => "0",
        SettingKind::Float => "0.0",
        SettingKind::Bool => "false",
        SettingKind::Map | SettingKind::Object => "{}",
        SettingKind::List => "[]",
        _ => "\"\"",
    }
}

fn section_header(name: &str) -> String {
    let head = format!("# ── {name} ");
    let pad = SEPARATOR_WIDTH.saturating_sub(UnicodeWidthStr::width(head.as_str()));
    format!("{head}{}", "─".repeat(pad))
}

/// 一行 YAML：`[indent][# ]text`。
///
/// 注释指示符在缩进**之后**（与 PyYAML 风格一致：`  # accent: "#3ac6e0"`）——
/// 取消注释就是删掉那个 `#`，缩进原样保留；整块注释的子树因此可以直接整块取消注释。
fn line(out: &mut String, indent: usize, text: &str, commented: bool) {
    for _ in 0..indent {
        out.push(' ');
    }
    if commented {
        out.push_str("# ");
    }
    out.push_str(text);
    out.push('\n');
}

/// 一段注释（自动按 [`COMMENT_WIDTH`] 折行）。
fn comment(out: &mut String, indent: usize, text: &str) {
    for wrapped in wrap(text, COMMENT_WIDTH.saturating_sub(indent + 2)) {
        line(out, indent, &format!("# {wrapped}"), false);
    }
}

/// 按显示宽度折行（`unicode-width`）：先断词，超宽的整词（CJK 长串）按列硬折。
///
/// 反引号代码段（`` `…` ``）不拆词——它内部可能带空格（`` `⦁ 深度思考中 4s` ``），
/// 断在中间会让注释里的行内代码读起来像笔误。
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in words(text) {
        if !current.is_empty() {
            let fits = UnicodeWidthStr::width(current.as_str())
                + 1
                + UnicodeWidthStr::width(word.as_str())
                <= width;
            if fits {
                current.push(' ');
                current.push_str(&word);
                continue;
            }
            lines.push(std::mem::take(&mut current));
        }
        let mut rest = word.as_str();
        while !rest.is_empty() {
            let (head, tail) = split_at_width(rest, width);
            if tail.is_empty() {
                current.push_str(head);
                break;
            }
            lines.push(head.to_owned());
            rest = tail;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 断词：空白分隔，但一个未闭合的反引号代码段会把它之后的词粘在一起。
fn words(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    for word in text.split_whitespace() {
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
        if word.matches('`').count() % 2 == 1 {
            in_code = !in_code;
        }
        if !in_code {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// 在显示宽度 `width` 处切一刀（至少一个字符）。
fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if index > 0 && used + w > width {
            return (&text[..index], &text[index..]);
        }
        used += w;
    }
    (text, "")
}

/// YAML 标量：能不加引号就不加；否则双引号 + 转义（非 ASCII 原样保留）。
fn quote(value: &str) -> String {
    if needs_quotes(value) {
        format!("\"{}\"", escape(value))
    } else {
        value.to_owned()
    }
}

fn needs_quotes(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if value.starts_with(char::is_whitespace) || value.ends_with(char::is_whitespace) {
        return true;
    }
    if value.chars().any(char::is_control) {
        return true;
    }
    // 注释 / 结构指示符：保守起见，出现即引（`#ff00ff` 这类值就是这么被保护的）。
    if value.contains('#') || value.contains(": ") || value.ends_with(':') {
        return true;
    }
    if value.starts_with(|c: char| {
        matches!(
            c,
            '-' | '?'
                | ':'
                | ','
                | '['
                | ']'
                | '{'
                | '}'
                | '&'
                | '*'
                | '!'
                | '|'
                | '>'
                | '\''
                | '"'
                | '%'
                | '@'
                | '`'
        )
    }) {
        return true;
    }
    looks_numeric(value) || is_yaml_keyword(value)
}

/// 会解析成别的标量类型（数字 / bool / null）的字符串必须加引号。
fn looks_numeric(value: &str) -> bool {
    let body = value.strip_prefix(['+', '-']).unwrap_or(value);
    if body.is_empty() {
        return false;
    }
    if matches!(body, ".inf" | ".Inf" | ".INF" | ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    if let Some(rest) = body
        .strip_prefix("0x")
        .or(body.strip_prefix("0o").or(body.strip_prefix("0b")))
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_hexdigit() || c == '_')
    {
        return true;
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    let mut dot = false;
    let mut digit = false;
    for ch in mantissa.chars() {
        match ch {
            '.' if !dot => dot = true,
            c if c.is_ascii_digit() => digit = true,
            _ => return false,
        }
    }
    digit
}

fn is_yaml_keyword(value: &str) -> bool {
    matches!(
        value,
        "null"
            | "Null"
            | "NULL"
            | "~"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "yes"
            | "Yes"
            | "YES"
            | "no"
            | "No"
            | "NO"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    )
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::config::AppConfig;
    use crate::config::ColorPreset;
    use crate::config::ColorsConfig;
    use crate::config::RenderingConfig;
    use crate::config::rendering::ImagesMode;
    use crate::config::rendering::MathMode;
    use crate::config::rendering::ThinkingMode;
    use crate::config::store::appconfig_from_doc;
    use crate::config::store::parse_document;

    // ── 样本与遍历辅助 ──────────────────────────────────────────

    /// 一张"每个字段都填上值"的样本。
    ///
    /// **穷尽 struct 字面量**（不用 `..Default::default()`）：给 `AppConfig` /
    /// `ColorsConfig` / `LayoutConfig` / `RenderingConfig` 加字段会让这里**编译失败**，
    /// 于是门禁的样本不可能悄悄过期。`Option<String>` + `skip_serializing_if` 的颜色槽
    /// 只有在这个样本里才会出现在 serde 输出中（default 样本里它们是缺席的）。
    fn full_sample() -> AppConfig {
        AppConfig {
            colors: ColorsConfig {
                preset: ColorPreset::Terminal,
                accent: Some("#ff00ff".into()),
                text: Some("#ffffff".into()),
                thinking: Some("gray".into()),
                tool_result: Some("dark_gray".into()),
                dim: Some("#101010".into()),
                success: Some("green".into()),
                warning: Some("yellow".into()),
                danger: Some("red".into()),
                math: Some("magenta".into()),
                surface: Some("#343541".into()),
                diff_add_bg: Some("#113311".into()),
                diff_del_bg: Some("#331111".into()),
                diff_add_bg_strong: Some("#225522".into()),
                diff_del_bg_strong: Some("#552222".into()),
                math_mode: MathMode::Off,
            },
            layout: LayoutConfig {
                max_input_lines: 21,
                max_popup_rows: 9,
                tool_output_max: 42,
            },
            rendering: RenderingConfig {
                thinking: ThinkingMode::Hidden,
                math: MathMode::Off,
                images: ImagesMode::Off,
            },
            api_key: Some("sk-test".into()),
        }
    }

    fn json_of(cfg: &AppConfig) -> Value {
        serde_json::to_value(cfg).expect("AppConfig serializes")
    }

    /// 两个样本（全字段 + 默认）覆盖到的 serde 叶子路径。
    ///
    /// 默认样本抓住"新加的非 Option 字段"，全样本抓住"被 `skip_serializing_if` 藏起来的
    /// Option 字段"——单靠任何一个都会漏。
    fn serde_leaves() -> BTreeSet<String> {
        let mut leaves = BTreeSet::new();
        collect_leaves(&json_of(&full_sample()), "", &mut leaves);
        collect_leaves(&json_of(&AppConfig::default()), "", &mut leaves);
        leaves
    }

    fn collect_leaves(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
        match value {
            // 空 object 也算叶子（"退化节点"）：它的路径必须被声明，否则面板拿不到它。
            Value::Object(map) if !map.is_empty() => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_leaves(child, &path, out);
                }
            }
            _ => {
                out.insert(prefix.to_owned());
            }
        }
    }

    fn doc_from_text(text: &str) -> Value {
        parse_document(text, Path::new("test.yaml")).expect("document parses")
    }

    /// 测试辅助：把注释掉的 **YAML 行**取消注释（保留缩进；文档注释行不匹配 `key:` 形状）。
    fn uncomment(text: &str) -> String {
        let mut out = String::new();
        for line in text.lines() {
            let indent_len = line.len() - line.trim_start().len();
            let (indent, rest) = line.split_at(indent_len);
            let Some(entry) = rest.strip_prefix("# ") else {
                continue;
            };
            let is_entry = entry.split_once(':').is_some_and(|(key, _)| {
                !key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            });
            if !is_entry {
                continue;
            }
            out.push_str(indent);
            out.push_str(entry);
            out.push('\n');
        }
        out
    }

    // ── 门禁：catalog ⇄ serde 双向对账 ─────────────────────────

    /// 每个 serde 叶子都必须有声明。给 `AppConfig` 加字段忘了声明 → 红。
    #[test]
    fn interface_catalog_covers_every_serde_leaf() {
        let catalog = interface_catalog();
        let leaves = serde_leaves();
        for leaf in &leaves {
            assert!(catalog.node_at(leaf).is_some(), "catalog 缺声明：{leaf}");
        }
        // 门禁真的看见了 14 个颜色槽（`skip_serializing_if` 会让默认样本漏掉它们）。
        let slots = leaves
            .iter()
            .filter(|path| path.starts_with("colors."))
            .count();
        assert_eq!(slots, 15, "colors 段应覆盖 preset + 14 槽：{leaves:?}");
    }

    /// 每个声明都必须对应真实 serde 键。声明了不存在的键 → 红。
    #[test]
    fn interface_catalog_declares_no_phantom_key() {
        let catalog = interface_catalog();
        let leaves = serde_leaves();
        for node in catalog.leaves() {
            assert!(
                leaves.contains(&node.path),
                "catalog 声明了不存在的键：{}",
                node.path
            );
        }
    }

    /// 分组与 catalog 的 root 子节点**双向对账**（左列锚点不许漏项 / 不许有幻影成员）。
    #[test]
    fn interface_groups_partition_the_catalog_root() {
        let catalog = interface_catalog();
        let groups = interface_groups();
        assert_eq!(groups.len(), 1, "Interface 是一个锚点");
        let members: Vec<&str> = groups[0].members.iter().map(String::as_str).collect();
        let keys: Vec<&str> = catalog.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(members, keys, "成员 = root 子节点（顺序也一致）");
        assert!(!groups[0].title.trim().is_empty());
        assert!(!groups[0].doc.trim().is_empty());
        assert!(!groups[0].id.trim().is_empty());
    }

    // ── catalog 内容 ───────────────────────────────────────────

    #[test]
    fn the_catalog_root_matches_the_backend_spelling_and_sections() {
        let catalog = interface_catalog();
        assert_eq!(catalog.key, "interface");
        assert_eq!(catalog.path, "interface");
        assert_eq!(catalog.title, "Interface");
        assert_eq!(catalog.kind, SettingKind::Object);
        let keys: Vec<&str> = catalog
            .children
            .iter()
            .map(|child| child.key.as_str())
            .collect();
        assert_eq!(keys, ["colors", "layout", "rendering", "api_key"]);
        // 带 / 不带根名前缀两种寻址都命中（与 06 的 node_at 口径对齐）
        assert!(catalog.node_at("colors.accent").is_some());
        assert!(catalog.node_at("interface.colors.accent").is_some());
        for child in &catalog.children {
            assert!(
                child.section.is_some(),
                "{} 缺 section（emitter 分组会漏）",
                child.key
            );
            assert!(child.section_doc.is_some(), "{} 缺 section_doc", child.key);
        }
    }

    #[test]
    fn every_color_slot_is_declared_with_a_color_hint() {
        let catalog = interface_catalog();
        let palette = ThemePalette::default();
        assert_eq!(SLOT_DOCS.len(), 14, "颜色槽的清单被改过？");
        for (key, _) in SLOT_DOCS {
            let path = format!("colors.{key}");
            let node = catalog
                .node_at(&path)
                .unwrap_or_else(|| panic!("缺声明：{path}"));
            let expected = color_to_string(slot_color(&palette, key));
            assert_eq!(node.kind, SettingKind::Str, "{path}");
            assert!(node.nullable, "{path} 必须 nullable（缺席 = 跟随 preset）");
            assert_eq!(
                node.value_hint.as_deref(),
                Some("color"),
                "{path} 缺 color hint"
            );
            assert!(!node.secret, "{path}");
            assert!(node.pattern.is_some(), "{path} 缺 pattern");
            assert!(node.has_default, "{path}");
            assert_eq!(node.default, Some(Value::from(expected.clone())), "{path}");
            // 默认值必须是 `parse_color` 认得的拼写（取消注释后就是一份有效配置）
            assert!(
                crate::config::colors::parse_color(&expected).is_some(),
                "{path} 的默认值不可解析：{expected}"
            );
        }
        // 颜色槽之外的字段不带预览提示
        assert_eq!(catalog.node_at("colors.preset").unwrap().value_hint, None);
        assert_eq!(
            catalog
                .node_at("layout.max_input_lines")
                .unwrap()
                .value_hint,
            None
        );
        assert_eq!(catalog.node_at("api_key").unwrap().value_hint, None);
    }

    #[test]
    fn the_api_key_is_a_secret_that_waits_for_a_restart() {
        let catalog = interface_catalog();
        let node = catalog.node_at("api_key").expect("api_key");
        assert_eq!(node.kind, SettingKind::Secret);
        assert!(node.secret);
        assert!(node.nullable);
        assert!(!node.has_default);
        assert_eq!(node.default, None);
        assert_eq!(node.example.as_deref(), Some(API_KEY_EXAMPLE));
        // 只在建连时读一次：保存它不会重连（见 catalog 声明处的注释）
        assert_eq!(node.apply, ApplyScope::Restart);
    }

    #[test]
    fn the_layout_bounds_and_defaults_come_from_the_real_types() {
        let defaults = LayoutConfig::default();
        let catalog = interface_catalog();
        let cases = [
            ("max_input_lines", defaults.max_input_lines),
            ("max_popup_rows", defaults.max_popup_rows),
            ("tool_output_max", defaults.tool_output_max),
        ];
        for (key, expected) in cases {
            let path = format!("layout.{key}");
            let node = catalog.node_at(&path).expect("layout key");
            assert_eq!(node.kind, SettingKind::Int, "{path}");
            assert!(node.has_default, "{path}");
            assert_eq!(node.default, Some(Value::from(expected)), "{path}");
            assert_eq!(node.min, Some(1.0), "{path} 的下界是 1");
            assert!(!node.exclusive_min, "{path}");
            assert_eq!(node.max, None, "{path}");
        }
    }

    #[test]
    fn the_enum_choices_are_lowercase_and_contain_their_defaults() {
        let catalog = interface_catalog();
        let cases: [(&str, &[&str], &str); 4] = [
            ("colors.preset", &["wing", "terminal"], "wing"),
            ("rendering.thinking", &["visible", "hidden"], "visible"),
            ("rendering.math", &["text", "off"], "text"),
            ("rendering.images", &["auto", "off"], "auto"),
        ];
        for (path, expected, default) in cases {
            let node = catalog
                .node_at(path)
                .unwrap_or_else(|| panic!("缺声明：{path}"));
            assert_eq!(node.kind, SettingKind::Enum, "{path}");
            let values: Vec<&str> = node.choices.iter().map(|c| c.value.as_str()).collect();
            assert_eq!(values, expected, "{path} 的 choices");
            for choice in &node.choices {
                assert_eq!(choice.value, choice.value.to_ascii_lowercase(), "{path}");
                assert!(choice.doc.is_some(), "{path} 的 choices 要带含义");
            }
            // 默认值就在 choices 里（面板高亮当前选项时不用做拼写归一化）
            assert_eq!(node.default, Some(Value::from(default)), "{path}");
        }
    }

    #[test]
    fn the_catalog_has_the_expected_shape() {
        let catalog = interface_catalog();
        let leaves: Vec<&SettingNode> = catalog.leaves().collect();
        let paths: Vec<&str> = leaves.iter().map(|node| node.path.as_str()).collect();
        assert_eq!(leaves.len(), 22, "{paths:?}");
        fn count(leaves: &[&SettingNode], predicate: impl Fn(&SettingNode) -> bool) -> usize {
            leaves.iter().filter(|node| predicate(node)).count()
        }
        assert_eq!(count(&leaves, |node| node.kind == SettingKind::Enum), 4);
        assert_eq!(count(&leaves, |node| node.secret), 1);
        assert_eq!(
            count(&leaves, |node| node.value_hint.as_deref() == Some("color")),
            14
        );
        assert_eq!(count(&leaves, |node| node.required), 0);
        assert_eq!(count(&leaves, |node| node.apply == ApplyScope::Restart), 1);
        assert_eq!(count(&leaves, |node| node.apply == ApplyScope::Hot), 21);
        assert!(leaves.iter().all(|node| node.editable));
    }

    // ── emitter ────────────────────────────────────────────────

    /// 全字段 round-trip：`doc → dump → 解析 → doc' → dump`，doc 与文本都逐字相同。
    #[test]
    fn a_fully_populated_config_round_trips_byte_for_byte() {
        let doc = json_of(&full_sample());
        let dumped = dump_config_yaml(&doc, DumpMode::Raw);
        let parsed = appconfig_from_doc(&doc_from_text(&dumped));
        let parsed_doc = json_of(&parsed);
        assert_eq!(parsed_doc, doc, "解析回来的文档必须逐值相同");
        assert_eq!(
            dump_config_yaml(&parsed_doc, DumpMode::Raw),
            dumped,
            "再 dump 必须逐字相同"
        );
    }

    /// 空文档 = 纯注释模板：没有任何键真的被写下，解析回来就是默认值。
    #[test]
    fn an_empty_document_dumps_a_commented_template() {
        let dumped = dump_config_yaml(&json!({}), DumpMode::Raw);
        let read_back = doc_from_text(&dumped);
        assert_eq!(read_back, json!({}), "全是注释：一个键都不该被写下");
        let parsed = appconfig_from_doc(&read_back);
        assert_eq!(json_of(&parsed), json_of(&AppConfig::default()));
        assert_eq!(parsed.colors.preset, ColorPreset::Wing);
    }

    /// N-1（09 review_r1）：文件头与第一个 section 之间**恰有一个**空行。
    #[test]
    fn the_header_is_followed_by_exactly_one_blank_line() {
        let dumped = dump_config_yaml(&json!({}), DumpMode::Raw);
        let mut lines = dumped.lines();
        // 文件头 4 行。
        for _ in 0..HEADER.len() {
            assert!(lines.next().is_some_and(|l| l.starts_with("# ")));
        }
        assert_eq!(lines.next(), Some(""), "文件头后是一个空行");
        let first_section = lines.next().expect("至少一个 section");
        assert!(
            first_section.starts_with("# ── "),
            "第二个非头行就是第一个 section 分隔线，实际：{first_section:?}"
        );
        // 反向：整篇不许出现连续两个空行。
        assert!(
            !dumped.contains("\n\n\n"),
            "输出里不该有连续两个空行：\n{dumped}"
        );
    }

    /// 稀疏文档保持稀疏：只有写下的键回到文档里，再 dump 逐字相同。
    #[test]
    fn a_sparse_document_stays_sparse_through_a_dump() {
        let doc = json!({"colors": {"accent": "#ff00ff"}});
        let dumped = dump_config_yaml(&doc, DumpMode::Raw);
        let read_back = doc_from_text(&dumped);
        assert_eq!(read_back, doc);
        assert_eq!(dump_config_yaml(&read_back, DumpMode::Raw), dumped);
    }

    /// 一个子树没有任何写下的值 → 整块注释（含容器行）。
    ///
    /// 裸容器键（`layout:` 带空 body）是 YAML 的 `null`，而 `AppConfig.layout` 不是 `Option`：
    /// 那种文件会让整份配置反序列化失败——整块注释就是为了不让它出现。
    #[test]
    fn an_absent_subtree_is_commented_as_one_block() {
        let empty = dump_config_yaml(&json!({}), DumpMode::Raw);
        assert!(empty.lines().any(|line| line == "# layout:"), "{empty}");
        assert!(!empty.lines().any(|line| line == "layout:"), "{empty}");

        let pinned = dump_config_yaml(&json!({"layout": {"max_input_lines": 25}}), DumpMode::Raw);
        assert!(pinned.lines().any(|line| line == "layout:"));
        assert!(pinned.lines().any(|line| line == "  max_input_lines: 25"));
        assert!(pinned.lines().any(|line| line == "  # max_popup_rows: 8"));
        let read_back = doc_from_text(&pinned);
        assert_eq!(read_back, json!({"layout": {"max_input_lines": 25}}));
        assert_eq!(appconfig_from_doc(&read_back).layout.max_input_lines, 25);

        // 反例（这只是文档，说明为什么整块注释是必须的）：
        assert!(
            serde_json::from_value::<AppConfig>(json!({"layout": null})).is_err(),
            "空 body 的容器键 = null，不是一份可解析的配置"
        );
    }

    /// 注释来自 catalog：分组、字段 doc / notes / example / 密文 / 生效域 / 注释掉的默认值。
    #[test]
    fn the_comments_come_from_the_catalog() {
        let dumped = dump_config_yaml(&json!({}), DumpMode::Raw);
        for section in [
            "# ── Colors",
            "# ── Layout",
            "# ── Rendering",
            "# ── Gateway",
        ] {
            assert!(dumped.contains(section), "缺分组：{section}\n{dumped}");
        }
        // ⚠ 这些字符串是 `config/mod.rs` 字段文档的**拷贝**：改那边时也要改 catalog
        //（门禁只保键集，文案靠这两处对齐）。
        assert!(dumped.contains("# Brand color: logo, commands, links, code, popups."));
        assert!(dumped.contains("# Maximum input area lines."));
        assert!(dumped.contains("# API key for gateway authentication."));
        // catalog 自己的 notes / 产品文案
        assert!(dumped.contains("# 缺席 = 跟随 `preset`。"));
        assert!(dumped.contains("# 配色：`preset` 选定底色，其余每个槽位键覆盖该槽位。"));
        assert!(dumped.contains("# e.g. sk-xxx"));
        assert!(dumped.contains("# 密钥：面板里只写不回显（末 4 位提示）"));
        assert!(dumped.contains("# 生效：重启 wing 后生效"));
        assert!(!dumped.contains("生效：hot"));
        // 注释掉的默认值（小写拼写 = choices 口径）
        assert!(dumped.contains("  # preset: wing"));
        assert!(dumped.contains("  # accent: \"#3ac6e0\""));
        assert!(dumped.contains("  # max_input_lines: 10"));
        assert!(dumped.contains("  # thinking: visible"));
        assert!(dumped.contains("# api_key: \"\""));
    }

    /// 引号策略：能不加就不加；需要时双引号 + 转义；非 ASCII 原样。全部 round-trip。
    #[test]
    fn values_are_quoted_only_when_needed_and_always_round_trip() {
        assert_eq!(quote("#ff00ff"), "\"#ff00ff\"");
        assert_eq!(quote("cyan"), "cyan");
        assert_eq!(quote("dark_gray"), "dark_gray");
        assert_eq!(quote("sk-abc123"), "sk-abc123");
        assert_eq!(quote("密钥"), "密钥");
        assert_eq!(quote("10"), "\"10\"");
        assert_eq!(quote("true"), "\"true\"");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a: b"), "\"a: b\"");
        assert_eq!(quote(" trailing"), "\" trailing\"");

        let tricky = [
            "#ff00ff",
            "a: b",
            "10",
            "true",
            "",
            " with spaces ",
            "密钥",
            "sk-abc123",
            "a\"b",
            "a\nb",
            "::",
            "[x]",
            "yes",
            "0x10",
            "-lead",
            "trailing:",
            "a#b",
            "~",
        ];
        for value in tricky {
            let doc = json!({ "api_key": value });
            let dumped = dump_config_yaml(&doc, DumpMode::Raw);
            let read_back = doc_from_text(&dumped);
            assert_eq!(read_back, doc, "值 {value:?} 没有 round-trip：\n{dumped}");
        }
    }

    /// 声明的标量拿到 list/map（手写文件的畸形值）也不静默丢：块输出，解析回来还是它。
    #[test]
    fn a_non_scalar_value_falls_back_to_a_block_but_is_not_lost() {
        let doc = json!({"api_key": ["a", "b"]});
        let dumped = dump_config_yaml(&doc, DumpMode::Raw);
        assert_eq!(doc_from_text(&dumped), doc);
        let doc = json!({"api_key": {}});
        assert_eq!(doc_from_text(&dump_config_yaml(&doc, DumpMode::Raw)), doc);
    }

    /// 注释行不超过列宽预算（长文档注释真的被折了）。
    #[test]
    fn comment_lines_stay_within_the_column_budget() {
        let dumped = dump_config_yaml(&json!({}), DumpMode::Raw);
        for line in dumped.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= COMMENT_WIDTH,
                "超宽：{line}"
            );
        }
        let quiet: Vec<&str> = dumped
            .lines()
            .filter(|line| line.contains("Quiet register") || line.contains("secondary"))
            .collect();
        assert!(quiet.len() >= 2, "长注释没有被折行：{quiet:?}");
    }

    /// 小写 choices 拼写的端到端 round-trip；同时确认既有的大小写不敏感解析没被动过。
    #[test]
    fn a_document_written_with_the_lowercase_choice_spellings_round_trips() {
        let doc = json!({
            "colors": {"preset": "terminal", "accent": "#ff00ff"},
            "layout": {"max_input_lines": 25},
            "rendering": {"thinking": "hidden", "math": "off", "images": "off"},
        });
        let dumped = dump_config_yaml(&doc, DumpMode::Raw);
        assert!(dumped.contains("preset: terminal"), "{dumped}");
        assert!(dumped.contains("thinking: hidden"), "{dumped}");
        let parsed = appconfig_from_doc(&doc_from_text(&dumped));
        assert_eq!(parsed.colors.preset, ColorPreset::Terminal);
        assert_eq!(parsed.rendering.thinking, ThinkingMode::Hidden);
        assert_eq!(parsed.rendering.math, MathMode::Off);
        assert_eq!(parsed.rendering.images, ImagesMode::Off);
        assert_eq!(parsed.colors.accent.as_deref(), Some("#ff00ff"));
        assert_eq!(
            parsed.colors.math_mode,
            MathMode::Off,
            "resolve() 折进了载体字段"
        );

        // `Serialize` 的变体名（`Wing` / `Hidden` / `Off` / `Auto`）也读得回来——这是既有行为，
        // 也正是"文件里已经写下的值原样输出"能 round-trip 的原因。
        let parse = |yaml: &str| -> AppConfig { serde_yaml::from_str(yaml).expect("yaml") };
        assert_eq!(
            parse("colors:\n  preset: Wing\n").colors.preset,
            ColorPreset::Wing
        );
        assert_eq!(
            parse("colors:\n  preset: terminal\n").colors.preset,
            ColorPreset::Terminal
        );
        assert_eq!(
            parse("rendering:\n  thinking: Visible\n")
                .rendering
                .thinking,
            ThinkingMode::Visible
        );
        assert_eq!(
            parse("rendering:\n  thinking: hidden\n").rendering.thinking,
            ThinkingMode::Hidden
        );
        assert_eq!(
            parse("rendering:\n  math: Text\n").rendering.math,
            MathMode::Text
        );
        assert_eq!(
            parse("rendering:\n  math: off\n").rendering.math,
            MathMode::Off
        );
        assert_eq!(
            parse("rendering:\n  images: Auto\n").rendering.images,
            ImagesMode::Auto
        );
        assert_eq!(
            parse("rendering:\n  images: off\n").rendering.images,
            ImagesMode::Off
        );
    }

    /// 取消注释注释掉的默认值 = 一份合法配置，且**生效值**就是默认值
    /// （`api_key` 的占位是空串——语义等于未设置：空 → 不发鉴权头）。
    #[test]
    fn uncommenting_the_commented_defaults_yields_the_default_config() {
        let dumped = dump_config_yaml(&json!({}), DumpMode::Raw);
        let uncommented = uncomment(&dumped);
        assert!(uncommented.contains("colors:"), "{uncommented}");
        assert!(uncommented.contains("preset: wing"), "{uncommented}");
        let parsed = appconfig_from_doc(&doc_from_text(&uncommented));

        assert_eq!(parsed.colors.preset, ColorPreset::Wing);
        assert_eq!(parsed.rendering.thinking, ThinkingMode::Visible);
        assert_eq!(parsed.rendering.math, MathMode::Text);
        assert_eq!(parsed.rendering.images, ImagesMode::Auto);
        assert_eq!(parsed.api_key.as_deref(), Some(""));
        assert_eq!(
            parsed.layout.max_input_lines,
            LayoutConfig::default().max_input_lines
        );
        assert_eq!(
            parsed.layout.max_popup_rows,
            LayoutConfig::default().max_popup_rows
        );
        assert_eq!(
            parsed.layout.tool_output_max,
            LayoutConfig::default().tool_output_max
        );
        // 14 个槽位全部钉住 → 解析出的调色板必须与默认调色板逐槽相等。
        let got = ThemePalette::from_config(&parsed.colors);
        let want = ThemePalette::default();
        for (name, got, want) in [
            ("accent", got.accent, want.accent),
            ("text", got.text, want.text),
            ("thinking", got.thinking, want.thinking),
            ("tool_result", got.tool_result, want.tool_result),
            ("dim", got.dim, want.dim),
            ("success", got.success, want.success),
            ("warning", got.warning, want.warning),
            ("danger", got.danger, want.danger),
            ("math", got.math, want.math),
            ("surface", got.surface, want.surface),
            ("diff_add_bg", got.diff_add_bg, want.diff_add_bg),
            ("diff_del_bg", got.diff_del_bg, want.diff_del_bg),
            (
                "diff_add_bg_strong",
                got.diff_add_bg_strong,
                want.diff_add_bg_strong,
            ),
            (
                "diff_del_bg_strong",
                got.diff_del_bg_strong,
                want.diff_del_bg_strong,
            ),
        ] {
            assert_eq!(got, want, "{name}");
        }
    }

    /// 未知键（今天的 `--dump-config` 一直如此）与 `null` 值都不写出文件。
    #[test]
    fn unknown_keys_and_nulls_are_dropped() {
        let doc = json!({
            "colors": {"accent": null, "unknown_slot": "#123456"},
            "unknown_top": 1,
        });
        let dumped = dump_config_yaml(&doc, DumpMode::Raw);
        assert!(!dumped.contains("unknown_top"));
        assert!(!dumped.contains("unknown_slot"));
        // `null` 与缺席等价：accent 回到注释掉的默认值
        assert!(dumped.contains("  # accent: \"#3ac6e0\""));
        assert_eq!(doc_from_text(&dumped), json!({}));
        // 空对象同样等价于缺席（整块回到注释状态）
        assert_eq!(
            doc_from_text(&dump_config_yaml(&json!({"colors": {}}), DumpMode::Raw)),
            json!({})
        );
    }

    // ── 密文发射（`DumpMode`） ──────────────────────────────────

    /// 含密文的样本：一份真值 + 一份短到不该给 hint 的。
    const SECRET: &str = "sk-dump-secret-9999";

    fn doc_with_secret() -> Value {
        json!({"api_key": SECRET, "colors": {"accent": "#ff00ff"}})
    }

    /// `Masked`（= `--dump-config` 的默认）不写出真值，但保留掩码 + 末 4 位 + 出口指引。
    #[test]
    fn masked_mode_hides_the_secret_and_points_at_show_secrets() {
        let masked = dump_config_yaml(&doc_with_secret(), DumpMode::Masked);
        assert!(
            !masked.contains(SECRET),
            "掩码后的 dump 不许带明文：\n{masked}"
        );
        assert!(
            !masked.contains("secret-9999"),
            "连片段都不该出现：\n{masked}"
        );
        // 视觉语言与 `wing config get` / 设置面板同一套。
        assert!(masked.contains("已掩码（•••••••• 9999）"), "{masked}");
        assert!(
            masked.contains(SHOW_SECRETS_HINT),
            "掩码注释必须给出真值的出口：\n{masked}"
        );
        // 值本身是 null（后端 `get` 对密文叶的口径），不是 bullets 字符串。
        assert!(masked.lines().any(|l| l == "api_key: null"), "{masked}");
        // 同一份文档里的非密文值照旧。
        assert!(masked.contains("accent: \"#ff00ff\""), "{masked}");
        // 掩码输出**不是** round-trip artifact，这条测试把代价钉死：`api_key: null` 解析回来是
        // "缺席"（`null` 与缺席等价，本模块既有口径），所以再 dump 一次会回到注释掉的模板。
        // 要保住真值必须 `--show-secrets`（`raw_mode_writes_the_secret_verbatim_and_round_trips`）。
        let again = dump_config_yaml(&doc_from_text(&masked), DumpMode::Masked);
        assert!(!again.contains(SECRET), "{again}");
        assert!(
            !again.contains("已掩码"),
            "没有值可掩时不该再出现掩码行：\n{again}"
        );
        assert!(again.lines().any(|l| l == "# api_key: \"\""), "{again}");
    }

    /// `Raw`（= 保存路径 / `--show-secrets`）写出真值，round-trip 逐字相同。
    #[test]
    fn raw_mode_writes_the_secret_verbatim_and_round_trips() {
        let doc = doc_with_secret();
        let raw = dump_config_yaml(&doc, DumpMode::Raw);
        assert!(raw.contains(&format!("api_key: {SECRET}")), "{raw}");
        assert!(!raw.contains('•'), "Raw 模式不该出现掩码：\n{raw}");
        assert_eq!(doc_from_text(&raw), doc, "Raw 必须逐值 round-trip");
        assert_eq!(
            dump_config_yaml(&doc_from_text(&raw), DumpMode::Raw),
            raw,
            "Raw 必须逐字幂等"
        );
    }

    /// **重定向回来的安全性**：掩码后的文本解析回来，密文键必须是 `null`。
    ///
    /// 若把 `•••••••• 9999` 当值写出去，`dump > f` 之后再 `cp f 回去` 就会把用户的密钥
    /// 替换成 8 个实心点——静默毁密钥。这条测试钉住那个形态没有被选。
    #[test]
    fn a_masked_secret_is_null_so_a_redirected_dump_never_writes_the_mask_back() {
        let masked = dump_config_yaml(&doc_with_secret(), DumpMode::Masked);
        let parsed = doc_from_text(&masked);
        assert_eq!(parsed.get("api_key"), Some(&Value::Null), "{parsed}");
        assert_eq!(
            json_of(&appconfig_from_doc(&parsed)).get("api_key"),
            Some(&Value::Null),
            "掩码文档解析成配置后 api_key 是 None（= 不发 auth header），不是掩码字符串"
        );
    }

    /// 文件里没有密文值时，`Masked` 与 `Raw` 逐字节相同——mode 是惰性的，
    /// 空文档 dump 出来的模板因此与掩码前一字不差。
    #[test]
    fn the_mode_is_inert_when_the_document_holds_no_secret_value() {
        let docs = [
            json!({}),
            json!({"colors": {"accent": "#ff00ff"}}),
            json!({"api_key": ""}),   // 显式清空过：空值不携带密钥，原样写出
            json!({"api_key": null}), // 与缺席等价
        ];
        for doc in docs {
            assert_eq!(
                dump_config_yaml(&doc, DumpMode::Masked),
                dump_config_yaml(&doc, DumpMode::Raw),
                "{doc}"
            );
        }
        // 空文档 dump 出来的仍是全注释模板（含 `# 密钥：…` 那行）。
        let empty = dump_config_yaml(&json!({}), DumpMode::Masked);
        assert!(empty.lines().any(|l| l == "# api_key: \"\""));
        assert!(empty.contains("# 密钥：面板里只写不回显（末 4 位提示）"));
    }

    /// 短密钥（< 8）既不出现在输出里，也不给 hint（比例过高 = 泄露）。
    #[test]
    fn a_short_secret_gets_no_hint() {
        let masked = dump_config_yaml(&json!({"api_key": "q7Z"}), DumpMode::Masked);
        assert!(!masked.contains("q7Z"), "{masked}");
        assert!(masked.contains("已掩码（••••••••）"), "{masked}");
        assert!(masked.lines().any(|l| l == "api_key: null"), "{masked}");
    }

    /// 非字符串的密文值（手写坏文件）也照掩——它是个值，就按密文处理。
    #[test]
    fn a_non_string_secret_value_is_masked_too() {
        let masked = dump_config_yaml(&json!({"api_key": 123456789}), DumpMode::Masked);
        assert!(!masked.contains("123456789"), "{masked}");
        assert!(masked.lines().any(|l| l == "api_key: null"), "{masked}");
    }

    #[test]
    fn secret_hint_needs_eight_chars() {
        assert_eq!(secret_hint("12345678"), Some("5678".into()));
        assert_eq!(secret_hint("1234567"), None);
        assert_eq!(secret_hint("sk-abcdefgh"), Some("efgh".into()));
        assert_eq!(secret_hint(""), None);
        // 非 ASCII：按字符数算，且不切坏字符（后端 `len()` / `value[-4:]` 同语义）。
        assert_eq!(secret_hint("密钥密钥密钥密钥"), Some("密钥密钥".into()));
        assert_eq!(secret_hint("密钥"), None);
    }
}
