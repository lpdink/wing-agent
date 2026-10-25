.PHONY: check check-python check-rust check-ts test test-python test-python-cov test-python-html test-rust test-rust-cov test-rust-html test-probe fmt fmt-python fmt-rust fmt-check fmt-check-python fmt-check-rust fmt-ts fmt-check-ts test-ts patch-cov-python patch-cov-rust run install gateway demo

# ── Unified commands (Python + Rust) ─────────────────────────

# ── TypeScript (extensions/vscode) ─────────────────────────────
# 门禁与 CI 的 typescript-check job 等价：先按 lockfile 安装（--prefer-offline：
# 本地已有 store 时不动网络），再依次跑 eslint / prettier / tsc / vitest。
# 未接进 `fmt` / `fmt-check`：那两条要在 pre-commit 里保持秒级，而且不该强依赖 node_modules。
#
# 工具链缺失时的行为：显式探测 node / pnpm，缺哪一个就打印
# 可操作的提示再退出 1 —— 而不是让 `pnpm: command not found` 混进结论块里，
# 让只改 Python 的人以为自己弄坏了什么。`SKIP_TS=1` 是本地逃生舱（CI 不设，
# 门禁照旧强制），collect_output.sh 会把 ts 组标成 skipped。
VSCODE_DIR := extensions/vscode

# vitest worker 上限：独占时交给 vitest 自己定（默认核数-1），与另外三组同时跑时收到
# `WING_TEST_BUDGET_TS`（数值，由 `scripts/collect_output.sh` 算，见 PYTEST_WORKERS 处）。
# `TS_ARGS` 是直接透传给 vitest 的逃生舱（`make test-ts TS_ARGS="--reporter=verbose"`）。
# 注意**不要**写成 `pnpm run test -- $(TS_ARGS)`：pnpm 会把 `--` 连同后面的实参一起吞掉，
# vitest 一个都收不到（静默失效）。`pnpm run test <args>` 才是透传。
TS_WORKERS ?= $(if $(WING_TEST_BUDGET_TS),--maxWorkers=$(WING_TEST_BUDGET_TS))
TS_ARGS ?= $(TS_WORKERS)

# Node/pnpm 探测 + 提示（单行，避免 make ↔ shell 的续行转义；见 docs/dev/vscode-extension.md §9.3）。
TS_SKIP_NOTE = echo "⏭️  SKIP_TS=$(SKIP_TS) — TypeScript gates (extensions/vscode) skipped on request."; echo "   CI never sets SKIP_TS: the group stays mandatory there."; exit 0
TS_TOOLING_CHECK = command -v node >/dev/null 2>&1 || { echo "❌ node not found — the TypeScript gates need Node ≥ 22.12 (vitest 5 / vite 8)."; echo "   Install Node 22 LTS, then re-run; or skip this group with: SKIP_TS=1 make check"; exit 1; }; command -v pnpm >/dev/null 2>&1 || { echo "❌ pnpm not found — this package pins pnpm 11 (packageManager in extensions/vscode/package.json)."; echo "   Enable it with: corepack enable   (corepack ships with Node ≥ 16.13; CI does the same)"; echo "   Or skip the TypeScript group: SKIP_TS=1 make check"; exit 1; }

# ── README 演示素材 ───────────────────────────────────────────

# 真 TUI + 假 Provider（剧本）的确定性回放：tmux 抓真彩屏幕 → asciinema cast →
# 重录 README 的 hero（剧情回放），渲染后发布到 readme-assets 这个 rolling release
# （URL 不变；速度演示那 4 张要单独跑，配方见 scripts/demo/README.md）。cast 与静态图
# 落在 target/demo/。
demo:
	uv run python scripts/demo/record.py --release

run:
	cargo run

install:
	cd crates/wing && maturin develop --release
	uv sync

# 职责划分：`check` 只跑静态检查、`test` 只跑测试 —— 两者不重合，可以放心连跑
# （同一件事不会被跑两遍；组清单见 scripts/collect_output.sh）。
check:
	bash scripts/collect_output.sh check

fmt: fmt-python fmt-rust

# 与 CI 的格式门禁等价（Ruff format check + cargo fmt --check），供 pre-commit 快速拦截
fmt-check: fmt-check-python fmt-check-rust

# 只跑测试，不做静态检查（与 `check` 不重合）。
test:
	bash scripts/collect_output.sh test

