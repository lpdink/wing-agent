# 开屏海鸥（welcome mascot）

欢迎屏的那只海鸥：待机是站姿 chibi，agent 干活时切成飞行扇翅 —— "它在飞" = "它在干活"。
代码在 `crates/wing/src/ui/welcome/`（`art` 数据 · `sprite` 渲染 · `motion` 规划 · `wordmark` 大字 · `mod` 组装）。

## 数据形态：字母网格

与 dsh 的像素鲸鱼同构：每帧是 `&[&str]`，一个字符 = 一个**像素**（不是终端格），
`.` 透明，其余字母是调色板键（`sprite::brand`）：

| 字母 | 颜色 | 用途 |
|------|------|------|
| `O` | `#1B2338` 深藏青 | 描边（不是纯黑；亮底终端下靠它撑剪影） |
| `W` | `#F7F9FC` | 羽白 |
| `G` / `S` | `#C9D2E0` / `#8E9BB0` | 浅灰 mantle·折翼 / 石板灰翼下缘 |
| `D` | `#3A4763` | 深石板翼尖·尾尖 |
| `B` / `M` | `#F5A93C` / `#C77E22` | 琥珀喙·脚 / 暗面 |
| `E` / `H` | `#141C33` / `#FFFFFF` | 眼 / 眼神光 |
| `A` | 主题 accent | 唯一跟主题走的字母（点缀预留） |

**品牌色固定、不跟主题走**：海鸥之所以是海鸥靠的就是这身颜色；跟主题呼吸的只有
`A` 和 wordmark 的渐变方向（暗底冰→accent，亮底 accent→深，见 `wordmark::is_light_theme`）。

渲染是半格：一个终端格装上下两像素，上=前景、下=背景；只画半格的格子只设前景
（给 `▄` 配背景会把透明的上半填成对面那半的颜色）。见 `sprite::lines`。

## 姿态与动作族

`motion::Motion` 是纯 deadline 状态机（无定时器、无 RNG：jitter 由内部计数器派生，
测试可复算）。语义借自 dsh 的 `whaleIdle.ts`：**每个动作族独立一条平面**
（眨眼 / 抖翅 / 跳各管各的 deadline），"眨眼落在抖翅中途"天然成立。

* 待机（`Pose::Perched`）：`Idle / Blink / Flutter1 / Flutter2` + `lift`
  （0 站定 / 1 呼吸 / 2 跳；抬升 = 标准姿势整帧上移 1 或 2 像素行，不额外存帧）；
  **呼吸是常驻平面**（每 ~1.9s 抬半格 ~320ms）—— 没有它待机就是"每几秒一闪的
  静态图"，用户实测原话"等了半天才看到动画"。节奏常数刻意比 web 宠物密
  （dsh 鲸鱼同款教训：终端 settled header 上按 web 节奏会 visibly stalls）；
* 干活（`Pose::Flying`）：`FLY_0..FLY_5` 六帧扇翅循环，90ms/帧；
* `working` 翻转时重新播种对面那档的节奏 —— 落回站姿不会"一落地就连眨三下"；
  欢迎屏从视口外回来时同样重播（`Motion::resume`），否则四个平面同时到点，
  回屏第一帧会叠成"眨眼 + 跳 + 呼吸"。
* **序列里不许有相邻同值步**（`BREATH_SEQ` / `HOP_SEQ` 都是单拍）：那种 deadline
  不改变姿态，是白醒 + 白重绘（曾经两个平面写成 `[1, 1]`，实测 20.5% 的 tick 空转）。

两个姿态在 header 里对齐到同一终端行数（`ART_TERM_ROWS = 13`，矮的那个顶部补透明），
姿态切换不跳版。盒子比站姿网格高两像素行（`PERCHED_TOP_PAD`）—— 抬升是把整帧上移，
没有头顶余量就会裁掉头冠，读起来像被压扁而不是跳起来。

## 重绘成本契约

海鸥是**常驻 idle 循环**，但只在欢迎屏真的在视口里时才走时钟：

* `ChatView::header_in_view()`（`scroll_offset < header_lines.len()`）是门控信号；
* 滚出视口 → `needs_rebuild` / `next_frame` 全部短路，run loop 的 select 臂停摆；
* 滚回顶部看历史 → 动作自然续上；
* 开屏扫光（wordmark 上那道）2.4s 后定格 —— 扫光期**逐帧**排 tick（~25fps），
  扫光**结束那一刻也要重建一次**（否则最后一帧的高光会留在屏上）；
* 定格之后 idle 循环以**实测 ~2.4 次/秒**唤醒（四平面并行），其中 ~99% 会真的
  改变姿态、各值一次 ~15 行的重绘。相比"滚出视口即 0"，这是常驻动画的代价，
  刻意如此（用户要的是 dsh 那种"活着"的欢迎屏）。

改这块时别破坏这条契约：看不见的东西不花钱，是 idle 循环能被接受的前提。

**首屏预算**：整块档占 15 行（1 + 13 + 1）；80×24 的终端上首屏 19 行里 15 行给了
欢迎屏。它随会话滚走，但如果你要再加内容，先想想这条。

## 品牌资产（README 页头 / 社交预览）

`assets/` 里的三份 SVG **不是手画的**，是 `cargo run -p wing --example export_logo`
从同一份数据导出的：网格读 `ui::welcome::art`（终端渲染的那几个网格），渐变读
`ui::welcome::wordmark::column_color`（终端那套渐变）—— 所以改画之后重跑一次，
README 与真机不会漂移。

| 文件 | 用途 |
|------|------|
| `assets/banner-dark.svg` / `banner-light.svg` | README 页头横版 lockup（`<picture>` 按 `prefers-color-scheme` 切） |
| `assets/gull.svg` | 站姿 mascot 单只（docs / 图标底稿） |
| `assets/social-preview.png` | 1280×640 仓库社交预览（Settings → Social preview） |

两条经验值得写下来：

* **大字按 2 倍像素画**（`WORDMARK_PIX`）。1px 笔画的 5 行字体放大到 3 倍以上，
  笔画之间的空隙会一起放大，字母开始读成虚线 —— 2 倍是散架的临界点。
* 社交预览 PNG 需要浏览器：`banner-dark.svg` 贴进一个 1280×640 的深色页面，
  headless Chrome `--screenshot --window-size=1280,640` 截一张即可（页面里的
  `<img>` 用相对路径时，包装页要放在 `assets/` 里，否则图加载不到）。

## 改画 / 预览的创作期工作流

帧数据是**定稿导出**，不是运行期生成：创作期用一个矢量光栅器（椭圆 / 胶囊 /
多边形 / 扫掠翼 → 4×4 超采样 → 硬边量化 → 外部 flood-fill 描边）画鸟，导出成
字母网格贴进 `art.rs`。改画的步骤：

1. 改形状参数，导出网格，渲染成 HTML（半格真宽高比 + 像素放大两视图）；
2. headless Chrome 截图（`--force-device-scale-factor=2`，长边 ≤1568px）；
3. `ReadImage` 看截图，改，循环 —— 像素画的细节（翼厚、喙角、描边脏不脏）
   只有看见才判得出来；
4. 定稿后把网格贴进 `art.rs`，跑 `cargo run -p wing --example welcome_preview`
   核对生产渲染与创作期一致（`--working` 看飞行态，`--ms` 落在某个动作帧）。

描边只加在**外部**透明像素上（边界 flood-fill）：形状之间 AA 缝隙留下的内部洞
不描边，否则满身麻点 —— 这是光栅器里最容易回退的一条。
