# README 演示素材（GIF）+ 性能水位尺

`assets/` 里的 GIF 不是手工截的图，是**真 TUI 的确定性回放**：模型输出由假 Provider
喂（剧本或按 tok/s 灌语料），其余全是真的——真网关、真 `wing` 二进制、真 agent 循环，
`Read`/`Grep`/`Edit`/`Bash` 在真工作区里真跑（diff 是真改出来的，测试输出是真
`python3 -m unittest` 打出来的）。录制器按帧抓 tmux 里的**真彩屏幕**，交给 `agg`
渲染成 GIF。

改了配色、文案、cell 渲染之后重跑一条命令，素材自动跟上，不存在"截图和真机不一样"。

## 两条产线

| 产线 | 脚本 | 产物 |
|------|------|------|
| **剧情回放**（hero） | `serve.py` 剧本 + `record.py` | `demo.gif`：开屏 → 打字 → thinking → 工具卡片 → diff → 测试由红转绿 → 任务清单勾完 |
| **速度演示**（渲染性能） | `stream.py` 按 tok/s 灌语料 + `record.py --seconds` | `speed-{30,60,240,3000}.gif`：同一份语料、四种速率 |
| **性能水位尺** | `stream.py --marker-every` + `latency.py` | 端到端显示延迟 p50/p99（README 里那张表） |

```bash
# 剧情 hero（≈35s）：录 → 校验 → 渲染 → 发布到 readme-assets release（= make demo）
uv run python scripts/demo/record.py --release

# 速度演示（每档 ≈35s；96 列是为了让底栏那栏累积 token 让出去，见下）
uv run python scripts/demo/record.py --serve stream.py --serve-arg=--tps=3000 \
    --serve-arg=--usage-every=1 --seconds 8 --cols 96 --rows 24 --fps 10 --fps-cap 10 \
    --name speed-3000 --release

# 端到端显示延迟（每档 ≈12s）
uv run python scripts/demo/latency.py --steps 3000,30000,45000 --marker-every 2000 --seconds 10
```

产物落在 `target/demo/`：`<name>.cast`（asciinema v2）、`<name>.gif`、`stills/*.png`、
`pane.raw`（pane 的原始字节流，排查启动问题用）、`timeline.txt`。

**录完会自检**：hero 默认要求画面里出现 `+ def fetch(`、`OK`、`All three tests pass.`；
速度图要求 ` t/s ` 之外还要**问句与答句对得上**（`walk me through` + `job queue`）——
后者是补的：曾经四张速度图里敲的是 hero 的问句、答的是队列文档，只校验 ` t/s ` 时
谁也看不出来。缺任何一项就判本次录制失败、**不渲染也不发布**，直接非零退出——防的是
"工具 schema 漂了 / Bash 被拦了，GIF 里是一张红卡片，却静默成功"。要录别的镜头用
`--expect TEXT`（可重复）指定自己的守门内容（改语料标题时记得同步 `SPEED_EXPECT`）。

依赖：`tmux`（必须）、`agg`（首次运行自动下到 `target/demo-tools/`，`$DEMO_AGG` 可覆盖；
网络要代理时先 `export https_proxy=...`）、出 PNG 用 `sips`（macOS）或 ImageMagick、
等宽字体（默认 `MesloLGS NF`，`$DEMO_FONT` 可换；**字体缺 CJK 字形会让中文变豆腐块**）。

## 机制

1. **`env.py`** —— 两条产线共用的骨架：隔离的 `$WING_HOME`（默认 `~/wing-demo/home`）、
   按需分配端口的网关子进程、config 生成、健康检查、收摊。绝不碰真实的 `~/.wing`。
2. **`serve.py`（剧情）** —— 假 Provider 按 model 名路由剧本：每轮一个 `Turn`
   （thinking / text / tool_calls / usage + 分片粒度 `chunk`、片间延迟 `delay`）。工具
   参数的流式分帧靠 `ToolCall.cut`（`call()` 的 `every=`），这样 diff 才会一行行长出来
   而不是一次性出现。最后一轮被消费时打 `[demo] script exhausted`，录制器据此收尾。
3. **`stream.py`（速度）** —— 按目标 tok/s 吐 `corpus.py` 生成的语料：发射用**绝对
   deadline 自校正**（`env.sleep_until`），所以 `--tps` 是真实速率；`--cpt` 决定
   一 token 折算多少字符（默认 2.83，与主流 provider 同量级）。
   - ``--usage-every 1`` = 每帧带累计 usage → 底栏的 `↓N out · M t/s` **流式过程中**
     就在跳（vLLM `continuous_usage_stats` 就是这个形态），GIF 要的就是这个；
     默认 `0`（只在回合末尾带一次）与真 provider 的常规形态一致。
     **代价**：每帧 usage 会让事件量翻倍，3000 tok/s 下显示延迟从 ~15ms 涨到 ~150ms
     ——所以量延迟时用默认值，配 GIF 时用 `--usage-every 1`。
   - 速度 GIF 用 **96 列**：状态栏的累积 token 只在 ≥100 列（`WIDE_THRESHOLD`）才显示，
     而每帧 usage 会让那个累积值虚高（每个事件都累加一次），窄一档正好把它让出去。