check-ts:
	@if [ -n "$(SKIP_TS)" ] && [ "$(SKIP_TS)" != "0" ]; then $(TS_SKIP_NOTE); fi; \
	$(TS_TOOLING_CHECK); \
	(cd $(VSCODE_DIR) && pnpm install --frozen-lockfile --prefer-offline --reporter=silent) || exit 1; \
	echo "🔍 Running eslint (extensions/vscode)..."; \
	if ! (cd $(VSCODE_DIR) && pnpm run lint); then echo "❌ eslint failed"; exit 1; fi; \
	echo "✅ eslint passed"; \
	echo ""; \
	echo "🔍 Running prettier --check (extensions/vscode)..."; \
	if ! (cd $(VSCODE_DIR) && pnpm run format:check); then echo "❌ prettier failed"; exit 1; fi; \
	echo "✅ prettier passed"; \
	echo ""; \
	echo "🔍 Running tsc --noEmit (extensions/vscode)..."; \
	if ! (cd $(VSCODE_DIR) && pnpm run typecheck); then echo "❌ typecheck failed"; exit 1; fi; \
	echo "✅ typecheck passed"

test-ts:
	@if [ -n "$(SKIP_TS)" ] && [ "$(SKIP_TS)" != "0" ]; then $(TS_SKIP_NOTE); fi; \
	$(TS_TOOLING_CHECK); \
	(cd $(VSCODE_DIR) && pnpm install --frozen-lockfile --prefer-offline --reporter=silent) || exit 1; \
	echo "🔍 Running vitest (extensions/vscode)..."; \
	if ! (cd $(VSCODE_DIR) && pnpm run test $(TS_ARGS)); then echo "❌ vitest failed"; exit 1; fi; \
	echo "✅ vitest passed"

fmt-ts:
	cd $(VSCODE_DIR) && pnpm run format

fmt-check-ts:
	cd $(VSCODE_DIR) && pnpm run format:check

# ── Python ────────────────────────────────────────────────────

gateway:
	uv run wing-gateway

# 单测并行度：独占时 min(8, 核数)，与另外三组同时跑时降到约 1/3 核数。
# `--dist loadfile` 把**整文件**派给一个 worker，所以墙钟 ≈ max(总工作量 / n, 最长
# 单文件)：12 核本机实测全套 ≈14s 工作量、最长单文件 ≈2s——n=8 已贴到「最长单文件」
# 那条下界，再加 worker 只多付每进程的启动与导入开销。xdist 每个 worker 是独立进程，
# 只看本文件内的状态；跨文件的时序耦合由 `--dist loadfile` 挡掉。
#
# `WING_TEST_BUDGET_PYTEST`（数值）由 `scripts/collect_output.sh` 在四组并行时导出：
# 四组各自按"我独占整机"来配并行度时，机器被压到三倍以上超订，**每一组都变慢**；
# 份额的算法与实测都在那一处（全仓唯一），这里只消费。不导出时按独占算——单独跑
# 一组的人仍然拿满。
PYTEST_WORKERS ?= $(or $(WING_TEST_BUDGET_PYTEST),$(shell n=$$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4); if [ -n "$(WING_TEST_SERIAL)" ] && [ "$(WING_TEST_SERIAL)" != "0" ]; then echo 1; elif [ "$$n" -gt 8 ]; then echo 8; else echo $$n; fi))
PYTEST_WORKER_ARGS = $(if $(filter 1,$(PYTEST_WORKERS)),,--dist loadfile -n $(PYTEST_WORKERS))

# 本地日常：不带覆盖率（`--cov` 让同一套用例多花 ~10s）。覆盖率数据只有 CI 的
# patch 门禁与 PR 评论要，走 test-python-cov。
test-python:
	uv run pytest libs/core/tests/ libs/wing-sdk/tests/ $(PYTEST_WORKER_ARGS)

test-python-cov:
	uv run pytest libs/core/tests/ libs/wing-sdk/tests/ --cov $(PYTEST_WORKER_ARGS)

# 覆盖率 HTML 报告（本地排查用；总量口径见 pyproject 的 [tool.coverage.*] ——
# 总量只作参考，PR 门禁是增量覆盖率：make patch-cov-python）
test-python-html:
	uv run pytest libs/core/tests/ libs/wing-sdk/tests/ --cov \
		--cov-report=term-missing --cov-report=html:target/coverage-python

