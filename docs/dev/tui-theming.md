# TUI 明暗主题（浅色终端支持）

一句话：TUI 的底色是终端的，我们只负责「在给定底色上可读且好看」。现在补上浅色底这份投影，并让默认配置自己认出终端是深是浅。

## 1. 问题

浅色终端下不是「略差」，是不可读：

- `ColorPreset::Dark`（当时的默认 `Wing`）的 16 个前景槽全按深底校准：白底上 15 个低于 WCAG 4.5:1，12 个连 3.0 都过不了。`text` `#e6eaf2` 在白底 1.21 -- 正文、tool 名、placeholder、settings 正文全部消失。
- 少数自绘背景是绝对深色：用户消息卡 `surface` `#232d42`（整行深色横杠）、四个 diff tint、toast 三种底（且不设前景，白底终端下 1.09）、popup 选中行（`dim` 当底 + 继承终端默认前景，2.47）。
- `is_light_theme` 判据自指：读 `palette.text`（默认 preset 自己就是浅色），默认配置 + 白底永远判「暗底」。wordmark 渐变、thinking 刷光、`heading_bright` 三处亮底分支从未在开箱配置下执行过。
- 曾评估过「跟随终端 ANSI」的 `terminal` 预设：`text: Color::White` 绑死 ANSI 15，在 Solarized Light / Tomorrow / xterm 默认下是 1.08 / 1.00 / 1.00，10~11 个槽不合格；`surface` 与 diff tint 仍是写死的深色。它同时让设置里的选项变成四个、命名不解释用途 —— 最终没有保留（见 §2）。

好消息：视觉权重几乎全部由 `ThemePalette` 的 21 个颜色槽（20 个 YAML 键 + 派生的 `heading_bright`）承担（约 450 处消费），硬编码只剩 5 个岛（dark 预设本体、4 处 syntect、toast 三色、海鸥品牌色、刷光端点）。

## 2. 决策

1. `colors.preset: auto | dark | light`，**默认 `auto`**。`auto` = 启动时探测终端背景，亮→`light`、暗→`dark`、探测失败→`dark`（现状，不猜）。
2. **运行时跟随（DECSET 2031）二期做**。一期只解决「启动时正确」，不碰 `stdin_proxy` 的拆接驳时序。
3. 只有三个值、每个值都直接解释自己：`auto` = 跟随终端、`dark` = 设计暗色、`light` = 设计浅色（旧名 `wing` 仍可读，等价 `dark`；已移除的 `terminal` 回落到 `auto`）。
4. 品牌图形（海鸥 / WING 字标）按 appearance 出两套，同一份像素网格。

## 3. 接口

### 3.1 配置

```yaml
colors:
  preset: auto            # auto | dark | light
  # 逐槽覆盖：跨 preset 生效（现状语义不变）
```

- `Auto` 的解析结果不落盘；每次启动重新探测。
- `--dump-config` / `wing config` 读写 `preset` 原值（`auto`），不写解析结果。

### 3.2 领域类型

```rust
pub enum ColorPreset { Auto, Dark, Light }   // 默认 Auto

/// 调色板自身的明暗属性 -- 显式，不从任何颜色反推。
pub enum Appearance { Dark, Light }

pub struct ThemePalette {
    // ...现有 24 槽不变...
    pub appearance: Appearance,
    pub syntax: SyntaxTheme,        // Dark | Light
}
```

- `ThemePalette::from_config(&ColorsConfig)` 只处理显式 preset（`Dark`/`Light`）；`Auto` 先由调用点解析成 `Dark`/`Light` 再传入，避免领域类型知道终端探测。
- 删除两处 `is_light_theme`（`config/mod.rs` 与 `ui/shimmer.rs`），改读 `palette.appearance`。混色方向、刷光峰值、渐变方向统一读它。

### 3.3 检测

落点：`crates/wing/src/tui/appearance.rs`（模块 `tui::appearance`）。

```rust
/// OSC 11 背景色查询。非 tty / 终端不答 / 应答不认识 / 超时 → None（调用方回退 `Dark`）。
pub fn detect(timeout: Duration) -> Option<Appearance>;
/// 进程级一次；按 preset 决定要不要问，答案存静态。
pub fn probe_once(preset: ColorPreset) -> Option<Appearance>;
/// 已知答案（没探过 / 没答案 = None）。
pub fn detected_appearance() -> Option<Appearance>;
pub(crate) fn parse_osc11(bytes: &[u8]) -> Option<Appearance>;   // 纯解析，可单测
```

