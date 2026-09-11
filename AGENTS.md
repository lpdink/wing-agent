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

**Protocols.** HTTP for lifecycle / queries / mutations (~22 RPC-style endpoints); WebSocket (`/ws`) carries the real-time ReAct event stream plus client→server message / Ask-reply frames (`ClientRequest`), while queries and mutations stay on HTTP. Session creation is decoupled from the WS handshake — clients create a session via HTTP, then subscribe to events. API key auth is opt-in at the Gateway (HTTP headers / WS query param); TLS is delegated to a reverse proxy.

**Persistence.** All durable session state (metadata, mixed message/event log, aux data like pending compactions) is owned by a single abstraction, `SessionStore` (`wing/store/`). No other module does storage I/O for session data. `SessionStore` composes `MessageLog` (append-only log durability + aux kv); `TrackedList` is a pure in-memory chain-topology engine (uuid/parentUuid) over ChainNode-family nodes (Message + WingEvent mixed) that delegates I/O to a `MessageLog`. Backends: `file` (default, `~/.wing/core/sessions/`) and `memory` (ephemeral, per-process), selected per session via the `backend` parameter. The interface is storage-agnostic — SQL backends (SQLite/PG/Supabase) are additive implementations.

**Unified event log.** `history.jsonl` is the single source of truth — a mixed log of Message records (LLM-context projection) and event records (`role="event"`), all sharing chain topology. The log stores **facts, not copies**: only `persist=true` fact events with no Message twin (diff, ask, interrupted, error, compact_done) hit the log the moment they complete; streaming deltas are pure broadcast (`persist=false` — never persisted, never buffered), their content carried by the Message record at turn close. `tool_call_result` / `llm_call_metrics` are Message twins (`Message.usage` + `Message.stop_reason` / the `role="tool"` Message) so they no longer persist; `turn_result` persists but its `result` field (final-text twin) is excluded from the disk record. `persist` is a `ClassVar[bool]` (not a pydantic field — a re-declared `Field(exclude=True)` was silently pierced by subclasses). Uncommitted turn content has a **single authority**: the caller-held provider stream accumulator (`ReActLoop._current_acc`), projected on demand via `snapshot_blocks()` (finalized blocks — interrupt partial-commit *and* resume) + `pending_tool_calls()` (unfinished tool args, raw text — live tool cards); the backend never parses partial JSON. Interrupt/max-tokens turns commit their partial content and capture `stop_reason` on the Message: unfinished tool calls are dropped (unverifiable args), while finalized-but-unexecuted tool calls are committed **with a synthesized interrupted tool result** each — no dangling tool_use in the next request; zero-info blocks (empty text / unsigned empty thinking) never enter the snapshot or the authoritative block array. rewind/fork/compact work on events for free via chain order; mid-turn subscribers get `messages + uncommitted + uncommitted_tools + events` (+ `turn_started_at`) in SyncSessionEvent, assembled in that order so a diff always anchors after its (finalized) tool_use cell — a view identical to subscribing from the start.

**Remote tools & orchestration.** Tools need not run in the gateway process. An external **tool host** registers tools over HTTP (`POST /api/tools/register`) and serves their calls over a held WebSocket (`tool_call_request` / `tool_call_result` frames, correlated by `call_id`). The core stays network-agnostic — a remote tool is an ordinary `Tool` whose callable is a gateway-injected dispatch closure (`gateway/remote_tools.py`). The host's `client_id` (chosen via `?client_id=` on WS connect) is both its tool namespace and identity, decoupled from the RBAC role (`admin` = full access, `tool_runtime` = pure executor). Tool sets can also change at runtime via `POST /api/session/update` (`tools` field) with KV-cache protection — a cold swap when the chain is empty, else a frozen declared view plus an injected System Reminder, policy owned by `ContextManager`. SDKs: Rust `wing-api-client::tool_host` and Python `wing-sdk`; `wing-orch` lifts the TUI Goal loop into a standalone background CLI (state persist + resume).