# 确定性集成测试（假 Provider + 临时 WING_HOME，离线、无外部 API key）。
# 场景见 libs/wing-probe/scenarios/，说明见 docs/dev/probe-testing.md。
#
# 两件事定在这里，依据见 docs/dev/probe-testing.md「为什么敢并行」：
#
# ① `--dist loadfile`（xdist 默认是 `load`＝按**单条用例**派发）。这一组的时长是
#    straggler 决定的（worker 之间不偷活，队列约 9s 就发空，剩下的墙钟由最慢那个
#    worker 手里的用例堆决定），而 `load` 的逐条派发会让同一个 worker 攒到整族慢用例
#    （安静窗口实测：`test_wake_restart` 6.3s + `test_wake_timer` 3.8s 落在同一个
#    worker 上）。按**文件**派发把慢族的累积上限压到"一个文件"（1~3 例）：
#    同窗口交错 3 轮中位数  load -n 12/20 = 22.8 / 20.2s → loadfile 19.2 / 17.3s
#    （`loadscope` 在 n=12 是 18.2s，与 loadfile 同侧 ⇒ 起作用的确实是"模块/文件"
#    这一级粒度，不是别的）。代价：同一 worker 里会连续跑多个文件，跨文件状态泄漏的
#    风险因此靠"每个用例自带 tmp WING_HOME + 网关子进程 + 假 Provider"来兜（实测
#    串行 589 例全绿＝一个进程里跑完所有文件）。
#
# ② worker 数 = **1.5×核数**（`n + n/2`）。快停机之后每例只剩一次网关启动
#    （~0.22s：新解释器把导入图跑一遍，CPU 密集），曲线就在 1.5× 附近拐平：
#    12 核实测（同窗口 3 轮中位数）n=12/14/16/18/20/24 → 19.2/18.3/17.7/17.4/17.3/18.0s，
#    CPU 104/114/121/127/129/136s。18 与 20 的墙钟差 0.04s（在噪声内），CPU 差 2s，
#    所以取 1.5× 而不是 2×——`make test` 里四组共用同一批核，多拿的线程是从别的组
#    预算里扣的。
#
# 每 worker 约 0.1~0.2GB（pytest worker ~120MB + 网关 RSS 45~72MB），要更大/更小
# 用 PROBE_WORKERS 覆盖。
# WING_TEST_SERIAL=1（排查并发干扰的逃生舱）也归串行——只关组间并行等于没关。
# 解析成 1 时不传 -n/--dist：逃生舱要的是原样的串行，不是"xdist 里只开一个 worker"。
PROBE_WORKERS ?= $(shell if [ -n "$(WING_TEST_SERIAL)" ] && [ "$(WING_TEST_SERIAL)" != "0" ]; then echo 1; else n=$$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4); echo $$((n + n / 2)); fi)
PROBE_WORKER_ARGS = $(if $(filter 1,$(PROBE_WORKERS)),,--dist loadfile -n $(PROBE_WORKERS))

test-probe:
	uv run pytest libs/wing-probe/ --timeout=120 $(PROBE_WORKER_ARGS)

check-python:
	@RUFF_FAILED=0; RUFF_FMT_FAILED=0; TY_FAILED=0; VULTURE_FAILED=0; \
	echo "🔍 Running ruff..."; \
	if ! uv run ruff check libs/ scripts/; then RUFF_FAILED=1; echo "❌ ruff failed"; else echo "✅ ruff passed"; fi; \
	echo ""; \
	echo "🔍 Running ruff format check..."; \
	if ! uv run ruff format --check libs/ scripts/; then RUFF_FMT_FAILED=1; echo "❌ ruff format failed"; else echo "✅ ruff format passed"; fi; \
	echo ""; \
	echo "🔍 Running ty..."; \
	if ! uv run ty check libs; then TY_FAILED=1; echo "❌ ty failed"; else echo "✅ ty passed"; fi; \
	echo ""; \
	echo "🔍 Running vulture..."; \
	if ! uv run vulture; then VULTURE_FAILED=1; echo "❌ vulture failed"; else echo "✅ vulture passed"; fi; \
	echo ""; \
	if [ $$RUFF_FAILED -eq 1 ] || [ $$RUFF_FMT_FAILED -eq 1 ] || [ $$TY_FAILED -eq 1 ] || [ $$VULTURE_FAILED -eq 1 ]; then \
		echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
		echo "❌ Python check failed"; \
		exit 1; \
	fi

fmt-python:
	uv run ruff format libs/ scripts/

fmt-check-python:
	uv run ruff format --check libs/ scripts/

# ── Rust ──────────────────────────────────────────────────────

# clippy 带 --all-targets：默认目标集不含 benches / examples / 集成测试，
# 少了它这些目标会无声腐烂（曾经的实例：benches 里 CellContext 缺字段，
# 编译不过却谁也不知道）。代价是这几个目标也在门禁里编译一遍。
check-rust:
	@echo "🔍 Running cargo fmt..."; \
	cargo fmt --check || { echo "❌ cargo fmt failed"; exit 1; }; \
	echo "✅ cargo fmt passed"; \
	echo ""; \
	echo "🔍 Running cargo clippy (--all-targets)..."; \
	cargo clippy --all-targets --quiet -- -D warnings 2>&1 || { echo "❌ cargo clippy failed"; exit 1; }; \
	echo "✅ cargo clippy passed"

fmt-rust:
	cargo fmt

fmt-check-rust:
	cargo fmt --check

