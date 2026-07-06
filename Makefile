.PHONY: check test format fmt run install gateway

# ── Unified commands (Python + Rust) ─────────────────────────

run:
	cargo run

install:
	pip install libs/core
	cargo install --path crates/wing --locked

check: check-python check-rust

fmt: fmt-python fmt-rust

test: test-python test-rust

# ── Python ────────────────────────────────────────────────────

gateway:
	uv run wing-gateway

test-python:
	uv run pytest libs/core/tests/ libs/wing_hooks/tests/

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
	cargo fmt --check && cargo clippy -- -D warnings && cargo test

fmt-rust:
	cargo fmt

test-rust:
	cargo test
