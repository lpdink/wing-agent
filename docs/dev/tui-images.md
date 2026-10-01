# TUI 图片：能力、上限与失效

终端里显示**本地图片**的完整一页：什么时候画、什么时候退回链接、资源上限是多少、什么会让图失效、
出问题时先看哪里。渲染层怎么「留位」在 [`tui-rendering.md`](tui-rendering.md) 第五节（锚点行数、
路径策略、侧信道），本页讲**能力与运行**（探测、缓存、上限、新鲜度、绘制契约、真机验收）。

代码：`ui/image/**`（终端图形层）· `render/markdown/images.rs`（锚点）· `ui/chat_view/image.rs`（帧内放置表）
· `app/images.rs`（app 侧 lane：配置门 / 元数据表 / 新鲜度 / 绘制通道）。测试：`app/tests/images.rs`
（端到端）· `ui/image/**` 单测与 `tests/image_pipeline.rs`（store 层）· `benches/image_frame.rs`（性能）。

## 一、两档能力阶梯（没有第三档）

配置 `rendering.images` 只有两个值：`off | auto`（默认 `auto`，见 [`config-logging.md`](config-logging.md)）。
**任何失败、任何不支持，都落回存量行为**——也就是今天那条链接路径；不画半块马赛克、不画空白块。

| 状态 | 屏幕上是什么 | 机械保证 |
|---|---|---|
| `off` / 终端无图形协议 / 探测失败 | 链接路径（`alt` 文本，点击用系统查看器打开） | `Images::new` 把两层门折叠成一个 `disabled` lane：不建 store、不读盘、不写 buffer |
| 元数据探测中（`Unknown`） | 链接路径 | 元数据表里还没有这条路径 |
| 元数据被拒（缺失 / 目录 / 0 字节 / >16 MiB / 超像素 / 非图 / 不可读） | 链接路径 | `Unavailable` 永不进表（`Images::sync` 只收 `Known`） |
| 元数据已知、编码中 / 编码失败 | 锚点盒子 + caption（`▢ alt · W×H`） | 行数是元数据的纯函数，盒子先占位；**永不**空白 |
| 元数据已知、重写后重探测失败 | 同上（盒子保留） | 见第四节的「退化自愈」 |
| `Ready` | 图片覆盖盒子 | `ui::image::paint` 是那个矩形上的**最后一次写** |

「逐 cell 等于今天」不是形容词：`app::tests::images::a_lane_that_cannot_draw_is_cell_for_cell_the_today_rendering`
与 `pathological_files_are_cell_for_cell_the_baseline` 把两种情况下的整帧与 `rendering.images: off`
的基线**逐 cell 比较**。

## 二、探测与配置

- **时机**：`auto` 时在 `run_app` 里、`spawn_event_stream()` **之前**探测一次（`ImageSupport::detect`，
  500 ms 上限）。探测要向 stdout 写查询序列并读 stdin，事件流线程一起来就会抢同一个 fd，所以顺序不能换。
- **`off` 短路**：不探测、不建 store、不读盘、不写 buffer —— 连那 500 ms 和 tmux `allow-passthrough`
  之类的副作用都没有。「关闭」是一个**零开销**分支，不是「画了但不显示」。
- **设备像素（cell 像素尺寸）来自这次探测**，之后不再查询：见第五节的已知限制。

## 三、数据流：三段，全部在 worker 线程

```text
markdown 图片 → 锚点候选（cell 的链接目标 + 已有锚点，走 resolve_image_path）
    │
    ├─ ImageStore::meta(path)      头信息探测（只读头，不解码）→ 像素尺寸 → 元数据表
    │      表变了 → ImageOpts 换新 → CachedCell 高度缓存失效 → 行数重算
    │
    ├─ ImageStore::request(path, target)  解码 + 协议编码（target = 整个盒子，不是可见部分）
    │      命中 LRU → Ready；在飞 → Pending；拒绝 → Unavailable
    │
    └─ ui::image::paint(...)       唯一的 buffer 写：把图铺进锚点矩形（band 内垂直裁剪）
```

三态：`Ready` 画 · `Pending` 保持 caption · `Unavailable` 保持 caption。渲染路径**零 I/O**：所有
`stat` / 读头 / 解码 / 编码都在 store 的 worker 线程（或 app lane 的新鲜度检查，见第四节）。

## 四、新鲜度：文件被重写要能看见

模型重写同一个路径（`plot.png` 二次生成）是最真实的使用形态。store 不会自己 `stat`（那就是渲染路径
I/O），所以由 app lane 检查：

