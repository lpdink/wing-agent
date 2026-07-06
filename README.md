# wing-agent

<p align="center">
  <strong>Towards general agent runtime.</strong>
</p>

<p align="center">
  <a href="https://pypi.org/project/wing-agent/"><img src="https://img.shields.io/pypi/v/wing-agent" alt="PyPI"></a>
  <a href="https://pypi.org/project/wing-agent/"><img src="https://img.shields.io/pypi/pyversions/wing-agent" alt="Python"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
</p>

**[中文文档](docs/zh/README.md)**

> **⚠️ Experimental** — Expect breaking changes until v1.0.

## Why wing?

**No magic in your context.** We never inject hidden system prompts. You see exactly what the model sees — your system prompt, your tools, your conversation. Nothing more.

**Maximum cache hit rate.** We commit to the theoretical maximum prompt caching. Beyond compaction, we never break your cache prefix.

**Minimal tool schemas.** Our built-in tools use the simplest possible schemas. Your context window starts with under 2K tokens of tool overhead — not 10K.

## Quick Start

```bash
pip install wing-agent
wing
```

On first run, wing creates a config template at `~/.wing/core/config.yaml` and exits. Open it and fill in **three fields**:

```yaml
llm:
  base_url: "https://your-api-endpoint/v1"   # ← your provider
  api_key: "sk-xxx"                          # ← your key
  model: "gpt-4o"                            # ← your model
```

Then start wing:

```bash
wing stop    # stop the gateway if it was already running
wing         # start fresh
```

> **Note:** The gateway loads config at startup. After editing `config.yaml`, always `wing stop` then `wing` to pick up changes. Hot-reload is tracked in [#xx](docs/known_issues.md).

## Configuration

Backend config: `~/.wing/core/config.yaml`
Frontend config: `~/.wing/tui/config.yaml`

Full reference: **[docs/en/config.md](docs/en/config.md)**

## Built-in Tools

| Tool | Description |
|------|-------------|
| `Bash` | Execute shell commands with safety review |
| `Read` | Read file contents with line range |
| `Write` | Create or overwrite files |
| `Edit` | Surgical string replacement in files |
| `Glob` | Find files by pattern |
| `Grep` | Search file contents with regex |
| `AskUserQuestion` | Ask the user a question |
| `TodoWrite` | Track task progress |
| `Explorer` | Autonomous code exploration agent |

Custom tools: **[docs/en/custom-tools.md](docs/en/custom-tools.md)**

## Magic Commands

Type `/` in the TUI to see available commands.

Full reference: **[docs/en/magic-commands.md](docs/en/magic-commands.md)**

## Documentation

| Document | English | 中文 |
|----------|---------|------|
| Configuration | [docs/en/config.md](docs/en/config.md) | [docs/zh/config.md](docs/zh/config.md) |
| Custom Tools | [docs/en/custom-tools.md](docs/en/custom-tools.md) | [docs/zh/custom-tools.md](docs/zh/custom-tools.md) |
| Magic Commands | [docs/en/magic-commands.md](docs/en/magic-commands.md) | [docs/zh/magic-commands.md](docs/zh/magic-commands.md) |

## License

[Apache-2.0](LICENSE)
