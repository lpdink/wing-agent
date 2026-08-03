# wing/default_config.py
"""Default configuration template for wing-agent.

This module contains a hand-maintained YAML string that serves as the
default configuration template. When a user's config file does not exist,
this template is written to ``$WING_HOME/core/config.yaml``.

Required fields use ``ChangeHere`` as a placeholder — the user MUST
replace these before the agent can start.

# SYNC: If Config gains new fields, this template must be updated manually.
"""

DEFAULT_CONFIG_YAML = """\
# ──────────────────────────────────────────────────────────────
# wing-agent configuration
# Location: $WING_HOME/core/config.yaml (default: ~/.wing/core/config.yaml)
# ──────────────────────────────────────────────────────────────

# ── LLM Providers ───────────────────────────────────────────
# Configure one or more LLM providers. Each provider declares
# its protocol (openai or anthropic), endpoint, and credentials.
providers:
  - name: default
    protocol: openai          # openai | anthropic
    base_url: ChangeHere      # e.g. https://api.openai.com/v1
    api_key: ChangeHere       # e.g. sk-xxx

    # Streaming first-chunk timeout (seconds).
    timeout_first_chunk: 300.0

    # Total response timeout for non-streaming calls (seconds).
    timeout_total: 600.0

    # Maximum retries for LLM calls on failure.
    max_retries: 10

    # Exponential backoff max retry delay (seconds); backoff is clamped
    # to this upper bound (default 3 minutes).
    max_retry_delay: 180.0

    # Explicit cache mode: appends cache_control ephemeral markers
    # to the last content block. Silently ignored by providers
    # that don't support it.
    explicit_cache_mode: true

    # Reasoning effort level. Options: low / medium / high / max.
    # Set to null to let the provider decide.
    reasoning_effort: null

    # Extra fields merged into the request body top-level.
    # Use for provider-specific parameters.
    #
    # For openai protocol, enable_thinking / preserve_thinking default
    # to true and are sent on every request — override here if needed:
    #   extra_body:
    #     enable_thinking: false
    #
    # Example for DeepSeek:
    #   extra_body:
    #     thinking:
    #       type: enabled
    extra_body: {}

  # Additional provider example (Anthropic):
  # - name: claude
  #   protocol: anthropic
  #   base_url: https://api.anthropic.com
  #   api_key: sk-ant-xxx
  #   anthropic_version: "2023-06-01"
  #   max_tokens: 8192            # REQUIRED for Anthropic (max output tokens)
  #   # Static model list — skips remote GET /models when set.
  #   models:
  #     - claude-sonnet-4-20250514
  #   # Extended thinking (drives the thinking toggle + interleaved beta header):
  #   extra_body:
  #     thinking:
  #       type: enabled
  #       budget_tokens: 4096

# ── Agent Templates ──────────────────────────────────────────
# At least one agent is required. Each agent defines a model,
# tool set, and optional system prompt.
agents:
  - name: default
    model: ChangeHere      # e.g. gpt-4, qwen-max, etc.
    # provider: default    # references providers[].name (defaults to first)

    # Mark as default agent (used when no agent is explicitly selected).
    default: true

    # System prompt (prepended to every conversation).
    system_prompt: ""

    # Tools available to this agent. Must be registered tool names.
    tools:
      - Bash
      - Read
      - Write
      - Edit
      - Glob
      - Grep
      - AskUserQuestion
      - TodoWrite
      - Explorer

    # Context window token limit. Compression is applied when reached.
    context_window_tokens: 256000

    # Number of recent tokens to keep after compression.
    keep_recent_tokens: 50000

    # Skills glob patterns — each match loads a SKILL.md file as agent context.
    skills:
      - ~/.agents/skills/*/SKILL.md
      - .claude/skills/*/SKILL.md
      - .agents/skills/*/SKILL.md

    # Rules glob patterns — each match loads a markdown file as agent rules.
    rules:
      - AGENTS.md

# ── Hooks ────────────────────────────────────────────────────
# Glob patterns for hook files. Hooks are Python modules that
# register handlers via the wing hook API.
hooks: []

# ── Bash Safety ──────────────────────────────────────────────
# Regex patterns for commands that are auto-approved (no user
# confirmation needed). Example: "^git\\s+(status|log|diff)"
safe_command_patterns: []

# Skip dangerous command safety checks entirely.
# Use with caution — all commands execute without confirmation.
yolo: false

# ── Agent Behavior ───────────────────────────────────────────
# Enable steer mode (guides agent behavior with steering prompts).
steer: true

# Preserve reasoning content in thinking blocks (don't strip).
preserved_thinking: true

# ── Tool Result Truncation ───────────────────────────────────
# Built-in truncation for overly long tool results. When a tool
# result exceeds max_length chars, the full output is saved to a
# temp file and only head/tail chars are kept in the context.
# Set max_length to null or a negative value to disable.
tool_result_truncate:
  max_length: 100000  # trigger threshold (chars). null or <0 disables
  keep_chars: 200     # chars to keep at head and tail

# ── Logging ──────────────────────────────────────────────────
log:
  # Log level: DEBUG, INFO, WARNING, ERROR, CRITICAL.
  level: WARNING

# ── Gateway ──────────────────────────────────────────────────
# WebSocket server settings when launching wing-gateway directly.
# The Rust TUI reads gateway settings from its own config
# (~/.wing/tui/config.yaml), so these only apply to standalone
# wing-gateway usage.
gateway:
  host: 127.0.0.1
  port: 32523

  # Remote tool call total timeout (seconds). A safety net, NOT a
  # short timeout — remote tools (e.g. Bash) may run for a long time.
  # Connection close is the primary failure signal (in-flight calls
  # fail immediately on WS disconnect); this only guards the extreme
  # case of a silently dead client that never closes the connection.
  remote_tool_timeout: 1800.0

  # API key authentication. When enabled, all HTTP/WS requests
  # (except /api/health) must carry a valid key via
  # "Authorization: Bearer <key>" or "X-API-Key: <key>" header.
  # WS also accepts ?api_key=<key> query parameter (note: query
  # params may appear in reverse proxy access logs).
  # Keys must be ASCII printable characters only.
  # WARNING: enabled=true with empty keys locks out ALL requests.
  auth:
    enabled: false
    keys: []
    # Example:
    # keys:
    #   - key: "my-secret-key"
    #     role: admin        # admin = full access; tool_runtime = may only
    #                        # register remote tools (and hold its WS)

# ── Prompt Commands ──────────────────────────────────────────
# Paths to directories containing prompt command definition files.
# Each .yaml file in these directories defines a slash command.
commands:
  paths: []

# ── User-Agent ───────────────────────────────────────────────
# Preset for HTTP User-Agent header. Options: opencode, qwen-code.
user_agent:
  preset: opencode
"""