```text
每一帧    App::draw → Images::observe_visible(chat.frame_images())
              记录「这一帧真正在屏幕上的锚点」= 目标集（去重；没有锚点就是空集）

事件循环  Images::freshness_deadline(now) → 排程；None 时 select 臂彻底停车
          到点 → Images::poll_freshness(now)：
              对目标集逐条 fs::metadata（这一层唯一的 I/O）→ 与基线比较
              基线 = store 记住的版本（ImageMeta 的 bytes + mtime）
                     └ 没有可用答案时（探测在飞 / 已判不可用）退回 lane 自己上次看到的 stamp
              变了 → ImageStore::refresh(path)（丢该路径的元数据 memo）→ mark_dirty()
下一帧    sync → 重新探测 → 新头信息 → 重排盒子 → request 用新 mtime 的 key → 重编码 → 新图
```

| 参数 | 值 | 理由 |
|---|---|---|
| 触发点 | 事件循环的 select 臂（`image_freshness_tick`） | **不在 draw / 不在 paint**：渲染路径零 I/O 是硬约束；帧率与新鲜度无关 |
| 节流 | `FRESHNESS_INTERVAL` = **1 s**（`app/images.rs`） | 任务书给的界；够快（重写后最多 1 s，通常下一帧）、够省（≤ N 次 stat/秒） |
| 目标集 | **当前可见的锚点**（去重后 ≤ band 行数，实测个位数） | 有界：会话里有一千张图也只 stat 屏幕上那几张；滚回视口时下一次窗口就会读 |
| 时钟 | `Instant` 由调用方注入 | 生产传 `Instant::now()`，测试传合成时间 → 节流无需 sleep 即可断言 |
| 空闲成本 | 无可见锚点时 `freshness_deadline` 返回 `None` → 臂停车 | 没有图的会话这个 lane 一次都不唤醒 |

**为什么不是文件监听（inotify / FSEvents）**：新依赖 + 平台分支 + 递归监听成本；而且「模型每次工具调用
写一两个文件」的节奏下，1 Hz 的 `stat` 已经足够快。也**不是**挂在工具事件上：那会漏掉 Bash 里跑的
`python plot.py`、外部进程、用户自己的编辑，还要再解析一遍模型写的原路径。

**退化自愈**（本节的另一半约定）：重写撞上非原子写的中间态（0 字节 / 半截头）时，store 会把它记成
`Unavailable`，而元数据表在一个内容代内**只增不减**——盒子与 caption 留着，图暂时没有；写完成后下一次
检查看到文件版本变了 → `refresh` → 重新探测成功 → 图回来。**不要**把「重探测失败」实现成「删表项退回
链接」：那样这条路径会立刻离开目标集（链接不产生锚点），于是再也不会被重新探测，图就死到下次内容重建。
硬重置仍然是内容重建（会话切换 / 压缩 / rewind / `/clear`）。

**边界**：比较的是 `(bytes, mtime)`（同尺寸改写的判别全靠时间戳，见 `a_same_size_rewrite_is_seen`）；同尺寸 **+ 同 mtime** 的原地重写（纳秒粒度下不会发生）看不见。检查自身只做 `stat`；当 store 的元数据 memo 刚好被 256 上限清空时，`meta()` 会顺手把该路径重新入队探测（I/O 仍不在帧里，只是「一次 stat」要读成「一次 stat + 可能的探测入队」）。
`fs::metadata` 在本地文件系统是微秒级；网络文件系统上可能慢——这是「秒级 × 个位数条」的已知代价。
`app::tests::images` 覆盖：重写可见、无变化不重编码、节流生效、屏幕外的图不检查、退化后恢复
（5 条用例，逐条有变异实验证明判别力）。

## 五、资源上限与预算

