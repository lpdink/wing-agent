# 覆盖率：增量门禁 + PR 评论

PR 的门禁是**增量（patch）覆盖率**：这个 PR 的改动行必须有测试覆盖。每个非 fork
PR 的 CI 把「增量覆盖率（门禁）」与「总量覆盖率（参考）」贴成一条 sticky comment
（marker `<!-- wing-coverage -->`）。

## 口径与阈值

| 项 | 口径 | 数据源 | 阈值的唯一来源 |
|----|------|--------|----------------|
| 增量 · Python | 改动行 ∩ 语句行（**行级**） | `coverage json` 的 executed / missing lines | `Makefile` → `PATCH_COV_MIN`（90.0） |
| 增量 · Rust | 改动行 ∩ DA 行（**行级**） | `cargo llvm-cov report --lcov` | 同上 |
| 总量 · Python | 语句 + 分支（coverage.py） | `coverage json` | —（参考，不门禁） |
| 总量 · Rust | 行 + 区域（cargo-llvm-cov） | `cargo llvm-cov report --json` | —（参考，不门禁） |

- diff 基线 = `git merge-base $PATCH_BASE HEAD`；CI 传 `PATCH_BASE=origin/<目标分支>`，
  本地默认 `origin/develop`。只看**已提交**的改动（工作区不参与）。
- **统计面** = 覆盖率产物覆盖的文件：Python 是 pyproject 的 `source`（`wing` +
  `wing_sdk`），Rust 是 workspace crates。只有面内文件的改动行参与判定；面外的
  代码（`scripts/`、`libs/wing_hooks`、探针等）不判定，但会在评论里**点名**
  （`unmeasured`）——测试文件按约定剔除（新测试行不判定是设计，不是盲区）。
- 90% 的定位是防倒退：给代码搬家（重构）、平台分支留出余量，同时挡住整块新代码
  裸奔。演练失败路径：`PATCH_COV_MIN=99 make patch-cov-python`。
- 判定边界：无改动可执行行（纯文档 / 配置 PR）→ 判过并在评论注明；coverage
  产物缺失 → 不判（`skipped` 写进评论，测试已经红着）；git 基线解析失败 → **硬
  失败**（配置错误必须可见，不能静默关掉门禁）。
- **已知边界（增量口径天生看不见的东西）**：删除行不进判定——净删除测试行的文件
  会被点名（`removed_tests`，评论里提示人工确认），但**没有任何门禁会拦持续的
  总量劣化**。若要堵死这条演化路径，加一个明显宽松的总量下限即可（例如
  Python 90 / Rust 88，低于现状几个点）——当前刻意没加，总量只作参考。

## 判定与实现

- `scripts/ci/patch_coverage.py`：解析 `git diff --unified=0 -M <merge-base> HEAD`
  的新增行，与覆盖率产物求交 → 判定 + 写 patch json（`--out-json`，评论渲染的
  就是它——**展示的数字 = 门禁的数字**）；同时点名统计面外的改动与净删除的测试行
  （不判定，供评论展示）。`--selftest` 离线自检（diff 状态机、引号路径、两种覆盖率
  格式解析、判定边界、临时 git 仓库端到端），CI 在 python-check 里每次跑。
- `make patch-cov-python` / `make patch-cov-rust`：本地 = CI 同一条命令——先由
  既有 `.coverage` / profraw 导出（不重跑测试），再判定；CI 用 env 覆盖
  `PATCH_BASE` 到目标分支。
- 为什么门禁放 check job：失败面贴在语言对应的检查项上（Python Check / Rust
  Check 红），评论 job 只负责渲染。

## Rust 覆盖率（cargo-llvm-cov）

- 工具链：`cargo install cargo-llvm-cov --locked` + `rustup component add llvm-tools-preview`。
  缺工具时 `make test-rust` 硬失败并打印安装指引，不静默退化 —— 否则本地「过了」而 CI 才拦。
- 插桩产物在 `target/llvm-cov-target/`（`RUSTFLAGS` 带 `-C instrument-coverage`），与普通
  `cargo test` 的产物**互不失效**：交替跑两者会各触发一次整树重编译，日常统一走 `make test-rust`。
- doctests 由 `make test-rust` 里单独的 `cargo test --doc` 兜住：cargo-llvm-cov 的 `--doctests`
  依赖 nightly 的 `-Z unstable-options`，stable 不可用。
- 集成测试 spawn 的子进程覆盖率**会被收集**（`CARGO_BIN_EXE_wing` 指向插桩二进制，
  `LLVM_PROFILE_FILE` 随环境继承）——e2e 的贡献没有丢，报告里那些按 PID 命名的 profraw 就是证据。
- HTML 报告：`make test-rust-html` → `target/coverage-rust/index.html`。

## PR 评论

- 数据流：`python-check` / `rust-check` 跑完测试各导出机器可读产物（`coverage json` /
  `cargo llvm-cov report --json`）与**门禁自己写下的 patch json** →
  `coverage-report` job 合并渲染（`scripts/ci/coverage_report.py`）→
  `scripts/ci/sticky_comment.py` 按 marker upsert。与门禁**同一次运行**，不重跑测试。
- 怎么读：主表「增量覆盖率（门禁）」两行（改动行覆盖 / 覆盖÷可执行 / 阈值 / 状态），
  下面折叠块逐文件列出**未覆盖的改动行**——补测试直接照它来；再往下是「总量（参考）」
  与分包 / 分 crate 明细（按覆盖率升序，低者在前）。`—` = 该侧产物缺失或未判定，
  「注意」区说明原因，并点名**统计面外的改动**与**净删除的测试行**（两者不判定）。
- 门禁红也发（评论要展示「低在哪」）；被并发取消的 run 不发；**fork PR 不发**
  （token 只读）——fork 的增量门禁照跑，只是没有评论。
- 阈值不在这条链路上重复声明：渲染脚本读 Makefile 的 `PATCH_COV_MIN`，patch json
  是门禁原样写下的判定。

## 本地复现

```bash
make test-python        # Python 测试（总量参考）· make test-python-html → target/coverage-python/
make test-rust          # Rust 测试（插桩构建）· make test-rust-html → target/coverage-rust/
make patch-cov-python   # 增量门禁（基线 PATCH_BASE=origin/develop，可覆盖）
make patch-cov-rust     # 同上（Rust；需先跑过 make test-rust）
uv run python scripts/ci/patch_coverage.py --selftest   # 门禁脚本自检

# 离线演练 PR 评论（不联网、不贴出去）
python3 scripts/ci/coverage_report.py --python-json coverage-python.json \
    --rust-json coverage-rust.json --patch-python patch-python.json \
    --patch-rust patch-rust.json --out-md /tmp/comment.md
python3 scripts/ci/sticky_comment.py --repo lpdink/wing-agent --pr 1 \
    --marker '<!-- wing-coverage -->' --body-file /tmp/comment.md --dry-run
```
