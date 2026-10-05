.PHONY: check test test-probe format fmt fmt-check fmt-check-python fmt-check-rust fmt-ts fmt-check-ts check-ts test-ts run install gateway demo

# ── Unified commands (Python + Rust) ─────────────────────────

# ── TypeScript (extensions/vscode) ─────────────────────────────
# 门禁与 CI 的 typescript-check job 等价：先按 lockfile 安装（--prefer-offline：
# 本地已有 store 时不动网络），再依次跑 eslint / prettier / tsc / vitest。
# 未接进 `fmt` / `fmt-check`：那两条要在 pre-commit 里保持秒级，而且不该强依赖 node_modules。
#
# 工具链缺失时的行为（评审 #109 [P3-7]）：显式探测 node / pnpm，缺哪一个就打印
# 可操作的提示再退出 1 —— 而不是让 `pnpm: command not found` 混进结论块里，
# 让只改 Python 的人以为自己弄坏了什么。`SKIP_TS=1` 是本地逃生舱（CI 不设，
# 门禁照旧强制），collect_output.sh 会把 ts 组标成 skipped。
VSCODE_DIR := extensions/vscode

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
	if ! (cd $(VSCODE_DIR) && pnpm run test); then echo "❌ vitest failed"; exit 1; fi; \
	echo "✅ vitest passed"

fmt-ts:
	cd $(VSCODE_DIR) && pnpm run format

fmt-check-ts:
	cd $(VSCODE_DIR) && pnpm run format:check

# ── Python ────────────────────────────────────────────────────

gateway:
	uv run wing-gateway

test-python:
	uv run pytest libs/core/tests/ libs/wing-orch/tests/ libs/wing-sdk/tests/

# 确定性集成测试（假 Provider + 临时 WING_HOME，离线、无外部 API key）。
# 场景见 libs/wing-probe/scenarios/，说明见 docs/dev/probe-testing.md。
#
# 并行度：场景之间零共享（各自 tmp + 各自网关子进程 + 各自假 Provider），所以
# 按核数铺开是安全的——实测全量 80s → 16s（12 核）。默认 min(8, 核数)：本机 12 核
# 取 8（给 `make test` 里同时跑的 rust / ts 组留核），CI 的 4 核 runner 取 4。
# PROBE_WORKERS 可覆盖；WING_TEST_SERIAL=1（排查并发干扰的逃生舱）也归串行——
# 只关组间并行等于没关。解析成 1 时不传 -n：逃生舱要的是原样的串行，不是
# "xdist 里只开一个 worker"。
PROBE_WORKERS ?= $(shell n=$$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4); if [ -n "$(WING_TEST_SERIAL)" ] && [ "$(WING_TEST_SERIAL)" != "0" ]; then echo 1; elif [ "$$n" -gt 8 ]; then echo 8; else echo $$n; fi)
PROBE_WORKER_ARGS = $(if $(filter 1,$(PROBE_WORKERS)),,-n $(PROBE_WORKERS))

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
	if ! uv run vulture libs/ --min-confidence 70 --exclude .venv/; then VULTURE_FAILED=1; echo "❌ vulture failed"; else echo "✅ vulture passed"; fi; \
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

check-rust:
	@echo "🔍 Running cargo fmt..."; \
	cargo fmt --check || { echo "❌ cargo fmt failed"; exit 1; }; \
	echo "✅ cargo fmt passed"; \
	echo ""; \
	echo "🔍 Running cargo clippy..."; \
	cargo clippy --quiet -- -D warnings 2>&1 || { echo "❌ cargo clippy failed"; exit 1; }; \
	echo "✅ cargo clippy passed"

fmt-rust:
	cargo fmt

fmt-check-rust:
	cargo fmt --check

test-rust:
	cargo test