- **位置**：`tui::begin_tui_session` 内、`enable_raw_mode()` 之后、`stdin_proxy::install()` 之前 —— 读应答要非规范模式，而接驳之后 fd 0 就不再是真实 tty。探测结果进进程级 `OnceLock`，第二段 TUI（向导 → 主 TUI）与设置面重算都读同一份。
- **只在 `preset == Auto` 时发查询**：别的 preset 零查询、零开销（`begin_tui_session` 把 `AppConfig::load().colors.preset` 交给 `probe_once`，非 `auto` 一个字节都不发）。设置面板把 preset 改成 `auto` 时，`detected_appearance()` 返回的就是启动时那一份；启动时不是 `auto` 的那次运行里它是 `None`（回退 `Dark`），下次启动才探测。
- **一问一答**：查询 `ESC]11;?\x07`（写 stdout + flush）；应答 `ESC]11;rgb:RRRR/GGGG/BBBB`，终止符 BEL 或 ST（`ESC \`），每通道 1~4 位 hex（`f` 与 `ffff` 同量程）。应答前后的杂字节（窗口内键入的按键）不影响解析：按 `ESC]11;` 定位。
- **判据**：WCAG 相对亮度（sRGB 线性化 + `0.2126/0.7152/0.0722` 加权），**> 0.5 判亮底**。注意 `#808080` 这类中灰落在暗侧（亮度 0.216）—— 按通道平均值判会给出相反的结论。
- **超时 150 ms**（`appearance::DETECT_TIMEOUT`），读侧是 `poll(2)` + 一次 `read`；累积上限 128 字节（超了说明进来的不是应答，放弃）。非 Unix 平台没有这一层（`detect` 直接 `None`）。
- **窗口内的 termios**：fd 0 临时切到「非规范 + 不回显」（保留 `ISIG`，Ctrl+C 仍是中断），`Drop` 原样写回。窗口内键入的按键会被读走 —— 与图像能力探测（≤500 ms）同款取舍，且这是 fd 0 还没换手的唯一时点。
- 结果写一行 `tracing::debug!`（`terminal background probe`）：用户报「浅色终端还是暗色」时，看得出是没探到还是探到了暗底。

### 3.4 CLI（`wing ps`）

- `AppConfig::load()` 之后按 preset 解析。`auto` 且**表格真的带颜色**时做一次探测；`--json` / stdout 非 tty / `NO_COLOR` / `TERM=dumb` 不探测（那些输出没有 ANSI 墨色，探了也不影响画面），按 `dark` 输出。
- **只覆盖 `wing ps`**（含 `--watch`）：其余表格命令（`branch` / `cache` / `query` / `wake` / 退出摘要）仍走 `from_config`，`auto` 在那里等价于 `Dark` —— 浅色终端下要正确颜色请显式写 `preset: light`，或等后续波次把探测接进同一处 CLI 入口。
- 与 TUI 共用 `ThemePalette::from_config_with_appearance`，不新增颜色路径。

## 4. light 调色板

设计原则：沿用 dark 的色相族，明度按白底重解；正文留余量（≥7:1），次要 ≥4.5，装饰 ≥3.0。逐槽覆盖仍然生效。

| 槽 | light | 白底对比度 |
|---|---|---|
| `text` | `#1f2328` | 15.80 |
| `thinking` | `#4b5563` | 7.56 |
| `tool_result` | `#57606a` | 6.39 |
| `dim` | `#626a73` | 5.49 |
| `accent` | `#0e7490` | 5.36 |
| `success` | `#1a7f37` | 5.08 |
| `warning` | `#8a5a00` | 5.93 |
| `danger` | `#b42318` | 6.57 |
| `math` / `heading` | `#6d28d9` | 7.10 |
| `heading_bright` | `#5b21b6` | 8.98 |
| `inline_code` | `#a4133c` | 7.67 |
| `marker` | `#5b6b8c` | 5.35 |
| `quote` | `#646b76` | 5.38 |
| `thinking_label` | `#8a5a00` | 5.93 |
| `strong` | `#7c4a03` | 7.40 |
| `surface` | `#e7ecf3` | 底：1.21 / 其上 text 13.31 |
| `diff_add_bg` | `#e6f4ea` | 1.14 / 13.91 |
| `diff_del_bg` | `#fdecec` | 1.14 / 13.83 |
| `diff_add_bg_strong` | `#c6ead0` | 1.31 / 12.10 |
| `diff_del_bg_strong` | `#f9c9c9` | 1.48 / 10.70 |

语法高亮按 appearance 配对：

- `Dark` → syntect `base16-ocean.dark`（现状）。
- `Light` → syntect `InspiredGitHub`（正文 12.8、字符串 10.6、关键字 7.1、函数 5.4；注释 2.9 属可接受的次要信息）。

## 5. 渲染层改动

| 位置 | 改动 |
|---|---|
| `config/mod.rs` | `ColorPreset` 三值；`Appearance`；light palette；`ALL/as_str/parse` 单一来源 |
| `render/syntax.rs` | 4 处 `base16-ocean.dark` → 按 `palette.syntax` 选主题 |
| `ui/toast.rs` | 三种底按 appearance 选（浅色下浅底），正文显式设 `text` |
| `ui/popup/selection.rs` | 选中行统一为「`surface` 底 + `text` 前景」；删除 ANSI `Gray` 描述色 |
| `ui/cells/model_picker.rs` | 选中色带与 selection 同语义（`surface` + `text`） |
| `ui/welcome/{sprite,mod}.rs` | 海鸥按 appearance 出两套羽色（同一像素网格）；wordmark 渐变方向改为读 `appearance`（`light` 参数早已存在） |
| `ui/cells/thinking.rs` | 刷光峰值方向读 `appearance` |
| `ui/shimmer.rs` | 删除 `is_light_theme`（自指判据）；混色仍以 `to_rgb` 兜底 |
| `ui/cells/todo_msg.rs` | 浅色下不再给已安静的槽叠 `Modifier::DIM` |
| `cmd/setup.rs` + `config/catalog.rs` | preset 三值及文案（目录驱动：向导与设置面板同一份声明）；向导每帧的预览调色板改走 `from_config_with_appearance`，吃 §3.3 的探测结果 |
| `examples/theme_preview.rs` | 支持 `--preset light` 与白/浅灰底导出，作为验收工具 |

