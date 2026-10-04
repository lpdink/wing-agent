<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/banner-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/banner-light.svg">
    <img src="assets/banner-dark.svg" alt="wing — pixel seagull mascot and WING wordmark" width="620">
  </picture>
</p>

# wing-agent

<p align="center">
  <strong>Towards general agent runtime.</strong>
</p>

<p align="center">
  <a href="https://pypi.org/project/wing-agent/"><img src="https://img.shields.io/pypi/v/wing-agent" alt="PyPI"></a>
  <a href="https://pypi.org/project/wing-agent/"><img src="https://img.shields.io/pypi/pyversions/wing-agent" alt="Python"></a>
  <a href="https://github.com/lpdink/wing-agent/blob/develop/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
</p>

**[中文文档](https://github.com/lpdink/wing-agent/blob/develop/docs/zh/README.md)**

> **⚠️ Experimental** — Expect breaking changes until v1.0.

<p align="center">
  <img src="https://github.com/lpdink/wing-agent/releases/download/readme-assets/demo.gif" alt="wing's TUI running a task: thinking streams in, an Edit fills in a colored diff, the test suite goes from red to green, and the todo list checks itself off" width="920">
</p>

## Why wing?

**Your context is yours.** No hidden system prompts, no scaffolding injected behind your back. What the model sees is what you wrote — your prompt, your tools, your history. When a run goes sideways, you can read exactly what happened.

**Cache-first by construction.** We never rewrite the prefix a provider can cache: apart from compaction, the conversation only grows. Long sessions stay fast and cheap instead of being re-processed from scratch every turn.

**Lean tool schemas.** The built-in tools use the smallest schemas that still do the job — under 2K tokens of tool overhead in your window, not 10K. Fewer tokens spent describing tools is more room for your code.

## Built for machine speed

The TUI never re-renders the whole answer: markdown blocks are promoted once when they
close, and every frame re-renders only the live tail. A frame costs what is *on screen*,
not what the answer has grown to.

Same text, same terminal, four feed rates — scripted feeds, real rendering:

**30 tok/s** — a top-tier reasoning model.
<p align="center"><img src="https://github.com/lpdink/wing-agent/releases/download/readme-assets/speed-30.gif" alt="wing streaming at 30 tokens per second" width="900"></p>

**60 tok/s** — a current flagship.
<p align="center"><img src="https://github.com/lpdink/wing-agent/releases/download/readme-assets/speed-60.gif" alt="wing streaming at 60 tokens per second" width="900"></p>

**240 tok/s** — a fast "flash" tier.
<p align="center"><img src="https://github.com/lpdink/wing-agent/releases/download/readme-assets/speed-240.gif" alt="wing streaming at 240 tokens per second" width="900"></p>

**3,000 tok/s** — about 10× the fastest models shipping today. The UI does not care.
<p align="center"><img src="https://github.com/lpdink/wing-agent/releases/download/readme-assets/speed-3000.gif" alt="wing streaming at 3000 tokens per second" width="900"></p>

End-to-end display latency — provider → gateway → WebSocket → TUI → terminal, one token
per frame (M6 Mac mini):

| Feed rate | p50 | p99 |
|---|---|---|
| 3,000 tok/s | 17 ms | 28 ms |
| 30,000 tok/s | 13 ms | 22 ms |
| 45,000 tok/s | 10 ms | 30 ms |

Reproduce it: `uv run python scripts/demo/latency.py --steps 3000,30000,45000 --marker-every 2000 --seconds 10`.

## Quick Start

```bash
pip install wing-agent
wing
```

On first run, wing creates a config template at `~/.wing/core/config.yaml` and exits. Open it and fill in your **provider, key, and model**:

```yaml
providers:
  - name: default
    protocol: openai                         # openai | anthropic
    base_url: "https://your-api-endpoint/v1" # ← your provider
    api_key: "sk-xxx"                        # ← your key

agents:
  - name: default
    model: "gpt-4o"                          # ← your model
    default: true
    tools: [Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, TodoWrite, Explorer]
```

Then start wing:

```bash
wing stop    # stop the gateway if it was already running
wing         # start fresh
```

> **Note:** The gateway loads config at startup. After editing `config.yaml`, run `/reload` in the TUI (or `wing stop` then `wing`) to pick up changes.

## Configuration

Backend config: `~/.wing/core/config.yaml` — generated on first run, fully annotated (see `wing/default_config.py` for the template).
Frontend config: `~/.wing/tui/config.yaml`

## Built-in Tools

| Tool | Description |
|------|-------------|
| `Bash` | Execute shell commands with safety review |
| `Read` | Read file contents with line range |
| `Write` | Create or overwrite files |
| `Edit` | Surgical string replacement in files |
| `Glob` | Find files by pattern |
| `Grep` | Search file contents with regex |
| `ReadImage` | Feed an image (screenshot, diagram, chart) to a vision model |
| `AskUserQuestion` | Ask the user a question |
| `TodoWrite` | Track task progress |
| `Explorer` | Autonomous code exploration sub-agent (blocking or background) |
| `BetterEdit` | Anchored `[upto]` edits (experimental) |

Custom tools: **[docs/en/custom-tools.md](https://github.com/lpdink/wing-agent/blob/develop/docs/en/custom-tools.md)**

A terminal UI that renders what the model actually produces, not a log of it: streaming
markdown with syntax highlighting, LaTeX (`$…$`, `$$…$$`, AMS environments) composed into
the character grid, and local images drawn inline where the terminal speaks a graphics
protocol (kitty / iTerm2) or clickable where it doesn't. The same goes the other way:
`ReadImage` hands a screenshot or chart to a vision model without ever putting image bytes
in the transcript.

Tool calls render where they happen: an `Edit` grows a syntax-highlighted diff against the
real file, a `Bash` run shows its real output, and `TodoWrite` keeps the plan visible while
the work moves forward — all three are in the demo at the top of this page.

## Magic Commands

Type `/` in the TUI to see available commands.

Full reference: **[docs/en/magic-commands.md](https://github.com/lpdink/wing-agent/blob/develop/docs/en/magic-commands.md)**

## Headless mode (stdio)

`wing` also runs headless with a Claude Code compatible stdio protocol — alias `wing` as `claude` to plug into external orchestrators, or drive it from scripts:

```bash
wing -p "list the files in this directory"                    # text (default): final result only
wing -p "list files" --output-format json                     # single result JSON object
wing -p "list files" --output-format stream-json              # real-time NDJSON stream
```

Useful flags: `-m/--model`, `-r/--resume`, `--system-prompt`, `--append-system-prompt`, `--max-turns`, `--effort`, `--input-format`, `--yolo`. Unknown `--xxx` flags are ignored for Claude compatibility.

## Documentation

| Document | English | 中文 |
|----------|---------|------|
| Custom Tools | [docs/en/custom-tools.md](https://github.com/lpdink/wing-agent/blob/develop/docs/en/custom-tools.md) | [docs/zh/custom-tools.md](https://github.com/lpdink/wing-agent/blob/develop/docs/zh/custom-tools.md) |
| Magic Commands | [docs/en/magic-commands.md](https://github.com/lpdink/wing-agent/blob/develop/docs/en/magic-commands.md) | [docs/zh/magic-commands.md](https://github.com/lpdink/wing-agent/blob/develop/docs/zh/magic-commands.md) |

## Developing

Start with **[AGENTS.md](https://github.com/lpdink/wing-agent/blob/develop/AGENTS.md)** (high-density project overview). For mechanism-level deep dives (data flow, full HTTP API, glossary), see **[docs/dev/](https://github.com/lpdink/wing-agent/tree/develop/docs/dev/)** (中文).

## License

[Apache-2.0](https://github.com/lpdink/wing-agent/blob/develop/LICENSE)