| 项 | 常量（`ui/image/store.rs`） | 默认 | 超限会怎样 |
|---|---|---|---|
| 文件大小 | `DEFAULT_FILE_BYTES` | 16 MiB | 探测**在读头之前**按 stat 拒绝 → 链接路径 |
| 像素数 | `DEFAULT_PIXELS` | 16 M | 读完头拒绝（不去分配 100000×100000 的解码缓冲）→ 链接路径 |
| 编码缓存（张数） | `DEFAULT_CACHE_ENTRIES` | 8 | LRU 逐出最久未用 |
| 编码缓存（字节） | `DEFAULT_CACHE_BYTES` | 24 MiB | 同上（估算值 = `cells × cell 像素 × 4`；单张超预算时至少保留它自己） |
| 元数据 memo | `MAX_META_ENTRIES` | 256 | 整表丢弃，下一帧重新探测 |
| 失败 memo | `MAX_FAILED_ENTRIES` | 64 | 整表丢弃（按 target 尺寸记账，防 resize 风暴磨爆） |
| 锚点行数 | `MAX_ANCHOR_ROWS`（`render/markdown/images.rs`） | 36 | 盒子封顶，图在盒内等比缩放留白 |
| app 元数据表（`Images::known`） | 无上限（随会话引用过的**不同图片数**增长） | — | 一代内容内**只增不减**（见 [`tui-rendering.md`](tui-rendering.md) 第五节：缩表会让锚点在链接/锚点之间振荡）；内容重建（`structure_epoch`）整表清空。实测 120 张 = 120 条（`a_hundred_pictures_stay_within_the_cache_budget` 的断言） |

**上限在读取处也成立**（review #135 [S1]）：`probe` 与真正解码之间隔着一个渲染周期，正是「模型重写同一
路径」的高发窗口，所以 `ui/image/encode.rs::decode` 在**同一个 fd** 上复核两道预算——超过 `file_bytes`
时一个字节都不读、超过像素预算时一个解码缓冲都不分配，失败给出与探测相同的 `TooLarge` /
`TooManyPixels` → 链接路径。**不是图**的仍然报 `NotAnImage`：头被截断到解码器构造期就失败的（只有
IHDR 的 30000×30000 PNG 实测如此，33 B / 1.6 ms）走的也是这条，同样不读像素、不分配（测试：
`encode.rs` 的两条复核 + `store.rs::a_file_swapped_after_the_probe_still_hits_the_limits`）。

**扩展名 ↔ codec 必须逐项对齐**（review #135 [S2]）：`IMAGE_EXTENSIONS`（`png jpg jpeg gif webp bmp`）
与根 `Cargo.toml` 的 `image` feature 集是同一份能力声明的两半——只开 `png`+`jpeg` 时，`.gif/.webp/.bmp`
会被路径策略放行但文件头永远解析不了，静默退回链接、不给任何提示。实测代价（release 冷构建，禁用
sccache）：**+0.9 s 构建时间、二进制 +450 KiB（13.14 → 13.60 MiB，+3.5%）**，因此选择把三个纯 Rust
codec 打开而不是收缩声明。`tests/image_pipeline.rs::every_claimed_image_extension_has_a_working_codec`
为每个扩展名写一个真 fixture 并走完「探测 → 编码」，两侧再漂移会红。

**压力验证**（`app/tests/images.rs::a_hundred_pictures_stay_within_the_cache_budget`）：120 张不同图片、
整屏滚动若干步，每一步断言 `cached ≤ 8`、`cached_bytes ≤ 24 MiB`、`memo ≤ 256`、`failed ≤ 64`、
worker 存活、可见图全部画出。变异实验（把 LRU 上限改成永不淘汰）→ `cached: 9` 立刻红。

**端到端上限**：空文件 / 非图 / 稀疏超大（`set_len(16 MiB + 1)`，探测不读一个字节）/ 缺失 → 整帧与
`off` 基线逐 cell 相同、不多占一行；极端宽高比 1×5000（36 行档）与 8000×100（1 行档）→ 盒子等于纯函数
给出的值、图照常画出。

## 六、失效触发点（谁让图失效）

| 触发 | 动作 | 位置 |
|---|---|---|
| `terminal.clear()` / resize / focus 重新获得 | `ImageStore::invalidate()`：丢编码、吊销已发出的句柄（终端可能已经丢弃我们发过的图） | `App::draw` 开头（`needs_full_redraw`） |
| 宽度变化 | 锚点 `cols` 变 → `target` 变 → cache key 变（key 含 canonical 路径 + mtime + target + 终端 cell 像素）→ 自动重编码 | `ImageStore` |
| 元数据迟到（探测完成） | 表变 → `ImageOpts` 换新 → `CachedCell` 高度缓存失效 → 行数重算 | `Images::sync` |
| 内容重建（会话切换 / 压缩重同步 / rewind / `/clear`） | `ImageStore::reset()`（丢 memo + 编码）+ 清空元数据表 → 下一帧按新内容重新探测 | `Images::set_structure_epoch` |
| 文件被重写（同一会话内） | 新鲜度检查 → `refresh(path)` → 重新探测 + 重编码 | `Images::poll_freshness`（第四节） |
| 文件被删除 / 写坏（正在显示） | 同一条通路：图消失、盒子 + caption 保留（此刻不可用），文件回来 1 s 内自愈；硬重置是内容重建 | 同上 |
| 图片行掉出视口 | 不画（不失效）：再滚回来直接命中缓存 | `Images::paint` 与几何 |

