# TUI 明暗主题（浅色终端支持）

一句话：TUI 的底色是终端的，我们只负责「在给定底色上可读且好看」。现在补上浅色底这份投影，并让默认配置自己认出终端是深是浅。

## 1. 问题

浅色终端下不是「略差」，是不可读：

- `ColorPreset::Wing`（默认）的 16 个前景槽全按深底校准：白底上 15 个低于 WCAG 4.5:1，12 个连 3.0 都过不了。`text` `#e6eaf2` 在白底 1.21 -- 正文、tool 名、placeholder、settings 正文全部消失。
- 少数自绘背景是绝对深色：用户消息卡 `surface` `#232d42`（整行深色横杠）、四个 diff tint、toast 三种底（且不设前景，白底终端下 1.09）、popup 选中行（`dim` 当底 + 继承终端默认前景，2.47）。
- `is_light_theme` 判据自指：读 `palette.text`（默认 preset 自己就是浅色），默认配置 + 白底永远判「暗底」。wordmark 渐变、thinking 刷光、`heading_bright` 三处亮底分支从未在开箱配置下执行过。
- `ColorPreset::Terminal`（「跟随终端 ANSI」）不是解：`text: Color::White` 绑死 ANSI 15，在 Solarized Light / Tomorrow / xterm 默认下是 1.08 / 1.00 / 1.00，10~11 个槽不合格；`surface` 与 diff tint 仍是写死的深色。

好消息：视觉权重几乎全部由 `ThemePalette` 的 24 个槽承担（约 450 处消费），硬编码只剩 6 个岛（Wing 预设本体、Terminal 的 surface+diff、4 处 syntect、toast 三色、海鸥品牌色、刷光端点）。

## 2. 决策

1. `colors.preset: auto | wing | light | terminal`，**默认 `auto`**。`auto` = 启动时探测终端背景，亮→`light`、暗→`wing`、探测失败→`wing`（现状，不猜）。
2. **运行时跟随（DECSET 2031）二期做**。一期只解决「启动时正确」，不碰 `stdin_proxy` 的拆接驳时序。
3. `terminal` 保留并修硬伤：`text: White → Color::Reset`（终端默认前景），diff tint 跟随新语义；文档定位为「高级逃生舱」。
4. 品牌图形（海鸥 / WING 字标）按 appearance 出两套，同一份像素网格。

## 3. 接口

### 3.1 配置

```yaml
colors:
  preset: auto            # auto | wing | light | terminal
  # 逐槽覆盖：跨 preset 生效（现状语义不变）
```

- `Auto` 的解析结果不落盘；每次启动重新探测。
- `--dump-config` / `wing config` 读写 `preset` 原值（`auto`），不写解析结果。

### 3.2 领域类型

```rust
pub enum ColorPreset { Auto, Wing, Light, Terminal }   // 默认 Auto

/// 调色板自身的明暗属性 -- 显式，不从任何颜色反推。
pub enum Appearance { Dark, Light }

pub struct ThemePalette {
    // ...现有 24 槽不变...
    pub appearance: Appearance,
    pub syntax: SyntaxTheme,        // Dark | Light
}
```

- `ThemePalette::from_config(&ColorsConfig)` 只处理显式 preset（`Wing`/`Light`/`Terminal`）；`Auto` 先由调用点解析成 `Wing`/`Light` 再传入，避免领域类型知道终端探测。
- 删除两处 `is_light_theme`（`config/mod.rs` 与 `ui/shimmer.rs`），改读 `palette.appearance`。混色方向、刷光峰值、渐变方向统一读它。

### 3.3 检测

```rust
/// OSC 11 背景色查询。仅在 raw mode 下、stdin 接驳安装前调用（fd 0 仍是真实 tty）。
/// 返回 None = 终端不答/格式不认识/超时 -- 调用方回退 `Wing`。
pub fn detect_appearance(timeout: Duration) -> Option<Appearance>;
```

- 实现位置：`tui::begin_tui_session` 内、`enable_raw_mode()` 之后、`stdin_proxy::install()` 之前。探测结果存进程级 `OnceLock<Option<Appearance>>`，供 `init_terminal` 之后的配置解析读取。
- 只在 `preset == Auto` 时发查询；其他 preset 零开销。
- 查询字节 `ESC]11;?\x07`；应答 `ESC]11;rgb:RRRR/GGGG/BBBB`（BEL 或 ST 终止）。用亮度判 `Appearance`（> 0.5 为亮底）。
- 超时 150 ms。失败不影响任何现有行为。