## 6. 边界与失败模式

- 终端不答 OSC 11（tmux、screen、部分 SSH、ConPTY）→ 回退 `dark`；浅色 tmux 用户需手动 `preset: light`，二期再评估 passthrough。
- 应答格式不认识 / 超时 → 同上，不猜。
- 非 Unix 平台没有探测原语（`detect` 直接 `None`）→ 回退 `dark`，与「终端不答」同一分支。
- 启动时 preset **不是** `auto` 的那次运行里没有探测；同一次运行内把设置面板改回 `auto` → 仍按 `dark`（`detected_appearance()` 为 `None`），重启后正确。这是「显式 preset 零开销」的直接代价。
- 只有 `wing ps` 的表格接入了 CLI 探测（见 §3.4）；其余表格命令的 `auto` 等价于 `dark`。
- `NO_COLOR`：TUI 不参与（现状）；CLI 表格已有 `no-color` 口径 —— 不探测，输出本就没有墨色。
- 非 tty、`--json`、`--plain`：不探测，不影响输出。
- 逐槽覆盖跨 preset 生效：用户显式值优先，文档写明。
- 探测会吞掉启动窗口（≤150 ms）内的按键（**包括 Ctrl+C**：TUI 路径的 `enable_raw_mode` 走 `cfmakeraw`，ISIG 已关），与现有图像能力探测（≤500 ms）同款取舍；打字机场景下这仍是唯一可行的时点。
- 旧配置里的 `preset: wing` 读作 `dark`；已移除的 `preset: terminal` 读作 `auto`（最近的后继语义），不报错。

## 7. 测试判据

- **对比度单测**：light preset 每个前景槽在 `#ffffff` **与 `#f7f7f7`** 上 ≥ 约定阈值（正文/信息 4.5、装饰 3.0）；`text` 在 `surface` 与四个 diff tint 上 ≥ 4.5，`dim` 在 `surface` / `diff_add_bg` / `diff_del_bg` 上 ≥ 4.5。toast 的正文在其浅色底上 ≥ 4.5。
- **appearance 单测**：`Dark → Dark`、`Light → Light`；`Auto` 解析在注入探测结果下的分支（亮/暗/None/超时）。
- **语法主题单测**：`palette.syntax` 切换后关键字/字符串色值随之变化。
- **渲染回归**：已落地——wordmark 亮底扫光使整行**变暗**（暗底反之）；popup 选中行在 `dark` / `light` 两套调色板下逐 span 校验前景（accent 不被 `Style::patch` 抹平）与选中带；toast 亮底正文对比度。**未落地（留二期）**：settings 全面板与欢迎屏整帧的白底快照。
- **检测**（`tui/appearance.rs` 单测，不碰真终端）：BEL / ST 两种终止符、1/2/4 位 hex、中灰落暗侧、通道加权（纯绿亮 / 纯红暗）、杂字节包围、畸形应答（缺 / 多通道、非 hex、非 `rgb:`）、应答跨 read 分片、终止前不解析、噪声按 128 字节封顶、答案只问一次。
- **检测的端到端证据**（`tests/appearance_probe_pty.rs`，pty 级）：白底应答 → `Light`；不答 → `None` 且耗时不越过预算。
- 更新受影响的既有测试（A 报告 §6 的 12 组：亮度序、`to_rgb → 白`、toast RGB、popup `bg == dim`、海鸥羽白、settings 预览等）。

## 8. 波次

| 波次 | 内容 | 产出 |
|---|---|---|
| W1 | 领域层：`ColorPreset`/`Appearance`/light palette/syntax 映射 + 对比度单测 | 可编译、palette 测试绿 |
| W2 | 渲染层：toast/popup/wordmark/sprite/shimmer/thinking/todo + 更新受影响测试 + theme_preview | 两套主题的截图验收 |
| W3 | `auto` 探测（OSC 11 + 启动接线）+ setup/设置面板/CLI + 文档 | 真机（Ghostty/iTerm2/tmux）验证 |
| W4 | 独立审查（正确性 + 架构）修 B/S → PR | PR |

## 9. 二期（不在本 PR）

- DECSET 2031 + `stdin_proxy` 扩展：拦截 `ESC]11;` 与 `ESC[?997;n`，运行中自动跟随。前置改动：拆接驳的判定条件从「5522 阴性」改为「5522 阴性且 OSC 11 窗口已收束」（否则 Ghostty/iTerm2 上应答会漏给 crossterm 污染键盘）。
- tmux passthrough 的外层查询。