**Streaming rendering.** During LLM argument generation the runtime emits `tool_call_stream` events carrying incremental raw args text fragments (`args_fragment`); it never parses partial JSON itself. The Rust frontend accumulates fragments and parses them locally (`util/partial_json.rs`, single-pass O(n)) to render live tool cards (Write/Edit previews, TodoWrite lists) before execution starts; the authoritative parsed args arrive with the `tool_call` event.

## Project Structure

```
libs/core/wing/                   Python runtime (pip: wing-agent)
├── agent/                        WingAgent package (public import paths unchanged via re-export)
│   ├── core.py                   WingAgent thin shell: assembly, public API, worker lifecycle, uncommitted projection
│   ├── react_loop.py             ReAct main loop: drain → hook → LLM → tools → commit; turn-level accumulator; partial commit on interrupt
│   ├── llm_caller.py             LLM streaming call + chunk → event projection
│   ├── tool_executor.py          Concurrent tool dispatch (asyncio.gather) + interrupt teardown
│   ├── event_sink.py             AgentEventSink — single event emission outlet + persist split (false = broadcast only)
│   ├── inbox.py                  Message queue (drain-and-merge) + feedback waiters
│   └── tool_context.py           ToolContext Protocol — narrow interface tools receive (`ctx`)
├── agent_template.py             AgentTemplate — model/tools/prompt from config `agents:`
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
│   ├── file.py                   File backend (~/.wing/core/sessions/, mixed log, zero-migration)
│   └── memory.py                 In-memory backend (ephemeral, no disk)
├── event/                        Event types (base, react, state_change, query_response) + EVENT_TYPES / FACT_EVENTS registries + persist ClassVar + wire_dump (strip null/storage fields)
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
    ├── auth.py                   Opt-in API key auth middleware (HTTP + WS; admin / tool_runtime roles)
    ├── remote_tools.py           RemoteToolManager — tool host connections + WS call dispatch
    ├── protocol.py               WS + HTTP Pydantic models
    ├── openapi.py                OpenAPI metadata
    └── routes/
        ├── session.py            Session lifecycle + queries + mutations (14 endpoints)
        ├── system.py             commands/models/agents/tools listing, reload, shutdown (6)
        ├── tools.py              POST /api/tools/register (remote tool registration)
        ├── health.py             GET /api/health (1)
        └── ws.py                 WebSocket /ws (event transport + tool call result frames)

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
│   ├── ask_panel.rs              AskUserQuestion panel (tabs/multi-select/inline editor; Esc owns interrupt)
│   ├── turn_state.rs / render_context.rs / constants.rs
│   ├── replay.rs                 SyncSession replay → ChatCells
│   └── popup_state.rs            Popup + candidate cache + dedup
├── ui/                           UI components
│   ├── chat_view.rs / header.rs / status_bar.rs / spinner.rs / toast.rs
│   ├── cells/                    Chat cell renderers (tool_call, thinking, todo, ask, diff)
│   ├── input_area/               Composer (editing, movement, wrap, paste)
│   ├── popup/                    Command palette + selection
│   └── ask_select.rs             Legacy required-choice selector (Bash confirm)
├── render/                       Markdown + syntax highlighting (code_blocks, tables, links)
│   └── markdown/stream.rs        StreamingRender — incremental streaming renderer (stable prefix + active tail)
├── tui/                          Terminal abstraction (crossterm)
├── config/                       TUI config (colors, rendering, goal)
└── util/                         clipboard, logging, osc9, terminal title, partial_json (streaming args parser)

crates/wing/benches/              Criterion benchmarks
└── stream_render.rs              Streaming-render perf (baseline vs incremental engines; corpus generators committed)
crates/wing/tests/                Integration tests
├── stream_render_reconcile.rs    Span-exactness reconcile matrix (incremental vs full render)
├── stream_render_throughput.rs   3000 tokens/s throughput judgment (p99 < 16ms, no backlog)
└── common/mod.rs                 Shared corpus generation + frame harness

crates/wing-api-client/src/       Hand-written Rust HTTP client for Gateway API
├── client.rs                     GatewayClient — all HTTP API methods (+ api_key)
├── tool_host.rs                  ToolHost — remote tool host (WS serve loop + builder)
├── models.rs                     Request/response types (mirrors Python protocol.py)
└── error.rs                      ApiClientError

libs/wing-sdk/                    Python SDK (pip: wing-sdk) — remote tool host
├── wing_sdk/host.py              ToolHost — decorator registration + WS serve loop
├── wing_sdk/http_client.py       GatewayClient — session/system HTTP API
├── wing_sdk/schema.py            ToolParam / RemoteToolSpec (gateway-independent)
└── wing_sdk/tools/               Standard tools (Bash/Read/Write/Edit/Glob/Grep, workspace-bound)

libs/wing-orch/                   Orchestration CLI (pip: wing-orch) — depends on wing-sdk
└── wing_orch/
    ├── cli.py                    `wing-orch goal` entry point
    ├── goal.py                   Goal state machine (port of crates/wing/src/app/goal.rs)
    └── runner.py                 asyncio driver: tool host + sessions + event loop + persistence
```

