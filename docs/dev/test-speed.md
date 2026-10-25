# 测试回环：时间花在哪，旋钮怎么定，怎么量

> 这页讲**跑测试这件事本身的成本**：`make test` 的四组为什么各自是那个速度、并行度按什么定、
> 改完怎么证明真的快了，以及加测试时要为回环付多少。功能层面的测试约定不在本页
> （probe → [probe-testing.md](probe-testing.md)，PR 级性能测量 → [perf-testing.md](perf-testing.md)）。

## 1. 四组与它们的旋钮

| 组 | 跑什么 | 独占时的并行度 | 与另外三组同时跑时 |
|---|---|---|---|
| `make test-python` | `libs/core/tests` + `libs/wing-sdk/tests`（xdist，`--dist loadfile`） | `min(8, 核数)`（`PYTEST_WORKERS`） | `min(4, 核数/3)`（`WING_TEST_SHARED=1`） |
| `make test-probe` | `libs/wing-probe`（xdist，`--dist loadfile`） | `1.5 × 核数`（`PROBE_WORKERS`） | 同左（见 §2.1，它要的是延迟覆盖不是算力） |
| `make test-rust` | 55 个测试二进制 + doctests（`scripts/test_rust_parallel.py`） | `--cores 核数` | `--cores max(2, 核数/3)` |
| `make test-ts` | vitest（两个 project） | vitest 自己定（核数-1） | `--maxWorkers max(2, 核数/3)`（`TS_WORKERS`） |

`WING_TEST_SHARED=1` 只由 `scripts/collect_output.sh` 在四组并行时导出。**单独跑一组的人拿满机器**——
"共享档"是四组共处一机时的配平，不是新默认。`WING_TEST_SERIAL=1` 仍然把四组（以及组内并行）一起退化成串行，
是排查并发干扰的逃生舱。

为什么需要"共享档"：四组各自按"我独占整机"配并行度，机器会被压到 2~3 倍超订，**每一组都变慢**。
本机 12 核实测：四组默认 → 墙钟 30.5s；把后三组各收到 1/3 核数 → **26.0s**，同样四组全绿。

## 2. 各组的成本结构

### 2.1 probe —— 每个场景一个真进程

一次 `ProbeEnv.start()` ≈ 0.24s，其中 **~0.19s 是网关子进程的 Python 导入**（fastapi / pydantic /
uvicorn / wing 自身的启动图），剩下是解释器启动与健康握手。230 个场景环境 × 0.19s ≈ 45s 的纯 CPU，
这是 probe 最大的单项成本，也是它的**硬地板**：想更快只能减少环境数量，或者让网关启动变便宜
（后者被评估过并否决：可达的延迟导入边际只有 1~2ms/次，见 `design.md` 的记录）。

因此 probe 的时间由两个量决定：

- **墙钟 = 最慢 worker 手里的那堆用例**。xdist 的 worker 之间不偷活，队列一发空就只剩 straggler 撑着。
  所以派发粒度必须是**文件**（`--dist loadfile`），不是逐条（`--dist load`）：逐条派发会把整族慢用例
  攒到同一个 worker 上（实测：12 核下 `load` 22.4s → `loadfile` 19.2s，而 worker 忙碌总时间两者相同，
  差的只是最后的尾巴摊不摊得平）。
- **停机不花时间**：`stop()` 是 SIGKILL（不跑 lifespan），`shutdown()` 才是优雅路径。
  判据只有一条：**这条 lane 之后还有没有人观察"停机时发生了什么"**——没有就用 `stop()`。
  uvicorn 的优雅停机固定 ≥0.2s（`main_loop` 与 `shutdown()` 各一次 `sleep(0.1)`），这笔钱在被测语义里
  一分都不值。要观察停机语义的场景显式 `await probe.shutdown()`。

### 2.2 python 单元测试 —— 一半的用例在等 I/O

`libs/core/tests` 里相当一部分用例在等真实的秒级超时或 `asyncio.sleep`。**"睡够"不是验证**：
把固定 `sleep` 换成"等到条件成立（上限秒级）"、把秒级超时缩到语义仍被跨过的量级，
墙钟掉一半而 CPU 基本不变。串行耗时是这个改动最严格的检验（并行会把等待藏进别的 worker 里）。

并行度受"最长单文件"约束：`--dist loadfile` 下墙钟 ≈ max(总工作量 / n, 最长单文件)。

### 2.3 rust —— 一半工作量在一个二进制里

55 个套件的耗时**直接相加**（cargo 内部只有单个二进制才多线程），而其中一半以上压在 `wing` 的 lib
单测一个套件里（3000+ 用例）。所以额度不是"每套件 N 线程"，而是按**用例份额**分配：

