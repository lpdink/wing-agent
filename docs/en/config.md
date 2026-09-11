# Configuration Reference

wing-agent reads backend configuration from `~/.wing/core/config.yaml`. On first run, a template with detailed comments is created automatically.

All fields are optional except `openai` and `agents`.

The TUI frontend has its own config at `~/.wing/tui/config.yaml` (colors, layout, gateway settings).

## Minimal Example

```yaml
openai:
  base_url: "https://api.openai.com/v1"
  api_key: "sk-your-key"

agents:
  - name: default
    model: "gpt-4"
    tools: [Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, TodoWrite, Explorer]
```

## Full Reference

```yaml
# ── LLM Provider ──────────────────────────────────────
openai:
  base_url: "https://api.openai.com/v1"   # OpenAI-compatible endpoint
  api_key: "sk-xxx"                        # API key
  timeout_first_chunk: 300.0              # Streaming first-chunk timeout (seconds)
  timeout_total: 600.0                     # Non-streaming total timeout (seconds)
  explicit_cache_mode: true               # Append cache_control markers for prompt caching
  reasoning_effort: null                  # Reasoning effort: low/medium/high/xhigh/max (null = provider default)

# ── Agent Templates ───────────────────────────────────
agents:
  - name: default                          # Template name (must be unique)
    model: "gpt-4"                         # Model identifier
    default: true                          # Use as default when unspecified
    system_prompt: ""                      # System prompt (empty = no system prompt injected)
    tools:                                 # Tools to enable
      - Bash
      - Read
      - Write
      - Edit
      - Glob
      - Grep
      - AskUserQuestion
      - TodoWrite
      - Explorer
    context_window_tokens: 256000          # Context window limit before compaction
    keep_recent_tokens: 50000              # Tokens to keep after compaction
    max_turns: null                        # Max agent loop turns (null = unlimited)
    yolo: null                             # Per-agent yolo override (null = inherit global)
    skills:                                # Skills glob patterns
      - ~/.agents/skills/*/SKILL.md
      - .claude/skills/*/SKILL.md
      - .agents/skills/*/SKILL.md
    rules:                                 # Rules glob patterns
      - AGENTS.md

# ── Hooks ─────────────────────────────────────────────
hooks: []                                  # Hook file glob patterns, e.g. ~/.wing/hooks/*.py

# ── Safety ────────────────────────────────────────────
yolo: false                                # Skip dangerous command safety review
steer: true                                # Enable steer mode

safe_command_patterns: []                  # Regex whitelist for auto-approved bash commands
                                           # e.g. ["^ls ", "^cat "]

# ── Tool Result Truncation ────────────────────────────
# Built-in truncation for overly long tool results. When a result
# exceeds max_length chars, the full output is saved to a temp file
# and only head/tail chars are kept in the context.
tool_result_truncate:
  max_length: 100000                       # Trigger threshold (chars). null or <0 disables
  keep_chars: 200                          # Chars to keep at head and tail

# ── Logging ───────────────────────────────────────────
log:
  level: "WARNING"                         # Log level (DEBUG/INFO/WARNING/ERROR/CRITICAL)

# ── Gateway ───────────────────────────────────────────
# Used when launching wing-gateway directly (standalone).
# The Rust TUI reads gateway settings from ~/.wing/tui/config.yaml.
gateway:
  host: "127.0.0.1"                        # Listen address
  port: 32523                              # Listen port
  auth:                                    # Opt-in API key auth (default: disabled)
    enabled: false                         # Master switch
    keys:
      - key: "my-secret"                   # ASCII printable; sent by clients
        role: admin                        # Reserved for future RBAC (not enforced)

# ── Prompt Commands ───────────────────────────────────
commands:
  paths: []                                # Additional prompt command (.md) file paths

# ── User Agent ────────────────────────────────────────
user_agent:
  preset: "qwen-code"                      # Client identity preset (opencode | qwen-code)
```

## Field Details

