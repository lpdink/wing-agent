.PHONY: check test test-e2e format fmt run install gateway check-ui fmt-ui build-ui

# ── Unified commands (Python + Rust + UI) ────────────────────

run:
	cargo run

install:
	cd crates/wing && maturin develop --release
	uv sync

check: check-python check-rust check-ui

fmt: fmt-python fmt-rust fmt-ui

test: test-python test-rust

# ── Python ────────────────────────────────────────────────────

gateway:
	uv run wing-gateway

test-python:
	uv run pytest libs/core/tests/

test-e2e:
	CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK=1 WING_SESSIONS_PATH=/tmp/wing-e2e-sessions uv run pytest e2e/claude-agent-sdk-integration/ -v --timeout=120

check-python:
	@RUFF_FAILED=0; TY_FAILED=0; VULTURE_FAILED=0; \
	echo "🔍 Running ruff..."; \
	if ! uv run ruff check libs/; then RUFF_FAILED=1; echo "❌ ruff failed"; else echo "✅ ruff passed"; fi; \
	echo ""; \
	echo "🔍 Running ty..."; \
	if ! uv run ty check libs; then TY_FAILED=1; echo "❌ ty failed"; else echo "✅ ty passed"; fi; \
	echo ""; \
	echo "🔍 Running vulture..."; \
	if ! uv run vulture libs/ --min-confidence 70 --exclude .venv/; then VULTURE_FAILED=1; echo "❌ vulture failed"; else echo "✅ vulture passed"; fi; \
	echo ""; \
	if [ $$RUFF_FAILED -eq 1 ] || [ $$TY_FAILED -eq 1 ] || [ $$VULTURE_FAILED -eq 1 ]; then \
		echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
		echo "❌ Python check failed"; \
		exit 1; \
	fi

fmt-python:
	uv run ruff format libs/

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

test-rust:
	cargo test

# ── UI (Electron / Web) ─────────────────────────────────────

check-ui:
	@echo "🔍 Running prettier..."; \
	(cd crates/wing-ui && pnpm format:check) || { echo "❌ prettier failed"; exit 1; }; \
	echo "✅ prettier passed"; \
	echo ""; \
	echo "🔍 Running oxlint..."; \
	(cd crates/wing-ui && pnpm lint) || { echo "❌ oxlint failed"; exit 1; }; \
	echo "✅ oxlint passed"; \
	echo ""; \
	echo "🔍 Running typecheck..."; \
	(cd crates/wing-ui && pnpm typecheck) || { echo "❌ typecheck failed"; exit 1; }; \
	echo "✅ typecheck passed"

fmt-ui:
	cd crates/wing-ui && pnpm format

build-ui:
	cd crates/wing-ui && pnpm build && pnpm build:web