## Configuration

Single source of truth: `$WING_HOME/core/config.yaml` (default `~/.wing/core/config.yaml`). Top-level keys: `openai` (provider), `agents` (templates: model/tools/prompt/skills/rules), `hooks`, `gateway` (host/port/`auth`), `safe_command_patterns`, `yolo`, `steer`, `tool_result_truncate`, `log`. See `wing/default_config.py` for the annotated template.

```
~/.wing/
├── core/
│   ├── config.yaml      Backend config
│   ├── logs/            Backend logs (see Logging below)
│   │   ├── wing_YYYY-MM-DD.log   Gateway runtime log (daily, local time, append)
│   │   ├── new.log → wing_YYYY-MM-DD.log   Symlink to the active backend log
│   │   └── gateway.log  Gateway daemon stdout/stderr (uvicorn errors, tracebacks; append)
│   └── sessions/        Session persistence (metadata + message log)
└── tui/
    ├── config.yaml      TUI config (colors, rendering, api_key, goal)
    └── logs/            TUI logs: wing_YYYY-MM-DD.log (daily, local time, append)
```

`WING_HOME` overrides `~/.wing` (backend data lives under `$WING_HOME/core`); `WING_SESSIONS_PATH` overrides the sessions directory.

**Logging policy (unified front & back).** Both sides write one file per **local** calendar day — `wing_YYYY-MM-DD.log` — opened in append mode, so gateway/TUI restarts never truncate or fork logs, and prune files older than 7 days (at startup and on rotation). Backend logs live in `~/.wing/core/logs/`; the `new.log` symlink there always points at the active backend log (backend-only; the gateway refreshes it on every rotation). TUI logs live in `~/.wing/tui/logs/` (same naming, no symlink). Logging initializes explicitly — the gateway CLI (`wing-gateway`, via `wing.common.logger.setup_logger`, console level from `log.level`) and the TUI (`util/logging.rs`) attach handlers at startup; **importing `wing` has no logging side effects** — tests and scripts never create files in `~/.wing`. Every line starts with `YYYY-MM-DD HH:MM:SS` (local time on both sides), so time-range greps work directly: `grep '^2026-09-08 23:' ~/.wing/core/logs/new.log`, or `awk '$0 >= "2026-09-08 23:10" && $0 < "2026-09-08 23:30"' ~/.wing/tui/logs/wing_2026-09-08.log`.

## Deep dives (docs/dev)

AGENTS.md stays a high-density overview. For mechanism-level detail, read `docs/dev/` (中文):

- [`docs/dev/architecture.md`](docs/dev/architecture.md) — 运行时/网关/前端数据流、stdio 模式、Goal 编排、远程工具与编排、会话生命周期与持久化。
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
- **SDK/Orch**: `wing-sdk` / `wing-orch` — uv workspace 包（`libs/`），未发布 PyPI；
  `wing-orch` 提供 `wing-orch` CLI（后台 Goal 编排），`wing-sdk` 提供远程工具宿主 SDK

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