### openai

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `base_url` | string | *(required)* | OpenAI-compatible API endpoint URL |
| `api_key` | string | *(required)* | API key |
| `timeout_first_chunk` | float | 300.0 | Streaming first-chunk timeout in seconds. Some providers have high latency before the first token when tools are involved. |
| `timeout_total` | float | 600.0 | Total timeout for non-streaming responses. |
| `explicit_cache_mode` | bool | true | When enabled, appends `cache_control: {"type": "ephemeral"}` markers to the last content block in each request. Providers that support prompt caching will use these; unsupported providers silently ignore them. |
| `reasoning_effort` | string? | null | Controls reasoning depth. Values: `low`, `medium`, `high`, `xhigh`, `max`. When null, the parameter is not sent and the provider uses its default. |

### agents

Each entry defines an agent template. You can have multiple templates and switch between them with `/agents <name>`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | *(required)* | Unique template name |
| `model` | string | *(required)* | Model identifier sent to the API |
| `default` | bool | false | Use as default when no template is specified |
| `system_prompt` | string | "" | System prompt. **Empty string means no system prompt is injected** — wing never adds hidden prompts. |
| `tools` | list[string] | [] | Tool names to enable. See [Built-in Tools](#built-in-tools). |
| `context_window_tokens` | int | 256000 | Token limit before context compaction triggers. |
| `keep_recent_tokens` | int | 50000 | How many recent tokens to preserve after compaction. |
| `skills` | list[string] | [] | Glob patterns for skill files (Markdown). Skills are injected into the system prompt. |
| `rules` | list[string] | [] | Glob patterns for rule files (Markdown). Rules are injected into the system prompt. |
| `max_turns` | int? | null | Max agent loop turns per request (null = unlimited). |
| `yolo` | bool? | null | Per-agent override for dangerous-command review (null = inherit global `yolo`). |

### hooks

Glob patterns for Python hook files. Hooks extend wing's behavior at defined extension points: `before_session_start`, `before_user_message`, `before_tool_call`, `after_tool_call`.

See [Custom Tools & Hooks](custom-tools.md) for details.

### safe_command_patterns

Regex patterns for bash commands that should be auto-approved without user confirmation. Example:

```yaml
safe_command_patterns:
  - "^ls "
  - "^cat "
  - "^git status"
  - "^git log"
```

Without matching patterns, all commands require user confirmation (or `yolo: true`).

### gateway

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | "127.0.0.1" | Listen address when launching `wing-gateway` directly. |
| `port` | int | 32523 | Listen port. |
| `auth.enabled` | bool | false | Master switch for API key authentication. |
| `auth.keys` | list | [] | List of `{key, role}` entries. `key` must be ASCII printable; `role` is reserved for future RBAC (not enforced). |

> **Note:** When using the Rust TUI (`wing`), gateway settings are read from `~/.wing/tui/config.yaml` instead. Set `api_key` there so the client sends it with every HTTP/WS request. `/api/health` is always exempt. Encryption (TLS) is delegated to a reverse proxy.

### tool_result_truncate

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_length` | int? | 100000 | Trigger threshold in chars. When a tool result exceeds this, the full output is saved to a temp file and only head/tail are kept in context. `null` or `<0` disables. |
| `keep_chars` | int | 200 | Chars to keep at head and tail when truncating (must be ≥ 0). |

### user_agent

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `preset` | string | "qwen-code" | Client identity preset (`opencode` \| `qwen-code`). Controls identifying headers sent to the provider. |

### commands

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `paths` | list[string] | [] | Additional prompt command (`.md`) file paths, beyond the default discovery locations. |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `WING_HOME` | Override home directory (default: `~/.wing`). Backend data goes to `$WING_HOME/core/`, frontend to `$WING_HOME/tui/`. |
| `WING_SESSIONS_PATH` | Override session storage directory |

## Built-in Tools

| Tool | Description |
|------|-------------|
| `Bash` | Execute shell commands with safety review |
| `Read` | Read file contents with optional line range |
| `Write` | Create or overwrite files |
| `Edit` | Surgical string replacement in files |
| `Glob` | Find files by glob pattern |
| `Grep` | Search file contents with regex |
| `AskUserQuestion` | Ask the user a question |
| `TodoWrite` | Track task progress |
| `Explorer` | Autonomous code exploration sub-agent (blocking or background) |
| `BetterEdit` | Anchored `[upto]` edits (experimental) |
