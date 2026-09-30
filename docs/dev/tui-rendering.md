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
| `--profile thinking\|content` | reasoning 视角 / 助手正文视角（两者只差两条规则，见第二节） |
| `-w <N>` / `--range A:B` | 渲染宽度 / 只看输出的某几行 |
| `-k` / `--kinds` | 打印 compose 之前的 IR：每行标出 `T` 正文、`C` 代码块、`B` 边框、`G` 行号、`M` 列表符、`i` 行内代码、`I` 图片锚点 caption——**「这行为什么是代码」看这个视图** |
| `--images` | 打开图片锚点（默认走链接路径，见第五节） |
| `--workspace <DIR>` | 相对图片路径的解析根（默认当前目录） |
| `--shape <P>=<W>x<H>` | 独立的图片元数据（可重复，`P` 按 markdown 里写的样子给），替代 chat view 的头信息探测 |
| `--chunk N` | 按 N 字节喂进 `StreamingRender`（模拟流式） |
| `--no-finalize` | 保留流式静息态（不跑回合结束的对账渲染） |
| `--check` | 流式静息态、finalize 后，各自与 `full_render` 参考渲染逐 span 比对（文本 + 样式 + 链接 + 图片锚点几何；`full_lines` 只是它的 `Off` 窄视图） |
| `--plain` | 去掉 ANSI 颜色（便于管道/diff） |

## 二、两个 profile：`Content` 与 `Thinking`

同一套管线，**只有两条规则不同**（都写在 `render/markdown/profile.rs`，不要再往别处加分支）：

| 规则 | `Content`（助手正文） | `Thinking`（reasoning） |
|---|---|---|
| 行内围栏归一化（`text:```rust` → 真围栏） | 归一化 | **不归一化**：reasoning 大量在正文里引用 ``` 讨论代码块本身，归一化会造出乱语言的代码块 |
| 缩进（4 空格）块 | CommonMark 语义 = 代码块 | **按正文渲染**（去缩进后当 markdown 重新解析）：reasoning 的缩进是「子思考层级」，不是代码 |

其余一律共享：**围栏代码块在两个 profile 下渲染完全相同**（syntect 高亮 + 行号 + 边框 + diff 底色）；正文颜色的差异发生在 cell compose（`thinking_segment_style` 只换正文前景色）。

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

渲染层只留位；**画**发生在 chat view 的帧内记录 + app 的绘制通道（`ui/chat_view/image.rs`、`app/images.rs`）：

| 环节 | 规则 |
|---|---|
| 记录 | `rows_exact && has_images()` 时，每个可见锚点记一条 `FrameImage { area, offset, target, path }`：`area` = 锚点盒子 ∩ band（垂直裁、宽度不裁），`offset` = 盒子左上角相对 band 的**带符号**偏移（图上滚为负），`target` = **整个盒子** `(cols, rows)`（不是可见部分，滚动不重编码） |
| 编码目标 | `target` 进 `ImageStore` 的 cache key（连同文件 canonical + mtime + 终端 cell 像素）→ 宽度/布局变化自动重编码 |
| 画 | `app` 在**所有 overlay 之后**（toast 之后、选择高亮之前）调 `paint`：一次 `request` → `Ready` 才画；`Pending` / `Unavailable` 什么都不写（caption 就是兜底，**永不**画空白）；图固定在盒子左上，图内等比缩放留白由 `ratatui-image` 的 `Resize::Fit` 决定 |
| 遮挡 | band 之外（status / composer / popup / 滚动条 gutter）天然画不到——`paint` 的 area 就是 band 的**内容矩形**；toast 覆盖到的图**整张**不画（协议载荷在逐 cell 里，局部覆盖会断图）；拖拽选择进行中**整帧**不画图（FrameSnapshot 抓的是文本） |
| 三态 | `Ready` 画 / `Pending` 保持 caption（worker 完成后唤醒重绘）/ `Unavailable` 保持 caption |
| 失效 | `terminal.clear()`、resize、focus 重新获得（= `needs_full_redraw`）→ `ImageStore::invalidate()`（终端可能已丢弃我们发过的图）；元数据迟到 → `CachedCell` 高度缓存失效、行数重算 |

