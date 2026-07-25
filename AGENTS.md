# wing-agent

Monorepo: Python agent runtime + Rust frontends (TUI + stdio).

## Architecture

```
   Frontends (wing binary)              Gateway (FastAPI)            Runtime (Python)
┌────────────────────────────┐  WS+HTTP ┌───────────────────┐       ┌────────────────┐
│ TUI mode (default)         ├─────────►│  GatewayServer    │──────►│  WingRuntime   │
│  ratatui event loop        │          │  · routes/session │       │  (coordinator) │
│  Goal loop (executor/      │◄─────────│  · routes/system  │◄──────│ SessionManager │
│   checker, TUI-side)       │  events  │  · routes/health  │       │ SessionStore   │
│ Stdio mode (wing -p)       │          │  · routes/ws      │       │ ContextManager │
│  Claude-protocol NDJSON    │          │  auth (opt-in)    │       │ EventBus       │
│ GatewayClient(WS)+ApiClient│          └───────────────────┘       └────────────────┘
└────────────────────────────┘
```

The `wing` binary is both frontends: **TUI** (interactive, human-in-the-loop) and **stdio** (`wing -p`, headless, Claude Code compatible — alias `wing` as `claude` to plug into external orchestrators). Both drive the same Gateway + Runtime.

**Protocols.** HTTP for lifecycle / queries / mutations (~20 RPC-style endpoints); WebSocket (`/ws`) carries the real-time ReAct event stream plus client→server message / Ask-reply frames (`ClientRequest`), while queries and mutations stay on HTTP. Session creation is decoupled from the WS handshake — clients create a session via HTTP, then subscribe to events. API key auth is opt-in at the Gateway (HTTP headers / WS query param); TLS is delegated to a reverse proxy.

**Persistence.** All durable session state (metadata, message log, aux data like pending compactions) is owned by a single abstraction, `SessionStore` (`wing/store/`). No other module does storage I/O for session data. `SessionStore` composes `MessageLog` (append-only message durability + aux kv); `TrackedList` is a pure in-memory chain-topology engine (uuid/parentUuid) that delegates I/O to a `MessageLog`. Backends: `file` (default, `~/.wing/core/sessions/`) and `memory` (ephemeral, per-process), selected per session via the `backend` parameter. The interface is storage-agnostic — SQL backends (SQLite/PG/Supabase) are additive implementations.

## Project Structure

```
libs/core/wing/                   Python runtime (pip: wing-agent)
├── agent.py                      WingAgent — LLM loop + concurrent tool dispatch
├── agent_template.py             AgentTemplate — model/tools/prompt from config `agents:`
├── agent_state_bag.py            Per-agent mutable state (yolo, reasoning effort, …)
├── session.py                    Session — messages + state + metadata (via SessionStore)
├── session_manager.py            SessionManager — multi-session, fork/resume, store registry
├── runtime.py                    WingRuntime — service-layer coordinator (thin routes → Session/CM)
├── context_manager.py            Context window tracking + compaction + rewind
├── compactor.py                  Compaction strategy (LLM summarization)
├── event_bus.py                  EventBus — global singleton event routing
├── config.py                     Config models + WING_HOME resolution
├── default_config.py             Hand-maintained default config.yaml template (source of truth)
├── schema.py                     Tool / ToolParam models (namespace, llm_name, to_openai)
├── tool_registry.py              ToolRegistry — namespace-aware registry + ToolRef resolution
├── openai_provider.py            OpenAI-compatible provider (streaming, retry, cache control)
├── request_context.py            Per-request context (ids, tracing)
├── hook_registry.py              Hook extension points (before/after user message & tool call)
├── store/                        Session persistence (single owner of durable state)
│   ├── base.py                   SessionStore + MessageLog ABCs, SessionMetadata model
│   ├── file.py                   File backend (~/.wing/core/sessions/, zero-migration)
│   └── memory.py                 In-memory backend (ephemeral, no disk)
├── event/                        Event types (base, react, state_change, query_response)
├── tools/                        Built-in tools
│   ├── bash.py / file.py / search.py   Bash · Read/Write/Edit · Glob/Grep
│   ├── ask_user.py / todo.py           AskUserQuestion · TodoWrite
│   ├── explorer.py                     Explorer sub-agent (run_in_background)
│   ├── experimental.py                 BetterEdit (experimental)
│   └── shell_safety.py                 Bash command safety review
├── magic_command/                Prompt-command metadata registry + $ARGUMENTS text expansion (no dispatch)
├── metrics_registry/             LLM / tool-call / compaction metrics
├── common/                       Logger, utils, token counter, process & retry helpers
│   ├── tracked_list.py           TrackedList — chain-topology engine (I/O via MessageLog)
│   └── fs.py                     Atomic write helpers (tmp + fsync + rename)
└── gateway/                      FastAPI server
    ├── app.py                    App factory (FastAPI + route registration)
    ├── server.py                 GatewayServer — lifecycle + EventBus subscriber + uptime
    ├── cli.py                    `wing-gateway` CLI entry point
    ├── auth.py                   Opt-in API key auth middleware (HTTP + WS)
    ├── protocol.py               WS + HTTP Pydantic models
    ├── openapi.py                OpenAPI metadata
    └── routes/
        ├── session.py            Session lifecycle + queries + mutations (14 endpoints)
        ├── system.py             commands/models/agents listing, reload, shutdown (5)
        ├── health.py             GET /api/health (1)
        └── ws.py                 WebSocket /ws (pure event transport)

crates/wing/src/                  Rust CLI: TUI + stdio frontends
├── main.rs                       Entry (clap; stdio mode detection → filter unknown args)
├── cmd/                          CLI subcommands
│   ├── mod.rs                    Cli/Command defs + dispatch (tui/start/stop/status + stdio flags)
│   ├── start.rs / stop.rs / status.rs   HTTP-based gateway lifecycle (health + /api/shutdown)
│   ├── backend_config.rs         Read gateway host:port + wing_home from backend config
│   └── discover.rs               Gateway discovery
├── stdio/                        Headless Claude-protocol mode (`wing -p`)
│   ├── mod.rs                    run_stdio + ensure_gateway_running + arg filtering
│   ├── ndjson.rs                 stream-json NDJSON framing
│   ├── renderer.rs               text / json / stream-json output rendering
│   └── stdin_handler.rs          SDK bidirectional stdin handshake
├── gateway/client.rs             GatewayClient — WS connection + read/write tasks
├── protocol/                     WingEvent + ClientRequest + ConnectResponse + CommandInfo
├── app/                          App state machine + event loop
│   ├── mod.rs                    run_app() main loop + handle_event()
│   ├── runner.rs                 Execute AppIntents (HTTP/WS side effects)
│   ├── intent.rs / transport.rs  AppIntent enum + gateway transport abstraction
│   ├── goal.rs                   Goal orchestration state machine (executor/checker loop)
│   ├── ask_flow.rs               AskUserQuestion interaction flow
│   ├── turn_state.rs / render_context.rs / constants.rs
│   ├── replay.rs                 SyncSession replay → ChatCells
│   └── popup_state.rs            Popup + candidate cache + dedup
├── ui/                           UI components
│   ├── chat_view.rs / header.rs / status_bar.rs / spinner.rs / toast.rs
│   ├── cells/                    Chat cell renderers (tool_call, thinking, todo, ask, diff)
│   ├── input_area/               Composer (editing, movement, wrap, paste)
│   ├── popup/                    Command palette + selection
│   └── ask_select.rs             Ask option selector
├── render/                       Markdown + syntax highlighting (code_blocks, tables, links)
├── tui/                          Terminal abstraction (crossterm)
├── config/                       TUI config (colors, rendering, goal)
└── util/                         clipboard, logging, osc9 notifications, terminal title

crates/wing-api-client/src/       Hand-written Rust HTTP client for Gateway API
├── client.rs                     GatewayClient — all HTTP API methods (+ api_key)
├── models.rs                     Request/response types (mirrors Python protocol.py)
└── error.rs                      ApiClientError
```

