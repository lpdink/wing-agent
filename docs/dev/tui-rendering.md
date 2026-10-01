# TUI Markdown 渲染：调试入口与已知边界

怀疑「为什么这段被渲染成代码块 / 为什么流式看着和终态不一样 / 为什么 reasoning 里颜色不对」时先读这里。渲染管线的代码在 `crates/wing/src/render/markdown/`，本文件只登记**怎么查**与**哪些是刻意为之**。

## 一、一条命令看渲染结果：`render_probe`

把任何一段文本喂给**生产用的同一套渲染函数**，直接看它被渲染成什么（这就是排查渲染异常的第一步：先把现象钉到某一段文本上）：

```bash
# 直接渲染文件（默认 thinking = reasoning 视角、宽 120）
cargo run -p wing --example render_probe -- --profile thinking /tmp/reasoning.md

# 从会话日志里取一条（reasoning_content / content），带每行 segment 类型
cargo run -p wing --example render_probe -- \
    --jsonl ~/.wing/core/sessions/<id>/history.jsonl --index 344 --field reasoning \
    --kinds --range 400:440

# 走流式引擎并和参考全量渲染对账（见第三节）
cargo run -p wing --example render_probe -- --chunk 1 --check /tmp/reasoning.md
```

| 选项 | 作用 |
|---|---|
| `--profile thinking\|content` | reasoning 视角 / 助手正文视角（渲染规则见第二节） |
| `-w <N>` / `--range A:B` | 渲染宽度 / 只看输出的某几行 |
| `-k` / `--kinds` | 打印 compose 之前的 IR：每行标出 `T` 正文、`C` 代码块、`B` 边框、`G` 行号、`M` 列表符、`i` 行内代码、`$` 公式、`I` 图片锚点 caption——**「这行为什么是代码」看这个视图** |
| `--math text\|off` | `off` = 不做数学解析（今天的行为：LaTeX 源码原样显示），用来对照公式渲染 |
| `--images` | 打开图片锚点（默认走链接路径，见第五节） |
| `--workspace <DIR>` | 相对图片路径的解析根（默认当前目录） |
| `--shape <P>=<W>x<H>` | 独立的图片元数据（可重复，`P` 按 markdown 里写的样子给），替代 chat view 的头信息探测 |
| `--chunk N` | 按 N 字节喂进 `StreamingRender`（模拟流式） |
| `--no-finalize` | 保留流式静息态（不跑回合结束的对账渲染） |
| `--check` | 流式静息态、finalize 后，各自与 `full_render` 参考渲染逐 span 比对（文本 + 样式 + 链接 + 图片锚点几何；`full_lines` 只是它的 `Off` 窄视图） |
| `--plain` | 去掉 ANSI 颜色（便于管道/diff） |

## 二、三个 profile 规则：`Content` 与 `Thinking`

同一套管线，**所有 profile 差异都在这里**（都写在 `render/markdown/profile.rs`，不要再往别处加分支）：

| 规则 | `Content`（助手正文） | `Thinking`（reasoning） |
|---|---|---|
| 行内围栏归一化（`text:```rust` → 真围栏） | 归一化 | **不归一化**：reasoning 大量在正文里引用 ``` 讨论代码块本身，归一化会造出乱语言的代码块 |
| 缩进（4 空格）块 | CommonMark 语义 = 代码块 | **按正文渲染**（去缩进后当 markdown 重新解析）：reasoning 的缩进是「子思考层级」，不是代码 |
| 数学定界符归一化（`\(…\)`→`$…$`、`\[…\]`→`$$…$$`、裸 AMS 环境→`$$…$$`） | 归一化 | **同样归一化**（第三个规则在两个 profile 下相同，见下） |

第三条是两个 profile **共享**的，之所以仍然登记在 `Profile` 上，是因为它和前两条同源：都是**解析前的源文本改写**（`math.rs`），必须能被流式引擎逐切片复用。公式不因写在 reasoning 里就换一种形态；规则只在「一个完整、且不含代码/`$` 数学的 span」上触发生效，未闭合或跨空行的 `\(` 一律保持字面量，所以它也不会误吞正文。

其余一律共享：**围栏代码块在两个 profile 下渲染完全相同**（syntect 高亮 + 行号 + 边框 + diff 底色）；正文颜色的差异发生在 cell compose（`thinking_segment_style` 只换正文前景色；公式是结构元素，同代码一样保留自己的颜色）。

