# Magic Commands Reference

Magic commands are slash commands you type in the TUI input area. Type `/` to open the command palette with fuzzy search and parameter hints.

## How they work

Despite the name, there is no backend "magic dispatch" anymore. A slash command resolves to one of three paths:

| Path | What happens | Examples |
|------|--------------|---------|
| **Frontend → HTTP** | The TUI intercepts the command and calls a Gateway HTTP endpoint | `/compact`, `/model`, `/rewind`, `/reload` |
| **Frontend-local** | Handled entirely in the TUI, never sent to the gateway | `/clear`, `/copy` |
| **Prompt expansion** | A user `.md` file is expanded (`$ARGUMENTS` replaced) and sent as a normal message | custom `/plan`, etc. |

> **Interrupt is not a slash command** — press **Esc** to interrupt the current agent turn (`POST /api/session/interrupt` under the hood).

## Command reference

| Command | Alias | Params | Description |
|---------|-------|--------|-------------|
| `/clear` | | | Clear the chat view |
| `/copy` | | `[N]` | Copy the last (or Nth) assistant message to clipboard |
| `/new` | | `[name]` | Create a new session |
| `/session` | `/ss` | `[session_id]` | Switch to a session, or list all sessions |
| `/fork` | | `<uuid>` | Fork a new session from a specific message |
| `/model` | `/m` | `[name]` | Show or switch the model |
| `/agents` | | `[name]` | Show or switch the agent template |
| `/title` | | `[name]` | Show or set the session title |
| `/workdir` | | `<path>` | Switch the working directory |
| `/think` | `/t` | `on\|off\|low\|medium\|high\|xhigh\|max` | Toggle thinking / set reasoning effort |
| `/yolo` | | `on\|off` | Toggle YOLO mode (skip dangerous-command review) |
| `/compact` | | | Compress the session context |
| `/context` | | | Show context stats and the system prompt |
| `/skills` | | | Show loaded skills |
| `/rewind` | | `<uuid>` | Rewind to a specific message (discards later messages) |
| `/reload` | | | Reload config, hooks, provider, skills (no restart needed) |
| `/goal` | | `<prompt>` | Start Goal orchestration (executor + checker loop) |
| `/goal-exit` | | | Exit Goal orchestration mode |

The source of truth for this list is `crates/wing/src/ui/popup/command.rs` (`TUI_ONLY_COMMANDS`).

## Session management

```
/new my-project        # create a named session
/ss                    # list all sessions
/ss abc123             # switch to session abc123
/fork a1b2c3d4         # fork from the message with this uuid
/rewind a1b2c3d4       # rewind to this message (discards everything after)
/title refactor-auth   # rename the current session
/workdir ~/code/other  # change working directory
```

## Model, agent & behavior

```
/model                 # show model selection popup
/model gpt-4           # switch model
/agents coder          # switch to the "coder" template
/think high            # set reasoning effort
/think off             # disable thinking
/yolo on               # skip command safety review (dangerous!)
```

## Context & maintenance

```
/compact               # summarize old messages, keep recent ones
/context               # message count, token usage, system prompt
/skills                # list loaded skill files
/reload                # pick up config/hooks/skills changes without restarting
```

## Goal orchestration

`/goal` pairs an **executor** (the current session) with an independent **checker** session that verifies the result in a loop, so the agent no longer judges its own work. See [docs/dev/architecture.md](../dev/architecture.md) for the mechanism.

```
/goal refactor the auth module and add tests
/goal-exit             # leave Goal mode
```

## Prompt commands

Beyond the built-in commands, wing supports **prompt commands** — custom commands defined as Markdown files. The file body (excluding YAML frontmatter) becomes the prompt; `$ARGUMENTS` is replaced with what you type after the command.

```yaml
# ~/.wing/core/config.yaml
commands:
  paths:
    - "~/.wing/commands/*.md"
```

Example `~/.wing/commands/plan.md`:

```markdown
---
description: Plan an implementation
---
Create a detailed implementation plan for: $ARGUMENTS
```

Then `/plan auth feature` sends the expanded text to the agent as a normal message. Prompt commands are the only commands returned by `GET /api/commands`.
