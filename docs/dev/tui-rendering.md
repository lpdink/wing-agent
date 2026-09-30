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
| `-k` / `--kinds` | 打印 compose 之前的 IR：每行标出 `T` 正文、`C` 代码块、`B` 边框、`G` 行号、`M` 列表符、`i` 行内代码、`$` 公式——**「这行为什么是代码」看这个视图** |
| `--math text\|off` | `off` = 不做数学解析（今天的行为：LaTeX 源码原样显示），用来对照公式渲染 |
| `--chunk N` | 按 N 字节喂进 `StreamingRender`（模拟流式） |
| `--no-finalize` | 保留流式静息态（不跑回合结束的对账渲染） |
| `--check` | 流式静息态、finalize 后，各自与 `full_lines` 参考渲染逐 span 比对 |
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
| 行内 `$x^2$` | 单行输出（`x²`）；装不进一行时（`\frac{a}{b}`、`\sum_{i=1}^n`）**保留完整源码** `$\frac{a}{b}$` |
| 显示 `$$…$$` | N 行网格（`N ≥ 1`），每行一个 `Math` 段，左对齐、按 cell 内容宽收窄；超宽/超预算 → **保留完整源码** `$$…$$` |
| 引擎不支持的命令（`\ce`、`\dfrac`）、未知环境、未闭合、跨空行 | 源码字面量，**逐字符完整**（绝不空串、绝不半截） |
| 代码围栏 / 行内代码 / 缩进代码块内的 `$`、`\(` | 不解析（不归一化） |
| 货币 `$100 and $200` | 不是数学（pulldown 的 `$` 开合规则） |
| `rendering.math = off`（见 `docs/dev/config-logging.md`） | 完全不解析、不归一化 = 今天的渲染（`render_probe --math off` 与基线逐字节相同） |

**降级语义**：`None` 不是错误，是「请显示源码」。降级时把定界符一起还原（`$\ce{2H2O}$`、`$$\frac{a}{b}$$`），所以看不出来或渲染不了的公式与 `off` 的表现一致；`\(…\)` 归一化后的降级形式是 `$…$`（定界符已被改写）。

**定界符归一化清单**（步骤 08 的 VSCode 侧要对齐同一组）：

| 输入 | 处理 |
|---|---|
| `\( … \)` | → `$ … $`（行内；首尾空白去掉——pulldown 的 `$` 只与非空白配对，否则定界符会漏进正文） |
| `\[ … \]` | → `$$ … $$`（显示；同样去首尾空白） |
| 裸环境 `\begin{ENV}…\end{ENV}` | → `$$…$$`；ENV ∈ `align(*)` `aligned` `alignat(*)` `alignedat` `flalign(*)` `split` `eqnarray(*)` `gather(*)` `multline(*)` `center` `equation(*)` `displaymath` `array` `cases` `matrix` `pmatrix` `bmatrix` `Bmatrix` `vmatrix` `Vmatrix`（白名单外不包，保持源码） |
| `$…$` / `$$…$$`（已有） | 不动（避免二次包裹） |
| 围栏 / 行内代码 / 缩进代码块 / 跨空行 / 未闭合 | 不动 |
| 插入的定界符紧邻已有 `$`，或本段里有未配对的 `$$` | 不动（否则插入的 `$$` 会和已有的 `$` 重新配对，渲染就不再是输入的纯函数） |
| 其它方言（`\begin{math}`、`$$` 内的裸环境等） | 不认 |

**症状 → 先看哪里**：公式没渲染（显示源码）= 引擎判 `None`（`--kinds` 看 `$` 是否落在该行；超宽就换大宽度或看 `--range`）；公式**整段消失** = B 级缺陷（事件没接线），先用 `--kinds` 确认 `$` 段在不在。

## 三、不变量：流式静息态 == 参考全量渲染

`StreamingRender` 是「稳定前缀 + 活动尾部」的增量引擎，但它的**静息态**（每帧 `lines()` 之后、`finalize()` 之前）必须与同一文本的 `full_lines` 参考渲染**逐 span 相同**；`finalize()` 直接换成参考渲染。这条不变量由 `crates/wing/tests/stream_render_reconcile.rs` 的矩阵（shape × chunk 大小 × 宽度 × profile）强制，`render_probe --check` 是它的手动版本。

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
| 行内 `e^{x}` / `\sqrt{2}` 没渲染（显示源码） | 引擎布局是 2 行，行内必须单行 → `None` | 引擎契约（`wing-math/src/api.rs`） |

## 五、症状 → 先看哪里

| 症状 | 先做 |
|---|---|
| 「这行明明是正文，怎么是代码块」 | `--kinds` 找到该行：`C/B/G` = 被判成代码，回看原文是缩进块还是围栏；再对照第四节的表 |
| 「流式和终态不一样 / 内容闪一下变了」 | `--chunk 1 --check`（分块边界是最容易出问题的地方） |
| 「代码块颜色不对 / 没高亮」 | `--kinds` 看是否真有 `C`；没语言标签的围栏本来就是单色 |
| 「reasoning 颜色和正文不同」 | 预期行为：正文用 thinking 色，代码/链接/边框保留主题色 |
| 「公式没渲染 / 公式整段消失了」 | 第二节·五的「症状 → 先看哪里」：`--kinds` 看 `$` 段在不在，`--math off` 对照旧行为 |