## 二·五、公式（数学）渲染

`$…$` 行内、`$$…$$` 与裸 AMS 环境（`\begin{align}…\end{align}`）显示公式 → 字符网格（引擎在 `crates/wing-math`，接线在 `render/markdown/math.rs`）。

| 情形 | 行为 |
|---|---|
| 行内 `$x^2$` | 单行输出（`x²`）；引擎判 `None`（`\frac{a}{b}`、`\sum_{i=1}^n` 是 3 行布局）或**渲染结果宽于该 cell 的可用宽度**时，**保留完整源码** `$\frac{a}{b}$` |
| 含行内公式的段落 | **参与普通正文换行**：Math 段不得改变行的正文/非正文分类，换行点与续行缩进必须与「同一段落、公式换成等宽的普通文本」完全一致（回归测试 `inline_math_keeps_prose_wrapping`）。超宽公式先降级成源码再交给同一套折行，**不做横向裁剪**（「半个公式」不是选项）。**例外窗口**：引用块 / 列表续行的前缀（`│ `）再吃 2 列，那里的正文行由 compose 硬折行兜底（前缀算不进 `fits_line` 的口径），这个窗口内仍可能被硬切——与引用块里任何超宽正文同源，属既有 compose 预算问题 |
| 显示 `$$…$$` | N 行网格（`N ≥ 1`），每行一个 `Math` 段，左对齐、按 cell 内容宽收窄；超宽/超预算 → **保留完整源码** `$$…$$` |
| 引擎不支持的命令（`\ce`、`\dfrac`）、未知环境、未闭合、跨空行 | 源码字面量，**逐字符完整**（绝不空串、绝不半截） |
| 代码围栏 / 行内代码 / 缩进代码块内的 `$`、`\(` | 不解析（不归一化）；**带块前缀的同样算**（`> ~~~`、`- ``` `、`> ␣␣␣␣` 都是代码）。围栏状态由**单一事实来源** [`FenceTrack`] 判定（见下），splitter 的切片边界与归一化扫描器共用同一套规则 |
| HTML 块、行内 HTML 标签 / autolink（`<…>`）、链接或图片的 destination 与 title（`](url "t")`）、引用定义行（`[label]: url`，含 destination/title 折到下一行的形态） | 不归一化：markdown 在这些位置不解析，改写了会多出可见 `$`，URL 还会被改坏（OSC8 与点击都用那个串）。判定按 **CommonMark §4.6 的 7 类起始条件**（`<div>` 这类块级标签、`<!--`/`<?`/`<!X`/`<![CDATA[` 各自带**同行**结束条件、type 7 要求整行只有一个完整 tag 且不能打断段落）与 **§4.7 的引用定义**（label 可含空格；destination/title 折行时，只有**真 title**（`"…"`/`'…'`/`(…)`）或非围栏行才算续行），HTML 块还受**容器边界**约束（`> <div>` 遇到不带 `>` 的行就结束） |
| 货币 `$100 and $200` | 不是数学（pulldown 的 `$` 开合规则） |
| `rendering.math = off`（见 `docs/dev/config-logging.md`） | 完全不解析、不归一化 = 今天的渲染（`render_probe --math off` 与基线的**参考渲染**逐字节相同；流式静息态见第三节的不变量） |

**降级语义**：`None` 不是错误，是「请显示源码」。降级时把定界符一起还原（`$\ce{2H2O}$`、`$$\frac{a}{b}$$`），所以看不出来或渲染不了的公式与 `off` 的表现一致；`\(…\)` 归一化后的降级形式是 `$…$`（定界符已被改写）。

**定界符归一化清单**（步骤 08 的 VSCode 侧要对齐同一组）：

| 输入 | 处理 |
|---|---|
| `\( … \)` | → `$ … $`（行内；首尾空白去掉——pulldown 的 `$` 只与非空白配对，否则定界符会漏进正文） |
| `\[ … \]` | → `$$ … $$`（显示；同样去首尾空白） |
| 裸环境 `\begin{ENV}…\end{ENV}` | → `$$…$$`；ENV ∈ `align(*)` `aligned` `alignat(*)` `alignedat` `flalign(*)` `split` `eqnarray(*)` `gather(*)` `multline(*)` `center` `equation(*)` `displaymath` `array` `cases` `matrix` `pmatrix` `bmatrix` `Bmatrix` `vmatrix` `Vmatrix`（白名单外不包，保持源码） |
| `$…$` / `$$…$$`（已有） | 不动（避免二次包裹） |
| 围栏 / 行内代码 / 缩进代码块 / 跨空行 / 未闭合 | 不动 |
| 插入的定界符紧邻已有 `$` | 不动（否则插入的 `$$` 会和已有的 `$` 重新配对，渲染就不再是输入的纯函数） |
| 扫描中**遇到**未配对的 `$$` 之后的 span | 不动（同上；规则是**位置相关**的——游离 `$$` 之前的 span 照常改写，之后的放弃。单个 `$` 不触发：`$100` 这类货币太常见且配不出 `$$`） |
| 超过 8192 字节的 span；单趟扫描超出 1 MiB 工作预算 | 不动（预算闸，剩下的文本原样保留） |
| 其它方言（`\begin{math}`、`$$` 内的裸环境等） | 不认 |

**单一事实来源：`FenceTrack`（`render/markdown/stream.rs`）**。「哪些行是代码」只在 `FenceTrack::step` 里判定一次，两个调用方都驱动它：

| 调用方 | 用途 |
|---|---|
| `StreamingRender` 的 `Mode::FencedCode` / `Mode::PrefixedFence` / `Mode::List{fence}` | 决定**切片边界**（绝不在围栏还开着时切片，否则切片内的解析与整篇不同） |
| `math::normalize_delimiters` 的行级扫描 | 决定**哪些行不归一化** |

规则（唯一一份）：普通围栏（`~~~`）只被同类围栏行闭合；**带前缀的围栏**（`> ~~~`、`- ``` `）只被仍带前缀的行闭合——无前缀的围栏行表示容器到此为止，它自己**另开一个顶层围栏**并吞掉后续内容（CommonMark），后续内容因此是代码、不得改写；列表项内的缩进行仍算 item 内容（`  ~~~` 是 item 自己的闭合行），但一旦出现**非空且比 item 内容列更靠左**的行，item 的内容就被打断，此后缩进围栏行也按「新顶层围栏」处理（`- ~~~\n> \n  ~~~\n\n\(x\)` 这类形状）。

> 历史：这三条规则原先在 splitter 与归一化器里各写了一份，两份漂移导致**代码块内容被改写**（`\(x\)` → `$x$`）——现在没有第二份可漂移。

`math.rs` 里剩下两类「不是正文」的判定（HTML 块、引用定义）也被收成**具名函数**：`html_block_start` / `html_block_ends`（对应 CommonMark §4.6 的类型 1–7 与各自的结束条件）、`reference_definition` / `ref_def_continuation` / `looks_like_title`（对应 §4.7）。这一类「扫描器的块结构模型」是历史上反复漂移的地方，回归由 `crates/wing/tests/markdown_math_drift.rs` 守着，判据两层：

1. **段级**：`rendering.math = text` 与 `= off` 两个视图下，`CodeBlock` / `InlineCode` / `Link` 段的文本必须逐字节相同（覆盖代码跨度、围栏、缩进代码、链接与图片目标）；
2. **文档级**：整篇都是非正文的文档（HTML 块、未闭合围栏、缩进代码块、引用定义），两个视图的**渲染文本**必须相同——HTML 块内容在 IR 里是 `Text` 段，段级判据看不见它，这条补上。

语料 ≥2.1 万份（随机 + 定向交叉组合），把任一启发式改回旧形态都会变红（实测：HTML 判定、引用定义闸门、空 item 规则、marker padding、列表标记分支、HTML 容器边界 六处）。

**症状 → 先看哪里**：公式没渲染（显示源码）= 引擎判 `None`（`--kinds` 看 `$` 是否落在该行；超宽就换大宽度或看 `--range`）；公式**整段消失** = B 级缺陷（事件没接线），先用 `--kinds` 确认 `$` 段在不在；**代码块里的 `\(` 变成了 `$`** = 围栏状态机问题（检查 `FenceTrack::step` 的规则与调用方是否都在用它）。

## 三、不变量：流式静息态 == 参考全量渲染

`StreamingRender` 是「稳定前缀 + 活动尾部」的增量引擎，但它的**静息态**（每帧 `lines()` 之后、`finalize()` 之前）必须与同一文本的 `full_render` 参考渲染**逐 span 相同**（文本 + 样式 + 链接 + 图片锚点几何）；`finalize()` 直接换成参考渲染。这条不变量由 `crates/wing/tests/stream_render_reconcile.rs` 的矩阵（shape × chunk 大小 × 宽度 × profile）强制，`render_probe --check` 是它的手动版本。

宽度矩阵分两段：**narrow（1..=6 列）** 与主矩阵（40/80/120）。窄带不是"边角料"——布局层没有最小列宽约束（tmux 窄 pane 可达 1 列），而硬折行在那里最粗暴：2 列 cell 前缀就能填满一整行，普通行会长得和锚点的空白覆盖行一模一样（历史上正是这一档把空行去重判据带歪过）。

因此：**流式与终态不一致 = bug**，不是"渲染风格问题"。反过来说，最终画面看着不对但 `--check` 通过，说明问题在解析/规则（第二节），不在增量引擎。

## 四、已知边界（刻意不修，看到别当 bug）

| 现象 | 原因 | 归类 |
|---|---|---|
| 流式中 `[foo]` 短暂显示成字面量 | 引用式链接的定义与引用落在不同 slice，切片独立解析；`finalize()` 收敛 | 切片隔离，见 `stream.rs` 模块文档 |
| reasoning 里模型把草稿套在围栏里、内部再套围栏，导致后半段正文被吃进代码块（或反之） | CommonMark 不允许嵌套围栏：一个裸围栏闭合外层后，后续围栏的配对整体翻转。任何 CommonMark 渲染器（GitHub / VS Code / Claude Code）结果相同；还原作者意图需要全局配对最优化，与稳定前缀模型冲突 | 输入歧义，见 `stream.rs` 模块文档「Known limit — nested fences」 |
| 段落后面紧跟列表（`para` 换行 `- item`）时，该列表项内的缩进续行会多出段落间空行 | splitter 的 Paragraph 模式不识别「列表打断段落」，于是项内缩进内容被切成独立的缩进块（缩进块是顶层块，没有「列表项内不加空行」的规则）。逐帧可见、`finalize()` 收敛 | 切片边界，见 `stream.rs` | 
| 已提升块之后的列表项 lazy continuation（缩进行）丢了列表续行前缀 | 该行自成一个 slice：文档上下文里它是上一列表项的内容，单独解析时是普通段落 | 切片隔离 |
| 缩进超过 `PROSE_DEPTH_LIMIT`（8 层 = 32 空格）的缩进块 | 缩进块按正文渲染是逐层重解析（每层一次 pulldown + 一个循环），无上界的话退化输出会变成每帧 O(depth × text) 并且栈上递归到打穿（SIGSEGV）。超预算即退回代码块渲染——那个缩进层级本来也长得像代码 | profile 语义（预算见 `profile.rs`） |
| 极端混排（CRLF + 缩进围栏 + 纯空格行 + 表格/引用片段）下仍有静息态分歧 | 预存在的切片边界族，`reconcile_matrix_shapes` 只钉住构造良好的形状；`cargo test -p wing --test stream_render_fuzz -- --ignored --nocapture` 可枚举当前数量（默认忽略：契约是「不 panic」，分歧数随修复下降） | 已知噪声，`finalize()` 收敛 |
| reasoning 里 `（```rust）` 这类行内引用没有变成代码块 | `Thinking` 刻意不做行内围栏归一化（第二节） | profile 语义 |
| reasoning 里 4 空格缩进的正文没有代码块样式 | `Thinking` 刻意按正文渲染（第二节） | profile 语义 |
| `$$` / `\begin{align}` 中间有空行的显示公式没渲染（显示源码） | pulldown 的数学配对不跨空行，归一化规则也刻意不跨（第二节·五的规则表）：全量与流式一致地降级为源码，内容不丢 | 定界符语义（第二节·五） |
| `f(x) = \begin{cases}…\end{cases}` 的 `f(x) =` 留在上一行 | 归一化只包环境本身，不吞前导正文（吞了就得猜公式起点，代价更大） | 定界符语义（第二节·五） |
| 行内 `e^{\pi}` / `\sqrt{2}` 没渲染（显示源码） | 引擎布局是 2 行，行内必须单行 → `None` | 引擎契约（`wing-math/src/api.rs`） |
| 一条公式里含 ≥ 2 个环境（两个矩阵相乘、`\begin{pmatrix}…\end{pmatrix}^{-1} \begin{pmatrix}…\end{pmatrix}`）没渲染（显示源码） | 引擎一次只渲染一个顶层环境，多个环境之间的拼接语义未定义 → 整条降级 | 引擎契约（`wing-math/src/guard.rs` 的 `MultipleEnvironments`） |
| 行内 accent（`\vec{v}`、`\hat{y}`、`\hat{H}\psi`）没渲染（显示源码） | accent 是「符号行 + 内容行」的 2 行布局，行内必须单行 → `None`（显示模式照常渲染成网格） | 引擎契约（`wing-math/src/grid/layout.rs::layout_accent`） |
| 行内带下标的命名算子（`\max_x f(x)`、`\min_y g(y)`）没渲染（显示源码） | 算子名与它的下标上下堆叠成 2 行，行内必须单行 → `None`（显示模式照常渲染成网格） | 引擎契约（`wing-math/src/grid/layout.rs::layout_limit`） |
| 货币 `$` 与代码跨度里的 `$` 被 pulldown 配成一对（`成本是 $100，用 \`$PATH\` 变量` → `$100` 的 `$` 被吞、代码跨度被拆） | pulldown 0.13 的数学配对是全局扫描：先埋 `$` token 再配对，**代码跨度不参与**，所以「先出现一个未配对 `$`，之后代码跨度里再有 `$`」必然错配。D5 的「代码/围栏内不解析」只对**开**定界符成立；`a946327` 起既有（基线 `94ac0be` 无此问题，因为它不解析数学）。修法需在归一化层保护代码跨度里的 `$`（不能简单加反斜杠：代码跨度内反斜杠是字面量），属独立决策 → 留给后续步骤 | pulldown 配对语义（既有） |
| 两个**货币符号**互相配对（`the price is $5-$10 today` → `the price is 5 -10 today`；`$5 and $10` 有空白则不配对） | 同上一行的配对语义，只是这次没有代码跨度参与：两个 `$` 恰好满足行内数学的两条空白规则，与货币意图无关。修法同上（需要一层「哪些 `$` 是货币」的判定），**不阻塞**、不在本 PR 内处理 | pulldown 配对语义（既有） |
| 容器里的反引号围栏（`> ``` `、`- ``` …`、`  - ``` …`）在**某些 chunk 边界**下静息态 ≠ 参考 | 根因是 `ensure_fences_on_own_line`：`> ` / `- ` 不是空白前缀，行首 `> ``` ` 会被当成「粘在文字后的围栏」而插入换行；插入发生在 `push` 里，**某个切片可能先被 promoted、插入还没落地**，于是切片文本与参考的归一化文本不同。分歧点随 profile / 形状 / chunk 漂移（实测 `> ~~~
> a
~~~` + 公式：content chunk=5 双方都分歧；`  - ``` … > ``` ` 这类形状：双方都在 thinking 全 chunk 分歧、本实现另有 content chunk=7 的零星分歧） | 既有噪声（`a946327` 同样分歧，只是组合不同），`finalize()` 收敛；`stream_render_reconcile` 因此不放这些形状（tilde 围栏的同类形状在矩阵里） |
| 列表标记后的 tab（`-\t~~~`、`  - \titem`） | r4 曾把 tab 当 1 字节 padding，导致 item 内容列算错（缩进代码块被当段落 → 内容被改写）；r5 起 `list_marker_bounds` 按 **CommonMark 的列**计算（tab 推进到下一个 4 的倍数），`prefix()` 同时给出字节偏移与列号，`FenceTrack::item_col` 用列 | 已修（r5） |
| `indent_of`（旧助手）把 tab 一律记 4 列 | `indent_of` 只看「是否 ≥4 列 / ==0 / ≥2」，在含 tab 的边角形态（如 4 空格 + tab）与真实列号有偏差；需要精确列号的地方（`prefix`/`FenceTrack`）已改用 `indent_columns` | 残留近似（仅影响 ≤3 列守卫的边角），登记 |
| HTML 块跨流式切片边界时，静息帧把块内的围栏行渲染成代码块 | 切片器不建模 HTML 块，块内的围栏行会被切到新切片；`<b>\n~~~~\n- a\n\\(x\\)` 实测在 `94ac0be`（无 math）基线上同样分歧 → 与 math 无关的既有噪声，`finalize()` 收敛 | 既有（b0 级），矩阵不放该形状（`math_html_block_swallows_fence_and_marker` 用的是会收敛的变体） |
| 行内 HTML 里的 `$` 被当数学（`<code>$x$</code>` → `x`） | `ENABLE_MATH` 的固有语义，且基线本来就在行内 HTML 里解析 markdown（`<span>*em*</span>` → `em`）；仅在显示公式跨标签时才会吃掉标签（`<b>$$…</b> and <i>…$$`） | 输入歧义（与既有语义一致） |
| 公式网格的最后一行是空行时，cell 末尾看不到那一行 | 文档级 `trim_trailing_blank` 会把末尾空行与块分隔空行合并（对任何块的末尾空行都一样）；被合并的是空行，视觉无损失 | 文档级空行语义 |

## 五、图片锚点（`Anchor` 模式）

`![alt](path)` 有两档（总任务书 D2 的两档降级），渲染层只负责「留不留位、留几行」：

| 档 | 什么时候 | 渲染成什么 |
|---|---|---|
| **链接路径**（存量行为） | 模式 `Off`；或 `Anchor` 但**元数据缺失 / 路径被拒 / 图片不独占一行** | 与今天逐 span 完全相同：alt 当链接文本（远端 URL 会补 ` (url)`），点击用系统查看器打开 |
| **锚点** | `Anchor` 且上面两条都不成立 | `rows` 行占位：第 0 行是可复制的 caption `▢ {alt 或文件名} · {W}×{H}`，其余是**覆盖行**（图片画在上面；图没画出来时它就是可见兜底） |

**行数是纯函数**（`render/markdown/images.rs`）：

```text
rows = clamp(round(W / (CELL_ASPECT × R)), MIN_ANCHOR_ROWS, MAX_ANCHOR_ROWS)
R            = px_w / px_h                （来自图片头，不解码）
CELL_ASPECT  = 2.0                        （字符格 高:宽 ≈ 2:1，8×16 字体）
MIN/MAX      = 1 / 36
W            = markdown 渲染宽 = 单元格宽 − 2 列前缀（也就是锚点盒子的 cols）
```

`rows` **不得**依赖终端图形能力或像素查询结果：它进 `CachedCell` 的高度缓存，也进第三节的流式不变量，一旦依赖能力就会两边同时打穿。数值示例（`W=118`）：800×600 → 44 → 上限 **36**；1920×1080 → **33**；400×400 → 59 → 36；800×6000 → 442 → 36；2000×20 → **1**。

**元数据从哪来**：渲染层零 I/O。调用方（chat view，背后是 `ui::image::ImageStore` 的头信息探测）把像素尺寸填成 `ImageOpts { mode, workspace, shapes }`，表的键 = `resolve_image_path` 的归一化产物；表里没有这条路径（未知 / 探测失败 / 不是图）就退回链接路径。

**路径策略**（纯词法：不 stat、不 canonicalize、不解析符号链接——这是显示边界不是安全边界）：

- 接受：workspace 相对路径（`.`/`..`/重复分隔符折掉）、绝对路径、`file://`（`file://host/…` 除外）；
- 拒绝：空、控制字符（防转义注入）、超过 512 字符、远程 scheme（`http:` / `https:` / `data:` / `ftp:` …）、`~`（要读环境变量）、没有 workspace 时的相对路径、`..` 越出 workspace、扩展名不在 `png jpg jpeg gif webp bmp`（大小写不敏感）。

**包含判定的大小写规则**（review #135 [N1]）：只在**相对路径**分支上做词法包含检查，而相对路径的前缀就是 workspace 自己的拼写（逐字节相同），所以「根之下的变体拼写」在两种规则下都放行；唯一能看出平台差异的输入是 `..` 折回根的**大小写变体**——大小写不敏感盘上 `../WORKSPACE/a.png` 与 `../workspace/a.png` 是同一个文件，故按平台折叠 **ASCII** 大小写（`images.rs::is_inside`，仍然不 stat、不 canonicalize；绝对路径一律接受，与「链接可打开任意路径」同口径）。

拒绝不是错误：该图片走链接路径。

**侧信道**：`ImageSpan { line, column, cols, rows, path, alt, px_w, px_h }`，与 `LinkSpan` 同构——逐行平行的 `Vec<Vec<ImageSpan>>`，`column` 与 `LinkSpan::start` 同一坐标系（行内显示列，含 2 列 cell 前缀）。它随 `ComposedLines`（缓存态）与 `StreamingRender`（流式态）一路带到 `ui/cached_cell.rs` 的 `CellFrame`（`compute_cell_frame`，`CellLines` 的超集）；`rows_exact == false`（同一 cell 里存在超宽行，见 `tui-link-open` 的同一判据）时行号算术不成立，**不要**按锚点画图。锚点几何 = `Rect(column, line, cols, rows)`，`line` 是 cell 行数组下标，屏幕行号由调用方加 cell 的 y 偏移。

**只在「独占一行的顶层图片」产锚点**——宁可降级也不产出错位的锚点：行内（前后还有文字）、一行多张、列表项/引用块/表格单元格里、标题里、链接里、代码围栏里、HTML `<img>`、缩进（thinking）块里、**alt 跨行**（`![l1\nl2](a.png)`：软换行会先把标签所在行 flush 掉，锚点会落到 alt 的最后一行），全部走链接路径。缩进块是「嵌套重解析 + 二次加前缀」，列坐标会整体偏移，因此嵌套渲染显式关了锚点（见 `code_blocks.rs` 的 `ImageOpts::off()`）。

**元数据表的契约**：键 = `resolve_image_path` 的产物，**同一路径不要登记两次**（重复时确定性地取第一条，但那是调用方的 bug）；`shape_for` 是线性查找，所以表应当只装当前视图要画的图（chat view 侧按需建表），不要拿整棵目录树当表。

**流式锚点的空行去重**：`compose_into` 用「上一行是否 markdown 空行」来判断要不要丢掉本批的首个空行；锚点的覆盖行**长得像空行但不是空行**，所以那条判据带一个**结构**例外（`images::row_is_cover_row`：该行落在某个锚点的行区间内、且不是它的 caption 行）。**不要**改回内容判据（"这段是不是空格"）：窄宽下硬折行会把前缀折成内容恰为 `" "` 的普通行，误判会让流式静息态多一行空白。

**复现**：

```bash
# 单次渲染 + 锚点几何（末尾打印 line/col/cols/rows）
cargo run -q -p wing --example render_probe -- --profile content \
    --images --workspace . --shape './plot.png=800x600' --plain /tmp/fig.md

# IR 视图：锚点行是 I（payload 只占一行，覆盖行在 compose 里展开）
cargo run -q -p wing --example render_probe -- --images --workspace . \
    --shape './plot.png=800x600' --kinds /tmp/fig.md

# 流式对账（含锚点几何）：resting 必须与参考渲染一致
cargo run -q -p wing --example render_probe -- --images --workspace . \
    --shape './plot.png=800x600' --chunk 7 --check /tmp/fig.md
```

`crates/wing/tests/stream_render_reconcile.rs` 的矩阵同时跑 `Off` 与 `Anchor` 两组（文本 + 样式 + 链接 + 锚点几何逐项对账）。

### 绘制契约（谁把图真的画上去）

渲染层只留位；**画**在 chat view 的帧内记录 + app 的绘制通道（`ui/chat_view/image.rs`、`app/images.rs`）。
完整契约——能力阶梯、三态、遮挡与选择、失效触发点、**新鲜度**（文件被重写）、资源上限、性能数字与
真机验收——在 [`tui-images.md`](tui-images.md)。这一页只留两条对**渲染层**有约束的事实：

- **记录**：每个可见锚点记一条 `FrameImage { area, offset, target, path }`。`area` = 锚点盒子 ∩ band
  （垂直裁、宽度不裁），`offset` = 盒子左上角相对 band 的**带符号**偏移（图上滚为负），
  `target` = **整个盒子** `(cols, rows)`（不是可见部分——滚动不重编码）。`target` 进 `ImageStore` 的
  cache key（连同文件 canonical + mtime + 终端 cell 像素），所以宽度/布局变化会自动重编码。
- **行号**：锚点的屏幕行 = 它的**行号**，除非它上面有超宽行——`Paragraph` 把它们折成多行、`scroll` 又是
  按行算的，所以此时要用包装后的行号（`CellFrame::image_row`，超宽行由 ratatui 自己的
  `Paragraph::line_count` 量，不是我们重写折行）。**下面**有超宽行不影响锚点自己的行，因此「cell 里有一行
  很长的代码」不会让整张图消失（`rows_exact` 是**链接层**的门，不是图片的）。

**一套 opts**：`CellContext.images` 是唯一权威，`CachedCell` 在每次投影（高度 / 帧）前从 ctx 同步给流式引擎与非流式 `RenderOpts`——这是「同一张图在流式与终态行数一致」的机械保证。

**元数据表怎么前进（06 的接线 + 07 的新鲜度）**：每个 cell 在算出线条时把链接目标过一遍 `resolve_image_path`（与产锚点同一个纯函数）得到候选路径，app 的 lane（`app/images.rs`）对候选调 `ImageStore::meta` 探测头信息并填表。表的生命周期有两条规则：**一个内容代内只增不减**（锚点会吃掉链接 span，缩表会让锚点在链接路径之间振荡）；**内容重建**（会话切换 / 压缩重同步 / rewind / `/clear`，即 `ChatView::structure_epoch` 变化）则整表清空 + `ImageStore::reset()`，下一帧按新内容重新探测——被替换的图按新头信息排版，被删掉的图不再产锚点（重建是重新读盘的**硬重置**；同一会话内的文件重写由 app lane 的**新鲜度检查**覆盖，见 [`tui-images.md`](tui-images.md) 第四节）。`rendering.images = off` / 终端无图形协议 / 探测失败 → 不建 store、不读盘，渲染层拿到的是共享的 `ImageOpts::off()`，逐 cell 等于今天的链接路径。

### 真机验收（有 TTY 的终端）

单测只能验到缓冲区与转义序列层；**出图**必须在真终端里看（Ghostty / kitty / iTerm2 等支持图形协议的
终端）。正例/对照例的完整清单（含本次新增的「重写同一个路径 ≤1 s 内换图」）在
[`tui-images.md`](tui-images.md) 第十节。

## 六、症状 → 先看哪里

| 症状 | 先做 |
|---|---|
| 「这行明明是正文，怎么是代码块」 | `--kinds` 找到该行：`C/B/G` = 被判成代码，回看原文是缩进块还是围栏；再对照第四节的表 |
| 「流式和终态不一样 / 内容闪一下变了」 | `--chunk 1 --check`（分块边界是最容易出问题的地方） |
| 「代码块颜色不对 / 没高亮」 | `--kinds` 看是否真有 `C`；没语言标签的围栏本来就是单色 |
| 「reasoning 颜色和正文不同」 | 预期行为：正文用 thinking 色，代码/链接/边框保留主题色 |
| 「公式没渲染 / 公式整段消失了」 | 第二节·五的「症状 → 先看哪里」：`--kinds` 看 `$` 段在不在，`--math off` 对照旧行为 |
| 「这段 `![]()` 怎么没变成图 / 图怎么没占位」 | 第五节的四道闸：模式是否 `Anchor`、路径是否被拒、`--shape` 表里有没有这条路径、图片是否独占一行（`--kinds` 看该行是不是 `I`） |
| 「占位有了，但图没出来」 | 终端是否支持图形协议（启动时探测一次，`rendering.images` 是否为 `off`）；文件是否真的存在/可解码（探测失败 → 退回链接路径）；是不是正被 toast 盖着或正在拖拽选择（那两档刻意不画）；换过工作目录（`/workdir`、会话切换）后相对路径要重新探测 |
| 「只有 caption、下面一片空白」 | 现在只可能是「这行没被画出来且没有其它解释」以外的原因——即图片此刻不可用（探测失败/编码中/终端不支持）。行号算术不再造成空白：超宽行只影响它**上面**的锚点，且那种情况的图会被画在包装后的正确行上 |
| 「图占了 36 行，太多了」 | `MAX_ANCHOR_ROWS`（`images.rs`）：长图的上限保护；调它等于改布局契约，需同步矩阵 |