## 七、遮挡与选择（图写在哪里、什么时候不写）

- **绘制是这一帧的最后一笔**：status → chat → 滚动条 → composer → popup → cursor → toast → **图片** →
  选择高亮。图片排在 toast 之后是「`paint` 必须是该矩形最后一次写」的直接结论。
- **画布 = chat band 的内容矩形**（已减去滚动条 gutter）：status / composer / popup / gutter 结构性画不到。
- **toast 与盒子相交 → 整张不画**（caption 兜底）：协议载荷在逐 cell 里，局部覆盖会断图。
- **拖拽选择进行中 → 整帧不画图**：`FrameSnapshot` 抓的是可见行文本，占位符会污染复制内容，且选择期间
  行内容必须稳定；松开后的下一帧恢复。
- **画之前先自检**：盒子左上角那一格必须确实是 caption 的 `▢`（行号算术漂了宁可留 caption，也不盖别人的字）。
- 图内宽度小于盒子时（36 行上限导致的长图），盒子首行没被图覆盖的尾部清空——图拥有整个盒子。

## 八、性能（`cargo bench --bench image_frame`）

本机实测（Apple silicon，release，120×60 的 band，每张图 290×20 px → 4 行盒子）：

| 场景 | 均值 | 读法 |
|---|---|---|
| `frame/text`（8 个 cell，图片走链接路径） | **34.2 µs** | 同内容的基线：帧成本由 cell 布局与 blit 主导 |
| `frame/pictures/1` | **8.4 µs** | 1 个 cell（1 张图） |
| `frame/pictures/4` | **19.3 µs** | 4 个 cell（4 张图） |
| `frame/pictures/8` | **33.5 µs** | 8 张同屏：**0.2%** 的 16 ms 帧预算 |
| `scroll/1 · 4 · 8` | 8.5 · 19.4 · 33.9 µs | 每次滚动一行；滚动一屏（60 行）≈ 2 ms |
| `first_encode/290x20` | **37.8 µs** | 首次编码（解码 + 协议编码 + 线程往返） |
| `first_encode/800x600` | **3.42 ms** | 一次重编码，用户可见为「一帧 caption」 |
| `first_encode/1920x1080` | **9.07 ms** | 同上（1920×1080 的图） |
| `freshness/1` | **0.89 µs** | 一次检查 1 个路径（memo 查表 + 1 次 `stat`） |
| `freshness/8` | **7.1 µs** | 同屏 8 张图时，每秒一次 |

口径：bench 走 `ChatView` + `ImageStore` + `paint`（公共 API），不含 `App::draw` 里每帧一次的 `Images::observe_visible`（`pub(crate)`，进不了 bench 目标；独立探针在 debug 构建下对 8 个锚点测得 2.09 µs/帧 ≈ 261 ns/锚点，相对这张表是噪声级）。

结论：图片通道不是帧预算风险（8 张同屏 ≈ 33.5 µs，与同内容的链接路径基线 34.2 µs 同阶；每多一张 ≈ 3.6 µs）；
真正的成本在**首次编码**（毫秒级，一次性，发生在写入之后的第一个窗口）；新鲜度检查每秒最多几微秒。

## 九、已知限制（刻意不做的）

| 限制 | 说明 |
|---|---|
| 不做半块马赛克 / chafa / ASCII art | 降级只有两档（第一节）：画真图，或者链接 |
| 不抓远程图片 | 只吃本地文件系统；`http(s)://`、`data:` 一律走链接路径 |
| 不做工具产物自动预览 | Bash / Read 输出里的图片路径不会被自动识别（只有 markdown `![]()` 是来源） |
| 字号 / tmux passthrough 变化观测不到 | 探测只在启动时做一次；改字号一般同时改行列数 → 以 `Resize` 到达（已覆盖）。只改像素不改网格时，图按旧 cell 尺寸继续显示（终端自己缩放），直到下一次 resize |
| toast 期间被盖的图整张消失 | 不做局部裁剪（会断图）；toast 是短暂 overlay，caption 兜底 |
| 没有文件监听 | 窗口内（≤1 s）发现重写；同尺寸 + 同 mtime 的原地重写看不见（纳秒粒度下不会发生） |
| 重写会闪一下 caption | `refresh` 之后到新编码就绪之间（毫秒级）没有图可画 —— 这是「旧编码不留在屏幕上」（否则就是在显示旧像素）的代价 |
| 不缓存「旧句柄」跨帧 | `ReadyImage` 每帧重新 `request`（命中 LRU 是哈希查找）；跨帧持有协议句柄会让已删除的图继续显示 |