### 3.4 CLI（`wing ps` 等）

- `AppConfig::load()` 之后按 preset 解析。`auto` 且 stdout 是 tty 时做一次探测；非 tty / `--json` 不探测，按 `wing` 输出。
- 与 TUI 共用 `ThemePalette::from_config`，不新增颜色路径。

## 4. light 调色板

设计原则：沿用 Wing 的色相族，明度按白底重解；正文留余量（≥7:1），次要 ≥4.5，装饰 ≥3.0。逐槽覆盖仍然生效。

| 槽 | light | 白底对比度 |
|---|---|---|
| `text` | `#1f2328` | 15.80 |
| `thinking` | `#4b5563` | 7.56 |
| `tool_result` | `#57606a` | 6.39 |
| `dim` | `#6e7781` | 4.55 |
| `accent` | `#0e7490` | 5.36 |
| `success` | `#1a7f37` | 5.08 |
| `warning` | `#8a5a00` | 5.93 |
| `danger` | `#b42318` | 6.57 |
| `math` / `heading` | `#6d28d9` | 7.10 |
| `heading_bright` | `#5b21b6` | 8.98 |
| `inline_code` | `#a4133c` | 7.67 |
| `marker` | `#5b6b8c` | 5.35 |
| `quote` | `#6b7280` | 4.83 |
| `thinking_label` | `#8a5a00` | 5.93 |
| `strong` | `#7c4a03` | 7.40 |
| `surface` | `#f0f3f8` | 底：1.11 / 其上 text 14.20 |
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
| `config/mod.rs` | `ColorPreset` 加两变体；`Appearance`；light palettes；`Terminal` 的 `text → Reset` |
| `render/syntax.rs` | 4 处 `base16-ocean.dark` → 按 `palette.syntax` 选主题 |
| `ui/toast.rs` | 三种底改为 appearance 感知（浅色下浅底 + 显式深前景） |
| `ui/popup/selection.rs` | 选中行统一为「`surface` 底 + `text` 前景」；删除 ANSI `Gray` 描述色 |
| `ui/popup/picker` | 与 selection 共用同一选中语义 |
| `ui/welcome/{wordmark,sprite,mod}.rs` | 按 appearance 选两套色（同一像素网格）；渐变方向读 `appearance` |
| `ui/cells/thinking.rs` | 刷光峰值方向读 `appearance`；不再把 `Reset` 当白 |
| `ui/shimmer.rs` | `to_rgb(Reset)` 不再假定白；`is_light_theme` 删除 |
| `ui/cells/todo_msg.rs`、`render/markdown/types.rs` | 浅色下避免「淡色槽 + DIM」双重淡化 |
| `cmd/setup.rs` + `config/catalog.rs` | preset 枚举加 `auto` / `light` 及文案；设置面板实时预览沿用现有机制 |
| `examples/theme_preview.rs` | 支持 `--preset light` 与白/浅灰底导出，作为验收工具 |

## 6. 边界与失败模式

- 终端不答 OSC 11（tmux、screen、部分 SSH、ConPTY）→ 回退 `wing`；浅色 tmux 用户需手动 `preset: light`，二期再评估 passthrough。
- 应答格式不认识 / 超时 → 同上，不猜。
- `NO_COLOR`：TUI 不参与（现状）；CLI 表格已有 `no-color` 口径，不动。
- 非 tty、`--json`、`--plain`：不探测，不影响输出。
- 逐槽覆盖跨 preset 生效：用户显式值优先，文档写明。
- 探测会吞掉启动窗口（≤150 ms）内的按键，与现有图像能力探测（≤500 ms）同款取舍；打字机场景下这仍是唯一可行的时点。

## 7. 测试判据

- **对比度单测**：light preset 每个前景槽在 `#ffffff` / `#f7f7f7` 上 ≥ 约定阈值；`surface` 与 diff tint 上的 `text` ≥ 4.5。
- **appearance 单测**：`Wing → Dark`、`Light → Light`、`Terminal → Dark`；`Auto` 解析在注入探测结果下的四条分支（亮/暗/None/超时）。
- **语法主题单测**：`palette.syntax` 切换后关键字/字符串色值随之变化。
- **渲染回归**：欢迎屏、popup、settings、toast 各一张白底与黑底的文本 + 颜色断言（复用现有 `theme_preview` / 捕获工具）。
- **检测**：探测函数注入 `Read`/`Write`（不依赖真终端），覆盖 BEL/ST、跨 read 分片、畸形响应、超时。
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
