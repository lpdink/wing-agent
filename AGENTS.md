# wing-agent

Monorepo: Python agent runtime + Rust TUI client.

## Architecture

```
Rust TUI (wing)               Gateway (FastAPI)              Runtime (Python)
┌────────────────┐  WS+HTTP  ┌───────────────────┐          ┌──────────────┐
│  GatewayClient ├──────────►│  GatewayServer    │────────► │ WingRuntime  │
│  (WS events)   │           │  routes/session   │          │ (service)    │
│  ApiClient     │◄──────────│  routes/ws        │◄──────── │SessionManager│
│  (HTTP api)    │  events   │  routes/health    │          │ EventBus     │
└───────┬────────┘           └───────────────────┘          └──────────────┘
        │ mpsc
┌───────▼────────┐
│     App        │  handle_event() → state mutation
│ (state machine)│ draw() → ratatui render
└────────────────┘
```

**Protocols**: HTTP (RPC-style, session lifecycle + queries) and WebSocket (real-time ReAct event streaming). Session creation is decoupled from WS handshake — clients create sessions via HTTP, then subscribe to events.

**Persistence**: All durable session state (metadata, message log, aux data like pending compactions) is owned by a single abstraction, `SessionStore` (`wing/store/`). No other module does storage I/O for session data. `SessionStore` composes `MessageLog` (append-only message durability + aux kv); `TrackedList` is a pure in-memory chain-topology engine (uuid/parentUuid) that delegates I/O to a `MessageLog`. Backends: `file` (default, `~/.wing/core/sessions/`) and `memory` (ephemeral, per-process). Session creation selects the backend via the `backend` parameter. The interface is storage-agnostic by design — SQL backends (SQLite/PG/Supabase) are additive implementations.

## Project Structure

```
libs/core/wing/                   Python runtime (pip: wing-agent)
├── agent.py                      WingAgent — LLM loop + tool dispatch
├── session.py                    Session — messages + template + serialization
├── session_manager.py            SessionManager — multi-session + magic commands
├── runtime.py                    WingRuntime — service layer entry point
├── event_bus.py                  EventBus — global singleton event routing
├── context_manager.py            Context window + compaction
├── config.py                     Config loading + WING_HOME
├── store/                        Session persistence layer (single owner of durable state)
│   ├── base.py                   SessionStore + MessageLog ABCs, SessionMetadata model
│   ├── file.py                   File backend (sessions/<sid>/ layout, zero-migration)
│   └── memory.py                 In-memory backend (ephemeral sessions, no disk)
├── event/                        Event types (base, react, state_change, query_response)
├── tools/                        Built-in tools (Bash, Read, Write, Edit, Grep, ...)
├── magic_command/                Slash command registry + handlers
├── metrics_registry/             LLM call + compaction metrics
├── common/                       Logger, utils, token counter, process helpers
│   ├── tracked_list.py           TrackedList — chain topology engine (I/O via MessageLog)
│   └── fs.py                     Atomic write helpers (tmp + fsync + rename)
└── gateway/                      FastAPI server
    ├── app.py                    App factory (FastAPI instance + route registration)
    ├── server.py                 GatewayServer — lifecycle + EventBus subscriber
    ├── routes/session.py         9 HTTP endpoints (create/resume/fork/subscribe/send/...)
    ├── routes/ws.py              WebSocket handler (pure transport, no session creation)
    ├── routes/health.py          GET /api/health
    ├── protocol.py               WS + HTTP Pydantic models
    └── openapi.py                OpenAPI metadata (tags, version, description)

libs/wing_hooks/wing_hooks/       Official hooks package

crates/wing/src/                  Rust TUI + CLI
├── main.rs                       Entry point (clap dispatch)
├── cmd/                          CLI commands (start/stop/status/tui) + daemon mgmt
│   ├── backend_config.rs         Reads gateway host:port from backend config.yaml
│   └── state.rs                  Daemon state persistence (~/.wing/state.json)
├── gateway/client.rs             GatewayClient — WS connection + read/write tasks
├── protocol/                     WingEvent + ClientRequest + ConnectResponse
├── app/                          App state machine + event loop
│   ├── mod.rs                    run_app() main loop + handle_event()
│   ├── replay.rs                 SyncSession message replay → ChatCells
│   └── popup_state.rs            Popup + candidate cache + dedup
├── ui/                           UI components (chat, input, popup, status_bar, toast)
├── render/                       Markdown + syntax highlighting
├── tui/                          Terminal abstraction (crossterm)
└── config/                       TUI config (colors, layout, rendering)

crates/wing-api-client/src/       Hand-written Rust HTTP client for Gateway API
├── client.rs                     GatewayClient — all HTTP API methods
├── models.rs                     Request/response types (mirrors Python protocol.py)
└── error.rs                      ApiClientError (transport/api/deserialize)
```

## Configuration

Single source of truth: `$WING_HOME/core/config.yaml` (default `~/.wing/core/config.yaml`).

```
~/.wing/
├── core/config.yaml      Backend config (openai, agents, gateway, hooks, commands)
├── tui/config.yaml       TUI config (colors, layout, rendering)
├── state.json            Gateway daemon state (pid, host, port)
├── gateway.log           Gateway log
├── logs/                 TUI logs
├── sessions/             Session persistence (messages + metadata)
└── templates/            Agent templates
```

`WING_HOME` env var overrides `~/.wing`.

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
- **Rust**: GitHub Release prebuilt binaries → `wing` CLI

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