4. **`corpus.py`** —— 速度演示的长语料（默认 ~260 KB）：一份虚构的"任务队列设计文档"，
   标题 / 段落 / 中英混排 / 行内代码 / 多语言代码块 / 表格 / 引用 / 公式都喂到，确定性生成
   （同 seed 同文本），可公开。
5. **`record.py`（录制）** —— tmux 里跑真 `wing`，按 `fps` 抓 `capture-pane -e`
   （真彩 ANSI），只在变化的行上写字节 → cast → `agg` → GIF；静态图按**屏幕内容标记**
   挑帧（`STILLS`），走同一条渲染路径。
   - **启动要应答 `ESC[6n`**：ratatui 的 `Terminal::clear()` 会问光标在哪，detached 的
     tmux 不代答，答不上来首帧永远不画。所以先让 pane 停在 shell 里等闸门文件，
     `pipe-pane` 挂好再 `exec wing`（否则会漏掉启动那一条查询），然后由
     `Terminal.answer_queries()` 盯着原始字节流补 `ESC[1;1R`。
   - `pipe-pane` 的出水管是 `rawdump.py` 而不是 `cat`：`cat` 写文件是全缓冲的，几百字节
     的启动序列会躺在它缓冲区里，表现为"有时能录、有时卡住"。
   - **会话继承客户端环境**：tmux 新会话复制客户端 env，录制器为下 agg 导出的
     `http(s)_proxy` 会漏进 pane，于是 TUI 连自己的 loopback 网关也要走代理 → health
     失败 → 报 `Port … already in use`。`record.py` 显式把这些变量清成空。
   - 收尾两种：剧情模式等「剧本耗尽 + 会话回 idle」；速度模式按 `--seconds` 计时。
6. **`latency.py`（水位尺）** —— 假 Provider 每 N 帧插一行 `⟦M#####⟧`，在真 TUI 的 pty
   字节流里找它首次出现：`lag = 出现在终端 − Provider 发射`，量的是用户真正感受到的那条链
   （Provider → 网关 → WS → TUI 读任务 → 事件通道 → 16ms 帧闸门 → 终端写出）。与
   fast-stream 时代的 `lag_marker.py` 同源，这里接的是仓库自己的 `stream.py`。

## 调镜头

| 想改什么 | 改哪里 |
|----------|--------|
| 剧情、模型说的话、工具调用 | `serve.py` 的 `build_script()` |
| 流式快慢 | `serve.py` 的 `FAST` / `ARGS`（+ 单轮的 `chunk`/`delay`） |
| 喂入速率 / 每轮长度 / ttft | `stream.py` 的 `--tps` / `--turn-chars` / `--ttft` |
| 语料内容 | `corpus.py` 的模板池（改完 GIF 里的文字就跟着变） |
| 终端尺寸、字体、字号、帧率 | `record.py` 顶部常量或同名 CLI 选项（**`font_size` 与 `cols` 直接决定成图宽度**） |
| 静态图取哪一帧 | `record.py` 的 `STILLS`：`(帧里出现的文本, 文件名)`，`<last>` = 最后一帧；临时试 `--still '标记=名字'` |

## 命名与落位（GIF 不进 git）

README 里的 GIF **不放在仓库里**，而是挂在同一个 rolling release 上：

```
https://github.com/lpdink/wing-agent/releases/download/readme-assets/<name>.gif
```

| 资产名 | 用在哪 |
|--------|--------|
| `demo.gif` | README 顶部 hero（剧情回放） |
| `speed-{30,60,240,3000}.gif` | README「Built for machine speed」（同一份语料、四种速率） |

`record.py --release`（`make demo` 就是它）渲染完会 `gh release upload readme-assets
target/demo/<name>.gif --clobber`：**同名覆盖，URL 不变**，所以"重录 → 重新上传"是干净的，
几 MB 二进制也不进 git object store（clone / tarball 都不含）。release 不存在时脚本会
自己建（标为 pre-release，避免混进正常版本列表）。

所以改完 UI 的闭环是：`make demo`（hero）+ 上面那条速度配方 → README 立刻看到新画面。
没有 `gh` 或不想联网时，去掉 `--release` 只录到 `target/demo/`，之后手工 `gh release upload`。

## 注意

- 屏幕上的 token / cache / t·s⁻¹ 数字来自**脚本**（假 Provider 报的 usage），不是实测；
  只有 `latency.py` 的延迟表是量出来的。
- 状态栏上的模型名与 provider 名（剧情里 `sonnet-4.5` / `anthropic`，速度里 `stream` /
  `demo`）只是文案，在 `serve.py` / `stream.py` 顶部的常量改。
- 录制会写 `$HOME/wing-demo/`（`$DEMO_ROOT` 可改）并**清空** `$WING_HOME/core/sessions`
  ——默认的 `$WING_HOME` 也在 `$HOME/wing-demo/` 下，别指向真实的 `~/.wing`。