- 套件的 libtest 线程数 = 它的用例份额 × 核数（下界 2）；
- 同时在跑的二进制数 = 核数；最长先跑；doctests（一次独立的 cargo 调用）第一个发车，不与池子串行。

于是大套件拿满线程、54 个 I/O 型小套件各拿 2 条——同时飞的是"核数"个二进制，而线程峰值高于核数
（集成套件的线程大半时间在等子进程 / 端口 / pty，多给不占 CPU）。规则由 `--selftest` 离线钉住。

### 2.4 ts —— 环境创建比测试本身贵

webview 工程的 50 个文件里有 17 个要吃 jsdom，而 jsdom 的创建占掉近四成追踪时间。
`pool: 'vmThreads'` 让每个 worker 建一次 jsdom（逐文件的模块隔离仍然保留），整组 4.4s → 2.4s。
只给 webview 工程：`node` 工程的 artifact 套件要在真实 Vite 管线里跑产物、launcher 套件要改进程 PATH，
两者在 vm 上下文里都不成立。

## 3. 三条原则（改测试时的判据）

1. **不携带被测语义的固定延迟一律砍掉**。超时、握手、优雅停机里的固定 `sleep` 是首选目标；
   砍之前先回答"这一步之后有人观察它吗"，答案写进代码里（显式的两条路径 > 注释里的约定）。
2. **并行度按"这组在等什么"定，不按核数拍**。等 I/O 的组要超订（probe 1.5× 核数），
   烧 CPU 的组不该（rust 按份额分线程）；四组共处一机时再按共享档收一遍。
3. **合并只合并完全重复的断言**。同一个 fixture 下只差一两个断言的用例可以合并，
   代价是失去失败模式的独立性——每一条断言、每一个失败模式都要在合并后仍然存在。

## 4. 怎么量

单次墙钟在满载机器上能翻倍（同一份代码见过 33s 与 67s），所以"改前 33s、改后 20s"这种比法把负载漂移
算成了收益。用交错 A/B：

```bash
# 两个 checkout（worktree 即可），先各预热一次，再逐轮交替跑、取中位数
uv run python scripts/bench_tests.py \
    --tree /path/to/baseline --tree /path/to/worktree \
    --label base --label after --repeat 3
```

- 每轮在 A、B 上各跑一次（相邻发生，负载条件相同），轮次之间交换先后，抵消单向漂移；
- 逐组数字来自 `scripts/collect_output.sh` 在 `WING_TEST_REPORT=<path>` 下落的那份 JSON
  （不设这个变量时它的输出与以前逐字一致，日常跑不为此付费）；
- 有负载门禁：1 分钟负载高于 `--max-load` 直接拒跑——**先确认本机没有别的会话在跑**
  （`wing ps` 看 status=working 的会话），否则测的是别人的 CPU；
- 某一侧本来就有已知偶发失败时用 `--allow-red`（墙钟照算，结论里逐轮列出哪一组红）。

## 5. 加测试要付多少

| 你加的 | 回环上的成本 |
|---|---|
| 一个 probe 场景 | ≈0.19s 的网关启动 + 断言本身；**每个用例一个环境**是设计（进程级隔离），别指望共享 |
| 一个 python 单测 | 通常毫秒级；写了 `sleep(1)` 就是 1s，写了"轮询 5 次 `sleep(0.2)`"就是 1s |
| 一个 rust 测试文件 | 一个新的测试二进制：多一次进程启动 + 多一个套件（几十毫秒） |
| 一个 vitest 文件 | 多一次 worker 启动（≈0.24s，除非复用 worker） |

## 6. 已知边界

- `make test` 的联合墙钟由 **probe 的关键路径**兜底：probe 单独跑 ≈17s，四组一起跑时它的墙钟会涨到
  ~26s（其它三组抢 CPU，而它是延迟受限、对 CPU 压力敏感）。想让联合时长再降，唯一有效的方向是降低
  probe 的环境启动成本（见 §2.1），而不是继续调并行度旋钮。
- probe 的并行度曲线在 1.5× 核数处拐平；再往上墙钟不降、CPU 还涨。
- 低核机器（< 6 核）上 rust 的份额公式会让大套件的线程数低于旧的固定值 4，收益会小一些；CI 不跑这个脚本
  （`rust-check` 走 `cargo llvm-cov`），不影响门禁。
- `crates/wing-math` 的墙钟比值断言（`budgets.rs`）在重负载下仍有小概率假红：成对采样已把频率压下来，
  彻底修需要量线程 CPU 时间而不是墙钟。
