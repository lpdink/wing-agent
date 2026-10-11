# 覆盖率：两层门禁 + PR 评论

`make test-python` / `make test-rust` 各自带覆盖率门禁（本地与 CI 同一条命令），每个非
fork PR 的 CI 把两层覆盖率贴成一条 sticky comment（marker `<!-- wing-coverage -->`）。

## 口径与阈值

| 层 | 口径 | 阈值的唯一来源 | 现状（2026-10 实测） |
|----|------|----------------|----------------------|
| Python（`wing` + `wing_sdk`） | 语句 + 分支（coverage.py） | `pyproject.toml` → `[tool.coverage.report] fail_under` | 96.2%（门禁 95.0） |
| Rust（workspace） | 行 + 区域（cargo-llvm-cov） | `Makefile` → `RUST_COV_LINES_MIN` / `RUST_COV_REGIONS_MIN` | 行 92.1% / 区域 92.5%（门禁 91.0 / 91.5） |

阈值定位是**防倒退**而非「已达标」：各留约 1pp 余量，避免正常重构（代码搬家、平台分支）
变成假报警。抬阈值前先实测现状 —— Rust 看 `make test-rust` 的 TOTAL 行与逐文件明细
（分 crate 聚合见 PR 评论），Python 看 `make test-python-html`。

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

- 数据流：`python-check` / `rust-check` 跑完门禁后各导出机器可读产物（`coverage json` /
  `cargo llvm-cov report --json`）→ `coverage-report` job 合并渲染（`scripts/ci/coverage_report.py`）
  → `scripts/ci/sticky_comment.py` 按 marker upsert。与门禁**同一次运行**，不重跑测试。
- 门禁红也发（评论要展示「低在哪」）；被并发取消的 run 不发；**fork PR 不发**（token 只读），
  正文只留在 run 的 Step summary 与 artifact。
- 阈值不在这条链路上重复声明：渲染脚本直接读 pyproject.toml / Makefile。
- 怎么读：主表三行（Python 语句+分支 / Rust 行 / Rust 区域），`—` = 该侧产物缺失，
  「注意」区说明原因；折叠块是 Python 分包与 Rust 分 crate 明细（按覆盖率升序，低者在前）。

## 本地复现

```bash
make test-python        # Python 门禁     · make test-python-html → target/coverage-python/
make test-rust          # Rust 门禁       · make test-rust-html   → target/coverage-rust/

# 离线演练 PR 评论（不联网、不贴出去）
uv run coverage json -o /tmp/py.json --fail-under=0
cargo llvm-cov report --json --output-path /tmp/rust.json
python3 scripts/ci/coverage_report.py --python-json /tmp/py.json \
    --rust-json /tmp/rust.json --out-md /tmp/comment.md
python3 scripts/ci/sticky_comment.py --repo lpdink/wing-agent --pr 1 \
    --marker '<!-- wing-coverage -->' --body-file /tmp/comment.md --dry-run
```