## Configuration

Single source of truth: `$WING_HOME/core/config.yaml` (default `~/.wing/core/config.yaml`). Top-level keys: `openai` (provider), `agents` (templates: model/tools/prompt/skills/rules), `hooks`, `gateway` (host/port/`auth`), `safe_command_patterns`, `yolo`, `steer`, `preserved_thinking`, `tool_result_truncate`, `log`. See `wing/default_config.py` for the annotated template.

```
~/.wing/
├── core/
│   ├── config.yaml      Backend config
│   └── sessions/        Session persistence (metadata + message log)
├── tui/config.yaml      TUI config (colors, rendering, api_key, goal)
├── gateway.log          Gateway log
└── logs/                TUI logs
```

`WING_HOME` overrides `~/.wing` (backend data lives under `$WING_HOME/core`); `WING_SESSIONS_PATH` overrides the sessions directory.

## Deep dives (docs/dev)

AGENTS.md stays a high-density overview. For mechanism-level detail, read `docs/dev/` (中文):

- [`docs/dev/architecture.md`](docs/dev/architecture.md) — 运行时/网关/前端数据流、stdio 模式、Goal 编排、会话生命周期与持久化。
- [`docs/dev/http-api.md`](docs/dev/http-api.md) — 完整 HTTP 端点表 + WebSocket 协议 + 鉴权。
- [`docs/dev/glossary.md`](docs/dev/glossary.md) — 核心概念：SessionStore/MessageLog/TrackedList、工具命名空间、prompt 命令、压缩等。

## Development

```bash
# Python
uv sync
uv run wing-gateway              # Start gateway
make test-python                  # pytest
make check-python                 # ruff + ty + vulture

# Rust
cargo build
cargo test
make check-rust                   # fmt + clippy + test

# All
make test                         # Python + Rust
make check                        # Python + Rust
make fmt                          # Format all
```

## Distribution

- **Python**: `pip install wing-agent` → `wing-gateway` CLI entry point
- **Rust**: GitHub Release prebuilt binaries → `wing` CLI (TUI + stdio + daemon control)

## Commit Messages

```
type(scope): short description

[optional body]
```

Types: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`.

Scopes follow module boundaries: `gateway`, `runtime`, `session`, `tui`, `protocol`, `tools`, `config`, etc.

Examples:
```
feat(gateway): add HTTP session/fork endpoint
refactor(runtime): clean up WingRuntime as service layer
fix(protocol): remove session_id from ConnectResponse
test(gateway): add HTTP endpoint unit tests
```

Keep the first line under 72 chars. Body explains *why*, not *what*.