**一套 opts**：`CellContext.images` 是唯一权威，`CachedCell` 在每次投影（高度 / 帧）前从 ctx 同步给流式引擎与非流式 `RenderOpts`——这是「同一张图在流式与终态行数一致」的机械保证。

**元数据表怎么前进（06 的接线）**：每个 cell 在算出线条时把链接目标过一遍 `resolve_image_path`（与产锚点同一个纯函数）得到候选路径，app 的 lane（`app/images.rs`）对候选调 `ImageStore::meta` 探测头信息并填表。表的生命周期有两条规则：**一个内容代内只增不减**（锚点会吃掉链接 span，缩表会让锚点在链接路径之间振荡）；**内容重建**（会话切换 / 压缩重同步 / rewind / `/clear`，即 `ChatView::structure_epoch` 变化）则整表清空 + `ImageStore::reset()`，下一帧按新内容重新探测——被替换的图按新头信息排版，被删掉的图不再产锚点（没有文件监听，重建就是重新读盘的时机）。`rendering.images = off` / 终端无图形协议 / 探测失败 → 不建 store、不读盘，渲染层拿到的是共享的 `ImageOpts::off()`，逐 cell 等于今天的链接路径。

### 真机验收（有 TTY 的终端）

单测只能验到缓冲区与转义序列层，**出图**必须在真终端里看（Ghostty / kitty / iTerm2 等支持图形协议的终端）：

```bash
mkdir -p /tmp/wing-img-demo && cd /tmp/wing-img-demo
cp ~/Desktop/any-shot.png plot.png     # 任意 png/jpg/jpeg/gif/webp/bmp，本地文件（远程 URL 一律走链接路径）
wing                                   # 起 TUI（网关没起就先 wing start），发一条消息，正文里原样写：
#   ![销售趋势](./plot.png)
```

预期：该行变成一个 `▢ 销售趋势 · W×H` 的占位块，随后被真图覆盖（首帧到出图之间隔几帧：探测头信息 → 编码）；
滚动/翻页时图跟着文本走，超出视口的部分不画；改窗口大小后重编码（一瞬的空白后回来）；
拖拽选择期间整帧只有文本（复制得到的是 caption），松开后图回来。退出 TUI 后终端无残留。

对照档：同一台机器上 `rendering.images: off`（或换到 Alacritty 这类没有图形协议的终端），同一行只显示
`销售趋势` 的链接文本——与今天逐 cell 相同。

## 六、症状 → 先看哪里

| 症状 | 先做 |
|---|---|
| 「这行明明是正文，怎么是代码块」 | `--kinds` 找到该行：`C/B/G` = 被判成代码，回看原文是缩进块还是围栏；再对照第四节的表 |
| 「流式和终态不一样 / 内容闪一下变了」 | `--chunk 1 --check`（分块边界是最容易出问题的地方） |
| 「代码块颜色不对 / 没高亮」 | `--kinds` 看是否真有 `C`；没语言标签的围栏本来就是单色 |
| 「reasoning 颜色和正文不同」 | 预期行为：正文用 thinking 色，代码/链接/边框保留主题色 |
| 「这段 `![]()` 怎么没变成图 / 图怎么没占位」 | 第五节的四道闸：模式是否 `Anchor`、路径是否被拒、`--shape` 表里有没有这条路径、图片是否独占一行（`--kinds` 看该行是不是 `I`） |
| 「占位有了，但图没出来」 | 终端是否支持图形协议（启动时探测一次，`rendering.images` 是否为 `off`）；文件是否真的存在/可解码（探测失败 → 退回链接路径）；是不是正被 toast 盖着或正在拖拽选择（那两档刻意不画） |
| 「图占了 36 行，太多了」 | `MAX_ANCHOR_ROWS`（`images.rs`）：长图的上限保护；调它等于改布局契约，需同步矩阵 |