## 十、真机验收（有 TTY 的终端，只能由人做）

单测只到缓冲区与转义序列层；**出图**必须在支持图形协议的终端（Ghostty / kitty / iTerm2）里看。

```bash
mkdir -p /tmp/wing-img-demo && cd /tmp/wing-img-demo
cp ~/Desktop/any-shot.png plot.png          # 任意 png/jpg/jpeg/gif/webp/bmp（远程 URL 一律走链接）
wing                                        # 起 TUI（网关没起就先 wing start），发一条消息，正文原样写：
#   ![销售趋势](./plot.png)
```

正例清单：

1. 该行先变成 `▢ 销售趋势 · W×H` 的占位块，随后被真图覆盖（几帧内：探测头信息 → 编码）。
2. **新鲜度（本次新增）**：让模型（或你自己）**重写同一个 `plot.png`**（换一张内容/尺寸不同的图），
   ≤1 s 内图变成新的；尺寸不同时盒子行数也跟着变。
3. 滚动 / 翻页：图跟着文本走，超出视口的部分不画；滚回来不重新编码（不闪）。
4. 改窗口大小：一瞬 caption 后按新宽度重画；拖拽选择期间整帧只有文本（复制到的是 caption），松开后回来。
5. 写一条空文件 / 一个 >16 MiB 的假 `.png`：那一行**保持链接文本**，不多占行、不空白。
   已经显示出来的图再被删掉：图消失、盒子与 `▢ alt · W×H` caption 保留（不空白、也不退回链接），文件写回后 1 s 内自愈；要它变回链接需要一次内容重建（会话切换 / 压缩 / rewind）。
6. 退出 TUI 后终端无残留（图不留在屏幕上）。

对照例（同机同内容，应逐 cell 等于今天）：`~/.wing/tui/config.yaml` 里 `rendering.images: off`，或换
Alacritty 这类没有图形协议的终端 → 同一行只显示 `销售趋势` 链接文本。

## 十一、症状 → 先看哪里

| 症状 | 先做 |
|---|---|
| 图没出来，只有链接文本 | 终端是否支持图形协议（启动探测一次）；`rendering.images` 是不是 `off`；文件是否存在/可解码（`file plot.png`）；相对路径是否落在当前 workdir（`/workdir`、会话切换后相对路径要重新探测） |
| 有 `▢` 盒子但没有图 | 图此刻不可用：编码中（毫秒级，稍等）、编码失败（截断/坏文件）、正被 toast 盖着、正在拖拽选择。看第五节的阶梯，盒子存在本身就说明元数据是好的 |
| 重写了同一个文件，图还是旧的 | 是否在 1 s 窗口内（第四节）；**是否同尺寸 + 同 mtime**（同尺寸的判别全靠时间戳，用例 `a_same_size_rewrite_is_seen`）；路径拼写是否一致（相对/绝对都会归一化到同一个 key） |
| 图占了 36 行 | `MAX_ANCHOR_ROWS` 的封顶（长图）；调它等于改布局契约，要同步流式矩阵 |
| 图跑到别的文字上 / 半张图 | 不该发生：`paint` 前会核对 caption；请带着 `cargo test -p wing --lib app::tests::images` 的输出报 issue |
| 图在滚动时闪 / 每次滚动都重编码 | `target` 只跟随盒子（宽度/行数），不跟随滚动位置——出现重编码说明盒子变了（窗口宽度变了？） |
| 内存担心 | 第五节的预算表：编码缓存 8 张 / 24 MiB，元数据与失败各有界；压力用例逐项断言 |

## 十二、复跑命令

```bash
# 端到端（缓冲区层）：新鲜度、上限、极端宽高比、压力、失效
cargo test -p wing --lib app::tests::images
# store 层：LRU / memo / 失败上限 / 句柄吊销 / worker 生命周期
cargo test -p wing --lib ui::image
cargo test -p wing --test image_pipeline
# 性能数字（第八节那张表）
cargo bench --bench image_frame
# 渲染层的调试入口（锚点几何、流式对账）
cargo run -q -p wing --example render_probe -- --profile content --images \
    --workspace . --shape './plot.png=800x600' --plain /tmp/fig.md
```
