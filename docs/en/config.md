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
preserved_thinking: true                   # Keep reasoning content (don't clear)

safe_command_patterns: []                  # Regex whitelist for auto-approved bash commands
                                           # e.g. ["^ls ", "^cat "]

# ── Logging ───────────────────────────────────────────
log:
  level: "WARNING"                         # Log level (DEBUG/INFO/WARNING/ERROR/CRITICAL)

# ── Gateway ───────────────────────────────────────────
# Used when launching wing-gateway directly (standalone).
# The Rust TUI reads gateway settings from ~/.wing/tui/config.yaml.
gateway:
  host: "127.0.0.1"                        # Listen address
  port: 32523                              # Listen port

# ── Prompt Commands ───────────────────────────────────
commands:
  paths: []                                # Additional magic command file paths

# ── User Agent ────────────────────────────────────────
user_agent:
  preset: "opencode"                       # Client identity preset (opencode | qwen-code)
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

### hooks

Glob patterns for Python hook files. Hooks extend wing's behavior at defined extension points (e.g., `after_tool_call`, `before_llm_call`).

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

> **Note:** When using the Rust TUI (`wing`), gateway settings are read from `~/.wing/tui/config.yaml` instead.

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
| `Explorer` | Autonomous code exploration agent |