# ── 增量（patch）覆盖率门禁 ──────────────────────────────────────
# PR 的**改动行**必须有测试覆盖（行级：Python 语句行 / Rust 行）。门禁在 CI 的
# python-check / rust-check 两个 job 里跑 patch-cov-python / patch-cov-rust；
# 阈值与基线在此声明，被门禁与 PR 评论共同读取（scripts/ci/*.py 按变量名解析）。
#
# 依据（2026-10）：总量行级 ~92%（Rust）/ 合并语句+分支 96%（Python），增量门禁
# 取 90% —— 定位是「防倒退」：给代码搬家（重构）与平台分支留出余量，同时挡住
# 整块新代码裸奔。总量覆盖率不再做门禁，只进 PR 评论做参考。
#
# `?=` 允许本地/CI 用环境变量覆盖：CI 传 PATCH_BASE=origin/<目标分支> 与目标分支
# 的 merge-base 对齐；本地默认 origin/develop（先提交再判——diff 只看提交，不看
# 工作区）。演练门禁失败：PATCH_COV_MIN=99 make patch-cov-python。
PATCH_COV_MIN ?= 90.0
PATCH_BASE ?= origin/develop

# 增量覆盖率门禁（Python）：先由既有 .coverage 导出 JSON（不重跑测试），再判定。
# 判定脚本是纯 stdlib——用 python3 直跑而不是 uv run：rust-check job 里没有 uv
# （也不该为它拉一套 venv），两侧门禁共用同一个脚本、同一种调用方式。
patch-cov-python:
	uv run coverage json -o coverage-python.json --fail-under=0
	python3 scripts/ci/patch_coverage.py --lang python \
		--coverage coverage-python.json --base $(PATCH_BASE) \
		--min $(PATCH_COV_MIN) --out-json patch-python.json

# 增量覆盖率门禁（Rust）：先由既有 profraw 出 LCOV（不重跑测试），再判定。
# 前提：最近一次 `make test-rust-cov` 的插桩产物还在（target/llvm-cov-target/）。
patch-cov-rust:
	cargo llvm-cov report --lcov --output-path coverage-rust.lcov
	python3 scripts/ci/patch_coverage.py --lang rust \
		--coverage coverage-rust.lcov --base $(PATCH_BASE) \
		--min $(PATCH_COV_MIN) --out-json patch-rust.json

# Rust 测试：普通构建。本地默认走这条。
#
# 跨二进制并行（`scripts/test_rust_parallel.py`）：cargo 逐个跑测试二进制（只有套件
# 内部才多线程），55 个套件的耗时直接相加，而其中一半以上的工作量压在 `wing` 的 lib
# 单测一个套件里。脚本复用 `--no-run` 的产物自己铺开：套件的 libtest 线程数 = 它的
# 用例份额 × 核数（下界 2），同时在跑的二进制数 = 核数，最长先跑，doctests 与池子并行。
# RUST_TEST_ARGS 覆盖"本池子认为自己独占几个核"（`--cores <N>`），排查并发干扰时用
# （或四组一起退化成串行：WING_TEST_SERIAL=1）。规则的离线自检：
# `uv run python scripts/test_rust_parallel.py --selftest`。
#
# 插桩构建会额外产出一整套 target/llvm-cov-target，并且与普通构建的产物互不通用
# （见下面 test-rust-cov）——本地日常测试不需要为覆盖率付这个代价，覆盖率交给 CI 跑。
RUST_TEST_ARGS ?=
test-rust:
	uv run python scripts/test_rust_parallel.py $(RUST_TEST_ARGS)

# Rust 测试（插桩构建；覆盖率产物供上面的增量门禁与 PR 评论复用）。
# 本地只在需要复现覆盖率问题时才跑；CI 的 rust-check job 走这条。
#
# - cargo-llvm-cov 是 cargo 子命令、不在 Cargo.toml 依赖里：缺失时硬失败 + 安装指引，
#   不静默退化成裸 `cargo test` —— 那样「跑过了」而门禁没有数据。
# - doctests 单独一条：cargo-llvm-cov 的 --doctests 依赖 nightly 的 -Z unstable-options，
#   stable 上不可用；分开跑保持与 `cargo test` 相同的目标覆盖面。
# - 插桩构建（RUSTFLAGS 带 -C instrument-coverage）与普通构建在 target 里是两套产物：
#   刚跑过裸 `cargo test` 再跑本命令会整树重编译（反之亦然）。
test-rust-cov:
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { \
		echo "❌ cargo-llvm-cov not found — the Rust coverage tooling needs it."; \
		echo "   Install:"; \
		echo "     cargo install cargo-llvm-cov --locked"; \
		echo "     rustup component add llvm-tools-preview"; \
		exit 1; \
	}
	cargo llvm-cov --workspace
	cargo test --workspace --doc

# 覆盖率 HTML 报告（本地排查用）
test-rust-html:
	cargo llvm-cov --workspace --html --output-dir target/coverage-rust
