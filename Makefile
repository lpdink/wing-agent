.PHONY: check test test-e2e test-probe format fmt fmt-check fmt-check-python fmt-check-rust fmt-ts fmt-check-ts check-ts test-ts run install gateway

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

run:
	cargo run

install:
	cd crates/wing && maturin develop --release
	uv sync

check:
	bash scripts/collect_output.sh check

fmt: fmt-python fmt-rust

# 与 CI 的格式门禁等价（Ruff format check + cargo fmt --check），供 pre-commit 快速拦截
fmt-check: fmt-check-python fmt-check-rust

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

test-e2e:
	CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK=1 WING_SESSIONS_PATH=/tmp/wing-e2e-sessions uv run pytest e2e/claude-agent-sdk-integration/ -v --timeout=120

# 确定性集成测试（假 Provider + 临时 WING_HOME，离线、无外部 API key）。
# 场景见 libs/wing-probe/scenarios/，说明见 docs/dev/probe-testing.md。
test-probe:
	uv run pytest libs/wing-probe/ --timeout=120

check-python:
	@RUFF_FAILED=0; RUFF_FMT_FAILED=0; TY_FAILED=0; VULTURE_FAILED=0; \
	echo "🔍 Running ruff..."; \
	if ! uv run ruff check libs/; then RUFF_FAILED=1; echo "❌ ruff failed"; else echo "✅ ruff passed"; fi; \
	echo ""; \
	echo "🔍 Running ruff format check..."; \
	if ! uv run ruff format --check libs/; then RUFF_FMT_FAILED=1; echo "❌ ruff format failed"; else echo "✅ ruff format passed"; fi; \
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
	uv run ruff format libs/

fmt-check-python:
	uv run ruff format --check libs/

# ── Rust ──────────────────────────────────────────────────────

check-rust:
	@echo "🔍 Running cargo fmt..."; \
	cargo fmt --check || { echo "❌ cargo fmt failed"; exit 1; }; \
	echo "✅ cargo fmt passed"; \
	echo ""; \
	echo "🔍 Running cargo clippy..."; \
	cargo clippy --quiet -- -D warnings 2>&1 || { echo "❌ cargo clippy failed"; exit 1; }; \
	echo "✅ cargo clippy passed"; \
	echo ""; \
	echo "🔍 Running cargo test..."; \
	OUTPUT=$$(cargo test 2>&1); \
	if [ $$? -eq 0 ]; then \
		echo "$$OUTPUT" | grep "^test result:"; \
		echo "✅ cargo test passed"; \
	else \
		echo "$$OUTPUT"; \
		echo "❌ cargo test failed"; \
		exit 1; \
	fi

fmt-rust:
	cargo fmt

fmt-check-rust:
	cargo fmt --check

test-rust:
	cargo test
